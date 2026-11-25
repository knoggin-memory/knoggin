// SPDX-License-Identifier: AGPL-3.0-only
//! `Store`: one store in memory, with every index. Operations live in `impl Store`
//! blocks spread over the modules that own them; this module holds the struct, its
//! constructor, the read accessors and (in `index`) the helpers that keep every index
//! in step with the memories.

mod index;
mod read;

use std::collections::BTreeMap;

use crate::embed::ModelIdentity;
use crate::graph::Graph;
use crate::id::{MemoryId, NamespaceId, StoreId};
use crate::model::limits::DEFAULT_NAMESPACE;
use crate::model::{Memory, Namespace, NamespacePolicy, Phase};
use crate::ops::{ChangeCounters, MemoryView, StoreSummary};
use crate::search::entity::EntityIndex;
use crate::search::fts::FtsIndex;
use crate::search::tags::TagIndex;
use crate::search::vector::VectorTable;
use crate::time::Timestamp;

pub use index::IndexSnapshot;

/// One stored idempotent answer.
#[derive(Clone, PartialEq, Eq)]
pub struct IdempotencyEntry {
    /// SHA-256 of the operation name, a zero byte and the canonical request JSON.
    pub request_hash: [u8; 32],
    /// When the operation ran.
    pub created_at: Timestamp,
    /// The JSON bytes of the result.
    pub response: Vec<u8>,
    /// Memories whose text the response carries; forgetting one drops the entry.
    pub text_ids: Vec<MemoryId>,
}

impl std::fmt::Debug for IdempotencyEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdempotencyEntry")
            .field("created_at", &self.created_at)
            .field("response_bytes", &self.response.len())
            .field("text_ids", &self.text_ids.len())
            .finish_non_exhaustive()
    }
}

/// Counts of one namespace, kept up to date by every change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NamespaceCounts {
    /// Memories in every phase.
    pub memories: u64,
    /// Phase `Full`.
    pub full: u64,
    /// Phase `Summary`.
    pub summary: u64,
    /// Phase `Ghost`.
    pub ghost: u64,
    /// Permanent memories.
    pub permastore: u64,
    /// Embedding `Ready`.
    pub ready: u64,
    /// Embedding `Pending`.
    pub pending: u64,
    /// Embedding `Failed`.
    pub failed: u64,
    /// Embedding `None` (ghosts).
    pub no_vector: u64,
    /// Bytes of every summary and full text.
    pub text_bytes: u64,
}

/// One store in memory: namespaces, memories, vectors, edges and every index.
///
/// Fields are crate-visible so that each module can add its operations; changes to
/// memories, edges, vectors and namespaces go through the helpers in `store::index`,
/// which keep the indexes, the counts and the size estimate in step.
pub struct Store {
    pub(crate) id: StoreId,
    pub(crate) model: ModelIdentity,
    pub(crate) namespaces: BTreeMap<NamespaceId, Namespace>,
    pub(crate) next_namespace_id: u32,
    pub(crate) memories: BTreeMap<MemoryId, Memory>,
    pub(crate) graph: Graph,
    pub(crate) vectors: VectorTable,
    pub(crate) tags: TagIndex,
    pub(crate) entities: EntityIndex,
    pub(crate) fts: FtsIndex,
    pub(crate) counts: BTreeMap<NamespaceId, NamespaceCounts>,
    pub(crate) changes: ChangeCounters,
    pub(crate) idempotency: BTreeMap<String, IdempotencyEntry>,
    pub(crate) last_write_at: Option<Timestamp>,
    pub(crate) last_write_client: Option<String>,
    pub(crate) estimated_bytes: u64,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("id", &self.id)
            .field("namespaces", &self.namespaces.len())
            .field("memories", &self.memories.len())
            .field("edges", &self.graph.len())
            .field("vectors", &self.vectors.len())
            .finish_non_exhaustive()
    }
}

impl Store {
    /// A new, empty store with the namespace `default`.
    pub fn create(id: StoreId, model: ModelIdentity, now: Timestamp) -> Store {
        let dimensions = usize::try_from(model.dimensions).unwrap_or(0);
        let mut store = Store {
            id,
            model,
            namespaces: BTreeMap::new(),
            next_namespace_id: 1,
            memories: BTreeMap::new(),
            graph: Graph::new(),
            vectors: VectorTable::new(dimensions),
            tags: TagIndex::new(),
            entities: EntityIndex::new(),
            fts: FtsIndex::new(),
            counts: BTreeMap::new(),
            changes: ChangeCounters::default(),
            idempotency: BTreeMap::new(),
            last_write_at: None,
            last_write_client: None,
            estimated_bytes: 0,
        };
        if let Some(ns_id) = store.allocate_namespace_id() {
            store.insert_namespace(Namespace {
                id: ns_id,
                name: DEFAULT_NAMESPACE.to_owned(),
                created_at: now,
                policy: NamespacePolicy::default(),
            });
        }
        store
    }

    /// The store's identity.
    pub fn id(&self) -> StoreId {
        self.id
    }

    /// The embedding model whose vectors the store holds.
    pub fn model(&self) -> &ModelIdentity {
        &self.model
    }

    /// One memory.
    pub fn memory(&self, id: MemoryId) -> Option<&Memory> {
        self.memories.get(&id)
    }

    /// Every memory in ID order.
    pub fn memories(&self) -> impl Iterator<Item = &Memory> {
        self.memories.values()
    }

    /// One namespace.
    pub fn namespace(&self, id: NamespaceId) -> Option<&Namespace> {
        self.namespaces.get(&id)
    }

    /// Every namespace in ID order.
    pub fn namespaces(&self) -> impl Iterator<Item = &Namespace> {
        self.namespaces.values()
    }

    /// A namespace by name: exact match first, then ignoring ASCII case.
    pub fn namespace_by_name(&self, name: &str) -> Option<&Namespace> {
        self.namespaces
            .values()
            .find(|n| n.name == name)
            .or_else(|| {
                self.namespaces
                    .values()
                    .find(|n| n.name.eq_ignore_ascii_case(name))
            })
    }

    /// The counts of one namespace (zeros for an unknown one).
    pub fn namespace_counts(&self, id: NamespaceId) -> NamespaceCounts {
        self.counts.get(&id).copied().unwrap_or_default()
    }

    /// The graph.
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// The vector of one memory.
    pub fn vector(&self, id: MemoryId) -> Option<&[f32]> {
        self.vectors.get(id)
    }

    /// The vector table.
    pub fn vectors(&self) -> &VectorTable {
        &self.vectors
    }

    /// The tag index.
    pub fn tag_index(&self) -> &TagIndex {
        &self.tags
    }

    /// The entity index.
    pub fn entity_index(&self) -> &EntityIndex {
        &self.entities
    }

    /// The full-text index.
    pub fn fts(&self) -> &FtsIndex {
        &self.fts
    }

    /// The content and bookkeeping change counters.
    pub fn changes(&self) -> ChangeCounters {
        self.changes
    }

    /// Roughly how many bytes the store uses in memory; kept up to date by every change.
    pub fn estimated_bytes(&self) -> u64 {
        self.estimated_bytes
    }

    /// Counts for status pages.
    pub fn summary(&self) -> StoreSummary {
        let (mut pending, mut ghosts) = (0_u64, 0_u64);
        for c in self.counts.values() {
            pending = pending.saturating_add(c.pending);
            ghosts = ghosts.saturating_add(c.ghost);
        }
        let memory_count = self.memories.len() as u64;
        StoreSummary {
            memory_count,
            namespace_count: self.namespaces.len() as u64,
            pending_embeddings: pending,
            embeddable: memory_count.saturating_sub(ghosts),
            last_write_at: self.last_write_at,
            last_write_client: self.last_write_client.clone(),
        }
    }

    /// The full view of a memory with its strengths at `now`. `fullText` only when
    /// `include_full_text`; `accessHistory` only when `include_history`.
    pub fn memory_view(
        &self,
        memory: &Memory,
        now: Timestamp,
        include_full_text: bool,
        include_history: bool,
    ) -> MemoryView {
        let (strength, effective_strength) = self.strengths(memory, now);
        MemoryView {
            id: memory.id,
            namespace: self
                .namespaces
                .get(&memory.namespace)
                .map(|n| n.name.clone())
                .unwrap_or_default(),
            created_at: memory.created_at,
            summary: (memory.phase != Phase::Ghost).then(|| memory.summary.clone()),
            full_text: if include_full_text {
                memory.full_text.clone()
            } else {
                None
            },
            tags: memory.tags.clone(),
            entities: memory.labels.entities.clone(),
            topics: memory.labels.topics.clone(),
            emotions: memory.labels.emotions.clone(),
            phase: memory.phase,
            strength,
            effective_strength,
            stability: memory.stability,
            is_permastore: memory.is_permastore,
            retrieved_at: memory.retrieved_at,
            reinforced_at: memory.reinforced_at,
            access_count: memory.access_count,
            edge_count: self.graph.degree(memory.id) as u64,
            embedding: memory.embedding,
            saved_by: memory.saved_by.clone(),
            client: memory.client.clone(),
            access_history: include_history.then(|| memory.access_history.clone()),
        }
    }
}
