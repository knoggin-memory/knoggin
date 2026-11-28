# knoggin-server: the foundation contract

The container and broker, the router (lease, pool, reconcile, rollout), and lifecycle, admin
and launchers: what exists and how the pieces fit.

## What exists

| Module | Offers |
| --- | --- |
| `settings` | `RouterSettings` (`trusted_proxies`, `image_digest`, `limits: RouterLimits`, `storage`, `retention`, `containers`, `internal`, `rollout`, `log_level`; `upload_bytes_per_hour()`), `StorageSettings`, `RetentionSettings`, `ContainerSettings` (`memory_limit_bytes()`), `InternalSettings` (`secret: InternalSecret`, `cert` PEM, `key: SecretText`), `KeySettings` (`Kms`/`KeyFile`), `LauncherSettings` (`Ecs`/`Docker`), `LauncherServiceSettings`, `RolloutSettings`, `RateSetting`, `SecretText`, `Cidr` (`contains` canonicalizes, `overlaps`), `store_budget(memory_limit_bytes)`, `MODEL_RESERVE_BYTES` |
| `link` | Secrets: `InternalSecret::launch_secret(id)`, `LaunchSecret::{router_token, container_token, hello_proof, check_proof, from_hex, to_hex}`, `Token::{matches, bearer, expose}`, `Nonce`, `CertPin::{of, matches}` (+`FromStr`/`Display`). Certificates: `ServerIdentity::{for_container, from_pem, pin}`, `new_internal_cert(now)` (for `admin internal-cert`). TLS: `server_config`, `pinned_client_config`, `hello_client_config`. Clients: `Peers` (ID to address and pin), `forwarding_client(peers)`, `router_client(pin, local_address)`, `fetch_unpinned(addr, id, request)` (the `hello` call, answer capped at `MAX_ANSWER_BYTES`). Listener: `serve_tls(listener, tls, router, admit, shutdown)` with `PeerAddr` in every request. Calls: `verify_container_call`, `verify_router_call`, `write_container_call`, `write_router_call`. Bodies: `link::api` (`HelloAnswer`, `ClaimRequest`/`ClaimSettings`/`ClaimLimits`, `ParkRequest`, `StatusAnswer`/`StoreReport`/`StoreLock`/`Counters`, `ContainerPhase`, `PROTOCOLS`, `shared_protocol`, `paths`) |
| `storage` | `ObjectStorage`, `ObjectInfo`, `SignedRequest`, `StorageError`; `names` (`snapshot_prefix`, `store_prefix`, `snapshot_key`, `parse_snapshot_key` (strict, for the blob route's raw path), `name_in`, `SELFTEST`); `S3Storage::{connect, from_sdk_config, from_conf, s3_config}`; `MemoryStorage::{start(clock), endpoint, bucket, set_fault, insert, get, remove, keys, seen}` with `Fault::{None, Fail, Stall, Gone, Answer(status)}` |
| `launcher` | `Launcher` (`launch`, `describe`, `list`, `stop`), `ContainerSpec`, `LaunchHandle`, `ContainerInfo` (`RunState`, `Ended { reason: EndReason, exit_code }`), `ContainerEnv` (`to_vars`, `from_lookup`, `env_names`), `LogLevel`, `LaunchError`, `BuildInfo::{new, compare, is_newer_than}` (semantic versions) |
| `aws` | `behavior_version()` (dated, `#[allow(deprecated)]`), `sdk_config(region, credentials_file)`. KMS and ECS clients use both |

Outside the crate: `knoggin_http::ctx::headers::{CONTAINER_ID, INTERNAL_STATUS}` and
`ctx::{link_protocol_ok, link_bearer, link_identity, write_link_identity, write_link_bearer}`
(all internal header code lives in `ctx`); `knoggin_core::ops::ModelState` deserializes.

## How the pieces fit

- **Launch secret and tokens.** The router derives `settings.internal.secret.launch_secret(id)`
  and puts it in `ContainerEnv`; `main` reads `ContainerEnv::from_lookup(|k| std::env::var(k).ok())`
  and passes it to the container role. Nobody stores a launch secret.
- **hello.** The router makes a `Nonce`, sends `GET {paths::HELLO}?nonce=<hex>` with
  `fetch_unpinned(address, &id.to_string(), req)` (no `Authorization`, but `Knoggin-Protocol`),
  parses `HelloAnswer`, refuses a different `container_id`, and checks
  `launch.check_proof(&nonce, &reply.cert_der, &answer.proof)`. The pin is
  `CertPin::of(&reply.cert_der)`. Wrap the call in the time limit.
- **Every later call to a container** (forward, `claim`, `park`, `status`): `peers.set(id, Peer
  { address, pin })` from the record just read, then `forwarding_client` on
  `https://<container id>:<port>/...` with `write_router_call(headers, &launch.router_token())`.
  Drop the entry when the record is `gone`. A forwarded request also gets
  `RequestContext::write_internal_headers` (which writes `Knoggin-Protocol`). Read whole answers
  through `http_body_util::Limited` with `MAX_ANSWER_BYTES`.
- **The container's listener**: `ServerIdentity::for_container(id)`, `server_config`, `serve_tls`
  with `admit = |_| Some(())`. Its middleware calls `verify_router_call` (except `hello`), and
  refuses `/internal/*` that carries `Knoggin-User-Id`. A forwarded request for another slot
  than its claim's is refused with `internal` (log kind `wrong_slot`). A parking answer is 503 with
  `Knoggin-Internal-Status: parking` (`paths::PARKING`).
- **The router's internal listener**: `ServerIdentity::from_pem(&s.internal.cert,
  s.internal.key.expose())`, `serve_tls` with an `admit` that applies the per-address bounds
  (8 open, 60 a minute, 5 failed tokens) and, with `one_host`, `user_net_cidr.contains(ip)`; the
  returned guard is held for the connection's life. Handlers read `Extension<PeerAddr>` and call
  `verify_container_call(&secret, &headers)`, then check the lease of the slot the call names
  (state, container, IP).
- **The container's client to the router**: `router_client(env.router_cert_sha256, local)`;
  every call gets `write_container_call(headers, &launch, slot, id)` (`Knoggin-Slot`).
- **Slots.** A container serves one `Slot` (`knoggin_state::record::Slot`): `User(user)`, the
  personal store and every app without a container of its own, or `Store(store)`, one isolated or
  shared app. `StoreRecord::slot()` is the only way to know where a store is served. Leases are
  kept per slot (`lease#user:<id>`, `lease#store:<id>`); the claim, `status` and container
  records carry the slot. The edge puts `knoggin_http::ctx::Serving { slot, owner }` in each
  store request; the forwarder calls `Fleet::acquire(slot, owner, wait)`. A store-slot claim
  counts toward its owner's `containers.max_per_user`.
- **The holder rule.** The broker opens a store only for a container of its slot (else 404
  `not_found`), and writes `StoreRecord.holder` (slot and generation) at open. Only the holder
  saves (snapshots, commit, blob route). An open while another lease that may hold unsaved
  writes is the holder asks it to park (`moved`) and answers 503.
- **Signed links.** `ObjectStorage::presign_put_new` / `presign_get` with
  `Duration::from_secs(s.storage.link_ttl_seconds)` and `now`. The container sends a
  `SignedRequest` exactly as given (method, URL, every header). `names::snapshot_key(store,
  name)` is the only way to build a key.
- **Memory budget.** `settings::store_budget(memory_limit_bytes)` on both sides; the broker's
  `sizeBytes` bound is twice `store_budget(containers.memory_limit_bytes())`.

## The router side

**The role, for `main`** (`knoggin router`): build the pieces, bind the listeners, then

```rust
let code: i32 = router::run_router(RouterStart {
    edge,                      // EdgeConfig from server.*, auth.*, limits.* (per_address/per_user filled from settings)
    settings,                  // RouterSettings
    one_host: (launcher.kind == "docker").then(|| "https://router:8443".into()),
    build: BuildInfo::new(env!("CARGO_PKG_VERSION")),
    spec,                      // launcher.ecs.task_definition, or "" on one host
    db,                        // already connected (60 s of tries) and, on Postgres, migrated
    keys, storage, launcher, clock,
    listeners: Listeners { public, internal, metrics },  // router::reserve(addr) each: bound by main, listening only after every other start-up step
    metrics: Some(edge::metrics_router(handle)),
    before_rollout: Some(Box::pin(async { self_test().await?; attach().await })), // after discovery, before the rollout record
    backends: Backends { state_db, key_provider, launcher },
    timing: Timing::default(),
}, shutdown_signal).await;
```

`run_router` runs the start-up steps it owns, in order (config guards with the one-host checks, schema,
field key, the mode-`none` users check and the local user, discovery (60 s), the rollout record, listen), then
serves the public API, the broker on the internal listener and the reconcile loop until `stop`,
and returns 1 when a step refuses, 0 after a stop (on one host it first parks its containers and
keeps the internal listener open up to 110 s). **The container role** is `container::serve(addr,
ServeConfig, stop) -> ServeEnd` on `container::runtime()`; `main` exits with `end.exit_code`.

| Module | Offers |
| --- | --- |
| `lease` | `Fleet::new(FleetConfig, db, field_keys, launcher, storage, clock)`; `acquire(slot, owner, wait)`, `mark_gone(id, Option<StopReason>, Caller)`, `finish_parked(id)`, `request_park(slot, reason)`, `ask_park(slot, reason)`, `status`, `park`, `limits()` (the internal listener's `PeerLimits`), `shutdown().await` (ends and joins its tasks) |
| `pool` | `Fleet::launch(BuildSpec)` (record `starting` first, the rest as an owned task) |
| `reconcile` | `run(fleet, stop)`, `Fleet::take_leadership`, `Fleet::pass(&mut Reconciler)` |
| `rollout` | `at_start(meta, own, allow_downgrade)` |
| `router` | `run_router`, `RouterStart`, `Backends`, `guards`, `broker(fleet, keys, settings, one_host)`, `internal_admit`, `public_router`, `guarded`, `client_address`, `run(RouterRole, Listeners, stop)`, `reserve(addr)` (a bound socket that listens only in `run`); `router::forward::{forwarder, STORE_ROUTES, Timing}` (`Timing` also holds `discovery` 60 s and `shutdown_wait` 110 s); `router::status::{mode_routes, StatusInfo}` |
| `testing` | `FakeLauncher::new(Runner, clock)` with `forget`, `drain`, `kill`, `unreachable`, `refuse`, `fail_stop`, `fixed_address`, `stop_grace`, `set_runner`; `serve_runner(build, test_mode, ServeTimings)` (the real role); `stub_runner(build, test_mode, StubControl)` (`Behaviour::{BigStatus, DripStatus}`, `hang_park`); `FlakyDb::new(db)` (`fail` a partition prefix's writes, count `queries`) |

- **One set of per-address bounds**: the broker's `PeerLimits`, held by the fleet
  (`fleet.limits()`), is the internal listener's `admit` (`router::internal_admit`, with the
  one-host `user_net_cidr` check first), takes the broker's failed tokens, and is cleared when an
  address passes to a new container.
- **The broker** is built by `router::broker` over the fleet; its `parked` runs
  `Fleet::finish_parked` (the records "Mark gone" writes once the container has ended, then a stop).
- **The fake launcher** runs each container on a Tokio runtime of its own thread, so `kill` ends
  it with every task it spawned. A `Runner` gets the environment, a listener bound to the
  container's `127.0.0.x` (127.0.0.1 off Linux) and the stop; it returns the exit code, and
  `describe`/`list` report `endReason` and `exitCode`.
- **The fleet tests** (`tests/fleet_*.rs`, helpers in `tests/fleet_support`) start routers with
  `run_router` on loopback and real containers through `serve_runner`.

## Container and broker

**Run a container in process** (fleet tests' fake launcher, and `main` for `knoggin serve`):

```rust
let end: ServeEnd = container::serve_on(listener, ServeConfig {
    env,                       // ContainerEnv: id, launch secret, router URL, router pin, log level
    build: BuildInfo::new("0.1.0"),
    test_mode: true,
    local_address: None,       // Some(127.0.0.x) in the fleet tests on Linux
    model: ServeModel::fake(), // or ServeModel::new(identity, state_rx, providers) in main
    timings: ServeTimings::default(),
}, stop_signal).await;         // `serve(addr, ..)` binds first; main uses `container::runtime()`
// end.exit_code: 0 after a park or an unclaimed stop, 1 after lease_lost or a failed start;
// end.reason: EndReason::{Parked, Stopped, LeaseLost, Failed}.
```

The role answers `hello` (needs `Knoggin-Protocol`; a nonce is answered once), `claim`
(409 `conflict` on a second one), `park` (202) and `status` behind the router token, and any
other path as a forwarded store request (`RequestContext::write_internal_headers` plus
`write_router_call`). While parking it answers 503 with `Knoggin-Internal-Status: parking`.

**Mount the broker** on the router's internal listener (the router does this in
`router::broker` and `router::run`):

```rust
let peers = PeerLimits::new(clock.clone());          // keep it: the listener's admit uses it too
let router = broker::broker_router(BrokerConfig {
    secret, leases, stores, keys, storage, clock,
    link_ttl: Duration::from_secs(s.storage.link_ttl_seconds),
    memory_limit_bytes: s.containers.memory_limit_bytes(),
    upload_bytes_per_hour: s.upload_bytes_per_hour(),
    one_host: one_host.then(|| "https://router:8443".into()), // the fleet tests: their router URL
    peers: peers.clone(),
    on_parked,                // Arc<dyn Fn(ContainerCaller) -> BoxFuture<Result<(), BrokerError>>>
    ask_park,                 // Arc<dyn Fn(Slot, ParkReason) -> BoxFuture<()>>: Fleet::ask_park
})?;
serve_tls(listener, tls, router, move |peer| {
    // one host: also refuse peers outside containers.user_net_cidr
    peers.admit(peer)
}, shutdown).await;
```

- `on_parked` writes the records "Mark gone" writes once that container has ended, plus a stop; the
  broker has already checked the caller's token, lease (`claiming` or `warm`, names it) and IP.
- `ask_park` records a park request on the slot's lease and sends `park` to its container at once
  (`moved` for a store that changes slot, `lifetime` for a generation that must rise).
- `PeerLimits::clear(ip)` when a new container is recorded at an address.
- The broker's bodies are `broker::api` (`OpenAnswer`, `UploadLink`, `CommitRequest`, ...);
  errors are `{"error":{"code","message","reason"?}}` with `BrokerCode` (`lease_lost` = 409).

## Router and container modules

| Module | Holds |
| --- | --- |
| `container` | `knoggin serve` role (takes `ContainerEnv`, `BuildInfo`, `test_mode`, listen address, optional local address); the middleware; `hello`/`claim`/`park`/`status` handlers with `link::api`; `RouterBackend` (a `StoreBackend` over the broker); the park sequence |
| `broker` | the internal listener's routes (`open`, `snapshots`, `commit`, `parked`, the one-host blob route) with their bodies, limits and per-address counts; the blob route checks the raw path with `names::parse_snapshot_key` and rebuilds the query from the signed parameters (`MemoryStorage` uses S3's names, `x-id` included) |
| `router` | `knoggin router` wiring (edge layer, forwarder, filter, internal listener, start-up order, peer checks with `Cidr`) |
| `lease` | lease state machine, claim, Mark gone, wait loop |
| `pool` | launching (`Launcher::launch`, `describe`, `hello`), pool size |
| `reconcile` | leader lock and the 30-second loop, rollout (`BuildInfo::compare`) |
| `testing` | a `Launcher` that runs the container role in process on `127.0.0.x` |

## Lifecycle, admin and launchers

| Module | Offers |
| --- | --- |
| `lifecycle` | `Lifecycle { db, postgres, storage, retention }::run_once(now) -> LifecycleReport` (thinning with `snapshot::thin`, `knoggin_state::erase`, old `selftest/` objects, `PostgresDb::delete_expired`); `run(lifecycle, every, clock, stop)`. The only caller of `ObjectStorage::delete`; a unit test scans `src/` for any other |
| `admin` | `Admin::new(db, fields, storage, clock)` with `restore(store, name, wait) -> RestoreOutcome::{Parked, Waiting}`, `cancel_restore`, `retire_before(RetireBefore::{Now, At, Clear}) -> active leases`, `snapshots(store, retention_days)`; `parse_snapshot` (pads), `snapshot_list` (the admin route's body, shared with `router::status`), `rotate_master_key(db, key_file)`, `bucket_selftest(storage, http, now) -> Result<(), BucketFailure { step: BucketStep, reason }>`, `selftest(db, keys, storage, http, now) -> SelftestReport` |
| `launcher::docker` | `DockerLauncher::new(url, token)` implements `Launcher`; `attach()` outside the trait |
| `launcher_service` | `LauncherService::start(docker, own_container, ServiceConfig)`, `router()`, `tidy(now)`; `run(LauncherServiceSettings, test_mode, docker, own, stop)` |
| `storage::Weakness` | `MemoryStorage::weaken(Some(Overwrites | Unsigned | NoExpiry | StaleListing))`: each bucket check can be seen to fail |

The binary (`crates/knoggin/src/server.rs`) checks every setting of a role before it connects,
then runs `router::run_router` with `before_rollout` = `bucket_selftest` and then
`DockerLauncher::attach` (60 s of tries), `container::serve` on `container::runtime()`,
`lifecycle::run` and `launcher_service::run`. `knoggin_state::thin_targets(db)` lists the stores
to thin without the field key.

Settings for `knoggin_http::edge` (public URL, auth, per-credential limits) stay in `EdgeConfig`;
the per-address and per-user rates are read from `RouterLimits` by `run_router` into the edge.

## AWS

| Module | Offers |
| --- | --- |
| `kms` | `KmsKeyProvider::new(&SdkConfig, key_arn)` implements `KeyProvider` (`GenerateDataKey`/`Decrypt` with the context `knoggin:store` and `knoggin:generation`); `with_timeouts(builder)` + `from_conf` for a test client |
| `launcher::ecs` | `EcsLauncher::new(&SdkConfig, EcsSettings)` implements `Launcher` (`RunTask`, `DescribeTasks` with tags, `ListTasks` running and stopped, `DescribeContainerInstances` for draining, `StopTask`); `with_timeouts` + `from_conf` for a test client |
| `aws` | `SdkConfig` re-exported, so the binary names it without `aws-config` |

`DynamoDb` lives in `knoggin-state` (`DynamoDb::new(aws_sdk_dynamodb::config::Builder::from(&sdk), table, clock)`).
The binary builds all three from `aws::sdk_config(None, None)` (the default chain; no endpoint
setting). Stack tests: `knoggin-state/tests/stack_aws_state.rs`, `tests/stack_aws_kms.rs`,
`tests/stack_aws_launcher.rs`, and the binary's `knoggin/tests/stack_aws_admin.rs`, all on the
pinned moto image.
