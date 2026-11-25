# Deviations from the spec

One line for every place where the build departed from the tech spec, with the reason.

- Engine, full-text index: the `tags` column holds a memory's plain tags only, not the `entity/`, `topic/` and `emotion/` label terms. recalld's native write path at the reference commit keeps labels apart from tags, so its FTS rows carry none; with label terms 14 of the 40 ranking-fixture queries came out in another order, without them all 40 match exactly.
- Engine, supersedes: a chain is walked through every successor (breadth first, 10 hops) and a cycle simply ends the walk, so a memory in a cycle can still be replaced by the newest member; recalld follows one successor and gives up on a cycle. The spec's "follow all of them" rule decides; no store made by Knoggin can hold a cycle.
- Engine, recall: a query vector of the wrong dimension or with a value that is not finite is treated as no vector (`degraded: model_unavailable`), since the engine has no error code for a host fault.
- Engine, context: a `topic` that is empty after trimming counts as no topic.
- Engine, recall trace: `suppressed` lists the candidates whose effective strength retrieval-induced forgetting lowered; recalld also listed neighbors that were not candidates.
- Engine, idempotency: `knoggin-core` builds the request-hash input (`store::request_hash_input`: name, zero byte, canonical JSON) but does not hash it; the host takes its SHA-256 with aws-lc-rs. The core may not depend on aws-lc-rs and the rules ban `sha2`, and a hand-written SHA-256 would be cryptography outside aws-lc-rs.
- Engine, signatures: every write takes `ctx` and `now` as its last arguments, and so do `record_access`, `sweep`, `apply_embedding` (edge times), `on_open`, `export_page` (the header's `exportedAt`) and `import_record`, which also takes `OnConflict`; `ExportPage` holds `TransferRecord` values and the host writes each as one JSON line.
- Engine, access recording: `record_access` adds 1 to the bookkeeping counter only when it recorded at least one hit, so a call with only ghosts or unknown IDs does not trigger a snapshot.
