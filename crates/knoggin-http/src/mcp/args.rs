// SPDX-License-Identifier: AGPL-3.0-only
//! The argument type of each tool: exactly the arguments the tool takes, with `null`
//! meaning "not given", converting into the `knoggin_core::ops` request. The input
//! schemas are generated from these types, the output schemas from the `ops` result
//! types, and every `$ref` is replaced by the schema it points at.

use std::collections::HashMap;

use knoggin_core::MemoryId;
use knoggin_core::Timestamp;
use knoggin_core::model::limits::{
    DEFAULT_CONTEXT_LIMIT, DEFAULT_INITIAL_STABILITY, DEFAULT_PAGE_LIMIT, DEFAULT_QUALITY,
    DEFAULT_RECALL_LIMIT, DEFAULT_SIMILAR_LIMIT,
};
use knoggin_core::model::{NamespacePolicy, Phase};
use knoggin_core::ops::{
    CreateNamespace, DeleteNamespace, Detail, FindSimilar, ForgetMemories, ForgetMemory,
    GetContext, GetMemory, ListMemories, ListTags, MoveMemories, NamespaceStats, Recall,
    ReinforceMemory, StoreMemories, StoreMemory, StoreMemoryItem, SupersedeMemory, TimeValue,
    UpdatePolicy,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use utoipa::ToSchema;

use crate::error::ApiError;

/// Parses a tool's arguments. An unknown or missing argument and a wrong type are
/// `invalid_argument`; nothing is coerced.
pub(crate) fn parse<T: DeserializeOwned>(args: Value) -> Result<T, ApiError> {
    serde_json::from_value(args).map_err(|e| {
        let message = e.to_string();
        // serde names the field in backquotes for an unknown or a missing field.
        let field = message
            .split('`')
            .nth(1)
            .filter(|_| message.contains("unknown field") || message.contains("missing field"))
            .unwrap_or("arguments")
            .to_owned();
        ApiError::invalid_argument(field, message)
    })
}

type Found = Vec<(
    String,
    utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
)>;

/// The schema of `T` as one JSON Schema object that stands alone.
pub(crate) fn standalone<T: ToSchema>() -> Value {
    with_defs::<T>(Vec::new())
}

/// The schema of a generic `T` whose type parameter is `I`: utoipa does not collect the
/// schemas a type parameter refers to, so they are added here.
pub(crate) fn output_of<T: ToSchema, I: ToSchema>() -> Value {
    let mut found = vec![(I::name().into_owned(), I::schema())];
    I::schemas(&mut found);
    object(with_defs::<T>(found))
}

fn with_defs<T: ToSchema>(mut found: Found) -> Value {
    T::schemas(&mut found);
    let defs: HashMap<String, Value> = found
        .into_iter()
        .map(|(name, s)| (name, serde_json::to_value(s).unwrap_or_default()))
        .collect();
    let mut root = serde_json::to_value(T::schema()).unwrap_or_default();
    inline(&mut root, &defs, 0);
    root
}

/// The output schema of `T`: as [`standalone`], always with `type: object` at the top,
/// which MCP requires of an output schema (an untagged or flattened type has none).
pub(crate) fn output<T: ToSchema>() -> Value {
    object(standalone::<T>())
}

fn object(mut schema: Value) -> Value {
    if let Value::Object(map) = &mut schema {
        map.entry("type").or_insert_with(|| "object".into());
    }
    schema
}

fn inline(value: &mut Value, defs: &HashMap<String, Value>, depth: u8) {
    match value {
        Value::Object(map) => {
            let target = map
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/components/schemas/"))
                .and_then(|name| defs.get(name));
            if let Some(def) = target
                && depth < 32
            {
                let mut merged = def.clone();
                inline(&mut merged, defs, depth + 1);
                if let Value::Object(m) = &mut merged {
                    for (k, v) in map.iter() {
                        if k != "$ref" {
                            m.insert(k.clone(), v.clone());
                        }
                    }
                }
                *value = merged;
                return;
            }
            for v in map.values_mut() {
                inline(v, defs, depth);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| inline(v, defs, depth)),
        _ => {}
    }
}

/// `get_context`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GetContextArgs {
    /// Recall the memories most relevant to this topic; without it, the newest ones.
    topic: Option<String>,
    /// The namespace; the default namespace when not given.
    namespace: Option<String>,
    /// How many memories, 1 to 25 (default 10).
    #[schema(minimum = 1, maximum = 25)]
    limit: Option<u32>,
}

impl From<GetContextArgs> for GetContext {
    fn from(a: GetContextArgs) -> Self {
        Self {
            topic: a.topic,
            namespace: a.namespace,
            limit: a.limit.unwrap_or(DEFAULT_CONTEXT_LIMIT),
        }
    }
}

/// `recall_memories`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RecallArgs {
    /// What to search for, by meaning; at most 4,096 bytes.
    query: String,
    /// The namespace; the default namespace when not given.
    namespace: Option<String>,
    /// How many memories, 1 to 100 (default 25).
    #[schema(minimum = 1, maximum = 100)]
    limit: Option<u32>,
    /// How many of the best matches carry their full text, 0 to `limit` (default 5, or
    /// `limit` if that is smaller).
    #[schema(minimum = 0, maximum = 100)]
    full_text_count: Option<u32>,
    /// Only memories that carry all of these tags.
    tags: Option<Vec<String>>,
    /// Only memories that name all of these entities.
    entities: Option<Vec<String>>,
    /// Only memories with all of these topics.
    topics: Option<Vec<String>>,
    /// Only memories with all of these emotions.
    emotions: Option<Vec<String>>,
    /// Only memories at least this strong, 0 to 1.
    #[schema(minimum = 0, maximum = 1)]
    min_strength: Option<f32>,
    /// How far to follow links between memories, 0 to 3 (default 0).
    #[schema(minimum = 0, maximum = 3)]
    depth: Option<u8>,
    /// Favour memories saved after this time (RFC 3339 or milliseconds); give both bounds.
    time_range_start: Option<TimeValue>,
    /// Favour memories saved before this time (RFC 3339 or milliseconds); give both bounds.
    time_range_end: Option<TimeValue>,
    /// `compact` (default) or `full`.
    detail: Option<Detail>,
    /// Record that the results were retrieved (default true).
    record_access: Option<bool>,
}

impl From<RecallArgs> for Recall {
    fn from(a: RecallArgs) -> Self {
        Self {
            query: a.query,
            namespace: a.namespace,
            limit: a.limit.unwrap_or(DEFAULT_RECALL_LIMIT),
            full_text_count: a.full_text_count,
            tags: a.tags.unwrap_or_default(),
            entities: a.entities.unwrap_or_default(),
            topics: a.topics.unwrap_or_default(),
            emotions: a.emotions.unwrap_or_default(),
            min_strength: a.min_strength,
            depth: a.depth.unwrap_or(0),
            time_range_start: a.time_range_start,
            time_range_end: a.time_range_end,
            detail: a.detail.unwrap_or_default(),
            record_access: a.record_access.unwrap_or(true),
        }
    }
}

/// `get_memory`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GetMemoryArgs {
    /// The memory's ID.
    id: MemoryId,
    /// Only if the memory is in this namespace; no check when not given.
    namespace: Option<String>,
    /// Include the memory's access history.
    include_history: Option<bool>,
    /// Record that the memory was retrieved (default true).
    record_access: Option<bool>,
}

impl From<GetMemoryArgs> for GetMemory {
    fn from(a: GetMemoryArgs) -> Self {
        Self {
            id: a.id,
            namespace: a.namespace,
            record_access: a.record_access.unwrap_or(true),
            include_history: a.include_history.unwrap_or(false),
        }
    }
}

/// `find_similar_memories`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FindSimilarArgs {
    /// The memory to compare with.
    id: MemoryId,
    /// How many memories, 1 to 100 (default 10).
    #[schema(minimum = 1, maximum = 100)]
    limit: Option<u32>,
    /// Only matches at least this close, 0 to 1.
    #[schema(minimum = 0, maximum = 1)]
    min_score: Option<f32>,
    /// Record that the results were retrieved (default true).
    record_access: Option<bool>,
}

impl From<FindSimilarArgs> for FindSimilar {
    fn from(a: FindSimilarArgs) -> Self {
        Self {
            id: a.id,
            limit: a.limit.unwrap_or(DEFAULT_SIMILAR_LIMIT),
            min_score: a.min_score,
            record_access: a.record_access.unwrap_or(true),
        }
    }
}

/// `list_memories`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ListMemoriesArgs {
    /// The namespace; the default namespace when not given.
    namespace: Option<String>,
    /// Only memories that carry all of these tags.
    tags: Option<Vec<String>>,
    /// Only memories in this phase.
    phase: Option<Phase>,
    /// Only memories saved after this time (RFC 3339).
    created_after: Option<Timestamp>,
    /// Only memories saved before this time (RFC 3339).
    created_before: Option<Timestamp>,
    /// Include each memory's full text.
    include_full_text: Option<bool>,
    /// Page size, 1 to 200 (default 50).
    #[schema(minimum = 1, maximum = 200)]
    limit: Option<u32>,
    /// The `nextCursor` of the previous page.
    cursor: Option<String>,
}

impl From<ListMemoriesArgs> for ListMemories {
    fn from(a: ListMemoriesArgs) -> Self {
        Self {
            namespace: a.namespace,
            tags: a.tags.unwrap_or_default(),
            phase: a.phase,
            created_after: a.created_after,
            created_before: a.created_before,
            include_full_text: a.include_full_text.unwrap_or(false),
            limit: a.limit.unwrap_or(DEFAULT_PAGE_LIMIT),
            cursor: a.cursor,
        }
    }
}

/// `list_namespaces`: no arguments.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NoArgs {}

/// `list_tags`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ListTagsArgs {
    /// The namespace; the default namespace when not given.
    namespace: Option<String>,
    /// Only tags that start with this.
    prefix: Option<String>,
    /// Page size, 1 to 200 (default 50).
    #[schema(minimum = 1, maximum = 200)]
    limit: Option<u32>,
    /// The `nextCursor` of the previous page.
    cursor: Option<String>,
}

impl From<ListTagsArgs> for ListTags {
    fn from(a: ListTagsArgs) -> Self {
        Self {
            namespace: a.namespace,
            prefix: a.prefix,
            limit: a.limit.unwrap_or(DEFAULT_PAGE_LIMIT),
            cursor: a.cursor,
        }
    }
}

/// `namespace_stats`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NamespaceStatsArgs {
    /// The namespace; the default namespace when not given.
    namespace: Option<String>,
}

impl From<NamespaceStatsArgs> for NamespaceStats {
    fn from(a: NamespaceStatsArgs) -> Self {
        Self {
            namespace: a.namespace,
        }
    }
}

/// `store_memory`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoreMemoryArgs {
    /// The memory in one short sentence, 1 to 2,000 bytes.
    summary: String,
    /// Optional detail, up to 65,536 bytes.
    full_text: Option<String>,
    /// The namespace; the default namespace when not given.
    namespace: Option<String>,
    /// Up to 64 tags.
    tags: Option<Vec<String>>,
    /// Up to 32 people, places or things the memory names.
    entities: Option<Vec<String>>,
    /// Up to 32 topics.
    topics: Option<Vec<String>>,
    /// Up to 32 emotions.
    emotions: Option<Vec<String>>,
    /// A memory in the same namespace that this one belongs under.
    parent_id: Option<MemoryId>,
    /// Report existing memories that closely resemble this one (default false).
    check_duplicates: Option<bool>,
    /// 1 to 128 printable ASCII characters; a retry with the same key saves only once.
    idempotency_key: Option<String>,
}

impl StoreMemoryArgs {
    pub(crate) fn split(self) -> (StoreMemory, Option<String>) {
        let req = StoreMemory {
            summary: self.summary,
            full_text: self.full_text,
            namespace: self.namespace,
            tags: self.tags.unwrap_or_default(),
            entities: self.entities.unwrap_or_default(),
            topics: self.topics.unwrap_or_default(),
            emotions: self.emotions.unwrap_or_default(),
            parent_id: self.parent_id,
            check_duplicates: self.check_duplicates.unwrap_or(false),
            created_at: None,
        };
        (req, self.idempotency_key)
    }
}

/// One memory of `store_memories`: the `store_memory` arguments without
/// `checkDuplicates`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoreItemArgs {
    /// The memory in one short sentence, 1 to 2,000 bytes.
    summary: String,
    /// Optional detail, up to 65,536 bytes.
    full_text: Option<String>,
    /// The namespace; the default namespace when not given.
    namespace: Option<String>,
    /// Up to 64 tags.
    tags: Option<Vec<String>>,
    /// Up to 32 people, places or things the memory names.
    entities: Option<Vec<String>>,
    /// Up to 32 topics.
    topics: Option<Vec<String>>,
    /// Up to 32 emotions.
    emotions: Option<Vec<String>>,
    /// A memory in the same namespace that this one belongs under.
    parent_id: Option<MemoryId>,
}

/// `store_memories`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoreMemoriesArgs {
    /// 1 to 100 memories, saved in order.
    #[schema(min_items = 1, max_items = 100)]
    memories: Vec<StoreItemArgs>,
    /// 1 to 128 printable ASCII characters; a retry with the same key saves only once.
    idempotency_key: Option<String>,
}

impl StoreMemoriesArgs {
    pub(crate) fn split(self) -> (StoreMemories, Option<String>) {
        let memories = self
            .memories
            .into_iter()
            .map(|m| StoreMemoryItem {
                summary: m.summary,
                full_text: m.full_text,
                namespace: m.namespace,
                tags: m.tags.unwrap_or_default(),
                entities: m.entities.unwrap_or_default(),
                topics: m.topics.unwrap_or_default(),
                emotions: m.emotions.unwrap_or_default(),
                parent_id: m.parent_id,
                created_at: None,
            })
            .collect();
        (StoreMemories { memories }, self.idempotency_key)
    }
}

/// `supersede_memory`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SupersedeArgs {
    /// The memory being replaced.
    id: MemoryId,
    /// The new version in one short sentence, 1 to 2,000 bytes.
    summary: String,
    /// Optional detail, up to 65,536 bytes.
    full_text: Option<String>,
    /// Up to 64 tags.
    tags: Option<Vec<String>>,
    /// Up to 32 people, places or things the memory names.
    entities: Option<Vec<String>>,
    /// Up to 32 topics.
    topics: Option<Vec<String>>,
    /// Up to 32 emotions.
    emotions: Option<Vec<String>>,
    /// Report existing memories that closely resemble the new version (default false).
    check_duplicates: Option<bool>,
    /// 1 to 128 printable ASCII characters; a retry with the same key runs only once.
    idempotency_key: Option<String>,
}

impl SupersedeArgs {
    pub(crate) fn split(self) -> (SupersedeMemory, Option<String>) {
        let req = SupersedeMemory {
            id: self.id,
            summary: self.summary,
            full_text: self.full_text,
            tags: self.tags.unwrap_or_default(),
            entities: self.entities.unwrap_or_default(),
            topics: self.topics.unwrap_or_default(),
            emotions: self.emotions.unwrap_or_default(),
            check_duplicates: self.check_duplicates.unwrap_or(false),
            created_at: None,
        };
        (req, self.idempotency_key)
    }
}

/// `reinforce_memory`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReinforceArgs {
    /// The memory.
    id: MemoryId,
    /// 3 or 4 if it was useful, 1 or 2 if not (default 3).
    #[schema(minimum = 1, maximum = 4)]
    quality: Option<u8>,
    /// 1 to 128 printable ASCII characters; a retry with the same key runs only once.
    idempotency_key: Option<String>,
}

impl ReinforceArgs {
    pub(crate) fn split(self) -> (ReinforceMemory, Option<String>) {
        let req = ReinforceMemory {
            id: self.id,
            quality: self.quality.unwrap_or(DEFAULT_QUALITY),
        };
        (req, self.idempotency_key)
    }
}

/// `move_memories`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MoveArgs {
    /// 1 to 100 unique memory IDs.
    #[schema(min_items = 1, max_items = 100)]
    ids: Vec<MemoryId>,
    /// Where they go.
    destination_namespace: String,
    /// Only if every memory is in this namespace; no check when not given.
    source_namespace: Option<String>,
    /// 1 to 128 printable ASCII characters; a retry with the same key runs only once.
    idempotency_key: Option<String>,
}

impl MoveArgs {
    pub(crate) fn split(self) -> (MoveMemories, Option<String>) {
        let req = MoveMemories {
            ids: self.ids,
            destination_namespace: self.destination_namespace,
            source_namespace: self.source_namespace,
        };
        (req, self.idempotency_key)
    }
}

/// `create_namespace`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CreateNamespaceArgs {
    /// 1 to 64 ASCII letters, digits, `-` and `_`.
    name: String,
    /// Days a new memory stays strong, 0.01 to 36,500 (default 3.7145).
    initial_stability: Option<f32>,
    /// How fast memories fade: 1 is normal, 0 turns fading off (default 1).
    decay_rate_multiplier: Option<f32>,
    /// 1 to 128 printable ASCII characters; a retry with the same key runs only once.
    idempotency_key: Option<String>,
}

impl CreateNamespaceArgs {
    pub(crate) fn split(self) -> (CreateNamespace, Option<String>) {
        let req = CreateNamespace {
            name: self.name,
            initial_stability: self.initial_stability.unwrap_or(DEFAULT_INITIAL_STABILITY),
            decay_rate_multiplier: self.decay_rate_multiplier,
        };
        (req, self.idempotency_key)
    }
}

/// `update_namespace_policy`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UpdatePolicyArgs {
    /// The namespace.
    name: String,
    /// The whole policy: all five fields.
    policy: NamespacePolicy,
    /// Change nothing and report what the next sweep would change (default false).
    dry_run: Option<bool>,
    /// 1 to 128 printable ASCII characters; a retry with the same key runs only once.
    idempotency_key: Option<String>,
}

impl UpdatePolicyArgs {
    pub(crate) fn split(self) -> (UpdatePolicy, Option<String>) {
        let req = UpdatePolicy {
            name: self.name,
            policy: self.policy,
            dry_run: self.dry_run.unwrap_or(false),
        };
        (req, self.idempotency_key)
    }
}

/// `forget_memory`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ForgetArgs {
    /// The memory.
    id: MemoryId,
    /// Only if the memory is in this namespace; no check when not given.
    namespace: Option<String>,
    /// 1 to 128 printable ASCII characters; a retry with the same key runs only once.
    idempotency_key: Option<String>,
}

impl ForgetArgs {
    pub(crate) fn split(self) -> (ForgetMemory, Option<String>) {
        let req = ForgetMemory {
            id: self.id,
            namespace: self.namespace,
        };
        (req, self.idempotency_key)
    }
}

/// `forget_memories`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ForgetManyArgs {
    /// 1 to 100 memory IDs.
    #[schema(min_items = 1, max_items = 100)]
    ids: Vec<MemoryId>,
    /// Remove nothing and report which IDs exist (default false).
    dry_run: Option<bool>,
    /// 1 to 128 printable ASCII characters; a retry with the same key runs only once.
    idempotency_key: Option<String>,
}

impl ForgetManyArgs {
    pub(crate) fn split(self) -> (ForgetMemories, Option<String>) {
        let req = ForgetMemories {
            ids: self.ids,
            dry_run: self.dry_run.unwrap_or(false),
        };
        (req, self.idempotency_key)
    }
}

/// `delete_namespace`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DeleteNamespaceArgs {
    /// The namespace; never `default`.
    name: String,
    /// Also delete every memory in it (default false).
    force: Option<bool>,
    /// Delete nothing and report the counts (default false).
    dry_run: Option<bool>,
    /// 1 to 128 printable ASCII characters; a retry with the same key runs only once.
    idempotency_key: Option<String>,
}

impl DeleteNamespaceArgs {
    pub(crate) fn split(self) -> (DeleteNamespace, Option<String>) {
        let req = DeleteNamespace {
            name: self.name,
            force: self.force.unwrap_or(false),
            dry_run: self.dry_run.unwrap_or(false),
        };
        (req, self.idempotency_key)
    }
}
