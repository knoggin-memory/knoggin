// SPDX-License-Identifier: AGPL-3.0-only
//! Snapshot content: a `Store` encoded to bytes and back (content format 1), the
//! snapshot names `<generation>-<sequence>`, and `thin`, the rule that says which
//! snapshots of a store are kept.
//!
//! Content format 1 is a section table (`u32 section_count`, then `u32 id, u64 length`
//! per section, little-endian) followed by six sections in a fixed order: `meta`,
//! `namespaces`, `memories`, `vectors`, `edges`, `idempotency`. All but `vectors` are
//! postcard; `vectors` is `u32 dimension, u32 count`, then per vector 16 ID bytes and
//! `dimension` little-endian `f32` values. The `V1` structs below are the wire format:
//! never change them; a change is new `V2` structs and content format 2.

mod name;

use std::collections::{BTreeMap, BTreeSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::embed::ModelIdentity;
use crate::graph::Graph;
use crate::id::{MemoryId, NamespaceId, StoreId};
use crate::model::limits::{
    ACCESS_HISTORY_MAX, DEFAULT_NAMESPACE, IDEMPOTENCY_KEY_MAX_BYTES, validate_namespace_name,
};
use crate::model::{
    AccessEvent, AccessKind, Edge, EdgeType, EmbeddingState, Labels, Memory, Namespace,
    NamespacePolicy, Phase, PhaseThresholds, Tag,
};
use crate::ops::ChangeCounters;
use crate::search::vector::{VectorTable, is_valid_vector};
use crate::store::{IdempotencyEntry, Store};
use crate::time::Timestamp;

pub use name::{KeptBecause, NameError, SnapshotName, SnapshotObject, thin};

/// The content format this build writes.
pub const WRITE_CONTENT_FORMAT: u32 = 1;
/// The oldest content format this build reads.
pub const MIN_CONTENT_FORMAT: u32 = 1;
/// The newest content format this build reads.
pub const MAX_CONTENT_FORMAT: u32 = 1;

/// Section IDs of format 1, in the order they are written.
const SECTIONS: [u32; 6] = [1, 2, 3, 4, 5, 6];
/// `u32 section_count` plus `u32 id, u64 length` per section.
const TABLE_LEN: usize = 4 + SECTIONS.len() * 12;

/// Why snapshot content cannot be opened.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    /// The content breaks the format; the store is not opened.
    #[error("corrupt snapshot content: {0}")]
    Corrupt(&'static str),
    /// Written by a newer build. The store is locked with the reason `format_too_new`
    /// and opens again once that build runs; there is no fall-back to an older snapshot.
    #[error("content format {0} is newer than this build reads")]
    FormatTooNew(u32),
}

impl SnapshotError {
    /// The `store_locked` reason a host gives when no snapshot can be opened because of this.
    pub fn lock_reason(&self) -> &'static str {
        match self {
            Self::Corrupt(_) => "snapshot_unreadable",
            Self::FormatTooNew(_) => "format_too_new",
        }
    }
}

/// Refuses a content format this build does not read: newer is `FormatTooNew`,
/// older than any format that ever existed is `Corrupt`.
pub fn check_content_format(format: u32) -> Result<(), SnapshotError> {
    if format > MAX_CONTENT_FORMAT {
        Err(SnapshotError::FormatTooNew(format))
    } else if format < MIN_CONTENT_FORMAT {
        Err(SnapshotError::Corrupt("unknown content format"))
    } else {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct MetaV1 {
    store_id: [u8; 16],
    created_ms: i64,
    model_identity: String,
    dimension: u32,
    next_namespace_id: u32,
    content_changes: u64,
    bookkeeping_changes: u64,
    forget_changes: u64,
    written_by: String,
}

#[derive(Serialize, Deserialize)]
struct NamespaceV1 {
    id: u32,
    name: String,
    created_ms: i64,
    initial_stability: f32,
    default_difficulty: f32,
    full_to_summary: f32,
    summary_to_ghost: f32,
    ghost_to_delete: f32,
    permastore_threshold: f32,
    decay_rate_multiplier: Option<f32>,
}

#[derive(Serialize, Deserialize)]
struct MemoryV1 {
    id: [u8; 16],
    namespace: u32,
    created_ms: i64,
    summary: String,
    full_text: Option<String>,
    tags: Vec<String>,
    entities: Vec<String>,
    topics: Vec<String>,
    emotions: Vec<String>,
    phase: u8,
    stability: f32,
    difficulty: f32,
    is_permastore: bool,
    decay_anchor_ms: i64,
    retrieved_ms: Option<i64>,
    reinforced_ms: Option<i64>,
    phase_changed_ms: i64,
    access_count: u64,
    access_history: Vec<AccessEventV1>,
    embedding: u8,
    embed_failures: u8,
    saved_by: String,
    client: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct AccessEventV1 {
    at_ms: i64,
    kind: u8,
}

#[derive(Serialize, Deserialize)]
struct EdgeV1 {
    source: [u8; 16],
    target: [u8; 16],
    edge_type: u8,
    weight: f32,
    auto_created: bool,
    created_ms: i64,
}

#[derive(Serialize, Deserialize)]
struct IdempotencyV1 {
    key: String,
    request_hash: [u8; 32],
    response: Vec<u8>,
    created_ms: i64,
    memory_ids: Vec<[u8; 16]>,
}

const fn phase_number(phase: Phase) -> u8 {
    match phase {
        Phase::Full => 1,
        Phase::Summary => 2,
        Phase::Ghost => 3,
    }
}

fn phase_from(n: u8) -> Result<Phase, SnapshotError> {
    match n {
        1 => Ok(Phase::Full),
        2 => Ok(Phase::Summary),
        3 => Ok(Phase::Ghost),
        _ => Err(SnapshotError::Corrupt("unknown phase")),
    }
}

const fn embedding_number(state: EmbeddingState) -> u8 {
    match state {
        EmbeddingState::None => 0,
        EmbeddingState::Pending => 1,
        EmbeddingState::Ready => 2,
        EmbeddingState::Failed => 3,
    }
}

fn embedding_from(n: u8) -> Result<EmbeddingState, SnapshotError> {
    match n {
        0 => Ok(EmbeddingState::None),
        1 => Ok(EmbeddingState::Pending),
        2 => Ok(EmbeddingState::Ready),
        3 => Ok(EmbeddingState::Failed),
        _ => Err(SnapshotError::Corrupt("unknown embedding state")),
    }
}

const fn kind_number(kind: AccessKind) -> u8 {
    match kind {
        AccessKind::Direct => 1,
        AccessKind::Associative => 2,
        AccessKind::Reinforcement => 4,
    }
}

fn kind_from(n: u8) -> Result<AccessKind, SnapshotError> {
    match n {
        1 => Ok(AccessKind::Direct),
        2 => Ok(AccessKind::Associative),
        4 => Ok(AccessKind::Reinforcement),
        _ => Err(SnapshotError::Corrupt("unknown access kind")),
    }
}

/// Appends the postcard of `value`.
#[expect(
    clippy::expect_used,
    reason = "postcard into a Vec fails only when a Serialize impl errors, and the V1 structs have none that can"
)]
fn put<T: Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) {
    let buf = std::mem::take(out);
    *out = postcard::to_extend(value, buf).expect("postcard into a Vec");
}

/// Appends what postcard writes for a `Vec` of these items, one item at a time, so that
/// no second copy of the whole store's text exists while it is encoded.
fn put_seq<T: Serialize>(out: &mut Vec<u8>, len: usize, items: impl Iterator<Item = T>) {
    put(out, &len);
    for item in items {
        put(out, &item);
    }
}

/// `take_from_bytes` with the remainder refused: `from_bytes` ignores trailing bytes.
fn take<T: DeserializeOwned>(bytes: &[u8], what: &'static str) -> Result<T, SnapshotError> {
    match postcard::take_from_bytes(bytes) {
        Ok((value, [])) => Ok(value),
        _ => Err(SnapshotError::Corrupt(what)),
    }
}

/// Fails with `what` unless `items` is strictly ascending (sorted, no repeats).
fn ascending<K: Ord>(
    items: impl Iterator<Item = K>,
    what: &'static str,
) -> Result<(), SnapshotError> {
    let mut last = None;
    for k in items {
        if last.as_ref().is_some_and(|l| *l >= k) {
            return Err(SnapshotError::Corrupt(what));
        }
        last = Some(k);
    }
    Ok(())
}

fn require(ok: bool, what: &'static str) -> Result<(), SnapshotError> {
    if ok {
        Ok(())
    } else {
        Err(SnapshotError::Corrupt(what))
    }
}

/// Splits format-1 content into its six sections, checking the table.
fn sections(content: &[u8]) -> Result<[&[u8]; 6], SnapshotError> {
    let bad = SnapshotError::Corrupt("section table");
    let table = content.get(..TABLE_LEN).ok_or(bad.clone())?;
    let (count, mut entries) = table.split_at(4);
    if count != (SECTIONS.len() as u32).to_le_bytes() {
        return Err(bad);
    }
    let mut out: [&[u8]; 6] = [&[]; 6];
    let mut rest = content.get(TABLE_LEN..).ok_or(bad.clone())?;
    for (id, slot) in SECTIONS.iter().zip(&mut out) {
        let (entry, tail) = entries.split_at(12);
        entries = tail;
        let (entry_id, len) = entry.split_at(4);
        if entry_id != id.to_le_bytes() {
            return Err(bad);
        }
        let len: [u8; 8] = len.try_into().map_err(|_| bad.clone())?;
        let len = usize::try_from(u64::from_le_bytes(len)).map_err(|_| bad.clone())?;
        if len > rest.len() {
            return Err(bad);
        }
        let (section, tail) = rest.split_at(len);
        *slot = section;
        rest = tail;
    }
    if rest.is_empty() { Ok(out) } else { Err(bad) }
}

/// The `vectors` section: `u32 dimension, u32 count`, then `count` times 16 ID bytes
/// and `dimension` values.
fn read_vectors(bytes: &[u8], dimension: u32) -> Result<Vec<(MemoryId, Vec<f32>)>, SnapshotError> {
    let bad = SnapshotError::Corrupt("vectors section");
    let (head, body) = bytes.split_at_checked(8).ok_or(bad.clone())?;
    let (dim, count) = head.split_at(4);
    if dim != dimension.to_le_bytes() {
        return Err(SnapshotError::Corrupt("vector dimension differs from meta"));
    }
    let count: [u8; 4] = count.try_into().map_err(|_| bad.clone())?;
    let count = usize::try_from(u32::from_le_bytes(count)).map_err(|_| bad.clone())?;
    let dim = usize::try_from(dimension).map_err(|_| bad.clone())?;
    let entry_len = dim
        .checked_mul(4)
        .and_then(|n| n.checked_add(16))
        .ok_or(bad.clone())?;
    if count.checked_mul(entry_len) != Some(body.len()) {
        return Err(bad);
    }
    let mut out = Vec::with_capacity(count);
    for entry in body.chunks_exact(entry_len) {
        let (id, values) = entry.split_at(16);
        let id: [u8; 16] = id.try_into().map_err(|_| bad.clone())?;
        let values = values
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        out.push((MemoryId::from_bytes(id), values));
    }
    Ok(out)
}

impl Store {
    /// The snapshot content of the store, in the current format
    /// ([`WRITE_CONTENT_FORMAT`]). The same store always gives the same bytes.
    pub fn encode(&self) -> Vec<u8> {
        self.encode_as(env!("CARGO_PKG_VERSION"))
    }

    /// [`Store::encode`] with another `written_by`, so that a frozen fixture written by an
    /// older version can be compared byte for byte. Tests only.
    #[doc(hidden)]
    pub fn encode_as(&self, written_by: &str) -> Vec<u8> {
        let mut out = vec![0_u8; TABLE_LEN];
        let mut lens = [0_usize; 6];
        let mut mark = out.len();
        let mut close = |out: &Vec<u8>, i: usize| {
            lens[i] = out.len() - mark;
            mark = out.len();
        };

        put(
            &mut out,
            &MetaV1 {
                store_id: *self.id.as_bytes(),
                created_ms: self.created_at.millis(),
                model_identity: self.model.id.clone(),
                dimension: self.model.dimensions,
                next_namespace_id: self.next_namespace_id,
                content_changes: self.changes.content,
                bookkeeping_changes: self.changes.bookkeeping,
                forget_changes: self.changes.forgets,
                written_by: written_by.to_owned(),
            },
        );
        close(&out, 0);

        put_seq(
            &mut out,
            self.namespaces.len(),
            self.namespaces.values().map(namespace_v1),
        );
        close(&out, 1);

        put_seq(
            &mut out,
            self.memories.len(),
            self.memories.values().map(memory_v1),
        );
        close(&out, 2);

        let mut vectors: Vec<(MemoryId, &[f32])> = self.vectors.iter().collect();
        vectors.sort_by_key(|(id, _)| *id);
        out.extend_from_slice(&self.model.dimensions.to_le_bytes());
        #[expect(
            clippy::expect_used,
            reason = "a store that fits in memory holds far fewer than 2^32 vectors"
        )]
        let count = u32::try_from(vectors.len()).expect("vector count fits in u32");
        out.extend_from_slice(&count.to_le_bytes());
        for (id, values) in vectors {
            out.extend_from_slice(id.as_bytes());
            for v in values {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        close(&out, 3);

        // The graph iterates in key order, `(source, edge_type, target)`.
        put_seq(
            &mut out,
            self.graph.len(),
            self.graph.iter().map(|e| EdgeV1 {
                source: *e.source.as_bytes(),
                target: *e.target.as_bytes(),
                edge_type: e.edge_type.as_u8(),
                weight: e.weight,
                auto_created: e.auto_created,
                created_ms: e.created_at.millis(),
            }),
        );
        close(&out, 4);

        put_seq(
            &mut out,
            self.idempotency.len(),
            self.idempotency.iter().map(|(key, e)| IdempotencyV1 {
                key: key.clone(),
                request_hash: e.request_hash,
                response: e.response.clone(),
                created_ms: e.created_at.millis(),
                memory_ids: e.text_ids.iter().map(|id| *id.as_bytes()).collect(),
            }),
        );
        close(&out, 5);

        let mut table = Vec::with_capacity(TABLE_LEN);
        table.extend_from_slice(&(SECTIONS.len() as u32).to_le_bytes());
        for (id, len) in SECTIONS.iter().zip(lens) {
            table.extend_from_slice(&id.to_le_bytes());
            table.extend_from_slice(&(len as u64).to_le_bytes());
        }
        out[..TABLE_LEN].copy_from_slice(&table);
        out
    }

    /// Opens snapshot content of `content_format` (the container header's field). Checks
    /// every section, rebuilds every in-memory index, and changes nothing else, so that
    /// encoding the result gives the same bytes. The host calls [`Store::on_open`] next.
    pub fn decode(content_format: u32, content: &[u8]) -> Result<Store, SnapshotError> {
        check_content_format(content_format)?;
        let [meta, namespaces, memories, vectors, edges, idempotency] = sections(content)?;
        let meta: MetaV1 = take(meta, "meta section")?;
        let namespaces: Vec<NamespaceV1> = take(namespaces, "namespaces section")?;
        let memories: Vec<MemoryV1> = take(memories, "memories section")?;
        let vectors = read_vectors(vectors, meta.dimension)?;
        let edges: Vec<EdgeV1> = take(edges, "edges section")?;
        let idempotency: Vec<IdempotencyV1> = take(idempotency, "idempotency section")?;

        let namespaces = decode_namespaces(namespaces, meta.next_namespace_id)?;
        let memories = decode_memories(memories, &namespaces)?;
        let dimensions =
            usize::try_from(meta.dimension).map_err(|_| SnapshotError::Corrupt("dimension"))?;
        let vectors = decode_vectors(vectors, &memories, dimensions)?;
        let graph = decode_edges(edges, &memories)?;
        let idempotency = decode_idempotency(idempotency)?;

        let model = ModelIdentity {
            id: meta.model_identity,
            dimensions: meta.dimension,
        };
        let created_at = Timestamp::from_millis(meta.created_ms);
        let mut store = Store::create(StoreId::from_bytes(meta.store_id), model, created_at);
        store.namespaces = namespaces;
        store.next_namespace_id = meta.next_namespace_id;
        store.memories = memories;
        store.graph = graph;
        store.vectors = vectors;
        store.idempotency = idempotency;
        store.changes = ChangeCounters {
            content: meta.content_changes,
            bookkeeping: meta.bookkeeping_changes,
            forgets: meta.forget_changes,
        };
        store.rebuild_indexes();
        Ok(store)
    }
}

fn namespace_v1(n: &Namespace) -> NamespaceV1 {
    let p = &n.policy;
    NamespaceV1 {
        id: n.id.get(),
        name: n.name.clone(),
        created_ms: n.created_at.millis(),
        initial_stability: p.initial_stability,
        default_difficulty: p.default_difficulty,
        full_to_summary: p.phase_thresholds.full_to_summary,
        summary_to_ghost: p.phase_thresholds.summary_to_ghost,
        ghost_to_delete: p.phase_thresholds.ghost_to_delete,
        permastore_threshold: p.permastore_threshold,
        decay_rate_multiplier: p.decay_rate_multiplier,
    }
}

fn memory_v1(m: &Memory) -> MemoryV1 {
    MemoryV1 {
        id: *m.id.as_bytes(),
        namespace: m.namespace.get(),
        created_ms: m.created_at.millis(),
        summary: m.summary.clone(),
        full_text: m.full_text.clone(),
        tags: m.tags.iter().map(|t| t.as_str().to_owned()).collect(),
        entities: m.labels.entities.clone(),
        topics: m.labels.topics.clone(),
        emotions: m.labels.emotions.clone(),
        phase: phase_number(m.phase),
        stability: m.stability,
        difficulty: m.difficulty,
        is_permastore: m.is_permastore,
        decay_anchor_ms: m.decay_anchor_at.millis(),
        retrieved_ms: m.retrieved_at.map(Timestamp::millis),
        reinforced_ms: m.reinforced_at.map(Timestamp::millis),
        phase_changed_ms: m.phase_changed_at.millis(),
        access_count: m.access_count,
        access_history: m
            .access_history
            .iter()
            .map(|e| AccessEventV1 {
                at_ms: e.at.millis(),
                kind: kind_number(e.kind),
            })
            .collect(),
        embedding: embedding_number(m.embedding),
        embed_failures: m.embed_failures,
        saved_by: m.saved_by.clone(),
        client: m.client.clone(),
    }
}

fn decode_namespaces(
    items: Vec<NamespaceV1>,
    next_id: u32,
) -> Result<BTreeMap<NamespaceId, Namespace>, SnapshotError> {
    ascending(items.iter().map(|n| n.id), "namespaces not sorted by id")?;
    let mut names = BTreeSet::new();
    let mut out = BTreeMap::new();
    for n in items {
        require(n.id >= 1 && n.id < next_id, "namespace id out of range")?;
        require(
            validate_namespace_name("name", &n.name).is_ok(),
            "invalid namespace name",
        )?;
        require(
            names.insert(n.name.to_ascii_lowercase()),
            "namespace names repeat ignoring case",
        )?;
        let policy = NamespacePolicy {
            initial_stability: n.initial_stability,
            default_difficulty: n.default_difficulty,
            phase_thresholds: PhaseThresholds {
                full_to_summary: n.full_to_summary,
                summary_to_ghost: n.summary_to_ghost,
                ghost_to_delete: n.ghost_to_delete,
            },
            permastore_threshold: n.permastore_threshold,
            decay_rate_multiplier: n.decay_rate_multiplier,
        };
        require(policy.validate().is_ok(), "invalid namespace policy")?;
        let id = NamespaceId::new(n.id);
        out.insert(
            id,
            Namespace {
                id,
                name: n.name,
                created_at: Timestamp::from_millis(n.created_ms),
                policy,
            },
        );
    }
    require(
        out.values().any(|n| n.name == DEFAULT_NAMESPACE),
        "no default namespace",
    )?;
    Ok(out)
}

fn decode_memories(
    items: Vec<MemoryV1>,
    namespaces: &BTreeMap<NamespaceId, Namespace>,
) -> Result<BTreeMap<MemoryId, Memory>, SnapshotError> {
    ascending(items.iter().map(|m| m.id), "memories not sorted by id")?;
    let mut out = BTreeMap::new();
    for m in items {
        let namespace = NamespaceId::new(m.namespace);
        require(
            namespaces.contains_key(&namespace),
            "memory in an unknown namespace",
        )?;
        let phase = phase_from(m.phase)?;
        let embedding = embedding_from(m.embedding)?;
        let mut tags = Vec::with_capacity(m.tags.len());
        for t in m.tags {
            let tag = Tag::new(t.as_str()).map_err(|_| SnapshotError::Corrupt("invalid tag"))?;
            require(tag.as_str() == t, "tag not in lower case")?;
            tags.push(tag);
        }
        require(
            m.access_history.len() <= ACCESS_HISTORY_MAX,
            "access history too long",
        )?;
        let access_history = m
            .access_history
            .into_iter()
            .map(|e| {
                Ok(AccessEvent {
                    at: Timestamp::from_millis(e.at_ms),
                    kind: kind_from(e.kind)?,
                })
            })
            .collect::<Result<Vec<_>, SnapshotError>>()?;
        require(
            m.stability.is_finite() && m.difficulty.is_finite(),
            "lifecycle number not finite",
        )?;
        let labels = Labels {
            entities: m.entities,
            topics: m.topics,
            emotions: m.emotions,
        };
        // The indexes rely on these: a ghost holds no text and never a vector, and only
        // a ghost has the embedding state `None`.
        if phase == Phase::Ghost {
            require(
                m.summary.is_empty()
                    && m.full_text.is_none()
                    && tags.is_empty()
                    && labels == Labels::default(),
                "ghost with content",
            )?;
        }
        require(
            (phase == Phase::Ghost) == (embedding == EmbeddingState::None),
            "embedding state does not fit the phase",
        )?;
        let id = MemoryId::from_bytes(m.id);
        out.insert(
            id,
            Memory {
                id,
                namespace,
                created_at: Timestamp::from_millis(m.created_ms),
                summary: m.summary,
                full_text: m.full_text,
                tags,
                labels,
                phase,
                stability: m.stability,
                difficulty: m.difficulty,
                is_permastore: m.is_permastore,
                decay_anchor_at: Timestamp::from_millis(m.decay_anchor_ms),
                retrieved_at: m.retrieved_ms.map(Timestamp::from_millis),
                reinforced_at: m.reinforced_ms.map(Timestamp::from_millis),
                phase_changed_at: Timestamp::from_millis(m.phase_changed_ms),
                access_count: m.access_count,
                access_history,
                embedding,
                embed_failures: m.embed_failures,
                saved_by: m.saved_by,
                client: m.client,
            },
        );
    }
    Ok(out)
}

fn decode_vectors(
    items: Vec<(MemoryId, Vec<f32>)>,
    memories: &BTreeMap<MemoryId, Memory>,
    dimensions: usize,
) -> Result<VectorTable, SnapshotError> {
    ascending(items.iter().map(|(id, _)| *id), "vectors not sorted by id")?;
    let ready = memories
        .values()
        .filter(|m| m.embedding == EmbeddingState::Ready)
        .count();
    require(
        items.len() == ready,
        "vector count differs from ready memories",
    )?;
    let mut table = VectorTable::new(dimensions);
    for (id, values) in items {
        require(
            memories
                .get(&id)
                .is_some_and(|m| m.embedding == EmbeddingState::Ready),
            "vector of a memory that is not ready",
        )?;
        require(is_valid_vector(&values, dimensions), "invalid vector")?;
        table.insert(id, &values);
    }
    Ok(table)
}

fn decode_edges(
    items: Vec<EdgeV1>,
    memories: &BTreeMap<MemoryId, Memory>,
) -> Result<Graph, SnapshotError> {
    ascending(
        items.iter().map(|e| (e.source, e.edge_type, e.target)),
        "edges not sorted by key",
    )?;
    let mut graph = Graph::new();
    for e in items {
        let edge = Edge {
            source: MemoryId::from_bytes(e.source),
            target: MemoryId::from_bytes(e.target),
            edge_type: EdgeType::from_u8(e.edge_type)
                .ok_or(SnapshotError::Corrupt("unknown edge type"))?,
            weight: e.weight,
            auto_created: e.auto_created,
            created_at: Timestamp::from_millis(e.created_ms),
        };
        require(
            memories.contains_key(&edge.source) && memories.contains_key(&edge.target),
            "edge to an unknown memory",
        )?;
        require(edge.weight.is_finite(), "edge weight not finite")?;
        require(graph.insert(edge), "edge is a loop")?;
    }
    Ok(graph)
}

fn decode_idempotency(
    items: Vec<IdempotencyV1>,
) -> Result<BTreeMap<String, IdempotencyEntry>, SnapshotError> {
    ascending(
        items.iter().map(|e| e.key.as_str()),
        "entries not sorted by key",
    )?;
    let mut out = BTreeMap::new();
    for e in items {
        require(
            (1..=IDEMPOTENCY_KEY_MAX_BYTES).contains(&e.key.len())
                && e.key.bytes().all(|b| (0x20..=0x7e).contains(&b)),
            "invalid idempotency key",
        )?;
        out.insert(
            e.key,
            IdempotencyEntry {
                request_hash: e.request_hash,
                created_at: Timestamp::from_millis(e.created_ms),
                response: e.response,
                text_ids: e.memory_ids.into_iter().map(MemoryId::from_bytes).collect(),
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
