# knoggin-core: the foundation contract

What exists, and where the next pieces go. The spec (tabs "Engine" and "Workspace and
crates") is the authority; this file says how the code meets it.

## Modules

| Module | Holds |
| --- | --- |
| `id` | `StoreId`, `MemoryId`, `UserId` (UUID newtypes, `Default` = nil), `NamespaceId(u32)` |
| `time` | `Timestamp` (ms; serde as RFC 3339 with ms, `MIN_API`/`MAX_API`), `elapsed_days`, `MILLIS_PER_DAY` |
| `sensitive` | `Sensitive<T>`. In a derived utoipa schema mark the field `#[schema(inline)]` |
| `error` | `EngineError` (six variants), `ErrorCode`. `Debug` prints code and field only |
| `model` | `Memory`, `Namespace`, `NamespacePolicy`, `PhaseThresholds`, `Edge`, `EdgeKey`, `EdgeType`, `Tag`, `Labels`, `Phase`, `EmbeddingState`, `AccessEvent`, `AccessKind`, `tag_terms` |
| `model::limits` | Every limit, default and engine constant; `validate_content` (save steps 1 to 6), `validate_tags`, `validate_labels`, `validate_namespace_name`, `validate_created_at`, `truncate_summary` |
| `model::markup` | `scan_client_markup`, `reject_client_markup` |
| `ops` | Request and result types of every operation (see the file header for the rules) |
| `embed` | `EmbeddingProvider`, `ModelIdentity`, `EmbedError`, `document_text`, `document_text_from_parts`, `QUERY_PREFIX`, `query_text` |
| `decay::fsrs` | `retrievability`, `stability_increase`, `review_stability` (quality 1 rule included), `effective_retrievability`, `days_until_threshold` |
| `graph` | `Graph` (edges by key, adjacency, `degree`, `joined`), `spreading_edge_factor`, `fan_strength` |
| `graph::bonus` | `Store::raw_strength`, `strengths`, `effective_strength`, `connection_bonus` |
| `search::vector` | `VectorTable` (`insert`, `remove` = swap-remove, `get`, `iter`, `top_k`), `dot`, `is_valid_vector` |
| `search::fts` | `FtsIndex` (`upsert`, `remove`, `set_namespace`, `search`, `docs`), `match_query`, `FtsError` |
| `search::entity` | `canonicalize_entity`, `entity_overlap` (Jaccard), `EntityIndex` |
| `search::tags` | `TagIndex` (plain tags only; `memories_with_all`, `counts`) |
| `store` | `Store` (fields `pub(crate)`), `Store::create(id, model, now)`, accessors, `memory_view`, `summary`, `changes`, `estimated_bytes`, `IdempotencyEntry`, `NamespaceCounts` |
| `store::index` | The index-maintenance helpers below, `rebuild_indexes_for_test`, `index_snapshot_for_test` |
| `testing` | `FakeProvider` (`new` = `fake-64`, `with_dim(768)` = `fake-768`) |

## Index maintenance (`store/index.rs`, all `pub(crate)`)

Change memories, edges, vectors, namespaces and idempotency entries only through these.
They keep the tag, entity and full-text indexes, the vector table, the graph, the
per-namespace counts, `estimated_bytes` and the idempotency purge in step.

- `insert_memory(memory, Option<&[f32]>) -> bool`
- `update_memory(id, |m| ...) -> Option<R>`: any change; reindexes what changed, drops the
  vector when the embedding is no longer `Ready`, purges idempotency when it becomes a ghost
- `fade_to_summary(id, now)`, `fade_to_ghost(id, now)`, `move_memory(id, ns)`, `remove_memory(id) -> (Memory, edges)`
- `attach_vector(id, &[f32])` (sets `Ready`, failures 0), `reset_vectors(model)` (model change: all vectors go, non-ghosts `Pending`)
- `insert_edge(edge)`, `remove_edge(&key)`, `remove_edges_where(pred)`
- `allocate_namespace_id()`, `insert_namespace(ns)`, `remove_namespace(id)` (empty only)
- `insert_idempotency(key, entry)`, `remove_idempotency(key)`
- `rebuild_indexes()` (load path), `estimate_from_scratch()`

Invariants: a vector exists exactly when the embedding is `Ready`; a ghost has no text, tags,
labels, vector or index entries; an edge joins two existing memories; `counts` has an entry
per namespace. The index-equivalence test takes `index_snapshot_for_test(queries)`, calls
`rebuild_indexes_for_test()`, takes another and asserts equality.

Not done by the helpers: change counters, `last_write_at`/`last_write_client`, and the
automatic links. The operations set those.

## Where the next pieces go

Add files with their own `impl Store` blocks and one `mod` line in the parent:

- `search/pipeline.rs` (recall, scoring, supersedes), `graph/activation.rs`, `rif.rs` (declare in `lib.rs`)
- `graph/links.rs` (automatic links), `store/write.rs` (save to delete), `store/read.rs` (get, similar, list, stats, context)
- `decay/sweep.rs` (phase planning, sweep), `store/idempotency.rs` (`run_idempotent`, request hash)
- `snapshot.rs`, `transfer.rs`, and `StoreBuilder` in `testing.rs`; property, budget and ranking tests in `tests/`

When the write operations use the helpers, delete the two `cfg_attr(not(test), expect(dead_code))`
attributes in `store/index.rs`; the compiler says when.
