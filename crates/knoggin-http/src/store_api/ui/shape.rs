// SPDX-License-Identifier: AGPL-3.0-only
//! The shaping of the UI read models over one store, under its read lock: ported from
//! recalld's `src/ui/daemon.rs`, `src/ui/memories.rs` and `src/health/report.rs`. Each
//! function reads; none records access or changes anything.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use knoggin_core::decay::fsrs::effective_retrievability;
use knoggin_core::model::{AccessKind, Memory, Phase};
use knoggin_core::ops::{Detail, FullHit, Recall, RecallResponse};
use knoggin_core::time::{MILLIS_PER_DAY, elapsed_days};
use knoggin_core::{MemoryId, NamespaceId, Store, Timestamp};

use super::wire::*;
use crate::error::ApiError;
use crate::host::live::UiEvent;

/// Largest edge page of the graph answer.
pub(crate) const MAX_EDGE_PAGE: usize = 200_000;
/// Largest neighborhood.
const MAX_NEIGHBORS: usize = 150;
/// `19 / 81`, the forgetting curve's factor (`decay::fsrs`).
const FACTOR: f32 = 19.0 / 81.0;

/// The namespace a `/v1/ui/*` route names; `None` (every namespace) when it names none.
pub(crate) fn scope(s: &Store, name: Option<&str>) -> Result<Option<NamespaceId>, ApiError> {
    match name {
        None => Ok(None),
        Some(name) => s
            .namespace_by_name(name)
            .map(|n| Some(n.id))
            .ok_or_else(|| ApiError::not_found("No such namespace")),
    }
}

fn in_scope(scope: Option<NamespaceId>, m: &Memory) -> bool {
    scope.is_none_or(|ns| ns == m.namespace)
}

fn namespace_name(s: &Store, id: NamespaceId) -> String {
    s.namespace(id).map(|n| n.name.clone()).unwrap_or_default()
}

fn summary(m: &Memory) -> Option<String> {
    (m.phase != Phase::Ghost).then(|| m.summary.clone())
}

fn phase_number(p: Phase) -> u8 {
    match p {
        Phase::Full => 1,
        Phase::Summary => 2,
        Phase::Ghost => 3,
    }
}

/// `now` plus `days`, kept inside the times the API can write.
fn after_days(now: Timestamp, days: f32) -> Timestamp {
    let ms = (f64::from(days) * MILLIS_PER_DAY as f64).min(Timestamp::MAX_API.millis() as f64);
    Timestamp::from_millis(
        now.millis()
            .saturating_add(ms as i64)
            .min(Timestamp::MAX_API.millis()),
    )
}

/// The next phase change of `m`, as the sweep decides it: days from `now` (0 when the
/// next sweep moves it) and the phase it goes to. `None` when the sweep never moves it
/// (permanent, a multiplier of 0, or a bonus that keeps it above its threshold).
/// The connection bonus is taken as it is now.
pub(crate) fn next_change(s: &Store, m: &Memory, now: Timestamp) -> Option<(f32, &'static str)> {
    let policy = s.namespace(m.namespace)?.policy;
    let multiplier = policy.multiplier();
    if m.is_permastore || multiplier == 0.0 || m.stability.is_nan() || m.stability <= 0.0 {
        return None;
    }
    let th = policy.phase_thresholds;
    let raw = s.raw_strength(m, now);
    let (target, bonus, to) = match m.phase {
        Phase::Full => (th.full_to_summary, s.connection_bonus(m.id, now), "summary"),
        Phase::Summary => (th.summary_to_ghost, s.connection_bonus(m.id, now), "ghost"),
        Phase::Ghost => (th.ghost_to_delete, 0.0, "deleted"),
    };
    if effective_retrievability(raw, bonus) <= target {
        return Some((0.0, to));
    }
    // R + b(1 - R) <= t  <=>  R <= (t - b) / (1 - b); then invert R(t) = (1 + F t / (m S))^-0.5.
    let raw_target = (target - bonus) / (1.0 - bonus);
    if raw_target.is_nan() || raw_target <= 0.0 {
        return None;
    }
    let crossing = multiplier * m.stability * (raw_target.powi(-2) - 1.0) / FACTOR;
    let elapsed = elapsed_days(now, m.decay_anchor_at);
    Some(((crossing - elapsed).max(0.0), to))
}

/// One Explore row.
pub(crate) fn row(s: &Store, m: &Memory, now: Timestamp) -> MemoryRow {
    let multiplier = s
        .namespace(m.namespace)
        .map_or(1.0, |n| n.policy.multiplier());
    MemoryRow {
        id: m.id,
        namespace: namespace_name(s, m.namespace),
        summary: summary(m),
        phase: m.phase,
        strength: s.raw_strength(m, now),
        decay_multiplier: multiplier,
        stability: m.stability,
        created_at: m.created_at,
        retrieved_at: m.retrieved_at,
        permastore: m.is_permastore,
        degree: u32::try_from(s.graph().degree(m.id)).unwrap_or(u32::MAX),
        next_change: next_change(s, m, now).map(|(days, to)| NextChange {
            at: after_days(now, days),
            to,
        }),
        tags: m.tags.iter().map(|t| t.as_str().to_owned()).collect(),
        entities: m.labels.entities.clone(),
    }
}

/// The filters and order of `GET /v1/ui/memories`.
#[derive(Default)]
pub(crate) struct RowQuery {
    pub(crate) namespace: Option<String>,
    pub(crate) phases: Vec<Phase>,
    pub(crate) tags: Vec<String>,
    pub(crate) entities: Vec<String>,
    pub(crate) clients: Vec<String>,
    pub(crate) ids: Vec<MemoryId>,
    pub(crate) sort: Sort,
    pub(crate) desc: Option<bool>,
    pub(crate) offset: usize,
    pub(crate) limit: usize,
}

/// Counts only: tags, entities, client names and the namespace are the user's words.
impl std::fmt::Debug for RowQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowQuery")
            .field("namespace", &self.namespace.is_some())
            .field("phases", &self.phases)
            .field("tags", &self.tags.len())
            .field("entities", &self.entities.len())
            .field("clients", &self.clients.len())
            .field("ids", &self.ids.len())
            .field("sort", &self.sort)
            .field("desc", &self.desc)
            .field("offset", &self.offset)
            .field("limit", &self.limit)
            .finish()
    }
}

/// Explore's orders.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Sort {
    #[default]
    LastAccess,
    Strength,
    Created,
    NextChange,
    Summary,
}

/// `GET /v1/ui/memories`. Tags and entities must all match (entities without regard to
/// case); phases, clients and IDs are alternatives.
pub(crate) fn memory_page(s: &Store, q: &RowQuery, now: Timestamp) -> Result<MemoryPage, ApiError> {
    let scope = scope(s, q.namespace.as_deref())?;
    let entities: Vec<String> = q.entities.iter().map(|e| e.to_lowercase()).collect();
    let wanted = |m: &&Memory| {
        in_scope(scope, m)
            && (q.phases.is_empty() || q.phases.contains(&m.phase))
            && (q.ids.is_empty() || q.ids.contains(&m.id))
            && (q.clients.is_empty() || m.client.as_ref().is_some_and(|c| q.clients.contains(c)))
            && q.tags
                .iter()
                .all(|t| m.tags.iter().any(|x| x.as_str() == t))
            && entities
                .iter()
                .all(|e| m.labels.entities.iter().any(|x| x.to_lowercase() == *e))
    };
    // ponytail: every request builds and sorts every row; cache by change counters if
    // Explore gets slow on large stores.
    let mut rows: Vec<MemoryRow> = match q.ids.is_empty() {
        true => s
            .memories()
            .filter(wanted)
            .map(|m| row(s, m, now))
            .collect(),
        false => q
            .ids
            .iter()
            .filter_map(|id| s.memory(*id))
            .filter(wanted)
            .map(|m| row(s, m, now))
            .collect(),
    };
    match q.sort {
        Sort::LastAccess => rows.sort_by_key(|r| r.retrieved_at.unwrap_or(r.created_at)),
        Sort::Strength => rows.sort_by(|a, b| a.strength.total_cmp(&b.strength)),
        Sort::Created => rows.sort_by_key(|r| r.created_at),
        Sort::NextChange => {
            rows.sort_by_key(|r| r.next_change.as_ref().map_or(Timestamp::MAX_API, |n| n.at))
        }
        Sort::Summary => {
            rows.sort_by_cached_key(|r| r.summary.as_deref().unwrap_or("\u{10FFFF}").to_lowercase())
        }
    }
    let desc = q
        .desc
        .unwrap_or(matches!(q.sort, Sort::LastAccess | Sort::Created));
    if desc {
        rows.reverse();
    }
    let total = rows.len() as u64;
    Ok(MemoryPage {
        total,
        offset: q.offset as u64,
        rows: rows.into_iter().skip(q.offset).take(q.limit).collect(),
    })
}

/// The 200 most frequent values of each filter.
pub(crate) fn facets(s: &Store, namespace: Option<&str>) -> Result<Facets, ApiError> {
    let scope = scope(s, namespace)?;
    let (mut clients, mut entities, mut tags) = (
        BTreeMap::<&str, u64>::new(),
        BTreeMap::<&str, u64>::new(),
        BTreeMap::<&str, u64>::new(),
    );
    for m in s.memories().filter(|m| in_scope(scope, m)) {
        if let Some(c) = &m.client {
            *clients.entry(c).or_default() += 1;
        }
        for e in &m.labels.entities {
            *entities.entry(e).or_default() += 1;
        }
        for t in &m.tags {
            *tags.entry(t.as_str()).or_default() += 1;
        }
    }
    let top = |map: BTreeMap<&str, u64>| {
        let mut v: Vec<Facet> = map
            .into_iter()
            .map(|(name, count)| Facet {
                name: name.to_owned(),
                count,
            })
            .collect();
        v.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
        v.truncate(200);
        v
    };
    Ok(Facets {
        clients: top(clients),
        entities: top(entities),
        tags: top(tags),
    })
}

fn not_found_memory() -> ApiError {
    ApiError::not_found("No such memory")
}

/// `GET /v1/ui/memories/{id}/history`, newest first.
pub(crate) fn history(s: &Store, id: MemoryId, limit: usize) -> Result<AccessHistory, ApiError> {
    let m = s.memory(id).ok_or_else(not_found_memory)?;
    Ok(AccessHistory {
        id,
        events: m.access_history.iter().rev().take(limit).copied().collect(),
        total: m.access_history.len() as u64,
    })
}

/// `GET /v1/ui/memories/{id}/neighbors`: the memories within `depth` links, at most 150.
pub(crate) fn neighbors(s: &Store, center: MemoryId, depth: u8) -> Result<Neighborhood, ApiError> {
    if s.memory(center).is_none() {
        return Err(not_found_memory());
    }
    let graph = s.graph();
    let mut hops: BTreeMap<MemoryId, u8> = BTreeMap::from([(center, 0)]);
    let mut frontier = vec![center];
    let mut truncated = false;
    'walk: for hop in 1..=depth {
        let mut next = Vec::new();
        for id in &frontier {
            for e in graph.edges_of(*id) {
                let other = if e.source == *id { e.target } else { e.source };
                if hops.contains_key(&other) {
                    continue;
                }
                if hops.len() >= MAX_NEIGHBORS {
                    truncated = true;
                    break 'walk;
                }
                hops.insert(other, hop);
                next.push(other);
            }
        }
        frontier = next;
    }
    let mut edges = Vec::new();
    let mut seen = HashSet::new();
    for id in hops.keys() {
        for e in graph.edges_of(*id) {
            if hops.contains_key(&e.source) && hops.contains_key(&e.target) && seen.insert(e.key())
            {
                edges.push(NeighborEdge {
                    src: e.source,
                    dst: e.target,
                    kind: e.edge_type,
                    weight: e.weight,
                });
            }
        }
    }
    let mut nodes: Vec<NeighborNode> = hops
        .iter()
        .filter_map(|(id, hop)| {
            let m = s.memory(*id)?;
            Some(NeighborNode {
                id: *id,
                namespace: namespace_name(s, m.namespace),
                summary: summary(m),
                phase: m.phase,
                hop: *hop,
            })
        })
        .collect();
    nodes.sort_by_key(|n| n.hop);
    Ok(Neighborhood {
        center,
        nodes,
        edges,
        truncated,
    })
}

fn b64(bytes: impl IntoIterator<Item = impl AsRef<[u8]>>) -> String {
    let mut out = Vec::new();
    for b in bytes {
        out.extend_from_slice(b.as_ref());
    }
    STANDARD.encode(out)
}

/// `GET /v1/ui/graph`, with edges from `edge_offset`, at most [`MAX_EDGE_PAGE`] a page.
pub(crate) fn graph(
    s: &Store,
    namespace: Option<&str>,
    edge_offset: usize,
    now: Timestamp,
) -> Result<GraphSnapshot, ApiError> {
    let scope = scope(s, namespace)?;
    let namespaces = s
        .namespaces()
        .filter(|n| scope.is_none_or(|id| id == n.id))
        .map(|n| NamespaceRef {
            id: n.id.get(),
            name: n.name.clone(),
            thresholds: [
                n.policy.phase_thresholds.full_to_summary,
                n.policy.phase_thresholds.summary_to_ghost,
                n.policy.phase_thresholds.ghost_to_delete,
            ],
            permastore_days: n.policy.permastore_threshold,
            decay_multiplier: n.policy.multiplier(),
        })
        .collect();
    let first = edge_offset == 0;
    let members: Vec<&Memory> = s.memories().filter(|m| in_scope(scope, m)).collect();
    let index: HashMap<MemoryId, u32> = members
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id, u32::try_from(i).unwrap_or(u32::MAX)))
        .collect();
    let nodes = first.then(|| {
        let strengths: Vec<(f32, f32)> = members.iter().map(|m| s.strengths(m, now)).collect();
        SnapshotNodes {
            ids: members.iter().map(|m| m.id).collect(),
            ns: b64(members.iter().map(|m| m.namespace.get().to_le_bytes())),
            phase: b64(members.iter().map(|m| [phase_number(m.phase)])),
            strength: b64(strengths.iter().map(|s| s.0.to_le_bytes())),
            decay_strength: b64(strengths.iter().map(|s| s.1.to_le_bytes())),
            stability: b64(members.iter().map(|m| m.stability.to_le_bytes())),
            created_at: b64(members
                .iter()
                .map(|m| (m.created_at.millis() as f64).to_le_bytes())),
            last_access: b64(members
                .iter()
                .map(|m| (m.retrieved_at.map_or(0, Timestamp::millis) as f64).to_le_bytes())),
            degree: b64(members.iter().map(|m| {
                u32::try_from(s.graph().degree(m.id))
                    .unwrap_or(u32::MAX)
                    .to_le_bytes()
            })),
            permastore: b64(members.iter().map(|m| [u8::from(m.is_permastore)])),
        }
    });
    let (mut src, mut dst, mut kind, mut weight) = (vec![], vec![], vec![], vec![]);
    let mut total = 0usize;
    for e in s.graph().iter() {
        let (Some(&a), Some(&b)) = (index.get(&e.source), index.get(&e.target)) else {
            continue;
        };
        if total >= edge_offset && src.len() < MAX_EDGE_PAGE {
            src.push(a.to_le_bytes());
            dst.push(b.to_le_bytes());
            kind.push([e.edge_type.as_u8()]);
            weight.push(e.weight.to_le_bytes());
        }
        total += 1;
    }
    let end = edge_offset.saturating_add(src.len());
    Ok(GraphSnapshot {
        generated_at: now,
        namespaces,
        node_count: members.len() as u64,
        nodes,
        edges: SnapshotEdges {
            src: b64(src),
            dst: b64(dst),
            kind: b64(kind),
            weight: b64(weight),
        },
        edge_total: total as u64,
        edge_offset: edge_offset as u64,
        next_edge_offset: (end < total).then_some(end as u64),
    })
}

/// `GET /v1/ui/search`: a recall with access recording off and an explanation of each
/// hit, in the named namespace, or once in each namespace with the hits merged by score
/// (highest first), then by ID.
pub(crate) fn search(
    s: &Store,
    query: &str,
    namespace: Option<&str>,
    limit: u32,
    vector: Option<&[f32]>,
    now: Timestamp,
) -> Result<UiSearch, ApiError> {
    scope(s, namespace)?;
    let names: Vec<String> = match namespace {
        Some(n) => vec![n.to_owned()],
        None => s.namespaces().map(|n| n.name.clone()).collect(),
    };
    let mut hits: Vec<UiSearchHit> = Vec::new();
    for name in names {
        let req = Recall {
            query: query.to_owned(),
            namespace: Some(name),
            limit,
            full_text_count: Some(0),
            tags: Vec::new(),
            entities: Vec::new(),
            topics: Vec::new(),
            emotions: Vec::new(),
            min_strength: None,
            depth: 1,
            time_range_start: None,
            time_range_end: None,
            detail: Detail::Full,
            record_access: false,
        };
        let result = s.recall(&req, vector, true, now)?;
        if let RecallResponse::Full(full) = result.response {
            hits.extend(full.memories.into_iter().map(|h| search_hit(s, h)));
        }
    }
    let spaces: HashSet<&str> = hits.iter().map(|h| h.namespace.as_str()).collect();
    let merged = spaces.len() > 1;
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    hits.truncate(limit as usize);
    Ok(UiSearch {
        merged,
        results: hits,
    })
}

fn search_hit(s: &Store, h: FullHit) -> UiSearchHit {
    let e = h.explanation;
    let via = e.and_then(|e| e.via);
    UiSearchHit {
        id: h.memory.id,
        namespace: h.memory.namespace,
        summary: h.memory.summary,
        phase: h.memory.phase,
        score: h.score,
        vector: e.and_then(|e| e.vector),
        text: e.and_then(|e| e.text),
        activation: e.and_then(|e| e.activation),
        hops: e.and_then(|e| e.hops),
        via,
        via_summary: via.and_then(|v| s.memory(v)).and_then(summary),
        via_edge_type: e.and_then(|e| e.via_edge_type),
        supersedes: e.and_then(|e| e.replaced),
    }
}

/// A Timeline row before it is written out.
struct Row {
    at: i64,
    kind: &'static str,
    id: Option<MemoryId>,
    count: Option<u32>,
    client: Option<String>,
    namespace: Option<String>,
}

/// `GET /v1/ui/activity`, as recalld's `activity` builds it from creation times and access
/// history, joined with the store's live events.
pub(crate) fn activity(
    s: &Store,
    namespace: Option<&str>,
    since: Option<Timestamp>,
    limit: usize,
    events: &[Arc<UiEvent>],
    now: Timestamp,
) -> Result<Activity, ApiError> {
    let scope = scope(s, namespace)?;
    let records: Vec<&Memory> = s.memories().filter(|m| in_scope(scope, m)).collect();
    let now_ms = now.millis();
    let oldest = records
        .iter()
        .map(|m| m.created_at.millis())
        .min()
        .unwrap_or(now_ms);
    let since = since.map_or(oldest, Timestamp::millis).min(now_ms);
    let bucket_ms = if now_ms - since > 730 * MILLIS_PER_DAY {
        30 * MILLIS_PER_DAY
    } else {
        7 * MILLIS_PER_DAY
    };
    let start = since - since.rem_euclid(bucket_ms);
    let n = usize::try_from((now_ms - start) / bucket_ms + 1).unwrap_or(1);
    let mut buckets: Vec<ActivityBucket> = (0..n)
        .map(|i| ActivityBucket {
            start: Timestamp::from_millis(start + i as i64 * bucket_ms),
            full: 0,
            summary: 0,
            ghost: 0,
            recalls: 0,
        })
        .collect();
    let slot = |t: i64| usize::try_from((t - start) / bucket_ms).ok();
    // Entries with the same time and row kind are one row: (id, count), a direct hit's
    // ID first.
    let mut groups: BTreeMap<(i64, &'static str), (MemoryId, u32, bool)> = BTreeMap::new();
    let mut rows: Vec<Row> = Vec::new();
    let links: HashMap<MemoryId, u32> = events
        .iter()
        .filter_map(|e| match &**e {
            UiEvent::Store(st) => Some((st.id, st.edges.len() as u32)),
            _ => None,
        })
        .collect();
    for m in &records {
        let created = m.created_at.millis();
        if created >= start {
            if let Some(b) = slot(created).and_then(|i| buckets.get_mut(i)) {
                match m.phase {
                    Phase::Full => b.full += 1,
                    Phase::Summary => b.summary += 1,
                    Phase::Ghost => b.ghost += 1,
                }
            }
            rows.push(Row {
                at: created,
                kind: "store",
                id: Some(m.id),
                count: links.get(&m.id).copied(),
                client: m.client.clone(),
                namespace: None,
            });
        }
        for e in &m.access_history {
            let at = e.at.millis();
            if at < start {
                continue;
            }
            let kind = match e.kind {
                AccessKind::Direct | AccessKind::Associative => "recall",
                AccessKind::Reinforcement => "reinforce",
            };
            if kind == "recall"
                && let Some(b) = slot(at).and_then(|i| buckets.get_mut(i))
            {
                b.recalls += 1;
            }
            let direct = e.kind == AccessKind::Direct;
            let entry = groups.entry((at, kind)).or_insert((m.id, 0, direct));
            entry.1 += 1;
            if direct && !entry.2 {
                (entry.0, entry.2) = (m.id, true);
            }
        }
    }
    let mut grouped: BTreeMap<(i64, &'static str), Row> = groups
        .into_iter()
        .map(|((at, kind), (id, count, _))| {
            let row = Row {
                at,
                kind,
                id: Some(id),
                count: Some(count),
                client: None,
                namespace: None,
            };
            ((at, kind), row)
        })
        .collect();
    // A live event adds what history lacks; one without a history group is a row of its own.
    for event in events {
        let at = event.at();
        if at < start {
            continue;
        }
        match &**event {
            UiEvent::Recall(r) => {
                if !scope.is_none_or(|ns| ns == r.namespace_id) {
                    continue;
                }
                let row = grouped.entry((at, "recall")).or_insert(Row {
                    at,
                    kind: "recall",
                    id: None,
                    count: None,
                    client: None,
                    namespace: Some(r.namespace.clone()),
                });
                row.client.clone_from(&r.client);
                row.id = r.returned.first().copied().or(row.id);
                row.count = Some(r.returned.len() as u32);
            }
            UiEvent::Reinforce(r) => {
                if !s.memory(r.id).is_some_and(|m| in_scope(scope, m)) {
                    continue;
                }
                let row = grouped.entry((at, "reinforce")).or_insert(Row {
                    at,
                    kind: "reinforce",
                    id: Some(r.id),
                    count: Some(1),
                    client: None,
                    namespace: None,
                });
                row.client.clone_from(&r.client);
            }
            UiEvent::Sweep(w) => {
                let moved =
                    w.moved.full_to_summary + w.moved.summary_to_ghost + w.moved.ghost_to_deleted;
                rows.push(Row {
                    at,
                    kind: "sweep",
                    id: None,
                    count: Some(u32::try_from(moved).unwrap_or(u32::MAX)),
                    client: None,
                    namespace: None,
                });
            }
            _ => {}
        }
    }
    rows.extend(grouped.into_values());
    rows.sort_by_key(|r| std::cmp::Reverse(r.at));
    rows.truncate(limit);
    let rows = rows
        .into_iter()
        .map(|r| {
            let m = r.id.and_then(|id| s.memory(id));
            ActivityRow {
                at: Timestamp::from_millis(r.at),
                kind: r.kind,
                id: r.id,
                summary: m.and_then(summary),
                namespace: m.map(|m| namespace_name(s, m.namespace)).or(r.namespace),
                count: r.count,
                client: r.client,
            }
        })
        .collect();
    Ok(Activity {
        bucket_ms,
        buckets,
        rows,
    })
}

/// `GET /v1/ui/health`. `snapshot_bytes` is the size of the store's last snapshot.
pub(crate) fn health(
    s: &Store,
    namespace: Option<&str>,
    snapshot_bytes: Option<u64>,
    now: Timestamp,
) -> Result<HealthReport, ApiError> {
    let scope = scope(s, namespace)?;
    let mut counts = ReportPhaseCounts {
        full: 0,
        summary: 0,
        ghost: 0,
    };
    let (mut total, mut permastore) = (0u64, 0u64);
    let mut forecast = [
        TransitionCounts::default(),
        TransitionCounts::default(),
        TransitionCounts::default(),
    ];
    let mut at_risk: Vec<AtRiskMemory> = Vec::new();
    for m in s.memories().filter(|m| in_scope(scope, m)) {
        total += 1;
        permastore += u64::from(m.is_permastore);
        match m.phase {
            Phase::Full => counts.full += 1,
            Phase::Summary => counts.summary += 1,
            Phase::Ghost => counts.ghost += 1,
        }
        let Some((days, to)) = next_change(s, m, now) else {
            continue;
        };
        for (horizon, t) in [7.0, 30.0, 90.0].into_iter().zip(forecast.iter_mut()) {
            if days <= horizon {
                match m.phase {
                    Phase::Full => t.full_to_summary += 1,
                    Phase::Summary => t.summary_to_ghost += 1,
                    Phase::Ghost => t.ghost_to_deleted += 1,
                }
            }
        }
        if m.phase != Phase::Ghost {
            at_risk.push(AtRiskMemory {
                id: m.id,
                summary: summary(m),
                strength: s.raw_strength(m, now),
                days_until_change: days,
                phase: m.phase,
                to,
            });
        }
    }
    at_risk.sort_by(|a, b| {
        a.days_until_change
            .total_cmp(&b.days_until_change)
            .then(a.id.cmp(&b.id))
    });
    at_risk.truncate(10);
    let [t7, t30, t90] = forecast;
    let text_bytes = s
        .namespaces()
        .map(|n| s.namespace_counts(n.id).text_bytes)
        .sum();
    let vector_bytes = (s.vectors().len() as u64)
        .saturating_mul(u64::from(s.model().dimensions))
        .saturating_mul(4);
    Ok(HealthReport {
        scope: namespace.unwrap_or("all").to_owned(),
        overview: HealthOverview {
            total_memories: total,
            phase_counts: counts,
            permastore_count: permastore,
        },
        decay_forecast: DecayForecast {
            transitions_7d: t7,
            transitions_30d: t30,
            transitions_90d: t90,
        },
        at_risk,
        storage: StorageBreakdown {
            text_bytes,
            vector_bytes,
            last_snapshot_bytes: snapshot_bytes,
        },
    })
}
