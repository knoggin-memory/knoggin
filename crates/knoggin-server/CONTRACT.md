# knoggin-server: the foundation contract

For the agents that build the container and broker, the router (lease, pool, reconcile,
rollout), and lifecycle, admin and launchers. The spec (tabs "Server", "Workspace and
crates", "Deployment", "Security") is the authority; this file says what exists and where
the rest goes. Deviations are in `docs/spec-deviations.md`.

## What exists

| Module | Offers |
| --- | --- |
| `settings` | `RouterSettings` (`trusted_proxies`, `image_digest`, `limits: RouterLimits`, `storage`, `retention`, `containers`, `internal`, `rollout`, `log_level`; `upload_bytes_per_hour()`), `StorageSettings`, `RetentionSettings`, `ContainerSettings` (`memory_limit_bytes()`), `InternalSettings` (`secret: InternalSecret`, `cert` PEM, `key: SecretText`), `KeySettings` (`Kms`/`KeyFile`), `LauncherSettings` (`Ecs`/`Docker`), `LauncherServiceSettings`, `RolloutSettings`, `RateSetting`, `SecretText`, `Cidr` (`contains` canonicalizes, `overlaps`), `store_budget(memory_limit_bytes)`, `MODEL_RESERVE_BYTES` |
| `link` | Secrets: `InternalSecret::launch_secret(id)`, `LaunchSecret::{router_token, container_token, hello_proof, check_proof, from_hex, to_hex}`, `Token::{matches, bearer, expose}`, `Nonce`, `CertPin::{of, matches}` (+`FromStr`/`Display`). Certificates: `ServerIdentity::{for_container, from_pem, pin}`, `new_internal_cert(now)` (for `admin internal-cert`). TLS: `server_config`, `pinned_client_config`, `hello_client_config`. Clients: `Peers` (ID to address and pin), `forwarding_client(peers)`, `router_client(pin, local_address)`, `fetch_unpinned(addr, id, request)` (the `hello` call, answer capped at `MAX_ANSWER_BYTES`). Listener: `serve_tls(listener, tls, router, admit, shutdown)` with `PeerAddr` in every request. Calls: `verify_container_call`, `verify_router_call`, `write_container_call`, `write_router_call`. Bodies: `link::api` (`HelloAnswer`, `ClaimRequest`/`ClaimSettings`/`ClaimLimits`, `ParkRequest`, `StatusAnswer`/`StoreReport`/`StoreLock`/`Counters`, `ContainerPhase`, `PROTOCOLS`, `shared_protocol`, `paths`) |
| `storage` | `ObjectStorage`, `ObjectInfo`, `SignedRequest`, `StorageError`; `names` (`snapshot_prefix`, `store_prefix`, `snapshot_key`, `parse_snapshot_key` (strict, for the blob route's raw path), `name_in`, `SELFTEST`); `S3Storage::{connect, from_sdk_config, from_conf, s3_config}`; `MemoryStorage::{start(clock), endpoint, bucket, set_fault, insert, get, remove, keys, seen}` with `Fault::{None, Fail, Stall, Gone}` |
| `launcher` | `Launcher` (`launch`, `describe`, `list`, `stop`), `ContainerSpec`, `LaunchHandle`, `ContainerInfo` (`RunState`, `Ended { reason: EndReason, exit_code }`), `ContainerEnv` (`to_vars`, `from_lookup`, `env_names`), `LogLevel`, `LaunchError`, `BuildInfo::{new, compare, is_newer_than}` (semantic versions) |
| `aws` | `behavior_version()` (dated, `#[allow(deprecated)]`), `sdk_config(region, credentials_file)`. KMS and ECS clients use both |

Outside the crate: `knoggin_http::ctx::headers::{CONTAINER_ID, INTERNAL_STATUS}` and
`ctx::{link_protocol_ok, link_bearer, link_identity, write_link_identity, write_link_bearer}`
(all internal header code lives in `ctx`); `knoggin_core::ops::ModelState` now deserializes.

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
  refuses `/internal/*` that carries `Knoggin-User-Id`. A parking answer is 503 with
  `Knoggin-Internal-Status: parking` (`paths::PARKING`).
- **The router's internal listener**: `ServerIdentity::from_pem(&s.internal.cert,
  s.internal.key.expose())`, `serve_tls` with an `admit` that applies the per-address bounds
  (8 open, 60 a minute, 5 failed tokens) and, with `one_host`, `user_net_cidr.contains(ip)`; the
  returned guard is held for the connection's life. Handlers read `Extension<PeerAddr>` and call
  `verify_container_call(&secret, &headers)`, then check the lease (state, container, IP).
- **The container's client to the router**: `router_client(env.router_cert_sha256, local)`;
  every call gets `write_container_call(headers, &launch, user, id)`.
- **Signed links.** `ObjectStorage::presign_put_new` / `presign_get` with
  `Duration::from_secs(s.storage.link_ttl_seconds)` and `now`. The container sends a
  `SignedRequest` exactly as given (method, URL, every header). `names::snapshot_key(store,
  name)` is the only way to build a key.
- **Memory budget.** `settings::store_budget(memory_limit_bytes)` on both sides; the broker's
  `sizeBytes` bound is twice `store_budget(containers.memory_limit_bytes())`.

## Where the rest goes

| Module | Owner | Holds |
| --- | --- | --- |
| `container` | container + broker | `knoggin serve` role (takes `ContainerEnv`, `BuildInfo`, `test_mode`, listen address, optional local address); the middleware; `hello`/`claim`/`park`/`status` handlers with `link::api`; `RouterBackend` (a `StoreBackend` over the broker); the park sequence |
| `broker` | container + broker | the internal listener's routes (`open`, `snapshots`, `commit`, `parked`, the one-host blob route) with their bodies, limits and per-address counts; the blob route checks the raw path with `names::parse_snapshot_key` and rebuilds the query from the signed parameters (`MemoryStorage` uses S3's names, `x-id` included) |
| `router` | router | `knoggin router` wiring (edge layer, forwarder, filter, internal listener, start-up order, peer checks with `Cidr`) |
| `lease` | router | lease state machine, claim, Mark gone, wait loop |
| `pool` | router | launching (`Launcher::launch`, `describe`, `hello`), pool size |
| `reconcile` | router | leader lock and the 30-second loop, rollout (`BuildInfo::compare`) |
| `lifecycle` | lifecycle + admin + launchers | `knoggin lifecycle`; the only caller of `ObjectStorage::delete` (I11 wants a unit test for that) |
| `admin` | lifecycle + admin + launchers | restore, rotate, retire-before, bucket self-test (uses `presign_*` with an earlier `now`, and `MemoryStorage` faults to break each property) |
| `launcher::docker` (`DockerLauncher`) | lifecycle + admin + launchers | client of the launcher service; `attach` outside the trait |
| `launcher_service` | lifecycle + admin + launchers | `knoggin launcher` with `LauncherServiceSettings` |
| `testing` (fake launcher) | the first agent that needs it, once `serve` exists | a `Launcher` that runs the container role in process on `127.0.0.x` |
| `kms` (`KmsKeyProvider`), `launcher::ecs` (`EcsLauncher`) | milestone 7 | use `aws::sdk_config` and `behavior_version` |

Settings for `knoggin_http::edge` (public URL, auth, per-credential limits) stay in `EdgeConfig`;
the per-address and per-user rates are in `RouterLimits` until the edge takes them.
