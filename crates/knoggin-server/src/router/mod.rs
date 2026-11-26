// SPDX-License-Identifier: AGPL-3.0-only
//! `knoggin router` (tab "Server"): the edge layer in front of the forwarder and the
//! router's mode routes, the client address with trusted proxies, the panic guard, the
//! internal listener for the broker, the reconcile loop, and the shutdown order.
//!
//! Start-up runs in the order of tab "Server", Rollout: the config guards ([`guards`]),
//! the state database (60 seconds; Postgres migrates), the schema record, the field key,
//! in mode `none` the users check (`knoggin_http::edge::guard::router_users`), discovery,
//! the bucket self-test, on one host the launcher's `attach`, the rollout record
//! ([`crate::rollout::at_start`]), and last the listeners ([`run`]). The binary does the
//! steps that build its pieces and calls these in that order.

pub mod forward;
pub mod status;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use http::{HeaderMap, HeaderValue, StatusCode};
use knoggin_http::ApiError;
use knoggin_http::edge::listen::{ListenConfig, Peer, serve_tcp_admitted};
use knoggin_http::edge::{Arrival, ClientAddr, EdgeConfig, EdgeState, guard};
use knoggin_state::record::{LeaseState, ParkReason};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::request_id::RequestId;
use uuid::Uuid;

use futures_util::future::BoxFuture;
use knoggin_crypto::KeyProvider;
use knoggin_http::edge::{AuthMode, DiscoveryError, EdgeParts};
use knoggin_state::record::BuildSpec;
use knoggin_state::{Clock, Meta, StateDb, Stores, Users};

use crate::broker::{BrokerConfig, BrokerError, OnParked, PeerGuard, PeerLimits, broker_router};
use crate::launcher::{BuildInfo, Launcher};
use crate::lease::{Fleet, FleetConfig, peer_of};
use crate::link::api::{ClaimLimits, ClaimSettings};
use crate::link::{ContainerCaller, LinkError};
use crate::link::{ServerIdentity, serve_tls, server_config};
use crate::settings::{Cidr, RouterSettings};
use crate::storage::ObjectStorage;

/// How long a stopping router on one host keeps its internal listener open while its
/// containers save.
pub const SHUTDOWN_WAIT: Duration = Duration::from_secs(110);

/// The router's config guards beyond the sign-in ones: the warnings to log, or why it
/// refuses to start. With `one_host`, `server.trusted_proxies` must be set and must not
/// overlap `containers.user_net_cidr`.
pub fn guards(
    edge: &EdgeConfig,
    settings: &RouterSettings,
    one_host: bool,
) -> Result<Vec<&'static str>, String> {
    let warnings = guard::router_settings(edge)?;
    if one_host {
        if settings.trusted_proxies.is_empty() {
            return Err("on one host server.trusted_proxies must name the proxy's address".into());
        }
        if settings
            .trusted_proxies
            .iter()
            .any(|p| p.overlaps(&settings.containers.user_net_cidr))
        {
            return Err("server.trusted_proxies must not overlap containers.user_net_cidr".into());
        }
    }
    Ok(warnings)
}

/// The client address of a request: the peer, or when the peer is a trusted proxy, the
/// right-most address in `X-Forwarded-For` that is not one. Canonical either way.
pub fn client_address(peer: IpAddr, headers: &HeaderMap, trusted: &[Cidr]) -> IpAddr {
    let peer = peer.to_canonical();
    let is_trusted = |ip: IpAddr| trusted.iter().any(|c| c.contains(ip));
    if !is_trusted(peer) {
        return peer;
    }
    let forwarded: Vec<IpAddr> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|t| t.trim().parse::<IpAddr>().ok())
        .map(|ip| ip.to_canonical())
        .collect();
    forwarded
        .into_iter()
        .rev()
        .find(|ip| !is_trusted(*ip))
        .unwrap_or(peer)
}

/// Whether a peer may connect to the public or the metrics listener: on one host only the
/// trusted proxies and the loopback address; elsewhere everyone.
pub fn public_peer_allowed(peer: IpAddr, trusted: &[Cidr], one_host: bool) -> bool {
    let peer = peer.to_canonical();
    !one_host || peer.is_loopback() || trusted.iter().any(|c| c.contains(peer))
}

/// The public API: the edge layer over the forwarder and the mode routes, with the client
/// address put in each request and the panic guard around all.
pub fn public_router(
    edge: EdgeState,
    fleet: Arc<Fleet>,
    mode_routes: Router,
    request_timeout: Duration,
    trusted_proxies: Vec<Cidr>,
    timing: forward::Timing,
) -> Router {
    let app = knoggin_http::edge_layer(
        edge,
        forward::forwarder(fleet, request_timeout, timing),
        mode_routes,
    );
    let trusted = Arc::new(trusted_proxies);
    guarded(app.layer(middleware::from_fn(
        move |mut request: Request, next: Next| {
            let trusted = trusted.clone();
            async move {
                if let Some(Peer(peer)) = request.extensions().get::<Peer>().copied() {
                    let ip = client_address(peer.ip(), request.headers(), &trusted);
                    request.extensions_mut().insert(ClientAddr(ip));
                }
                next.run(request).await
            }
        },
    )))
}

/// The panic guard: `CatchPanicLayer::custom` drops the payload, logs one fixed line and
/// answers a bare 500; the outer layer opens the request's span with its ID (which the
/// edge layer then uses) and turns the bare 500 into `internal` with that ID.
pub fn guarded(app: Router) -> Router {
    app.layer(CatchPanicLayer::custom(
        |_payload: Box<dyn std::any::Any + Send>| {
            tracing::error!(kind = "panic", "a request handler panicked");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        },
    ))
    .layer(middleware::from_fn(
        |mut request: Request, next: Next| async move {
            let id = Uuid::now_v7();
            if let Ok(value) = HeaderValue::from_str(&id.to_string()) {
                request.extensions_mut().insert(RequestId::new(value));
            }
            let span = tracing::info_span!("request", request_id = %id);
            let response = tracing::Instrument::instrument(next.run(request), span).await;
            if response.status() == StatusCode::INTERNAL_SERVER_ERROR
                && response.extensions().get::<ApiError>().is_none()
                && response
                    .headers()
                    .get(knoggin_http::ctx::headers::REQUEST_ID)
                    .is_none()
            {
                let mut r = ApiError::internal().with_request_id(id).into_response();
                if let Ok(value) = HeaderValue::from_str(&id.to_string()) {
                    r.headers_mut()
                        .insert(knoggin_http::ctx::headers::REQUEST_ID, value);
                }
                return r;
            }
            response
        },
    ))
}

/// The broker on the router's internal listener, over the fleet's leases, stores and
/// per-address counts. `one_host` is the router's address as containers reach it, which
/// turns the blob route on. A container's `parked` runs "Mark gone" steps 2 to 4 and a
/// stop ([`Fleet::finish_parked`]).
pub fn broker(
    fleet: &Arc<Fleet>,
    keys: Arc<dyn KeyProvider>,
    settings: &RouterSettings,
    one_host: Option<String>,
) -> Result<Router, LinkError> {
    let parked_fleet = fleet.clone();
    let on_parked: OnParked = Arc::new(move |caller: ContainerCaller| {
        let fleet = parked_fleet.clone();
        Box::pin(async move {
            fleet
                .finish_parked(caller.container_id)
                .await
                .map_err(|_| BrokerError::unavailable())
        })
    });
    broker_router(BrokerConfig {
        secret: Arc::new(fleet.config().secret.clone()),
        leases: fleet.leases.clone(),
        stores: fleet.stores.clone(),
        keys,
        storage: fleet.storage.clone(),
        clock: fleet.clock.clone(),
        link_ttl: Duration::from_secs(settings.storage.link_ttl_seconds),
        memory_limit_bytes: settings.containers.memory_limit_bytes(),
        upload_bytes_per_hour: settings.upload_bytes_per_hour(),
        one_host,
        peers: fleet.limits().clone(),
        on_parked,
    })
}

/// The internal listener's `admit`: on one host only peers inside
/// `containers.user_net_cidr`, then the bounds per address that the broker's token checks
/// share.
pub fn internal_admit(
    limits: Arc<PeerLimits>,
    user_net: Option<Cidr>,
) -> impl Fn(SocketAddr) -> Option<PeerGuard> {
    move |peer| {
        if user_net
            .as_ref()
            .is_some_and(|c| !c.contains(peer.ip().to_canonical()))
        {
            return None;
        }
        limits.admit(peer)
    }
}

/// The pieces of a running router.
#[derive(Debug)]
pub struct RouterRole {
    /// The fleet.
    pub fleet: Arc<Fleet>,
    /// The public API ([`public_router`]).
    pub public: Router,
    /// The broker ([`broker`]), served on the internal listener.
    pub broker: Router,
    /// The metrics router, if `metrics.bind` is set.
    pub metrics: Option<Router>,
    /// The router's settings.
    pub settings: RouterSettings,
    /// One host: the peer allow lists and parking everyone on a stop.
    pub one_host: bool,
}

/// The listeners, bound by the caller.
#[derive(Debug)]
pub struct Listeners {
    /// `server.bind`.
    pub public: TcpListener,
    /// `internal.bind`.
    pub internal: TcpListener,
    /// `metrics.bind`.
    pub metrics: Option<TcpListener>,
}

/// The names the admin status gives this deployment's backends.
#[derive(Debug, Clone, Copy)]
pub struct Backends {
    /// `local_file`, `postgres` or `dynamodb`.
    pub state_db: &'static str,
    /// `key_file` or `kms`.
    pub key_provider: &'static str,
    /// `docker` or `ecs`.
    pub launcher: &'static str,
}

/// Everything `knoggin router` starts with: the settings, and the pieces the binary built
/// (the state database already connected, and on Postgres migrated, within its 60
/// seconds; the key provider; the bucket; the launcher; the bound listeners).
pub struct RouterStart {
    /// The edge layer's settings (`server.*`, `auth.*`, `limits.*`); `per_address` and
    /// `per_user` are filled from `settings.limits` here.
    pub edge: EdgeConfig,
    /// The router's settings.
    pub settings: RouterSettings,
    /// One host (`launcher.kind = "docker"`): the router's address as containers reach it,
    /// `https://router:8443`. `None` on AWS, where containers use `internal.router_url`.
    pub one_host: Option<String>,
    /// This build.
    pub build: BuildInfo,
    /// This router's spec (`launcher.ecs.task_definition`; empty on one host).
    pub spec: String,
    /// The state database.
    pub db: Arc<dyn StateDb>,
    /// The master key.
    pub keys: Arc<dyn KeyProvider>,
    /// The bucket.
    pub storage: Arc<dyn ObjectStorage>,
    /// The launcher.
    pub launcher: Arc<dyn Launcher>,
    /// Milliseconds since 1970.
    pub clock: Clock,
    /// The bound listeners.
    pub listeners: Listeners,
    /// The metrics router, for `listeners.metrics`.
    pub metrics: Option<Router>,
    /// Start-up steps 7 and 8, which the binary owns: the bucket self-test and, on one
    /// host, `DockerLauncher::attach`. An error stops the start.
    pub before_rollout: Option<BoxFuture<'static, Result<(), String>>>,
    /// For the admin status.
    pub backends: Backends,
    /// The forwarder's waits; `Timing::default()` outside tests.
    pub timing: forward::Timing,
}

impl std::fmt::Debug for RouterStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouterStart")
            .field("build", &self.build)
            .field("one_host", &self.one_host)
            .finish_non_exhaustive()
    }
}

/// `knoggin router`: the start-up steps from the config guards to the rollout record (tab
/// "Server", Rollout), then [`run`] until `stop`. Returns the exit code: 1 when a start-up
/// step refuses, 0 after a stop.
pub async fn run_router(start: RouterStart, stop: impl Future<Output = ()> + Send) -> i32 {
    match prepare(start).await {
        Ok((role, listeners)) => run(role, listeners, stop).await,
        Err(why) => {
            tracing::error!(kind = "start", why = %why, "the router refuses to start");
            1
        }
    }
}

async fn prepare(start: RouterStart) -> Result<(RouterRole, Listeners), String> {
    let RouterStart {
        mut edge,
        settings,
        one_host,
        build,
        spec,
        db,
        keys,
        storage,
        launcher,
        clock,
        listeners,
        metrics,
        before_rollout,
        backends,
        timing,
    } = start;
    let rate = |r: crate::settings::RateSetting| knoggin_http::edge::Rate {
        per_second: r.rps,
        burst: r.burst,
    };
    edge.per_address = Some(rate(settings.limits.per_address));
    edge.per_user = Some(rate(settings.limits.per_user));
    edge.idle_seconds = Some(settings.containers.idle_seconds);
    // 1. The config guards.
    for warning in guards(&edge, &settings, one_host.is_some())? {
        tracing::warn!(kind = "start", "{warning}");
    }
    let identity =
        ServerIdentity::from_pem(&settings.internal.cert, settings.internal.key.expose())
            .map_err(|e| format!("internal.cert and internal.key: {e}"))?;
    let router_url = match (&one_host, &settings.internal.router_url) {
        (Some(url), _) | (None, Some(url)) => url.clone(),
        (None, None) => return Err("internal.router_url must be set outside one host".into()),
    };
    // 3. The schema record; 4. the field key.
    let meta = Meta::new(db.clone());
    meta.check_schema()
        .await
        .map_err(|e| format!("the state database: {e}"))?;
    let fields = Arc::new(
        meta.field_keys(&*keys)
            .await
            .map_err(|e| format!("the field key: {e}"))?,
    );
    // 5. In mode none, no user but the local one; then that user exists.
    let users = Users::new(db.clone(), fields.clone());
    guard::router_users(&edge, &users).await?;
    if edge.auth_mode == AuthMode::None {
        knoggin_http::local::ensure_local_user(&users, &*keys, clock())
            .await
            .map_err(|e| format!("the local user: {e}"))?;
    }
    let state = EdgeState::new(
        edge.clone(),
        EdgeParts {
            db: db.clone(),
            field_keys: fields.clone(),
            keys: keys.clone(),
            clock: clock.clone(),
            host: None,
            local: None,
        },
    )
    .await
    .map_err(|e| format!("the edge layer: {e}"))?;
    // 6. Discovery: an unreachable provider is tried again; anything else stops the start.
    loop {
        match state.discover().await {
            Ok(()) => break,
            Err(DiscoveryError::Unreachable) => {
                tracing::warn!(
                    kind = "start",
                    "the identity provider cannot be reached yet"
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    // 7 and 8. The bucket self-test and `attach`.
    if let Some(step) = before_rollout {
        step.await?;
    }
    // 9. The rollout record.
    let own = BuildSpec {
        build: build.version.clone(),
        spec: spec.clone(),
    };
    crate::rollout::at_start(&meta, &own, settings.rollout.allow_downgrade)
        .await
        .map_err(|e| format!("the rollout record: {e}"))?;
    let limits = &edge.limits;
    let fleet = Fleet::new(
        FleetConfig {
            router_id: Uuid::now_v7(),
            build: build.clone(),
            spec,
            one_host: one_host.is_some(),
            test_mode: edge.test_mode,
            containers: settings.containers.clone(),
            claim: ClaimSettings {
                idle_seconds: settings.containers.idle_seconds,
                memory_limit_bytes: settings.containers.memory_limit_bytes(),
                public_url: edge.public_url.clone().unwrap_or_default(),
                source_url: edge.source_url.clone(),
                limits: ClaimLimits {
                    body_bytes: limits.body_bytes as u64,
                    request_timeout_ms: limits.request_timeout_ms,
                    store_concurrency: limits.store_concurrency as u64,
                },
            },
            secret: settings.internal.secret.clone(),
            router_url,
            router_pin: identity.pin(),
            log_level: settings.log_level,
            allow_downgrade: settings.rollout.allow_downgrade,
        },
        db.clone(),
        fields.clone(),
        launcher,
        storage,
        clock,
    )
    .map_err(|e| format!("the link clients: {e}"))?;
    let broker = broker(&fleet, keys, &settings, one_host.clone())
        .map_err(|e| format!("the broker: {e}"))?;
    let modes = status::mode_routes(
        fleet.clone(),
        Stores::new(db.clone(), fields),
        users,
        status::StatusInfo {
            image_digest: settings.image_digest.clone(),
            state_db: backends.state_db,
            key_provider: backends.key_provider,
            launcher: backends.launcher,
            bucket: settings.storage.bucket.clone(),
            region: settings.storage.region.clone(),
            retention_days: settings.retention.days,
        },
    );
    let public = public_router(
        state,
        fleet.clone(),
        modes,
        Duration::from_millis(edge.limits.request_timeout_ms),
        settings.trusted_proxies.clone(),
        timing,
    );
    Ok((
        RouterRole {
            fleet,
            public,
            broker,
            metrics,
            settings,
            one_host: one_host.is_some(),
        },
        listeners,
    ))
}

/// Serves until `stop` resolves, runs the reconcile loop, then stops in the order of
/// tab "Server": the public and metrics listeners close; on one host every claimed
/// container is asked to park and the internal listener stays open up to 110 seconds
/// while they save; then the rest. Returns the exit code.
pub async fn run(
    role: RouterRole,
    listeners: Listeners,
    stop: impl Future<Output = ()> + Send,
) -> i32 {
    let RouterRole {
        fleet,
        public,
        broker,
        metrics,
        settings,
        one_host,
    } = role;
    let identity = match ServerIdentity::from_pem(
        &settings.internal.cert,
        settings.internal.key.expose(),
    )
    .and_then(|i| server_config(&i))
    {
        Ok(tls) => tls,
        Err(e) => {
            tracing::error!(kind = "internal_cert", error = %e, "the internal certificate cannot be used");
            return 1;
        }
    };
    let (public_stop, public_stopped) = watch::channel(false);
    let (internal_stop, internal_stopped) = watch::channel(false);
    let wait = |mut rx: watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|s| *s).await;
    };
    let trusted = Arc::new(settings.trusted_proxies.clone());
    let mut tasks = tokio::task::JoinSet::new();
    {
        let trusted = trusted.clone();
        tasks.spawn(serve_tcp_admitted(
            listeners.public,
            public,
            Arrival::Public,
            ListenConfig::PUBLIC,
            move |p: SocketAddr| public_peer_allowed(p.ip(), &trusted, one_host),
            wait(public_stopped.clone()),
        ));
    }
    if let (Some(listener), Some(router)) = (listeners.metrics, metrics) {
        let trusted = trusted.clone();
        tasks.spawn(serve_tcp_admitted(
            listener,
            router,
            Arrival::Public,
            ListenConfig::DEFAULT,
            move |p: SocketAddr| public_peer_allowed(p.ip(), &trusted, one_host),
            wait(public_stopped.clone()),
        ));
    }
    let user_net = one_host.then_some(settings.containers.user_net_cidr);
    let internal = tokio::spawn(serve_tls(
        listeners.internal,
        identity,
        broker,
        internal_admit(fleet.limits().clone(), user_net),
        wait(internal_stopped),
    ));
    let (reconcile_stop, reconcile_stopped) = watch::channel(false);
    let reconcile = tokio::spawn(crate::reconcile::run(
        fleet.clone(),
        wait(reconcile_stopped),
    ));
    stop.await;
    tracing::info!(kind = "shutdown", "the router is stopping");
    let _ = public_stop.send(true);
    if one_host {
        park_everyone(&fleet).await;
    }
    let _ = reconcile_stop.send(true);
    let _ = internal_stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), reconcile).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), internal).await;
    tasks.abort_all();
    fleet.shutdown();
    0
}

/// One host's stop: `park` with the reason `shutdown` to every claimed container, then
/// wait up to 110 seconds until their leases are parked.
async fn park_everyone(fleet: &Arc<Fleet>) {
    let mut users = Vec::new();
    let mut after = None;
    loop {
        let Ok(page) = fleet.leases.list_active(100, after).await else {
            break;
        };
        users.extend(page.iter().copied());
        match page.last() {
            Some(last) if page.len() == 100 => after = Some(*last),
            _ => break,
        }
    }
    for &user in &users {
        let Ok(Some(lease)) = fleet.leases.get(user).await else {
            continue;
        };
        if let (Some(id), Some(peer)) = (
            lease.value.container_id,
            peer_of(
                lease.value.address.as_deref(),
                lease.value.cert_sha256.as_deref(),
            ),
        ) {
            let _ = fleet.request_park(user, ParkReason::Shutdown).await;
            fleet.park(id, peer, ParkReason::Shutdown).await;
        }
    }
    let deadline = tokio::time::Instant::now() + SHUTDOWN_WAIT;
    while tokio::time::Instant::now() < deadline {
        let mut waiting = false;
        for &user in &users {
            if let Ok(Some(l)) = fleet.leases.get(user).await
                && l.value.state != LeaseState::Parked
            {
                waiting = true;
                break;
            }
        }
        if !waiting {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tracing::warn!(kind = "shutdown", "some containers did not park in time");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_forwarding_header_counts_only_from_a_trusted_peer() {
        let trusted: Vec<Cidr> = vec!["192.0.2.10/32".parse().unwrap()];
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 198.51.100.4"),
        );
        let untrusted: IpAddr = "198.51.100.77".parse().unwrap();
        assert_eq!(client_address(untrusted, &h, &trusted), untrusted);
        let proxy: IpAddr = "192.0.2.10".parse().unwrap();
        assert_eq!(
            client_address(proxy, &h, &trusted),
            "198.51.100.4".parse::<IpAddr>().unwrap()
        );
        // A forwarded address that is itself a proxy is skipped.
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 192.0.2.10"),
        );
        assert_eq!(
            client_address(proxy, &h, &trusted),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        // Mapped addresses are made canonical.
        let mapped: IpAddr = "::ffff:192.0.2.10".parse().unwrap();
        assert_eq!(
            client_address(mapped, &h, &trusted),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn one_host_admits_only_proxies_and_loopback() {
        let trusted: Vec<Cidr> = vec!["192.0.2.10/32".parse().unwrap()];
        let user: IpAddr = "10.201.0.3".parse().unwrap();
        assert!(!public_peer_allowed(user, &trusted, true));
        assert!(public_peer_allowed(user, &trusted, false));
        assert!(public_peer_allowed(
            "127.0.0.1".parse().unwrap(),
            &trusted,
            true
        ));
        assert!(public_peer_allowed(
            "192.0.2.10".parse().unwrap(),
            &trusted,
            true
        ));
    }
}
