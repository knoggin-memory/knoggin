// SPDX-License-Identifier: AGPL-3.0-only
//! Index maintenance. Every change to a memory, an edge, a vector or a namespace goes
//! through these helpers, which update the tag, entity and full-text indexes, the
//! vector table, the graph, the per-namespace counts, the size estimate and the
//! idempotency purge in one step. Nothing rebuilds an index after a change.
//!
//! Invariants they keep:
//! - a memory has a vector exactly when its embedding is `Ready`;
//! - a ghost has no tags, labels, text, vector, or tag, entity or full-text entry;
//! - an edge joins two existing memories;
//! - `counts` has one entry per namespace;
//! - `estimated_bytes` is what [`Store::estimate_from_scratch`] computes.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::embed::ModelIdentity;
use crate::graph::Graph;
use crate::id::{MemoryId, NamespaceId};
use crate::model::{Edge, EdgeKey, EmbeddingState, Labels, Memory, Namespace, Phase, Tag};
use crate::search::entity::EntityIndex;
use crate::search::fts::{FtsDoc, FtsIndex};
use crate::search::tags::TagIndex;
use crate::search::vector::{VectorTable, is_valid_vector};
use crate::time::Timestamp;

use super::{IdempotencyEntry, NamespaceCounts, Store};

/// Fixed part of the size estimate of each memory.
const MEMORY_OVERHEAD_BYTES: u64 = 300;
/// Size estimate of each edge.
const EDGE_BYTES: u64 = 100;
/// Fixed part of the size estimate of each idempotency entry.
const IDEMPOTENCY_OVERHEAD_BYTES: u64 = 200;

/// The size estimate of one memory: three copies of its text, four bytes per vector
/// value unless it is a ghost (counted before the vector lands), and a fixed overhead.
fn memory_bytes(memory: &Memory, dimensions: usize) -> u64 {
    let vector = if memory.phase == Phase::Ghost {
        0
    } else {
        (dimensions as u64).saturating_mul(4)
    };
    memory
        .text_bytes()
        .saturating_mul(3)
        .saturating_add(vector)
        .saturating_add(MEMORY_OVERHEAD_BYTES)
}

fn entry_bytes(entry: &IdempotencyEntry) -> u64 {
    IDEMPOTENCY_OVERHEAD_BYTES.saturating_add(entry.response.len() as u64)
}

fn count(counts: &mut NamespaceCounts, memory: &Memory, add: bool) {
    let step = |field: &mut u64, by: u64| {
        *field = if add {
            field.saturating_add(by)
        } else {
            field.saturating_sub(by)
        };
    };
    step(&mut counts.memories, 1);
    match memory.phase {
        Phase::Full => step(&mut counts.full, 1),
        Phase::Summary => step(&mut counts.summary, 1),
        Phase::Ghost => step(&mut counts.ghost, 1),
    }
    if memory.is_permastore {
        step(&mut counts.permastore, 1);
    }
    match memory.embedding {
        EmbeddingState::Ready => step(&mut counts.ready, 1),
        EmbeddingState::Pending => step(&mut counts.pending, 1),
        EmbeddingState::Failed => step(&mut counts.failed, 1),
        EmbeddingState::None => step(&mut counts.no_vector, 1),
    }
    let text = memory.summary.len() + memory.full_text.as_ref().map_or(0, String::len);
    step(&mut counts.text_bytes, text as u64);
}

/// The tags column holds the plain tags only, without label terms: recalld's native
/// write path keeps labels apart from tags, and the ranking fixture depends on it.
fn index_text(fts: &mut FtsIndex, memory: &Memory) {
    fts.upsert(
        memory.namespace,
        memory.id,
        &memory.summary,
        memory.full_text.as_deref(),
        &memory
            .tags
            .iter()
            .map(Tag::as_str)
            .collect::<Vec<_>>()
            .join(" "),
    );
}

/// Adds a memory that is not a ghost to the tag, entity and full-text indexes.
fn index_content(tags: &mut TagIndex, entities: &mut EntityIndex, fts: &mut FtsIndex, m: &Memory) {
    if m.phase == Phase::Ghost {
        return;
    }
    tags.add(m.namespace, m.id, &m.tags);
    entities.add(m.namespace, m.id, &m.labels.entities);
    index_text(fts, m);
}

fn purge_idempotency(
    idempotency: &mut BTreeMap<String, IdempotencyEntry>,
    estimated_bytes: &mut u64,
    id: MemoryId,
) {
    idempotency.retain(|_, entry| {
        let keep = !entry.text_ids.contains(&id);
        if !keep {
            *estimated_bytes = estimated_bytes.saturating_sub(entry_bytes(entry));
        }
        keep
    });
}

impl Store {
    /// Adds a new memory to the store and every index. With `vector`, which must be
    /// valid for the store's model, the memory's embedding becomes `Ready`; without it a
    /// `Ready` memory becomes `Pending`. A ghost gets no vector and embedding `None`.
    ///
    /// Returns false, changing nothing, when the ID exists, the namespace does not, or
    /// the vector is not valid.
    pub(crate) fn insert_memory(&mut self, mut memory: Memory, vector: Option<&[f32]>) -> bool {
        if self.memories.contains_key(&memory.id)
            || !self.namespaces.contains_key(&memory.namespace)
        {
            return false;
        }
        let dimensions = self.vectors.dimensions();
        if vector.is_some_and(|v| !is_valid_vector(v, dimensions)) {
            return false;
        }
        if memory.phase == Phase::Ghost {
            memory.embedding = EmbeddingState::None;
        } else if let Some(v) = vector {
            self.vectors.insert(memory.id, v);
            memory.embedding = EmbeddingState::Ready;
        } else if memory.embedding == EmbeddingState::Ready {
            memory.embedding = EmbeddingState::Pending;
        }
        index_content(&mut self.tags, &mut self.entities, &mut self.fts, &memory);
        count(
            self.counts.entry(memory.namespace).or_default(),
            &memory,
            true,
        );
        self.estimated_bytes = self
            .estimated_bytes
            .saturating_add(memory_bytes(&memory, dimensions));
        self.memories.insert(memory.id, memory);
        true
    }

    /// Changes one memory with `f` and brings every index in step: counts and estimate
    /// always; tag, entity and full-text entries when its namespace, phase, text, tags
    /// or labels changed; the vector when its embedding is no longer `Ready`; the
    /// idempotency purge when it lost its text by becoming a ghost.
    ///
    /// `f` must not change the ID. To give a memory a vector use
    /// [`Store::attach_vector`]; to move it, [`Store::move_memory`], which checks the
    /// destination.
    pub(crate) fn update_memory<R>(
        &mut self,
        id: MemoryId,
        f: impl FnOnce(&mut Memory) -> R,
    ) -> Option<R> {
        let Store {
            memories,
            tags,
            entities,
            fts,
            vectors,
            counts,
            estimated_bytes,
            idempotency,
            ..
        } = self;
        let memory = memories.get_mut(&id)?;
        let before = memory.clone();
        let out = f(memory);
        memory.id = id;
        let after: &Memory = memory;
        let dimensions = vectors.dimensions();

        count(counts.entry(before.namespace).or_default(), &before, false);
        count(counts.entry(after.namespace).or_default(), after, true);
        *estimated_bytes = estimated_bytes
            .saturating_sub(memory_bytes(&before, dimensions))
            .saturating_add(memory_bytes(after, dimensions));

        let moved = before.namespace != after.namespace;
        let ghost_changed = (before.phase == Phase::Ghost) != (after.phase == Phase::Ghost);
        if moved || ghost_changed || before.tags != after.tags {
            if before.phase != Phase::Ghost {
                tags.remove(before.namespace, id, &before.tags);
            }
            if after.phase != Phase::Ghost {
                tags.add(after.namespace, id, &after.tags);
            }
        }
        if moved || ghost_changed || before.labels.entities != after.labels.entities {
            entities.remove(before.namespace, id);
            if after.phase != Phase::Ghost {
                entities.add(after.namespace, id, &after.labels.entities);
            }
        }
        let text_changed = before.summary != after.summary
            || before.full_text != after.full_text
            || before.tags != after.tags
            || before.labels != after.labels;
        if after.phase == Phase::Ghost {
            fts.remove(id);
        } else if ghost_changed || text_changed {
            index_text(fts, after);
        } else if moved {
            fts.set_namespace(id, after.namespace);
        }
        if after.embedding != EmbeddingState::Ready {
            vectors.remove(id);
        }
        if before.phase != Phase::Ghost && after.phase == Phase::Ghost {
            purge_idempotency(idempotency, estimated_bytes, id);
        }
        Some(out)
    }

    /// Full to Summary: the full text fades. Returns false unless the memory is `Full`.
    pub(crate) fn fade_to_summary(&mut self, id: MemoryId, now: Timestamp) -> bool {
        self.update_memory(id, |m| {
            if m.phase != Phase::Full {
                return false;
            }
            m.phase = Phase::Summary;
            m.full_text = None;
            m.phase_changed_at = now;
            true
        })
        .unwrap_or(false)
    }

    /// To Ghost: summary, full text, tags, labels and vector go; edges and the
    /// lifecycle numbers stay; idempotency entries that carry its text are dropped.
    /// Returns false for a ghost or an unknown ID.
    pub(crate) fn fade_to_ghost(&mut self, id: MemoryId, now: Timestamp) -> bool {
        self.update_memory(id, |m| {
            if m.phase == Phase::Ghost {
                return false;
            }
            m.phase = Phase::Ghost;
            m.summary.clear();
            m.full_text = None;
            m.tags.clear();
            m.labels = Labels::default();
            m.embedding = EmbeddingState::None;
            m.embed_failures = 0;
            m.phase_changed_at = now;
            true
        })
        .unwrap_or(false)
    }

    /// Moves a memory to another namespace, with its index entries. Content, vector,
    /// history and edges stay. Returns false when either does not exist.
    pub(crate) fn move_memory(&mut self, id: MemoryId, to: NamespaceId) -> bool {
        if !self.namespaces.contains_key(&to) {
            return false;
        }
        self.update_memory(id, |m| m.namespace = to).is_some()
    }

    /// Removes a memory with its vector, its edges and its index entries, and the
    /// idempotency entries that carry its text. Returns it with the removed edges.
    pub(crate) fn remove_memory(&mut self, id: MemoryId) -> Option<(Memory, Vec<Edge>)> {
        let memory = self.memories.remove(&id)?;
        let dimensions = self.vectors.dimensions();
        if memory.phase != Phase::Ghost {
            self.tags.remove(memory.namespace, id, &memory.tags);
        }
        self.entities.remove(memory.namespace, id);
        self.fts.remove(id);
        self.vectors.remove(id);
        count(
            self.counts.entry(memory.namespace).or_default(),
            &memory,
            false,
        );
        let edges = self.graph.remove_node(id);
        self.estimated_bytes = self
            .estimated_bytes
            .saturating_sub(memory_bytes(&memory, dimensions))
            .saturating_sub(EDGE_BYTES.saturating_mul(edges.len() as u64));
        purge_idempotency(&mut self.idempotency, &mut self.estimated_bytes, id);
        Some((memory, edges))
    }

    /// Stores the vector of a memory that is not a ghost and marks it `Ready`, with its
    /// failure count back at 0. Returns false, changing nothing, for an unknown ID, a
    /// ghost or a vector that is not valid for the store's model.
    pub(crate) fn attach_vector(&mut self, id: MemoryId, vector: &[f32]) -> bool {
        let valid = is_valid_vector(vector, self.vectors.dimensions());
        if !valid
            || self
                .memories
                .get(&id)
                .is_none_or(|m| m.phase == Phase::Ghost)
        {
            return false;
        }
        self.vectors.insert(id, vector);
        self.update_memory(id, |m| {
            m.embedding = EmbeddingState::Ready;
            m.embed_failures = 0;
        })
        .is_some()
    }

    /// Takes a new model: every vector goes and every memory that is not a ghost is
    /// `Pending` with no failures. Edges are not touched.
    pub(crate) fn reset_vectors(&mut self, model: ModelIdentity) {
        let dimensions = usize::try_from(model.dimensions).unwrap_or(0);
        self.model = model;
        self.vectors.reset(dimensions);
        let ids: Vec<MemoryId> = self
            .memories
            .values()
            .filter(|m| m.phase != Phase::Ghost)
            .map(|m| m.id)
            .collect();
        for id in ids {
            self.update_memory(id, |m| {
                m.embedding = EmbeddingState::Pending;
                m.embed_failures = 0;
            });
        }
        self.estimated_bytes = self.estimate_from_scratch();
    }

    /// Adds an edge between two existing memories. Returns false, changing nothing,
    /// when an end is missing, the edge is a loop or one with the same key exists.
    pub(crate) fn insert_edge(&mut self, edge: Edge) -> bool {
        if !self.memories.contains_key(&edge.source) || !self.memories.contains_key(&edge.target) {
            return false;
        }
        let inserted = self.graph.insert(edge);
        if inserted {
            self.estimated_bytes = self.estimated_bytes.saturating_add(EDGE_BYTES);
        }
        inserted
    }

    /// Removes one edge.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no operation removes a single edge yet")
    )]
    pub(crate) fn remove_edge(&mut self, key: &EdgeKey) -> Option<Edge> {
        let edge = self.graph.remove(key)?;
        self.estimated_bytes = self.estimated_bytes.saturating_sub(EDGE_BYTES);
        Some(edge)
    }

    /// Removes every edge for which `drop` holds (automatic `Associative` edges on a
    /// model change, for example).
    pub(crate) fn remove_edges_where(&mut self, drop: impl FnMut(&Edge) -> bool) -> Vec<Edge> {
        let edges = self.graph.remove_where(drop);
        self.estimated_bytes = self
            .estimated_bytes
            .saturating_sub(EDGE_BYTES.saturating_mul(edges.len() as u64));
        edges
    }

    /// The next namespace ID; IDs are never reused. `None` once `u32` is used up.
    pub(crate) fn allocate_namespace_id(&mut self) -> Option<NamespaceId> {
        let id = self.next_namespace_id;
        self.next_namespace_id = id.checked_add(1)?;
        Some(NamespaceId::new(id))
    }

    /// Adds a namespace. Returns false, changing nothing, when its ID exists.
    pub(crate) fn insert_namespace(&mut self, namespace: Namespace) -> bool {
        if self.namespaces.contains_key(&namespace.id) {
            return false;
        }
        self.counts.insert(namespace.id, NamespaceCounts::default());
        self.namespaces.insert(namespace.id, namespace);
        true
    }

    /// Removes a namespace that holds no memories.
    pub(crate) fn remove_namespace(&mut self, id: NamespaceId) -> Option<Namespace> {
        if self.namespace_counts(id).memories > 0 {
            return None;
        }
        self.counts.remove(&id);
        self.namespaces.remove(&id)
    }

    /// Records an idempotent answer under `key`, replacing an older one.
    pub(crate) fn insert_idempotency(&mut self, key: String, entry: IdempotencyEntry) {
        self.estimated_bytes = self.estimated_bytes.saturating_add(entry_bytes(&entry));
        if let Some(old) = self.idempotency.insert(key, entry) {
            self.estimated_bytes = self.estimated_bytes.saturating_sub(entry_bytes(&old));
        }
    }

    /// Drops the idempotent answer under `key`.
    pub(crate) fn remove_idempotency(&mut self, key: &str) -> Option<IdempotencyEntry> {
        let entry = self.idempotency.remove(key)?;
        self.estimated_bytes = self.estimated_bytes.saturating_sub(entry_bytes(&entry));
        Some(entry)
    }

    /// The size estimate computed by walking the whole store.
    pub(crate) fn estimate_from_scratch(&self) -> u64 {
        let dimensions = self.vectors.dimensions();
        let memories = self.memories.values().fold(0_u64, |acc, m| {
            acc.saturating_add(memory_bytes(m, dimensions))
        });
        let entries = self
            .idempotency
            .values()
            .fold(0_u64, |acc, e| acc.saturating_add(entry_bytes(e)));
        memories
            .saturating_add(EDGE_BYTES.saturating_mul(self.graph.len() as u64))
            .saturating_add(entries)
    }

    /// Builds every index again from the memories, edges and vectors alone: the tag,
    /// entity and full-text indexes, the graph's adjacency, the vector table's slots, the
    /// counts and the size estimate. Tests only; nothing else ever rebuilds an index.
    #[doc(hidden)]
    pub fn rebuild_indexes_for_test(&mut self) {
        self.rebuild_indexes();
    }

    /// Rebuilds every derived structure. Used when a store is loaded and by tests.
    pub(crate) fn rebuild_indexes(&mut self) {
        self.tags = TagIndex::new();
        self.entities = EntityIndex::new();
        self.fts.reset();
        for memory in self.memories.values() {
            index_content(&mut self.tags, &mut self.entities, &mut self.fts, memory);
        }
        let mut graph = Graph::new();
        for edge in self.graph.iter() {
            graph.insert(*edge);
        }
        self.graph = graph;
        // Slot order is kept, so the ID-to-slot table of the live index compares.
        let mut vectors = VectorTable::new(self.vectors.dimensions());
        for (id, v) in self.vectors.iter() {
            vectors.insert(id, v);
        }
        self.vectors = vectors;
        self.counts = self
            .namespaces
            .keys()
            .map(|id| (*id, NamespaceCounts::default()))
            .collect();
        for memory in self.memories.values() {
            count(
                self.counts.entry(memory.namespace).or_default(),
                memory,
                true,
            );
        }
        self.estimated_bytes = self.estimate_from_scratch();
    }

    /// Everything the indexes hold, in a form that compares: take one before and one
    /// after [`Store::rebuild_indexes_for_test`] and assert they are equal. Tags and
    /// entity keys appear as hashes. For the full-text index it holds the indexed
    /// documents and the ranked IDs of every query in `queries` in every namespace.
    #[doc(hidden)]
    pub fn index_snapshot_for_test(&self, queries: &[&str]) -> IndexSnapshot {
        let hash = |text: &str| {
            let mut h = DefaultHasher::new();
            text.hash(&mut h);
            h.finish()
        };
        let mut fts_answers = Vec::new();
        for ns in self.namespaces.keys() {
            for q in queries {
                let hits = self
                    .fts
                    .search(*ns, q, self.memories.len().max(1))
                    .map(|hits| hits.into_iter().map(|h| h.0).collect());
                fts_answers.push((*ns, hash(q), hits.map_err(|_| ())));
            }
        }
        IndexSnapshot {
            tags: self
                .tags
                .entries()
                .map(|(ns, tag, ids)| (ns, hash(tag.as_str()), ids.iter().copied().collect()))
                .collect(),
            entities: self
                .entities
                .entries()
                .map(|(key, keys)| (*key, keys.iter().map(|k| hash(k)).collect()))
                .collect(),
            entity_postings: self
                .entities
                .postings()
                .map(|(ns, key, ids)| (ns, hash(key), ids.iter().copied().collect()))
                .collect(),
            fts_docs: self.fts.docs().iter().map(|(id, d)| (*id, *d)).collect(),
            fts_answers,
            adjacency: self
                .graph
                .adjacency()
                .iter()
                .map(|(id, keys)| (*id, keys.iter().copied().collect()))
                .collect(),
            vectors: self
                .vectors
                .iter()
                .map(|(id, v)| (id, v.iter().map(|x| x.to_bits()).collect()))
                .collect(),
            vector_slots: self
                .vectors
                .slots()
                .iter()
                .map(|(id, slot)| (*id, *slot, self.vectors.get(*id).map(<[f32]>::len)))
                .collect(),
            counts: self.counts.iter().map(|(id, c)| (*id, *c)).collect(),
            estimated_bytes: self.estimated_bytes,
        }
    }
}

impl Store {
    /// The invariants that a rebuild cannot restore, because a rebuild starts from the
    /// memories and edges themselves: a vector exactly for `Ready` memories, nothing
    /// left of a ghost but its numbers and edges, both ends of every edge, and one
    /// count entry per namespace and no memory outside one. Each broken rule is one
    /// line, naming only IDs.
    #[doc(hidden)]
    pub fn invariant_violations_for_test(&self) -> Vec<String> {
        let mut out = Vec::new();
        let tagged: BTreeSet<MemoryId> = self
            .tags
            .entries()
            .flat_map(|(_, _, ids)| ids.iter().copied())
            .collect();
        let with_keys: BTreeSet<MemoryId> =
            self.entities.entries().map(|((_, id), _)| *id).collect();
        let posted: BTreeSet<MemoryId> = self
            .entities
            .postings()
            .flat_map(|(_, _, ids)| ids.iter().copied())
            .collect();
        for m in self.memories.values() {
            let id = m.id;
            if self.vectors.contains(id) != (m.embedding == EmbeddingState::Ready) {
                out.push(format!(
                    "{id}: vector present = {}, embedding {:?}",
                    self.vectors.contains(id),
                    m.embedding
                ));
            }
            if !self.namespaces.contains_key(&m.namespace) {
                out.push(format!("{id}: namespace {:?} does not exist", m.namespace));
            }
            let ghost = m.phase == Phase::Ghost;
            if ghost != (m.embedding == EmbeddingState::None) {
                out.push(format!(
                    "{id}: phase {:?} with embedding {:?}",
                    m.phase, m.embedding
                ));
            }
            if ghost {
                if !m.summary.is_empty()
                    || m.full_text.is_some()
                    || !m.tags.is_empty()
                    || m.labels != Labels::default()
                {
                    out.push(format!("{id}: a ghost keeps text, tags or labels"));
                }
                if tagged.contains(&id)
                    || with_keys.contains(&id)
                    || posted.contains(&id)
                    || self.fts.contains(id)
                {
                    out.push(format!("{id}: a ghost has index entries"));
                }
            }
        }
        for e in self.graph.iter() {
            if !self.memories.contains_key(&e.source) || !self.memories.contains_key(&e.target) {
                out.push(format!("edge {:?}: an end is missing", e.key()));
            }
        }
        if !self.counts.keys().eq(self.namespaces.keys()) {
            out.push("counts do not have exactly one entry per namespace".to_owned());
        }
        out
    }
}

/// What the indexes hold, for comparing live indexes with a rebuild. Holds IDs, counts
/// and hashes only.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSnapshot {
    tags: Vec<(NamespaceId, u64, Vec<MemoryId>)>,
    entities: Vec<((NamespaceId, MemoryId), Vec<u64>)>,
    entity_postings: Vec<(NamespaceId, u64, Vec<MemoryId>)>,
    fts_docs: Vec<(MemoryId, FtsDoc)>,
    fts_answers: Vec<(NamespaceId, u64, Result<Vec<MemoryId>, ()>)>,
    adjacency: Vec<(MemoryId, Vec<EdgeKey>)>,
    vectors: Vec<(MemoryId, Vec<u32>)>,
    vector_slots: Vec<(MemoryId, usize, Option<usize>)>,
    counts: Vec<(NamespaceId, NamespaceCounts)>,
    estimated_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::EmbeddingProvider;
    use crate::id::StoreId;
    use crate::model::{EdgeType, NamespacePolicy, Tag};
    use crate::testing::FakeProvider;

    const QUERIES: [&str; 6] = ["alpha", "beta", "gamma", "alpha beta", "maya", "travel"];

    fn mid(n: u8) -> MemoryId {
        let mut b = [0; 16];
        b[0] = 1;
        b[15] = n;
        MemoryId::from_bytes(b)
    }

    fn t(ms: i64) -> Timestamp {
        Timestamp::from_millis(ms)
    }

    fn store() -> Store {
        let model = FakeProvider::new().identity().clone();
        Store::create(StoreId::default(), model, t(1_000))
    }

    fn memory(n: u8, ns: NamespaceId, summary: &str) -> Memory {
        Memory {
            id: mid(n),
            namespace: ns,
            created_at: t(1_000),
            summary: summary.to_owned(),
            full_text: Some(format!("{summary} full text gamma")),
            tags: vec![Tag::new("travel").unwrap()],
            labels: Labels {
                entities: vec!["Maya Smith".into()],
                topics: vec!["Seats".into()],
                emotions: vec![],
            },
            phase: Phase::Full,
            stability: 3.7145,
            difficulty: 5.0,
            is_permastore: false,
            decay_anchor_at: t(1_000),
            retrieved_at: None,
            reinforced_at: None,
            phase_changed_at: t(1_000),
            access_count: 0,
            access_history: Vec::new(),
            embedding: EmbeddingState::Pending,
            embed_failures: 0,
            saved_by: "local".into(),
            client: None,
        }
    }

    fn vector(text: &str) -> Vec<f32> {
        FakeProvider::new().embed(text).unwrap()
    }

    fn assert_matches_rebuild(s: &mut Store) {
        let live = s.index_snapshot_for_test(&QUERIES);
        s.rebuild_indexes_for_test();
        let rebuilt = s.index_snapshot_for_test(&QUERIES);
        assert_eq!(live, rebuilt);
        assert_eq!(s.invariant_violations_for_test(), Vec::<String>::new());
    }

    fn second_namespace(s: &mut Store) -> NamespaceId {
        let id = s.allocate_namespace_id().unwrap();
        assert!(s.insert_namespace(Namespace {
            id,
            name: "Work".into(),
            created_at: t(0),
            policy: NamespacePolicy::default(),
        }));
        id
    }

    #[test]
    fn store_can_sit_behind_a_shared_lock() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Store>();
    }

    #[test]
    fn create_makes_the_default_namespace() {
        let s = store();
        let ns = s.namespace_by_name("default").unwrap();
        assert_eq!(ns.id, NamespaceId::new(1));
        assert_eq!(s.namespace_by_name("DEFAULT").unwrap().id, ns.id);
        assert!(s.namespace_by_name("nope").is_none());
        assert_eq!(s.summary().namespace_count, 1);
        assert_eq!(s.estimated_bytes(), 0);
        assert_eq!(s.model().id, "fake-64");
    }

    #[test]
    fn insert_indexes_everything_and_counts() {
        let mut s = store();
        let ns = NamespaceId::new(1);
        assert!(s.insert_memory(memory(1, ns, "alpha"), None));
        assert!(s.insert_memory(memory(2, ns, "beta"), Some(&vector("beta"))));
        assert!(!s.insert_memory(memory(2, ns, "dup"), None));
        assert!(!s.insert_memory(memory(3, NamespaceId::new(9), "no ns"), None));
        assert!(!s.insert_memory(memory(3, ns, "bad vector"), Some(&[1.0, 0.0])));

        assert_eq!(s.memory(mid(2)).unwrap().embedding, EmbeddingState::Ready);
        assert!(s.vector(mid(2)).is_some());
        let c = s.namespace_counts(ns);
        assert_eq!((c.memories, c.full, c.ready, c.pending), (2, 2, 1, 1));
        assert_eq!(
            s.tag_index()
                .memories_with_all(ns, &[Tag::new("travel").unwrap()]),
            [mid(1), mid(2)]
        );
        assert_eq!(
            s.entity_index()
                .find_by_entities(ns, &["Smith".into()], mid(0))
                .len(),
            2
        );
        let hits = s.fts().search(ns, "alpha", 10).unwrap();
        assert_eq!(hits[0].0, mid(1));
        assert_eq!(s.fts().search(ns, "travel", 10).unwrap().len(), 2);
        assert!(
            s.fts().search(ns, "maya", 10).unwrap().is_empty(),
            "label terms are not indexed, as in recalld's native write path"
        );
        assert_eq!(s.estimated_bytes(), s.estimate_from_scratch());
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn phase_changes_update_every_index() {
        let mut s = store();
        let ns = NamespaceId::new(1);
        s.insert_memory(memory(1, ns, "alpha"), Some(&vector("alpha")));
        s.insert_memory(memory(2, ns, "beta"), None);
        assert!(s.fade_to_summary(mid(1), t(5_000)));
        assert!(!s.fade_to_summary(mid(1), t(5_000)));
        let m = s.memory(mid(1)).unwrap();
        assert_eq!((m.phase, m.full_text.is_none()), (Phase::Summary, true));
        assert_eq!(m.phase_changed_at, t(5_000));
        assert!(
            s.fts()
                .search(ns, "gamma", 10)
                .unwrap()
                .iter()
                .all(|h| h.0 != mid(1))
        );
        assert!(s.vector(mid(1)).is_some(), "a summary keeps its vector");
        assert_matches_rebuild(&mut s);

        assert!(s.fade_to_ghost(mid(1), t(6_000)));
        assert!(!s.fade_to_ghost(mid(1), t(6_000)));
        let m = s.memory(mid(1)).unwrap();
        assert_eq!(m.embedding, EmbeddingState::None);
        assert!(m.summary.is_empty() && m.tags.is_empty() && m.labels.entities.is_empty());
        assert!(s.vector(mid(1)).is_none());
        assert!(!s.fts().contains(mid(1)));
        assert!(s.entity_index().keys_of(ns, mid(1)).is_none());
        let c = s.namespace_counts(ns);
        assert_eq!((c.ghost, c.no_vector, c.full, c.ready), (1, 1, 1, 0));
        assert!(
            !s.attach_vector(mid(1), &vector("x")),
            "a ghost gets no vector"
        );
        assert_eq!(s.estimated_bytes(), s.estimate_from_scratch());
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn moves_carry_index_entries() {
        let mut s = store();
        let ns = NamespaceId::new(1);
        let work = second_namespace(&mut s);
        s.insert_memory(memory(1, ns, "alpha"), Some(&vector("alpha")));
        s.insert_memory(memory(2, ns, "alpha beta"), None);
        assert!(s.move_memory(mid(1), work));
        assert!(!s.move_memory(mid(1), NamespaceId::new(99)));
        assert_eq!(s.namespace_counts(ns).memories, 1);
        assert_eq!(s.namespace_counts(work).memories, 1);
        assert_eq!(s.fts().search(work, "alpha", 10).unwrap()[0].0, mid(1));
        assert_eq!(s.fts().search(ns, "alpha", 10).unwrap().len(), 1);
        assert_eq!(
            s.entity_index()
                .find_by_entities(work, &["Maya Smith".into()], mid(0)),
            [(mid(1), 2)]
        );
        assert!(s.vector(mid(1)).is_some());
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn removal_takes_edges_vectors_entries_and_idempotency() {
        let mut s = store();
        let ns = NamespaceId::new(1);
        s.insert_memory(memory(1, ns, "alpha"), Some(&vector("alpha")));
        s.insert_memory(memory(2, ns, "beta"), None);
        s.insert_memory(memory(3, ns, "gamma"), None);
        let e = |a, b, edge_type| Edge {
            source: mid(a),
            target: mid(b),
            edge_type,
            weight: 0.5,
            auto_created: true,
            created_at: t(1_000),
        };
        assert!(s.insert_edge(e(1, 2, EdgeType::Entity)));
        assert!(s.insert_edge(e(3, 1, EdgeType::Temporal)));
        assert!(s.insert_edge(e(2, 3, EdgeType::Associative)));
        assert!(
            !s.insert_edge(e(2, 9, EdgeType::Associative)),
            "both ends must exist"
        );
        let entry = |ids: Vec<MemoryId>| IdempotencyEntry {
            request_hash: [0; 32],
            created_at: t(1_000),
            response: b"{}".to_vec(),
            text_ids: ids,
        };
        s.insert_idempotency("a".into(), entry(vec![mid(1)]));
        s.insert_idempotency("b".into(), entry(vec![]));
        s.insert_idempotency("c".into(), entry(vec![mid(2)]));
        assert_eq!(s.estimated_bytes(), s.estimate_from_scratch());

        let (gone, edges) = s.remove_memory(mid(1)).unwrap();
        assert_eq!(gone.id, mid(1));
        assert_eq!(edges.len(), 2);
        assert!(s.remove_memory(mid(1)).is_none());
        assert!(s.vector(mid(1)).is_none());
        assert_eq!(s.graph().len(), 1);
        assert_eq!(s.graph().degree(mid(2)), 1);
        assert!(s.idempotency.contains_key("b") && !s.idempotency.contains_key("a"));
        assert!(s.fade_to_ghost(mid(2), t(2_000)));
        assert!(
            !s.idempotency.contains_key("c"),
            "losing its text purges too"
        );
        assert_eq!(s.graph().len(), 1, "a ghost keeps its edges");
        assert_eq!(s.estimated_bytes(), s.estimate_from_scratch());
        assert!(s.remove_idempotency("b").is_some());
        assert_eq!(s.estimated_bytes(), s.estimate_from_scratch());
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn vectors_attach_and_reset_with_the_model() {
        let mut s = store();
        let ns = NamespaceId::new(1);
        s.insert_memory(memory(1, ns, "alpha"), None);
        s.insert_memory(memory(2, ns, "beta"), None);
        s.update_memory(mid(2), |m| m.embed_failures = 2);
        assert!(s.attach_vector(mid(2), &vector("beta")));
        assert!(!s.attach_vector(mid(1), &[0.5; 64]), "not of unit length");
        assert!(!s.attach_vector(mid(9), &vector("x")));
        let m = s.memory(mid(2)).unwrap();
        assert_eq!((m.embedding, m.embed_failures), (EmbeddingState::Ready, 0));
        assert_eq!(s.namespace_counts(ns).ready, 1);
        s.update_memory(mid(2), |m| m.embedding = EmbeddingState::Failed);
        assert!(
            s.vector(mid(2)).is_none(),
            "the vector goes with the Ready state"
        );
        assert!(s.attach_vector(mid(2), &vector("beta")));

        let new_model = FakeProvider::with_dim(768).identity().clone();
        s.reset_vectors(new_model);
        assert_eq!(s.model().id, "fake-768");
        assert!(s.vectors().is_empty());
        assert_eq!(s.vectors().dimensions(), 768);
        assert!(s.memories().all(|m| m.embedding == EmbeddingState::Pending));
        assert_eq!(s.namespace_counts(ns).pending, 2);
        assert!(s.attach_vector(mid(1), &FakeProvider::with_dim(768).embed("a").unwrap()));
        assert_eq!(s.estimated_bytes(), s.estimate_from_scratch());
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn access_updates_touch_no_index_but_keep_counts() {
        let mut s = store();
        let ns = NamespaceId::new(1);
        s.insert_memory(memory(1, ns, "alpha"), None);
        let before = s.index_snapshot_for_test(&QUERIES);
        s.update_memory(mid(1), |m| {
            m.access_count += 1;
            m.retrieved_at = Some(t(2_000));
        });
        assert_eq!(s.index_snapshot_for_test(&QUERIES), before);
        s.update_memory(mid(1), |m| m.is_permastore = true);
        assert_eq!(s.namespace_counts(ns).permastore, 1);
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn namespaces_and_edges_by_predicate() {
        let mut s = store();
        let work = second_namespace(&mut s);
        assert_eq!(s.namespace_counts(work), NamespaceCounts::default());
        s.insert_memory(memory(1, work, "alpha"), None);
        s.insert_memory(memory(2, work, "beta"), None);
        assert!(s.remove_namespace(work).is_none(), "not empty");
        let e = Edge {
            source: mid(1),
            target: mid(2),
            edge_type: EdgeType::Associative,
            weight: 0.9,
            auto_created: true,
            created_at: t(0),
        };
        s.insert_edge(e);
        assert_eq!(
            s.remove_edges_where(|e| e.auto_created && e.edge_type == EdgeType::Associative)
                .len(),
            1
        );
        assert!(s.remove_edge(&e.key()).is_none());
        s.remove_memory(mid(1));
        s.remove_memory(mid(2));
        assert!(s.remove_namespace(work).is_some());
        assert_eq!(
            s.allocate_namespace_id(),
            Some(NamespaceId::new(3)),
            "IDs are not reused"
        );
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn memory_view_reads_strengths_and_hides_ghost_summary() {
        let mut s = store();
        let ns = NamespaceId::new(1);
        s.insert_memory(memory(1, ns, "alpha"), None);
        let m = s.memory(mid(1)).unwrap().clone();
        let v = s.memory_view(&m, t(1_000), false, false);
        assert_eq!((v.strength, v.effective_strength), (1.0, 1.0));
        assert_eq!(v.namespace, "default");
        assert!(v.full_text.is_none() && v.access_history.is_none());
        let v = s.memory_view(&m, t(1_000), true, true);
        assert!(v.full_text.is_some() && v.access_history.is_some());
        s.fade_to_ghost(mid(1), t(2_000));
        let m = s.memory(mid(1)).unwrap().clone();
        assert!(s.memory_view(&m, t(2_000), true, false).summary.is_none());
    }
}
