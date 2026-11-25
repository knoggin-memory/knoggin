// SPDX-License-Identifier: AGPL-3.0-only
//! The recall pipeline: candidates from three channels, fusion, graph expansion,
//! filters, effective strength, retrieval-induced forgetting, scoring, the time boost
//! and supersedes resolution.
//!
//! Ported from recalld `search_admitted` in `src/search/pipeline.rs` (lines 921 to
//! 1399, the "contract V1" path) and the helpers it calls: `fuse_scores`,
//! `expand_graph_candidates`, `apply_filters`, `passes_filters`, `apply_rif`,
//! `apply_boosts`, `compute_composite_score` and `resolve_supersedes`, with the
//! supersedes tests. The step numbers are those of the spec tab "Engine".

use std::collections::{BTreeMap, BTreeSet};

use crate::error::EngineError;
use crate::id::{MemoryId, NamespaceId};
use crate::model::limits::{
    DEFAULT_NAMESPACE, MAX_RECALL_DEPTH, MAX_RECALL_LIMIT, MAX_SUPERSEDE_HOPS, QUERY_MAX_BYTES,
    validate_tags, validate_text_len,
};
use crate::model::{AccessKind, EdgeType, Memory, Namespace, Phase, Tag};
use crate::ops::{
    AccessHit, CompactHit, CompactRecall, Degraded, Detail, Explanation, FullHit, FullRecall,
    Recall, RecallResponse, RecallResult, RecallTrace, Related, TraceSpread,
};
use crate::search::entity::entity_overlap;
use crate::store::Store;
use crate::time::{MILLIS_PER_DAY, Timestamp};

/// Most vector and full-text candidates per channel.
const MAX_FETCH_K: usize = 1_600;
/// Fewest vector and full-text candidates per channel.
const MIN_FETCH_K: usize = 100;
/// Highest full-text boost added to a cosine.
const FTS_BOOST_CAP: f32 = 0.05;
/// How fast the full-text boost saturates.
const FTS_BOOST_RATE: f32 = 0.5;
/// Highest relevance of a full-text-only candidate.
const FTS_ONLY_CAP: f32 = 0.30;
/// How fast the full-text-only relevance saturates.
const FTS_ONLY_RATE: f32 = 0.3;
/// A full-text score at or below this adds nothing to the final score.
const FTS_COMPOSITE_THRESHOLD: f32 = 0.5;
/// Steps walked back from a graph hit to its seed for `hops` (recalld's UI limit).
const MAX_EXPLAINED_HOPS: u8 = 8;

/// The hard filters of one recall. Label filters are lower-cased once here.
#[derive(Clone, PartialEq)]
pub(crate) struct HardFilter {
    pub(crate) namespace: NamespaceId,
    pub(crate) min_strength: Option<f32>,
    pub(crate) tags: Vec<Tag>,
    pub(crate) entities: Vec<String>,
    pub(crate) topics: Vec<String>,
    pub(crate) emotions: Vec<String>,
}

impl std::fmt::Debug for HardFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HardFilter")
            .field("namespace", &self.namespace)
            .field("min_strength", &self.min_strength)
            .field("tags", &self.tags.len())
            .finish_non_exhaustive()
    }
}

/// Every wanted label is among `have`, comparing lower-cased whole strings.
fn has_labels(have: &[String], want: &[String]) -> bool {
    want.iter()
        .all(|w| have.iter().any(|h| h.to_lowercase() == *w))
}

fn lower(labels: &[String]) -> Vec<String> {
    labels.iter().map(|l| l.to_lowercase()).collect()
}

impl HardFilter {
    /// Whether `memory` may be returned: in the namespace, not a ghost, strong enough,
    /// with every tag and label asked for.
    pub(crate) fn passes(&self, store: &Store, memory: &Memory, now: Timestamp) -> bool {
        memory.namespace == self.namespace
            && memory.phase != Phase::Ghost
            && self.tags.iter().all(|t| memory.tags.contains(t))
            && has_labels(&memory.labels.entities, &self.entities)
            && has_labels(&memory.labels.topics, &self.topics)
            && has_labels(&memory.labels.emotions, &self.emotions)
            && self
                .min_strength
                .is_none_or(|min| store.raw_strength(memory, now) >= min)
    }
}

/// A validated recall request.
#[derive(Debug)]
struct Checked {
    filter: HardFilter,
    limit: usize,
    full_text_count: usize,
    depth: u8,
    time_range: Option<(Timestamp, Timestamp)>,
}

/// One memory in the running, with what each step learned about it.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    id: MemoryId,
    /// Fused vector and full-text relevance; `None` for graph and entity-only finds.
    relevance: Option<f32>,
    /// The cosine, when the vector channel found it.
    vector: Option<f32>,
    /// The full-text score, when the full-text channel found it.
    text: Option<f32>,
    /// Spreading activation, when only the graph found it.
    activation: Option<f32>,
    /// Shared entity keys over query entities, when only the entity channel found it.
    entity_recall: f32,
    /// Jaccard overlap of query and memory entities.
    entity_score: f32,
    effective_r: f32,
    score: f32,
    /// The superseded candidate this one stands in for.
    replaced: Option<MemoryId>,
}

impl Candidate {
    fn new(id: MemoryId) -> Self {
        Self {
            id,
            relevance: None,
            vector: None,
            text: None,
            activation: None,
            entity_recall: 0.0,
            entity_score: 0.0,
            effective_r: 0.0,
            score: 0.0,
            replaced: None,
        }
    }

    /// Found only by graph expansion.
    fn by_graph(&self) -> bool {
        self.relevance.is_none() && self.activation.is_some()
    }
}

/// Step 4: vector and full text into one relevance.
fn fuse(vector: Option<f32>, text: Option<f32>) -> Option<f32> {
    match (vector, text) {
        (Some(cos), Some(fts)) => Some(cos + FTS_BOOST_CAP * (1.0 - (-fts * FTS_BOOST_RATE).exp())),
        (Some(cos), None) => Some(cos),
        (None, Some(fts)) => Some(FTS_ONLY_CAP * (1.0 - (-fts * FTS_ONLY_RATE).exp())),
        (None, None) => None,
    }
}

/// Step 9: the score before the time boost.
fn composite_score(c: &Candidate) -> f32 {
    let fts_signal = match c.text {
        Some(fts) if fts > FTS_COMPOSITE_THRESHOLD => 1.0 - (-fts * FTS_BOOST_RATE).exp(),
        _ => 0.0,
    };
    match (c.relevance, c.activation) {
        (Some(rel), _) => 0.90 * rel + 0.05 * c.entity_score + 0.05 * fts_signal,
        (None, Some(activation)) => 0.70 * activation + 0.30 * c.entity_score,
        (None, None) if c.entity_recall > 0.0 => {
            0.40 * c.entity_recall
                + 0.20 * c.entity_score
                + 0.10 * c.effective_r
                + 0.10 * fts_signal
        }
        (None, None) => c.effective_r * 0.1,
    }
}

/// Step 10: `1 + 0.5 * exp(-0.5 * ((created_at - mid) / sigma)^2)`, in `f64` as recalld.
fn time_boost(created_at: Timestamp, start: Timestamp, end: Timestamp) -> f32 {
    let (start, end) = (start.millis() as f64, end.millis() as f64);
    let mid = (start + end) / 2.0;
    let sigma = ((end - start) / 2.0).max(MILLIS_PER_DAY as f64);
    let distance = (created_at.millis() as f64 - mid).abs();
    (1.0 + 0.5 * (-0.5 * (distance / sigma).powi(2)).exp()) as f32
}

/// Score descending, then ID ascending.
fn sort(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
}

fn positions(candidates: &[Candidate]) -> BTreeMap<MemoryId, usize> {
    candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id, i))
        .collect()
}

impl Store {
    /// The namespace `name`, or `default` when none is given; `not_found` if absent.
    pub(crate) fn resolve_namespace(&self, name: Option<&str>) -> Result<&Namespace, EngineError> {
        let name = name.unwrap_or(DEFAULT_NAMESPACE);
        self.namespace_by_name(name)
            .ok_or_else(|| EngineError::NotFound(format!("namespace '{name}' not found")))
    }

    /// Runs one recall at `now`. `query_vector` is the embedded query, or `None` when
    /// the model is not ready; `explain` adds an explanation to each full hit and is
    /// set by the host for the web UI's search only.
    ///
    /// The answer carries the trace for the web UI and the hits for `record_access`;
    /// a recall itself records nothing.
    pub fn recall(
        &self,
        req: &Recall,
        query_vector: Option<&[f32]>,
        explain: bool,
        now: Timestamp,
    ) -> Result<RecallResult, EngineError> {
        // 1. Validate.
        let q = self.check_recall(req)?;
        let ns = q.filter.namespace;
        // A vector the store cannot compare is treated as no vector.
        let query_vector = query_vector
            .filter(|v| v.len() == self.vectors.dimensions() && v.iter().all(|x| x.is_finite()));

        // 2. Eligible set.
        let eligible: BTreeSet<MemoryId> = self
            .memories
            .values()
            .filter(|m| q.filter.passes(self, m, now))
            .map(|m| m.id)
            .collect();

        // 3. Candidates: vector, then full text, then entities.
        let fetch_k = q.limit.saturating_mul(8).clamp(MIN_FETCH_K, MAX_FETCH_K);
        let mut candidates: Vec<Candidate> = Vec::new();
        if let Some(vector) = query_vector {
            for (id, cos) in self
                .vectors
                .top_k(vector, fetch_k, |id| eligible.contains(&id))
            {
                candidates.push(Candidate {
                    vector: Some(cos),
                    ..Candidate::new(id)
                });
            }
        }
        let text_hits = self
            .fts
            .search(ns, &req.query, fetch_k)
            .unwrap_or_else(|_| {
                // The error can quote the query, so it is not logged.
                tracing::warn!("full-text search failed; the recall continues without it");
                Vec::new()
            });
        let mut at = positions(&candidates);
        for (id, score) in text_hits {
            // Hits that are not eligible still used up `fetch_k`, as in recalld.
            if !eligible.contains(&id) {
                continue;
            }
            match at.get(&id) {
                Some(&i) => {
                    if let Some(c) = candidates.get_mut(i) {
                        c.text = Some(score);
                    }
                }
                None => {
                    at.insert(id, candidates.len());
                    candidates.push(Candidate {
                        text: Some(score),
                        ..Candidate::new(id)
                    });
                }
            }
        }
        // 4. Fusion.
        for c in &mut candidates {
            c.relevance = fuse(c.vector, c.text);
        }
        if !req.entities.is_empty() {
            let query_entities = req.entities.len() as f32;
            let cap = q.limit.saturating_mul(2);
            let found = self.entities.find_by_entities_filtered(
                ns,
                &req.entities,
                MemoryId::default(),
                |id| eligible.contains(&id),
            );
            let novel = found
                .into_iter()
                .filter(|(id, _)| !at.contains_key(id))
                .take(cap);
            let mut added = Vec::new();
            for (id, shared) in novel {
                added.push(Candidate {
                    entity_recall: shared as f32 / query_entities,
                    ..Candidate::new(id)
                });
            }
            candidates.extend(added);
        }

        // 5. Graph expansion.
        let mut trace = RecallTrace::default();
        let mut spreads: Vec<TraceSpread> = Vec::new();
        if q.depth > 0 {
            let seeds: Vec<(MemoryId, f32)> = candidates
                .iter()
                .filter_map(|c| c.relevance.map(|r| (c.id, r)))
                .collect();
            if !seeds.is_empty() {
                let spread = self.spreading_activation(&seeds, ns, q.depth, now);
                let known = positions(&candidates);
                for (id, activation) in spread.activated {
                    if !known.contains_key(&id) {
                        candidates.push(Candidate {
                            activation: Some(activation),
                            ..Candidate::new(id)
                        });
                    }
                }
                trace.seeds = spread.seeds;
                spreads = spread.spreads;
            }
        }

        // 6. Entity score, the hard filters again, and relevance at least 0.
        candidates.retain(|c| {
            !c.relevance.is_some_and(|r| r < 0.0)
                && self
                    .memories
                    .get(&c.id)
                    .is_some_and(|m| q.filter.passes(self, m, now))
        });
        for c in &mut candidates {
            if let Some(m) = self.memories.get(&c.id) {
                if !req.entities.is_empty() {
                    c.entity_score = entity_overlap(&req.entities, &m.labels.entities);
                }
                // 7. Effective strength.
                c.effective_r = self.strengths(m, now).1;
            }
        }

        // 8. Retrieval-induced forgetting.
        let ids: Vec<MemoryId> = candidates.iter().map(|c| c.id).collect();
        let at = positions(&candidates);
        let mut suppressed = BTreeSet::new();
        for (target, multiplier) in self.rif_suppressions(&ids, now) {
            if let Some(c) = at.get(&target).and_then(|&i| candidates.get_mut(i)) {
                c.effective_r *= multiplier;
                suppressed.insert(target);
            }
        }

        // 9 to 11. Score, time boost, sort.
        for c in &mut candidates {
            c.score = composite_score(c);
            if let Some((start, end)) = q.time_range
                && let Some(m) = self.memories.get(&c.id)
            {
                c.score *= time_boost(m.created_at, start, end);
            }
        }
        sort(&mut candidates);

        // 12, 13. Supersedes, sort again and cut.
        self.resolve_supersedes(&mut candidates, &q.filter, &req.entities, now);
        sort(&mut candidates);
        candidates.truncate(q.limit);

        // 14. The result.
        trace.returned = candidates.iter().map(|c| c.id).collect();
        trace.suppressed = suppressed.into_iter().collect();
        let hits = candidates
            .iter()
            .map(|c| AccessHit {
                id: c.id,
                kind: if c.by_graph() {
                    AccessKind::Associative
                } else {
                    AccessKind::Direct
                },
            })
            .collect();
        let degraded = query_vector.is_none().then_some(Degraded::ModelUnavailable);
        let count = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
        let response = match req.detail {
            Detail::Compact => RecallResponse::Compact(CompactRecall {
                memories: self.compact_hits(candidates.iter().map(|c| c.id), q.full_text_count),
                count,
                degraded,
            }),
            Detail::Full => RecallResponse::Full(FullRecall {
                memories: self.full_hits(&candidates, &q, explain, &trace, &spreads, now),
                count,
                degraded,
            }),
        };
        trace.spreads = spreads;
        Ok(RecallResult {
            response,
            trace,
            hits,
        })
    }

    /// Step 1: every limit of the request, and the namespace.
    fn check_recall(&self, req: &Recall) -> Result<Checked, EngineError> {
        if req.query.trim().is_empty() {
            return Err(EngineError::invalid("query", "query must not be empty"));
        }
        validate_text_len("query", &req.query, QUERY_MAX_BYTES)?;
        let namespace = self.resolve_namespace(req.namespace.as_deref())?.id;
        if !(1..=MAX_RECALL_LIMIT).contains(&req.limit) {
            return Err(EngineError::invalid(
                "limit",
                format!("limit must be between 1 and {MAX_RECALL_LIMIT}"),
            ));
        }
        let full_text_count = req.full_text_count();
        if full_text_count > req.limit {
            return Err(EngineError::invalid(
                "fullTextCount",
                "fullTextCount must be between 0 and limit",
            ));
        }
        let tags = validate_tags("tags", &req.tags)?;
        if let Some(min) = req.min_strength
            && !(0.0..=1.0).contains(&min)
        {
            return Err(EngineError::invalid(
                "minStrength",
                "minStrength must be between 0 and 1",
            ));
        }
        if req.depth > MAX_RECALL_DEPTH {
            return Err(EngineError::invalid(
                "depth",
                format!("depth must be between 0 and {MAX_RECALL_DEPTH}"),
            ));
        }
        let time_range = match (&req.time_range_start, &req.time_range_end) {
            (Some(start), Some(end)) => Some((
                start.to_timestamp("timeRangeStart")?,
                end.to_timestamp("timeRangeEnd")?,
            )),
            (None, None) => None,
            (None, Some(_)) => {
                return Err(EngineError::invalid(
                    "timeRangeStart",
                    "give timeRangeStart and timeRangeEnd together",
                ));
            }
            (Some(_), None) => {
                return Err(EngineError::invalid(
                    "timeRangeEnd",
                    "give timeRangeStart and timeRangeEnd together",
                ));
            }
        };
        Ok(Checked {
            filter: HardFilter {
                namespace,
                min_strength: req.min_strength,
                tags,
                entities: lower(&req.entities),
                topics: lower(&req.topics),
                emotions: lower(&req.emotions),
            },
            // Both fit: they were checked against limits of 100.
            limit: req.limit as usize,
            full_text_count: full_text_count as usize,
            depth: req.depth,
            time_range,
        })
    }

    /// The memories that supersede `id`, following every `Supersedes` edge into it
    /// for at most 10 hops. `id` itself is never one of them, even in a cycle.
    pub(crate) fn successors(&self, id: MemoryId) -> Vec<MemoryId> {
        let mut seen = BTreeSet::from([id]);
        let mut frontier = vec![id];
        let mut out = Vec::new();
        for _ in 0..MAX_SUPERSEDE_HOPS {
            let mut next = Vec::new();
            for current in frontier {
                for edge in self.graph.edges_of(current) {
                    if edge.edge_type == EdgeType::Supersedes
                        && edge.target == current
                        && seen.insert(edge.source)
                    {
                        next.push(edge.source);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            out.extend_from_slice(&next);
            frontier = next;
        }
        out
    }

    /// Step 12: each superseded candidate gives way to the newest successor that
    /// passes the hard filters; the successor takes the best score of what it replaces
    /// when that is higher than its own. Without such a successor the candidate stays.
    fn resolve_supersedes(
        &self,
        candidates: &mut Vec<Candidate>,
        filter: &HardFilter,
        query_entities: &[String],
        now: Timestamp,
    ) {
        let mut resolved = BTreeSet::new();
        // Replacement -> (best dropped score, the candidate it came from).
        let mut inherited: BTreeMap<MemoryId, (f32, MemoryId)> = BTreeMap::new();
        for (idx, candidate) in candidates.iter().enumerate() {
            let newest = self
                .successors(candidate.id)
                .into_iter()
                .filter_map(|id| self.memories.get(&id))
                .filter(|m| filter.passes(self, m, now))
                .max_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
            let Some(replacement) = newest else {
                continue;
            };
            resolved.insert(idx);
            let entry = inherited
                .entry(replacement.id)
                .or_insert((candidate.score, candidate.id));
            if candidate.score > entry.0 {
                *entry = (candidate.score, candidate.id);
            }
        }
        if resolved.is_empty() {
            return;
        }
        let at = positions(candidates);
        let mut injected = Vec::new();
        for (id, (score, replaced)) in inherited {
            if let Some(c) = at.get(&id).and_then(|&i| candidates.get_mut(i)) {
                c.score = c.score.max(score);
                c.replaced = Some(replaced);
                continue;
            }
            let Some(m) = self.memories.get(&id) else {
                continue;
            };
            injected.push(Candidate {
                effective_r: self.strengths(m, now).1,
                entity_score: if query_entities.is_empty() {
                    0.0
                } else {
                    entity_overlap(query_entities, &m.labels.entities)
                },
                score,
                replaced: Some(replaced),
                ..Candidate::new(id)
            });
        }
        let mut idx = 0_usize;
        candidates.retain(|_| {
            let keep = !resolved.contains(&idx);
            idx = idx.saturating_add(1);
            keep
        });
        candidates.extend(injected);
    }

    /// Compact hits for `ids` in order; the first `full_text_count` carry full text.
    pub(crate) fn compact_hits(
        &self,
        ids: impl Iterator<Item = MemoryId>,
        full_text_count: usize,
    ) -> Vec<CompactHit> {
        ids.filter_map(|id| self.memories.get(&id))
            .enumerate()
            .map(|(rank, m)| CompactHit {
                id: m.id,
                date: m.created_at.utc_weekday_date().unwrap_or_default(),
                summary: m.summary.clone(),
                full_text: if rank < full_text_count {
                    m.full_text.clone()
                } else {
                    None
                },
            })
            .collect()
    }

    /// Full hits: the memory view, the score, the edges to other hits on the page and,
    /// when asked, the explanation.
    fn full_hits(
        &self,
        candidates: &[Candidate],
        q: &Checked,
        explain: bool,
        trace: &RecallTrace,
        spreads: &[TraceSpread],
        now: Timestamp,
    ) -> Vec<FullHit> {
        let page: BTreeSet<MemoryId> = candidates.iter().map(|c| c.id).collect();
        // The largest spread into each memory; the first one wins a tie.
        let mut best: BTreeMap<MemoryId, &TraceSpread> = BTreeMap::new();
        for s in spreads {
            let slot = best.entry(s.to).or_insert(s);
            if s.amount > slot.amount {
                *slot = s;
            }
        }
        let seeds: BTreeSet<MemoryId> = trace.seeds.iter().map(|s| s.id).collect();
        let hops_to_seed = |id: MemoryId| -> Option<u8> {
            let mut current = best.get(&id)?.from;
            let mut hops = 1_u8;
            while !seeds.contains(&current) && hops < MAX_EXPLAINED_HOPS {
                current = best.get(&current)?.from;
                hops = hops.saturating_add(1);
            }
            Some(hops)
        };
        candidates
            .iter()
            .enumerate()
            .filter_map(|(rank, c)| {
                let memory = self.memories.get(&c.id)?;
                let related = self
                    .graph
                    .edges_of(c.id)
                    .filter(|e| page.contains(&e.other(c.id)))
                    .map(|e| Related {
                        id: e.other(c.id),
                        edge_type: e.edge_type,
                        weight: e.weight,
                    })
                    .collect();
                let explanation = explain.then(|| {
                    let via = c.by_graph().then(|| best.get(&c.id)).flatten();
                    Explanation {
                        vector: c.vector,
                        text: c.text,
                        activation: c.activation,
                        hops: c.by_graph().then(|| hops_to_seed(c.id)).flatten(),
                        via: via.map(|s| s.from),
                        via_edge_type: via.map(|s| s.edge_type),
                        replaced: c.replaced,
                    }
                });
                Some(FullHit {
                    memory: self.memory_view(memory, now, rank < q.full_text_count, false),
                    score: c.score,
                    related,
                    explanation,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
