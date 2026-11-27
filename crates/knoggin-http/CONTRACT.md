# knoggin-http: the store layer contract

For anyone working on `mcp`, `edge` (with the control routes) or `local`: what exists and
how to use it.

## Modules

| Module | Offers |
| --- | --- |
| `ctx` | `RequestContext`, `CredentialRef`, `CredentialKind`, `Permissions`; `headers::*` (every header name, lower case); `RequestContext::{write_internal_headers, from_internal_headers, write_context, require_read, require_write}`; `namespace_header(&HeaderMap)` (the checked `Knoggin-Namespace`); `PROTOCOL_VERSION` |
| `error` | `Code` (the 17 codes, `status()`, `as_str()`), `LockReason`, `ApiError` (constructors per code, `with_field/with_reason/with_retry_after/with_request_id`, `from_join(&JoinError)`, `from_bare_status`, `From<EngineError>`), `fill_request_id(Response, Uuid)` |
| `trace` | `MakeRequestUuidV7` (for `SetRequestIdLayer` on `Knoggin-Request-Id`), `request_id(&Request)`, `route(&Request)`, `request_span(&Request)`, `observe` (middleware: span, one log line, `knoggin_requests_total` / `knoggin_request_seconds`) |
| `panic` | `log_location(&PanicHookInfo)` (the hook body; `main` installs it), `location_line` |
| `settings` | `StoreLimits` (`limits.body_bytes`, `request_timeout_ms`, `store_concurrency`), `DEFAULT_BODY_BYTES`, `IMPORT_LINE_BYTES` |
| `host` | `StoreHost`, `HostConfig`, `SnapshotTiming`, `StoreBackend` + `OpenedStore`, `SnapshotSource`, `SealedSnapshot`, `BackendError`, `LocalBackend`, `Embedder`, `Providers`, `EmbedWaits`, `Reply<T>`, `request_hash`, `StoreStatus`/`StoreState`, `HostReport`, `HostCounters` |
| `store_api` | `store_layer(Arc<StoreHost>) -> Router`, `export_file_name(name, date)`; `store_api::ui` (the `/v1/ui/*` read models: `wire` holds the answer types, `shape` the shaping over `&Store`, `tests` the TypeScript export) |
| `mcp` | `McpConfig` (`source_url`, in `HostConfig::mcp`). `/mcp` and `/mcp/plugin` are mounted by `store_layer` itself |
| `testing` | `TestHost` (host over a `LocalBackend` in a folder you own, fake model, one user and personal store, `ctx`, `ctx_for`, `new_store`, `make_ready`), `fake_config`, `fake_model_state`; `testing::issuer` (the test issuer, below) |

## Building a host

```rust
let (model_tx, model_rx) = watch::channel(model_state);          // the binary's model loop sends here
let providers = Arc::new(OnceCell::new());                         // filled with Providers { query, document } once loaded
let embedder = Embedder::new(model_rx, providers.clone(), EmbedWaits::default());
let host = StoreHost::new(config, backend, embedder);              // inside a Tokio runtime
let router = store_layer(host.clone());
```

`HostConfig::local(model)` is the daemon's configuration; a container sets `timing:
SnapshotTiming::SERVER`, `memory_budget: Some((limit - 928 MiB) * 0.4)`, `idle_close:
Some(idleSeconds)`, `embed_allowance: Some(30 min)`, `web_url`, and `limits` from the claim.
Fill the `OnceCell` before sending `ModelStatus::Ready`.

Lifecycle: `close(store)` (save, then drop; stays open if the save fails), `drop_store(store)`
(deleted: no save), `save_all()` (park, shutdown; returns the failures, the role retries),
`shutdown()` (stop tasks, refuse requests), `lease_lost()` (a watch the container waits on),
`end_event_streams()` (ends every event stream at once and answers new ones 204; call it
first on a stop and at step 1 of a park, before waiting for requests in flight).
Status: `store_status(store)` (also retries a stale lock in the background), `report()` (the
container's `status` answer: open stores, opening, locks), `counters`, `model_state()`.

## The operations (for `mcp`)

Every memory operation is a method of `StoreHost` that takes `&RequestContext` and the
request value of `knoggin_core::ops`, and does the host's part: permissions, the default
namespace (body, else `ctx.default_namespace`, else `default`; never a check-only namespace),
the request hash and idempotency, the duplicate check's vector, the query vector, access
recording (only with `memory:write`), the budget and the upload block.

- Writes take `key: Option<String>` and return `Reply<T>`: `Fresh(T)` or `Replayed(bytes)`.
  `reply.into_json()` gives the same bytes for both, so a tool result over MCP and a REST body
  match. Dry runs are never recorded. Names: `store_memory`, `store_memories`,
  `supersede_memory`, `reinforce_memory`, `forget_memory`, `forget_memories`, `move_memories`,
  `create_namespace`, `update_namespace_policy`, `delete_namespace`.
- Reads: `get_memory`, `find_similar`, `list_memories`, `list_namespaces`, `list_tags`,
  `namespace_stats`, `recall` (returns `RecallResponse`), `context` (returns the whole
  `Context` with `model` and `webUrl`).
- `export(ctx) -> ExportStream` (a `Stream` of JSON Lines frames), `import(ctx, OnConflict, Body)`.
- Set `ctx.client` to the MCP client's name; it becomes the memory's `client`.

Errors are `ApiError`; on MCP map `code` to the tool error. Path IDs (`id`, `name`) are fields
of the request types: set them before calling (`req.id = ...`).

## The router (for `edge`)

- `store_layer(host)` holds every REST store route, `/v1/ui/*` included.
  It has no fallback, so merge it into the edge router and set the edge's fallback (404 as
  `not_found`) there. It applies its own body limits (256 KiB, `body_bytes` on the batch, none
  on the import), the request timeout (export and import exempt), `trace::observe`, error
  normalization (bare 405, 413, 504 become `ApiError`s with the request ID) and
  `Cache-Control: no-store`. Do not wrap it in `trace::observe` again.
- Put a `RequestContext` in the request extensions before it runs. Without one a route answers
  `internal`. The request ID in error bodies comes from `ctx.request_id`.
- The store layer never reads a client header except `Idempotency-Key`; check and strip
  `Knoggin-*` client headers in the edge. `Knoggin-Namespace` goes into
  `ctx.default_namespace` through `ctx::namespace_header`.
- `GET /v1/export`: the store layer accepts the `store` parameter and ignores it. The edge
  resolves it into `ctx.store_id`, applies the credential rule, the per-user cap of 2 exports
  and 2 imports, and sets `Content-Disposition` with `store_api::export_file_name`.
- Missing credential, another user's store, `rate_limited`, `store_deleting` for a deleted
  account, `Knoggin-Store` and the browser checks are the edge's.
- A container rebuilds the context with `RequestContext::from_internal_headers` in a small
  middleware in front of `store_layer`; the router writes them with `write_internal_headers`.

## Local install (for `local`)

`LocalBackend::new(data_dir, key_provider, Stores, retention_days)`. Snapshot files live in
`<data_dir>/stores/<store id>/`. Call `flush_records()` once a minute and at shutdown: it
writes each store's last save (newest name, counts, last write) into its record.

## The MCP endpoint (for `edge` and the binary)

`store_layer(host)` serves `POST`/`DELETE`/`GET` on `/mcp` and `/mcp/plugin` under the body
limit `limits.body_bytes` and the request timeout, behind the same `RequestContext`. The edge
checks the credential and the origin first; a missing or bad credential is its HTTP 401, not a
JSON-RPC error. Set `HostConfig::mcp.source_url` from the setting `source_url` and
`HostConfig::web_url` from `server.public_url` (`None` on a local install without one): both
go into the server instructions. Sessions of the older revisions live in the store layer's
memory, bound to the store, the API key and the endpoint. `StoreHost::clients(store)` gives
the MCP client names an open store has seen (at most 32, with when), for `/v1/ui/system`.

## The web UI

- `host::live`: each open store's `EventLog` (the last 1,000 events and a broadcast channel
  of 1,024). Host writes add their events inside the write (`Op` closures get a
  `&mut Vec<UiEvent>`), so a replayed idempotent write makes none. `StoreHost::subscribe`
  gives a stream's receiver without opening the store; the channel closes with the store,
  and the stream also ends on `end_event_streams()`.
- `GET /v1/ui/events` is exempt from the request timeout; the edge counts it against the
  per-user cap of 10 (`Cap::Events`) and re-checks its credential every 60 seconds, as an
  export.
- `edge::ui` serves `ui/dist` (rust-embed, `allow_missing`) from the edge's fallback for a
  `GET`/`HEAD` off the API's paths, and from `local::locked_router`. A UI answer carries an
  `edge::ui::CacheControl` extension that `edge::finish` keeps.
- The UI's TypeScript types: `cargo nextest run -p knoggin-http --run-ignored only -E
  'test(export_ui_types)'` writes `ui/src/lib/api/types/`; a normal test fails when it is
  stale.

## Where to add things

- `src/mcp/` (module `mcp` in `lib.rs`): converts tool arguments into `ops` types and calls the
  host methods above.
- `src/edge/`, `src/local/`: own modules; `edge_layer(edge, store_layer(host), mode_routes)`.
- Store-status route: `host.store_status(id)` plus `host.model_state()`.

## The edge layer (`edge`, `local`)

- `EdgeState::new(EdgeConfig, EdgeParts)`: the settings (`EdgeConfig::local(own_url, version)`
  for the daemon's defaults) and the state database, field keys, key provider, clock, and on
  the daemon the store host and `LocalSessions`. The one `local` user must exist first:
  `local::ensure_local_user(users, keys, now)`.
- `edge_layer(edge, store_layer(host), local::mode_routes(LocalState { .. }))` is the daemon's
  whole API. Every request must carry an `edge::Arrival` extension: the listeners in
  `edge::listen` (`serve_tcp(listener, router, arrival, ListenConfig, shutdown)`,
  `serve_unix(..)`) tag it; a test inserts it by hand.
- Control and mode handlers read `Extension<edge::Caller>`; store routes get the
  `RequestContext`. `Credential` has `Oauth` (an access token) and `Cookie` (a browser
  session from web sign-in) beside the local ones.
- Sign-in: set `EdgeConfig::auth_mode = Oauth`, `public_url`, `test_mode` and
  `oauth: Some(OauthConfig { .. })`, then call `edge.discover().await` (and again while it
  answers `DiscoveryError::Unreachable`). The daemon does this in a background task and
  serves at once; the router's other errors stop its start.
  `edge::oauth` checks tokens and caches the signing keys; `edge::signin` serves
  `/auth/login`, `/auth/callback`, `POST /auth/logout` (also the local web session's
  logout) and the protected-resource metadata. The router's start-up guards are
  `edge::guard::{router_settings, router_users, check_public_url}`.
- The test issuer: `testing::issuer::TestIssuer::start(test_mode)` (or `serve`), with
  `url`, `claims`, `mint`, `mint_with`, `rotate_keys`, `remove_key`, `kids`, `secrets`,
  `token_requests` and `require_basic`;
  its client secret is `TEST_SECRET`.
- `local`: `prepare_run_dir`, `bind_socket`, `bind_loopback(port)` (127.0.0.1 and [::1]),
  `web_label` + `web_authority` (the web listener's origin), `LocalSessions`, `erase_step`
  (now and hourly), `locked_router(reason, version, docs_url)`.
- `edge::metrics_router(PrometheusHandle)` serves `/metrics` for `metrics.bind`; the binary
  installs the recorder and its 5-second upkeep task.
- `docs/openapi.json` is refreshed with
  `cargo nextest run -p knoggin-http --run-ignored only -E 'test(write_openapi)'`.
