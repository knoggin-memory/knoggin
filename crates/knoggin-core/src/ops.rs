// SPDX-License-Identifier: AGPL-3.0-only
//! Request and result types of every `Store` operation.
//!
//! These are also the REST payload types and the MCP result types: `camelCase`,
//! unknown request fields refused. Request fields with a fixed default hold the value
//! itself, filled in by serde, so that leaving a field out and sending its default hash
//! the same (tab "Engine", Idempotency). A field whose absence means something that
//! varies (`createdAt`, a namespace that is only a check) stays an `Option`.
//!
//! A path parameter that is part of a request (`id`, `name`) is a field marked
//! `skip_deserializing`: the REST handler sets it from the path, it is refused in a
//! body, and it is part of the request hash. Types that can hold memory text have a
//! hand-written `Debug` that prints IDs and counts only.

use serde::{Deserialize, Serialize};

use crate::error::{EngineError, ErrorCode};
use crate::id::MemoryId;
use crate::model::limits::{
    ContentRef, DEFAULT_CONTEXT_LIMIT, DEFAULT_FULL_TEXT_COUNT, DEFAULT_INITIAL_STABILITY,
    DEFAULT_PAGE_LIMIT, DEFAULT_QUALITY, DEFAULT_RECALL_LIMIT, DEFAULT_SIMILAR_LIMIT,
};
use crate::model::{
    AccessEvent, AccessKind, Edge, EdgeType, EmbeddingState, NamespacePolicy, Phase, Tag,
};
use crate::time::Timestamp;

/// `Debug` from the listed fields (which must be safe to print) and the lengths of the
/// listed collections.
macro_rules! safe_debug {
    ($ty:ident $(<$($g:ident),+>)?: $($f:ident),* $(; len $($l:ident),*)?) => {
        impl$(<$($g),+>)? std::fmt::Debug for $ty$(<$($g),+>)? {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let mut d = f.debug_struct(stringify!($ty));
                $(d.field(stringify!($f), &self.$f);)*
                $($(d.field(stringify!($l), &self.$l.len());)*)?
                d.finish_non_exhaustive()
            }
        }
    };
}

const fn default_true() -> bool {
    true
}
const fn default_quality() -> u8 {
    DEFAULT_QUALITY
}
const fn default_recall_limit() -> u32 {
    DEFAULT_RECALL_LIMIT
}
const fn default_similar_limit() -> u32 {
    DEFAULT_SIMILAR_LIMIT
}
const fn default_page_limit() -> u32 {
    DEFAULT_PAGE_LIMIT
}
const fn default_context_limit() -> u32 {
    DEFAULT_CONTEXT_LIMIT
}
const fn default_initial_stability() -> f32 {
    DEFAULT_INITIAL_STABILITY
}

// ═════════════════════════════════════════════════════════════════════
// Engine-side values that are not payloads
// ═════════════════════════════════════════════════════════════════════

/// Who saves, and through which client. Taken beside every client write, never
/// part of a request, so no client can send it and it is not hashed.
#[derive(Clone, PartialEq, Eq)]
pub struct WriteContext {
    /// `api_key:<id>`, `oauth:<user>`, `session:<user>`, `local`, `none` or `import`.
    pub saved_by: String,
    /// The MCP client's name, at most 64 bytes.
    pub client: Option<String>,
}
safe_debug!(WriteContext: saved_by);

/// One hit to record as accessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessHit {
    /// The memory.
    pub id: MemoryId,
    /// `Direct` for ordinary hits, `Associative` for graph or similar-search hits.
    pub kind: AccessKind,
}

/// The two change counters. The host compares them with those of the last snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeCounters {
    /// Saves, forgets, moves, reinforcements, namespace writes, imports.
    pub content: u64,
    /// Access recording, vectors landing, failed embeddings, sweeps, model changes.
    pub bookkeeping: u64,
}

/// Counts for status pages and for the snapshot commit.
#[derive(Clone, PartialEq, Eq)]
pub struct StoreSummary {
    /// Memories in every phase.
    pub memory_count: u64,
    /// Namespaces.
    pub namespace_count: u64,
    /// Memories that are not ghosts and wait in the embedding queue.
    pub pending_embeddings: u64,
    /// Memories that can hold a vector: every memory that is not a ghost.
    pub embeddable: u64,
    /// The last client write.
    pub last_write_at: Option<Timestamp>,
    /// The client of the last client write.
    pub last_write_client: Option<String>,
}
safe_debug!(StoreSummary: memory_count, namespace_count, pending_embeddings, embeddable, last_write_at);

/// One job of the embedding queue.
#[derive(Clone, PartialEq, Eq)]
pub struct EmbedJob {
    /// The memory.
    pub id: MemoryId,
    /// Its document text (recipe `doc-v1`).
    pub text: String,
}
safe_debug!(EmbedJob: id; len text);

/// What `apply_embedding` did.
#[derive(Debug, Clone, PartialEq)]
pub enum ApplyOutcome {
    /// The vector was stored; these automatic semantic links were made.
    Applied {
        /// The new edges.
        links: Vec<Edge>,
    },
    /// Nothing was stored.
    Skipped(SkipReason),
}

/// Why `apply_embedding` stored nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The memory no longer exists.
    Missing,
    /// The memory is not `Pending` (it became a ghost, or already has a vector).
    NotPending,
    /// The vector came from another model than the store's.
    ModelMismatch,
    /// Wrong dimension, not finite, or not of unit length; counted as a failure.
    InvalidVector,
}

/// The answer of an idempotent write.
#[derive(Clone, PartialEq)]
pub enum Idempotent<T> {
    /// The operation ran now.
    Ran(T),
    /// The stored JSON answer of an earlier run with the same key and request.
    Replayed(Vec<u8>),
}

impl<T> std::fmt::Debug for Idempotent<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ran(_) => f.write_str("Idempotent::Ran"),
            Self::Replayed(bytes) => write!(f, "Idempotent::Replayed({} bytes)", bytes.len()),
        }
    }
}

/// What a sweep changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Memories whose full text faded.
    pub full_to_summary: u64,
    /// Memories that became ghosts.
    pub summary_to_ghost: u64,
    /// Ghosts that were deleted.
    pub deleted: u64,
    /// IDs of the memories whose phase changed.
    pub phase_changed: Vec<MemoryId>,
    /// IDs of the memories that were removed.
    pub removed: Vec<MemoryId>,
}

// ═════════════════════════════════════════════════════════════════════
// Shared shapes
// ═════════════════════════════════════════════════════════════════════

/// The full view of a memory (schema `Memory`). `summary` is absent for a ghost;
/// `fullText` is absent when there is none or the route leaves it out.
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[schema(as = Memory)]
#[ts(rename = "Memory")]
pub struct MemoryView {
    /// Identity.
    pub id: MemoryId,
    /// The namespace's name.
    pub namespace: String,
    /// When it was saved.
    pub created_at: Timestamp,
    /// The short text; absent for a ghost.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub summary: Option<String>,
    /// The long text, where the route includes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub full_text: Option<String>,
    /// Free labels.
    pub tags: Vec<Tag>,
    /// Entities, as given.
    pub entities: Vec<String>,
    /// Topics, as given.
    pub topics: Vec<String>,
    /// Emotions, as given.
    pub emotions: Vec<String>,
    /// How much content is left.
    pub phase: Phase,
    /// Raw strength now.
    pub strength: f32,
    /// Strength with the connection bonus.
    pub effective_strength: f32,
    /// Days.
    pub stability: f32,
    /// Exempt from decay.
    pub is_permastore: bool,
    /// Last recorded access.
    pub retrieved_at: Option<Timestamp>,
    /// Last reinforcement.
    pub reinforced_at: Option<Timestamp>,
    /// Total recorded accesses.
    pub access_count: u64,
    /// Edges stored for the memory, of every type, whatever is at the other end.
    pub edge_count: u64,
    /// Whether it has a vector.
    pub embedding: EmbeddingState,
    /// The credential that saved it.
    pub saved_by: String,
    /// The MCP client that saved it.
    pub client: Option<String>,
    /// Oldest first; only where a route asks for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub access_history: Option<Vec<AccessEvent>>,
}
safe_debug!(MemoryView: id, phase, strength, effective_strength, embedding, edge_count; len tags);

/// A page of a list (`nextCursor` is `null` on the last page).
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    /// The items.
    pub items: Vec<T>,
    /// Opaque cursor of the next page.
    pub next_cursor: Option<String>,
}
safe_debug!(Page<T>: ; len items);

/// A list that is not paged.
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct Items<T> {
    /// The items.
    pub items: Vec<T>,
}
safe_debug!(Items<T>: ; len items);

/// A namespace as the API shows it (schema `Namespace`).
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[schema(as = Namespace)]
#[ts(rename = "Namespace")]
pub struct NamespaceView {
    /// Name.
    pub name: String,
    /// When it was made.
    pub created_at: Timestamp,
    /// Memories in every phase, ghosts included.
    pub memory_count: u64,
    /// Decay policy.
    pub policy: NamespacePolicy,
    /// On a policy dry run: what the next sweep would move under the new policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub would_change: Option<WouldChange>,
}
safe_debug!(NamespaceView: created_at, memory_count, would_change);

/// What the next sweep would move under a new policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct WouldChange {
    /// Full to Summary.
    pub full_to_summary: u64,
    /// Summary to Ghost.
    pub summary_to_ghost: u64,
    /// Ghosts deleted.
    pub deleted: u64,
}

/// One tag with the number of memories that carry it.
#[derive(Clone, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct TagCount {
    /// The tag.
    pub tag: String,
    /// Memories in the namespace that carry it.
    pub count: u64,
}
safe_debug!(TagCount: count);

/// Why a recall has no vector channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum Degraded {
    /// The query could not be embedded; keyword and entity search only.
    ModelUnavailable,
}

/// `{ pending, total }` of the embedding queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct EmbeddingProgress {
    /// Not ghosts and still `Pending`.
    pub pending: u64,
    /// Every memory that is not a ghost.
    pub total: u64,
}

// ═════════════════════════════════════════════════════════════════════
// Saving
// ═════════════════════════════════════════════════════════════════════

/// Body of `POST /v1/memories`; the `store_memory` tool converts into it.
#[derive(Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoreMemory {
    /// 1 to 2,000 bytes.
    pub summary: String,
    /// 1 to 65,536 bytes.
    #[serde(default)]
    pub full_text: Option<String>,
    /// Filled in by the host from the header or `default` before hashing.
    #[serde(default)]
    pub namespace: Option<String>,
    /// At most 64.
    #[serde(default)]
    pub tags: Vec<String>,
    /// At most 32.
    #[serde(default)]
    pub entities: Vec<String>,
    /// At most 32.
    #[serde(default)]
    pub topics: Vec<String>,
    /// At most 32.
    #[serde(default)]
    pub emotions: Vec<String>,
    /// A memory in the same namespace that becomes the parent.
    #[serde(default)]
    pub parent_id: Option<MemoryId>,
    /// Report near duplicates.
    #[serde(default)]
    pub check_duplicates: bool,
    /// An earlier creation time, for imports and the benchmark.
    #[serde(default)]
    pub created_at: Option<Timestamp>,
}
safe_debug!(StoreMemory: parent_id, check_duplicates, created_at; len tags, entities, topics, emotions);

impl StoreMemory {
    /// The content to validate.
    pub fn content(&self) -> ContentRef<'_> {
        ContentRef {
            summary: &self.summary,
            full_text: self.full_text.as_deref(),
            tags: &self.tags,
            entities: &self.entities,
            topics: &self.topics,
            emotions: &self.emotions,
            created_at: self.created_at,
        }
    }
}

/// One item of `POST /v1/memories/batch`: as `StoreMemory` without `checkDuplicates`.
#[derive(Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoreMemoryItem {
    /// 1 to 2,000 bytes.
    pub summary: String,
    /// 1 to 65,536 bytes.
    #[serde(default)]
    pub full_text: Option<String>,
    /// Filled in by the host before hashing.
    #[serde(default)]
    pub namespace: Option<String>,
    /// At most 64.
    #[serde(default)]
    pub tags: Vec<String>,
    /// At most 32.
    #[serde(default)]
    pub entities: Vec<String>,
    /// At most 32.
    #[serde(default)]
    pub topics: Vec<String>,
    /// At most 32.
    #[serde(default)]
    pub emotions: Vec<String>,
    /// A memory in the same namespace that becomes the parent.
    #[serde(default)]
    pub parent_id: Option<MemoryId>,
    /// An earlier creation time.
    #[serde(default)]
    pub created_at: Option<Timestamp>,
}
safe_debug!(StoreMemoryItem: parent_id, created_at; len tags, entities, topics, emotions);

impl StoreMemoryItem {
    /// The content to validate.
    pub fn content(&self) -> ContentRef<'_> {
        ContentRef {
            summary: &self.summary,
            full_text: self.full_text.as_deref(),
            tags: &self.tags,
            entities: &self.entities,
            topics: &self.topics,
            emotions: &self.emotions,
            created_at: self.created_at,
        }
    }

    /// The same save as a single `StoreMemory`, without the duplicate check.
    pub fn into_store_memory(self) -> StoreMemory {
        StoreMemory {
            summary: self.summary,
            full_text: self.full_text,
            namespace: self.namespace,
            tags: self.tags,
            entities: self.entities,
            topics: self.topics,
            emotions: self.emotions,
            parent_id: self.parent_id,
            check_duplicates: false,
            created_at: self.created_at,
        }
    }
}

/// Body of `POST /v1/memories/batch`: 1 to 100 items, each with its own namespace.
#[derive(Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoreMemories {
    /// The items, saved in order.
    pub memories: Vec<StoreMemoryItem>,
}
safe_debug!(StoreMemories: ; len memories);

/// `POST /v1/memories/{id}/supersede`: a new memory in the old one's namespace.
#[derive(Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SupersedeMemory {
    /// The memory being replaced (path parameter).
    #[serde(skip_deserializing)]
    #[ts(skip)]
    pub id: MemoryId,
    /// 1 to 2,000 bytes.
    pub summary: String,
    /// 1 to 65,536 bytes.
    #[serde(default)]
    pub full_text: Option<String>,
    /// At most 64.
    #[serde(default)]
    pub tags: Vec<String>,
    /// At most 32.
    #[serde(default)]
    pub entities: Vec<String>,
    /// At most 32.
    #[serde(default)]
    pub topics: Vec<String>,
    /// At most 32.
    #[serde(default)]
    pub emotions: Vec<String>,
    /// Report near duplicates, leaving the replaced memory out.
    #[serde(default)]
    pub check_duplicates: bool,
    /// An earlier creation time.
    #[serde(default)]
    pub created_at: Option<Timestamp>,
}
safe_debug!(SupersedeMemory: id, check_duplicates, created_at; len tags, entities, topics, emotions);

impl SupersedeMemory {
    /// The content to validate.
    pub fn content(&self) -> ContentRef<'_> {
        ContentRef {
            summary: &self.summary,
            full_text: self.full_text.as_deref(),
            tags: &self.tags,
            entities: &self.entities,
            topics: &self.topics,
            emotions: &self.emotions,
            created_at: self.created_at,
        }
    }
}

/// Why the duplicate check did not run although it was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum DuplicateCheck {
    /// The model was not ready or the wait timed out; the memory was saved without a vector.
    Skipped,
}

/// One existing memory that closely resembles a new one.
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct NearDuplicate {
    /// The existing memory.
    pub id: MemoryId,
    /// Cosine, at least the threshold.
    pub score: f32,
    /// Its summary, cut to 240 bytes with `…`.
    pub summary: String,
}
safe_debug!(NearDuplicate: id, score);

/// The near-duplicate advice of a save. Absent when nothing matches.
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct NearDuplicates {
    /// 0.85.
    pub threshold: f32,
    /// Closest first, at most 3.
    pub matches: Vec<NearDuplicate>,
}
safe_debug!(NearDuplicates: matches);

/// The answer of a save. `links` are the edges made for the memory, for the host's live
/// event; they are not serialized.
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct StoredMemory {
    /// The new memory.
    pub id: MemoryId,
    /// Its namespace's name.
    pub namespace: String,
    /// `full`.
    pub phase: Phase,
    /// Raw strength now: 1.0 unless an earlier `createdAt` was sent.
    pub strength: f32,
    /// Days.
    pub stability: f32,
    /// When it was saved.
    pub created_at: Timestamp,
    /// `pending`, or `ready` when a vector came with the save.
    pub embedding: EmbeddingState,
    /// Near duplicates, when asked for and found.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub near_duplicates: Option<NearDuplicates>,
    /// Set by the host when the duplicate check could not run.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub duplicate_check: Option<DuplicateCheck>,
    /// On a supersede: the replaced memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub supersedes: Option<MemoryId>,
    /// Edges made for the memory.
    #[serde(skip)]
    #[ts(skip)]
    pub links: Vec<Edge>,
}
safe_debug!(StoredMemory: id, phase, strength, embedding, near_duplicates, duplicate_check, supersedes; len links);

/// `ok` or `error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    /// Saved.
    Ok,
    /// Refused; see `error`.
    Error,
}

/// The error of one batch item.
#[derive(Clone, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct ItemError {
    /// The error code.
    pub code: ErrorCode,
    /// For people.
    pub message: String,
    /// The request field at fault.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub field: Option<String>,
}
safe_debug!(ItemError: code, field);

impl From<&EngineError> for ItemError {
    fn from(e: &EngineError) -> Self {
        Self {
            code: e.code(),
            message: e.to_string(),
            field: e.field().map(str::to_owned),
        }
    }
}

/// One entry of a batch result, in request order.
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct BatchItemResult<T> {
    /// Position in the request.
    pub index: u32,
    /// `ok` or `error`.
    pub status: BatchStatus,
    /// The saved memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub memory: Option<T>,
    /// Why it was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<ItemError>,
}
safe_debug!(BatchItemResult<T>: index, status, error);

/// The answer of a batch save.
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct BatchResult<T> {
    /// One entry per item, in order.
    pub results: Vec<BatchItemResult<T>>,
    /// Items saved.
    pub stored: u32,
    /// Items refused.
    pub errors: u32,
}
safe_debug!(BatchResult<T>: stored, errors; len results);

// ═════════════════════════════════════════════════════════════════════
// Reinforce, forget, move
// ═════════════════════════════════════════════════════════════════════

/// `POST /v1/memories/{id}/reinforce`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReinforceMemory {
    /// The memory (path parameter).
    #[serde(skip_deserializing)]
    #[ts(skip)]
    pub id: MemoryId,
    /// 1 (forgot) to 4 (easy).
    #[serde(default = "default_quality")]
    pub quality: u8,
}

/// The answer of a reinforcement.
#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct Reinforced {
    /// The memory.
    pub id: MemoryId,
    /// 1.0: the anchor moved to now.
    pub strength: f32,
    /// The new stability.
    pub stability: f32,
    /// Unchanged.
    pub phase: Phase,
    /// Set once stability reached the threshold.
    pub is_permastore: bool,
}

/// `DELETE /v1/memories/{id}` and the `forget_memory` tool.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ForgetMemory {
    /// The memory (path parameter).
    #[serde(skip_deserializing)]
    pub id: MemoryId,
    /// A check only: never filled in; a mismatch counts as an unknown ID.
    #[serde(default)]
    pub namespace: Option<String>,
}
safe_debug!(ForgetMemory: id);

/// The answer of a forget. `deleted` is false when no such memory exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct Forgotten {
    /// The ID that was asked for.
    pub id: MemoryId,
    /// Whether a memory was removed.
    pub deleted: bool,
}

/// `POST /v1/memories/forget`: 1 to 100 IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ForgetMemories {
    /// The IDs.
    pub ids: Vec<MemoryId>,
    /// Report which IDs exist and remove nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// One entry of a batch forget, in request order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct ForgetItem {
    /// Position in the request.
    pub index: u32,
    /// The ID.
    pub id: MemoryId,
    /// Removed (or, on a dry run, exists).
    pub deleted: bool,
}

/// The answer of a batch forget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct ForgetManyResult {
    /// One entry per ID.
    pub results: Vec<ForgetItem>,
    /// How many were removed (or, on a dry run, exist).
    pub deleted: u32,
}

/// `POST /v1/memories/move`: 1 to 100 unique IDs, all or nothing.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MoveMemories {
    /// The memories.
    pub ids: Vec<MemoryId>,
    /// Where they go.
    pub destination_namespace: String,
    /// A check only: every ID must be in it. Never filled in.
    #[serde(default)]
    pub source_namespace: Option<String>,
}
safe_debug!(MoveMemories: ; len ids);

/// The answer of a move.
#[derive(Clone, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct Moved {
    /// How many moved.
    pub moved: u32,
    /// Where they went.
    pub destination_namespace: String,
}
safe_debug!(Moved: moved);

// ═════════════════════════════════════════════════════════════════════
// Namespaces and tags
// ═════════════════════════════════════════════════════════════════════

/// `POST /v1/namespaces`.
#[derive(Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateNamespace {
    /// 1 to 64 bytes of ASCII letters, digits, `-` and `_`.
    pub name: String,
    /// Days; default 3.7145.
    #[serde(default = "default_initial_stability")]
    pub initial_stability: f32,
    /// `None` means 1.0.
    #[serde(default)]
    pub decay_rate_multiplier: Option<f32>,
}
safe_debug!(CreateNamespace: initial_stability, decay_rate_multiplier);

/// `PUT /v1/namespaces/{name}/policy`. The body is the whole `NamespacePolicy`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdatePolicy {
    /// The namespace (path parameter).
    #[serde(skip_deserializing)]
    pub name: String,
    /// The new policy.
    pub policy: NamespacePolicy,
    /// Change nothing and report `wouldChange`.
    #[serde(default)]
    pub dry_run: bool,
}
safe_debug!(UpdatePolicy: policy, dry_run);

/// `DELETE /v1/namespaces/{name}`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeleteNamespace {
    /// The namespace; never `default` (path parameter).
    #[serde(skip_deserializing)]
    pub name: String,
    /// Remove its memories too.
    #[serde(default)]
    pub force: bool,
    /// Delete nothing and report the counts.
    #[serde(default)]
    pub dry_run: bool,
}
safe_debug!(DeleteNamespace: force, dry_run);

/// The answer of a namespace delete.
#[derive(Clone, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceDeleted {
    /// The namespace.
    pub name: String,
    /// False on a dry run.
    pub deleted: bool,
    /// Memories removed.
    pub memories_deleted: u64,
    /// Edges removed.
    pub edges_removed: u64,
}
safe_debug!(NamespaceDeleted: deleted, memories_deleted, edges_removed);

/// `GET /v1/tags`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListTags {
    /// Filled in by the host.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Only tags that start with this.
    #[serde(default)]
    pub prefix: Option<String>,
    /// 1 to 200.
    #[serde(default = "default_page_limit")]
    pub limit: u32,
    /// From the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}
safe_debug!(ListTags: limit);

/// `GET /v1/namespaces/{name}/stats` and the `namespace_stats` tool.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NamespaceStats {
    /// Filled in by the host.
    #[serde(default)]
    pub namespace: Option<String>,
}
safe_debug!(NamespaceStats:);

/// Memories per phase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct PhaseCounts {
    /// Full.
    pub full: u64,
    /// Summary.
    pub summary: u64,
    /// Ghost.
    pub ghost: u64,
}

/// Memories per embedding state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct EmbeddingCounts {
    /// Ready.
    pub ready: u64,
    /// Pending.
    pub pending: u64,
    /// Failed.
    pub failed: u64,
    /// None (ghosts).
    pub none: u64,
}

/// The statistics of one namespace (schema `NamespaceStats`).
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[schema(as = NamespaceStats)]
#[ts(rename = "NamespaceStats")]
pub struct NamespaceStatsResult {
    /// Name.
    pub name: String,
    /// Every phase, ghosts included.
    pub memory_count: u64,
    /// Per phase.
    pub phase_counts: PhaseCounts,
    /// Permanent memories.
    pub permastore_count: u64,
    /// Mean strength now of the memories that are not ghosts; 0 when there are none.
    pub average_strength: f32,
    /// Edges whose two ends are both in the namespace.
    pub edge_count: u64,
    /// Per embedding state.
    pub embedding: EmbeddingCounts,
    /// Bytes of every summary and full text.
    pub text_bytes: u64,
    /// Oldest `createdAt`.
    pub oldest_created_at: Option<Timestamp>,
    /// Newest `createdAt`.
    pub newest_created_at: Option<Timestamp>,
    /// Decay policy.
    pub policy: NamespacePolicy,
}
safe_debug!(NamespaceStatsResult: memory_count, phase_counts, edge_count);

// ═════════════════════════════════════════════════════════════════════
// Get, similar, list
// ═════════════════════════════════════════════════════════════════════

/// `GET /v1/memories/{id}` and the `get_memory` tool.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetMemory {
    /// The memory.
    pub id: MemoryId,
    /// A check only: never filled in; a mismatch is `not_found`.
    #[serde(default)]
    pub namespace: Option<String>,
    /// The host records one `direct` access afterwards (not for a ghost).
    #[serde(default = "default_true")]
    pub record_access: bool,
    /// Include `accessHistory`.
    #[serde(default)]
    pub include_history: bool,
}
safe_debug!(GetMemory: id, record_access, include_history);

/// `POST /v1/memories/{id}/similar`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FindSimilar {
    /// The memory whose vector is the query (path parameter).
    #[serde(skip_deserializing)]
    #[ts(skip)]
    pub id: MemoryId,
    /// 1 to 100.
    #[serde(default = "default_similar_limit")]
    pub limit: u32,
    /// Lowest cosine.
    #[serde(default)]
    pub min_score: Option<f32>,
    /// The host records the hits as `associative` afterwards.
    #[serde(default = "default_true")]
    pub record_access: bool,
}

/// One similar memory (without `fullText`).
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct SimilarHit {
    /// The memory.
    pub memory: MemoryView,
    /// Cosine.
    pub score: f32,
}
safe_debug!(SimilarHit: memory, score);

/// The answer of a similar search: `{ items: [{ memory, score }] }`.
pub type SimilarResult = Items<SimilarHit>;

/// `GET /v1/memories` and the `list_memories` tool. Newest first.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListMemories {
    /// Filled in by the host.
    #[serde(default)]
    pub namespace: Option<String>,
    /// All must match.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Only this phase.
    #[serde(default)]
    pub phase: Option<Phase>,
    /// Created strictly after.
    #[serde(default)]
    pub created_after: Option<Timestamp>,
    /// Created strictly before.
    #[serde(default)]
    pub created_before: Option<Timestamp>,
    /// Include `fullText`.
    #[serde(default)]
    pub include_full_text: bool,
    /// 1 to 200.
    #[serde(default = "default_page_limit")]
    pub limit: u32,
    /// From the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}
safe_debug!(ListMemories: phase, created_after, created_before, include_full_text, limit; len tags);

// ═════════════════════════════════════════════════════════════════════
// Recall and context
// ═════════════════════════════════════════════════════════════════════

/// `compact` or `full`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS,
)]
#[serde(rename_all = "snake_case")]
pub enum Detail {
    /// `{ id, date, summary, fullText? }`.
    #[default]
    Compact,
    /// The full memory view with `score` and `related`.
    Full,
}

/// A time-range bound: an RFC 3339 string or whole milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(untagged)]
pub enum TimeValue {
    /// Milliseconds since 1970.
    Millis(i64),
    /// RFC 3339.
    Text(String),
}

impl TimeValue {
    /// The time, or `invalid_argument` on `field` outside 1970 to 9999.
    pub fn to_timestamp(&self, field: &'static str) -> Result<Timestamp, EngineError> {
        let parsed = match self {
            Self::Millis(ms) => Timestamp::from_api_millis(*ms),
            Self::Text(text) => Timestamp::parse_rfc3339(text),
        };
        parsed.map_err(|e| EngineError::invalid(field, format!("{field}: {e}")))
    }
}

/// `POST /v1/recall` and the `recall_memories` tool.
#[derive(Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Recall {
    /// Not empty after trimming; at most 4,096 bytes.
    pub query: String,
    /// Filled in by the host.
    #[serde(default)]
    pub namespace: Option<String>,
    /// 1 to 100.
    #[serde(default = "default_recall_limit")]
    pub limit: u32,
    /// 0 to `limit`. Its default depends on `limit`; see [`Recall::full_text_count`].
    #[serde(default)]
    pub full_text_count: Option<u32>,
    /// All must match; each a valid tag.
    #[serde(default)]
    pub tags: Vec<String>,
    /// All must match (lower-cased whole strings); also feeds scoring and the entity channel.
    #[serde(default)]
    pub entities: Vec<String>,
    /// All must match.
    #[serde(default)]
    pub topics: Vec<String>,
    /// All must match.
    #[serde(default)]
    pub emotions: Vec<String>,
    /// 0 to 1, on raw strength.
    #[serde(default)]
    pub min_strength: Option<f32>,
    /// 0 to 3: the graph expansion profile.
    #[serde(default)]
    pub depth: u8,
    /// Give both bounds or neither. A boost, not a filter.
    #[serde(default)]
    pub time_range_start: Option<TimeValue>,
    /// Give both bounds or neither.
    #[serde(default)]
    pub time_range_end: Option<TimeValue>,
    /// `compact` or `full`.
    #[serde(default)]
    pub detail: Detail,
    /// The host records access afterwards; always false without `memory:write`.
    #[serde(default = "default_true")]
    pub record_access: bool,
}
safe_debug!(Recall: limit, full_text_count, min_strength, depth, detail, record_access; len tags, entities, topics, emotions);

impl Recall {
    /// How many top hits carry full text: the value sent, else 5 or `limit` when smaller.
    pub fn full_text_count(&self) -> u32 {
        self.full_text_count
            .unwrap_or(DEFAULT_FULL_TEXT_COUNT.min(self.limit))
    }
}

/// One compact recall or context hit.
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct CompactHit {
    /// The memory.
    pub id: MemoryId,
    /// UTC date with weekday, for example `Fri 2023-01-20`.
    pub date: String,
    /// The short text.
    pub summary: String,
    /// For the top hits that still have it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub full_text: Option<String>,
}
safe_debug!(CompactHit: id);

/// An edge between a hit and another hit on the same page.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct Related {
    /// The other hit.
    pub id: MemoryId,
    /// The edge type's name.
    pub edge_type: EdgeType,
    /// The edge weight.
    pub weight: f32,
}

/// Why a hit ranked where it did; only for `GET /v1/ui/search`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct Explanation {
    /// Cosine as a candidate, or `null`.
    pub vector: Option<f32>,
    /// Full-text score as a candidate, or `null`.
    pub text: Option<f32>,
    /// Activation, for a hit the graph found.
    pub activation: Option<f32>,
    /// Hops, for a hit the graph found.
    pub hops: Option<u8>,
    /// The memory whose spread into this hit was the largest.
    pub via: Option<MemoryId>,
    /// That edge's type.
    pub via_edge_type: Option<EdgeType>,
    /// The superseded memory this hit stands in for.
    pub replaced: Option<MemoryId>,
}

/// One full recall hit: the memory view plus `score`, `related` and, for the UI, `explanation`.
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct FullHit {
    /// The memory.
    #[serde(flatten)]
    pub memory: MemoryView,
    /// The final score.
    pub score: f32,
    /// Edges to other hits on the page.
    pub related: Vec<Related>,
    /// Only when the host asked for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub explanation: Option<Explanation>,
}
safe_debug!(FullHit: memory, score; len related);

/// The compact recall answer.
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct CompactRecall {
    /// Hits, best first.
    pub memories: Vec<CompactHit>,
    /// How many.
    pub count: u32,
    /// `model_unavailable` when the query had no vector.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub degraded: Option<Degraded>,
}
safe_debug!(CompactRecall: count, degraded);

/// The full recall answer.
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
pub struct FullRecall {
    /// Hits, best first.
    pub memories: Vec<FullHit>,
    /// How many.
    pub count: u32,
    /// `model_unavailable` when the query had no vector.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub degraded: Option<Degraded>,
}
safe_debug!(FullRecall: count, degraded);

/// The recall answer in the shape `detail` asked for.
#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(untagged)]
pub enum RecallResponse {
    /// `detail: compact`.
    Compact(CompactRecall),
    /// `detail: full`.
    Full(FullRecall),
}

/// A seed of the graph expansion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceSeed {
    /// The memory.
    pub id: MemoryId,
    /// Its starting activation.
    pub score: f32,
}

/// One spread of activation from one memory to another.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceSpread {
    /// From.
    pub from: MemoryId,
    /// To.
    pub to: MemoryId,
    /// The amount added.
    pub amount: f32,
    /// Along this edge type.
    pub edge_type: EdgeType,
    /// Place in the firing order, from 0.
    pub order: u32,
}

/// What a recall did, for the web UI's live event. Never sent to the client that asked,
/// and it holds no query text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecallTrace {
    /// Seeds with their scores.
    pub seeds: Vec<TraceSeed>,
    /// Every spread.
    pub spreads: Vec<TraceSpread>,
    /// IDs returned, in order.
    pub returned: Vec<MemoryId>,
    /// IDs suppressed by retrieval-induced forgetting.
    pub suppressed: Vec<MemoryId>,
}

/// What `Store::recall` returns: the answer, the trace, and the hits to record.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallResult {
    /// The answer for the client.
    pub response: RecallResponse,
    /// The trace for the web UI.
    pub trace: RecallTrace,
    /// The hits with their access kind, for `record_access`.
    pub hits: Vec<AccessHit>,
}

/// `GET /v1/context` and the `get_context` tool.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetContext {
    /// When given, a recall for it (depth 0, no filters, access recording off).
    #[serde(default)]
    pub topic: Option<String>,
    /// Filled in by the host.
    #[serde(default)]
    pub namespace: Option<String>,
    /// 1 to 25.
    #[serde(default = "default_context_limit")]
    pub limit: u32,
}
safe_debug!(GetContext: limit);

/// One namespace with its memory count.
#[derive(Clone, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceCount {
    /// Name.
    pub name: String,
    /// Memories in every phase.
    pub memory_count: u64,
}
safe_debug!(NamespaceCount: memory_count);

/// The engine's part of the context answer. The host adds `model` and `webUrl` ([`Context`]).
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct ContextResult {
    /// The namespace used.
    pub default_namespace: String,
    /// Every namespace, by name.
    pub namespaces: Vec<NamespaceCount>,
    /// The first 3 carry `fullText`.
    pub memories: Vec<CompactHit>,
    /// The embedding queue of the store.
    pub embedding: EmbeddingProgress,
    /// Only when a topic could not be embedded.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub degraded: Option<Degraded>,
}
safe_debug!(ContextResult: embedding, degraded; len namespaces, memories);

/// The state of the embedding model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus {
    /// Loaded.
    Ready,
    /// Loading.
    Loading,
    /// Downloading.
    Downloading,
    /// Not available; see `reason`.
    Unavailable,
}

/// Why the model is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum ModelUnavailableReason {
    /// No download source completed.
    Offline,
    /// Files are missing and downloading is off.
    FilesMissing,
    /// A file failed its hash at every source.
    HashMismatch,
    /// The disk is full.
    DiskFull,
    /// ONNX Runtime could not load the model.
    LoadFailed,
}

/// The model state of the process that serves the store (schema `ModelState`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct ModelState {
    /// The state.
    pub state: ModelStatus,
    /// The model identity string.
    pub identity: String,
    /// The name to show people.
    pub name: String,
    /// Bytes downloaded so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub bytes_done: Option<u64>,
    /// Bytes to download.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub bytes_total: Option<u64>,
    /// Why it is unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reason: Option<ModelUnavailableReason>,
}

/// The whole answer of `GET /v1/context` (schema `Context`).
#[derive(Clone, PartialEq, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    /// The engine's part.
    #[serde(flatten)]
    pub result: ContextResult,
    /// The model state.
    pub model: ModelState,
    /// Where people browse the store, or `null`.
    pub web_url: Option<String>,
}
safe_debug!(Context: result, model);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_are_filled_so_absent_and_default_serialize_the_same() {
        let absent: StoreMemory = serde_json::from_value(json!({ "summary": "s" })).unwrap();
        let explicit: StoreMemory = serde_json::from_value(json!({
            "summary": "s", "tags": [], "entities": [], "topics": [], "emotions": [],
            "checkDuplicates": false, "fullText": null, "parentId": null, "namespace": null,
            "createdAt": null
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&absent).unwrap(),
            serde_json::to_value(&explicit).unwrap()
        );
        let r: ReinforceMemory = serde_json::from_value(json!({})).unwrap();
        assert_eq!(r.quality, 3);
        let d: DeleteNamespace = serde_json::from_value(json!({})).unwrap();
        assert!(!d.force && !d.dry_run);
        let c: CreateNamespace = serde_json::from_value(json!({ "name": "x" })).unwrap();
        assert_eq!(c.initial_stability, 3.7145);
        let g: GetMemory = serde_json::from_value(json!({ "id": MemoryId::default() })).unwrap();
        assert!(g.record_access && !g.include_history);
    }

    #[test]
    fn unknown_fields_are_refused() {
        let err = serde_json::from_value::<StoreMemory>(json!({ "summary": "s", "x": 1 }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `x`"), "{err}");
        assert!(
            serde_json::from_value::<StoreMemoryItem>(
                json!({ "summary": "s", "checkDuplicates": true })
            )
            .is_err()
        );
    }

    #[test]
    fn path_ids_are_refused_in_a_body_but_serialized_for_the_hash() {
        let id = json!({ "id": MemoryId::default() });
        assert!(serde_json::from_value::<ReinforceMemory>(id.clone()).is_err());
        assert!(serde_json::from_value::<ForgetMemory>(id).is_err());
        assert!(serde_json::from_value::<DeleteNamespace>(json!({ "name": "x" })).is_err());
        let policy = serde_json::to_value(NamespacePolicy::default()).unwrap();
        assert!(
            serde_json::from_value::<UpdatePolicy>(json!({ "name": "x", "policy": policy }))
                .is_err()
        );
        let u: UpdatePolicy = serde_json::from_value(json!({ "policy": policy })).unwrap();
        assert_eq!(u.name, "");
        let mut s: SupersedeMemory = serde_json::from_value(json!({ "summary": "s" })).unwrap();
        s.id = MemoryId::from_bytes([7; 16]);
        assert_eq!(
            serde_json::to_value(&s).unwrap()["id"],
            json!("07070707-0707-0707-0707-070707070707")
        );
    }

    #[test]
    fn recall_defaults_and_full_text_count() {
        let r: Recall = serde_json::from_value(json!({ "query": "q" })).unwrap();
        assert_eq!(
            (r.limit, r.depth, r.detail, r.record_access),
            (25, 0, Detail::Compact, true)
        );
        assert_eq!(r.full_text_count(), 5);
        let r: Recall = serde_json::from_value(json!({ "query": "q", "limit": 3 })).unwrap();
        assert_eq!(r.full_text_count(), 3);
        let r: Recall =
            serde_json::from_value(json!({ "query": "q", "limit": 3, "fullTextCount": 0 }))
                .unwrap();
        assert_eq!(r.full_text_count(), 0);
    }

    #[test]
    fn time_values_take_strings_and_millis() {
        let r: Recall = serde_json::from_value(json!({
            "query": "q", "timeRangeStart": 1_719_223_200_000_i64,
            "timeRangeEnd": "2024-06-24T10:00:00Z"
        }))
        .unwrap();
        let start = r
            .time_range_start
            .unwrap()
            .to_timestamp("timeRangeStart")
            .unwrap();
        let end = r
            .time_range_end
            .unwrap()
            .to_timestamp("timeRangeEnd")
            .unwrap();
        assert_eq!(start, end);
        let err = TimeValue::Millis(-1)
            .to_timestamp("timeRangeStart")
            .unwrap_err();
        assert_eq!(err.field(), Some("timeRangeStart"));
        assert!(
            TimeValue::Text("10000-01-01T00:00:00Z".into())
                .to_timestamp("timeRangeEnd")
                .is_err()
        );
    }

    #[test]
    fn stored_memory_serializes_without_links_and_absent_options() {
        let s = StoredMemory {
            id: MemoryId::default(),
            namespace: "default".into(),
            phase: Phase::Full,
            strength: 1.0,
            stability: 3.7145,
            created_at: Timestamp::from_millis(0),
            embedding: EmbeddingState::Pending,
            near_duplicates: None,
            duplicate_check: Some(DuplicateCheck::Skipped),
            supersedes: None,
            links: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({
                "id": "00000000-0000-0000-0000-000000000000", "namespace": "default",
                "phase": "full", "strength": 1.0, "stability": 3.7145_f32,
                "createdAt": "1970-01-01T00:00:00.000Z", "embedding": "pending",
                "duplicateCheck": "skipped"
            })
        );
    }

    #[test]
    fn debug_prints_no_text() {
        let s: StoreMemory = serde_json::from_value(json!({
            "summary": "marker-secret", "tags": ["marker-tag"], "namespace": "marker-ns"
        }))
        .unwrap();
        let d = format!("{s:?}");
        assert!(!d.contains("marker"), "{d}");
        assert!(d.contains("tags: 1"), "{d}");
        let c = ItemError::from(&EngineError::NotFound(
            "namespace 'marker' not found".into(),
        ));
        assert!(!format!("{c:?}").contains("marker"));
    }

    #[test]
    fn schemas_use_the_public_names() {
        use utoipa::{PartialSchema, ToSchema};
        assert_eq!(MemoryView::name(), "Memory");
        assert_eq!(NamespaceView::name(), "Namespace");
        assert_eq!(NamespaceStatsResult::name(), "NamespaceStats");
        let schema = serde_json::to_value(SupersedeMemory::schema()).unwrap();
        assert!(schema["properties"].get("id").is_none(), "{schema}");
        assert!(schema["properties"].get("summary").is_some());
        let schema = serde_json::to_value(StoredMemory::schema()).unwrap();
        assert!(schema["properties"].get("links").is_none(), "{schema}");
        let ts = <SupersedeMemory as ts_rs::TS>::decl(&ts_rs::Config::default());
        assert!(!ts.contains("id:"), "{ts}");
        let ts = <MemoryView as ts_rs::TS>::decl(&ts_rs::Config::default());
        assert!(ts.starts_with("type Memory"), "{ts}");
        assert!(ts.contains("createdAt: string"), "{ts}");
    }
}
