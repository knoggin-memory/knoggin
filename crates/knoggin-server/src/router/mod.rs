// SPDX-License-Identifier: AGPL-3.0-only
//! `knoggin router`: the edge layer in front of the forwarder and the
//! router's mode routes, the client address with trusted proxies, the panic guard, the
//! internal listener for the broker, the reconcile loop, and the shutdown order.
//!
//! Start-up runs in this order: the config guards ([`guards`]),
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
use tokio::net::TcpSocket;
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
/// right-most address in `X-Forwarded-For` that is not one. Canonical either way. An entry
/// that does not parse ends the walk with the peer: what lies left of it cannot be told
/// apart from what the client wrote itself.
pub fn client_address(peer: IpAddr, headers: &HeaderMap, trusted: &[Cidr]) -> IpAddr {
    let peer = peer.to_canonical();
    let is_trusted = |ip: IpAddr| trusted.iter().any(|c| c.contains(ip));
    if !is_trusted(peer) {
        return peer;
    }
    let entries: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .flat_map(|v| v.to_str().unwrap_or(",").split(','))
        .collect();
    for entry in entries.into_iter().rev() {
        match entry.trim().parse::<IpAddr>() {
            Ok(ip) if is_trusted(ip.to_canonical()) => {}
            Ok(ip) => return ip.to_canonical(),
            Err(_) => return peer,
        }
    }
    peer
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
/// turns the blob route on. A container's `parked` writes the records "Mark gone" writes
/// once a container has ended, then stops it ([`Fleet::finish_parked`]).
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
    /// How long a stop waits for parking containers and requests in flight (110 s).
    pub shutdown_wait: Duration,
}

/// The listeners' sockets, bound by the caller but not listening: nothing can connect
/// until [`run`] makes them listen, the last step of the start.
#[derive(Debug)]
pub struct Listeners {
    /// `server.bind`.
    pub public: TcpSocket,
    /// `internal.bind`.
    pub internal: TcpSocket,
    /// `metrics.bind`.
    pub metrics: Option<TcpSocket>,
}

/// A socket bound to `addr` that does not listen yet, for [`Listeners`].
pub fn reserve(addr: SocketAddr) -> std::io::Result<TcpSocket> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    // As `TcpListener::bind` does on Unix.
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    Ok(socket)
}

/// The backlog `TcpListener::bind` uses.
const BACKLOG: u32 = 1024;

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
/// seconds; the key provider; the bucket; the launcher; the listeners' sockets, bound but not listening).
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
    /// The listeners' sockets, bound but not listening; [`run`] makes them listen last.
    pub listeners: Listeners,
    /// The metrics router, for `listeners.metrics`.
    pub metrics: Option<Router>,
    /// The start-up steps the binary owns, run after discovery and before the rollout
    /// record: the bucket self-test and, on one host, `DockerLauncher::attach`. An error
    /// stops the start.
    pub before_rollout: Option<BoxFuture<'static, Result<(), String>>>,
    /// For the admin status.
    pub backends: Backends,
    /// The router's waits; `Timing::default()` outside tests.
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

/// `knoggin router`: the start-up steps from the config guards to the rollout record,
/// then [`run`] until `stop`. Returns the exit code: 1 when a start-up
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
    // 6. Discovery: an unreachable provider is tried again for `timing.discovery` (60
    // seconds), then the start fails; anything else stops it at once.
    let deadline = tokio::time::Instant::now() + timing.discovery;
    loop {
        match state.discover().await {
            Ok(()) => break,
            Err(DiscoveryError::Unreachable) => {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                if left.is_zero() {
                    return Err(format!(
                        "the identity provider could not be reached in {} seconds",
                        timing.discovery.as_secs()
                    ));
                }
                tracing::warn!(
                    kind = "start",
                    "the identity provider cannot be reached yet"
                );
                tokio::time::sleep(left.min(Duration::from_secs(2))).await;
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
            shutdown_wait: timing.shutdown_wait,
        },
        listeners,
    ))
}

/// Serves until `stop` resolves, runs the reconcile loop, then stops in this order:
/// the public and metrics listeners stop accepting and the reconcile loop
/// ends; on one host every claimed container is asked to park, all at once, and the
/// internal listener stays open while they save; the requests in flight finish. Both
/// waits end together, `shutdown_wait` (110 seconds) after the stop. Then the internal
/// listener closes and every task the router owns has ended before it returns the exit
/// code.
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
        shutdown_wait,
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
    // 10. Last, the listeners.
    let listen = |s: TcpSocket| s.listen(BACKLOG);
    let (public_listener, internal_listener, metrics_listener) = match (
        listen(listeners.public),
        listen(listeners.internal),
        listeners.metrics.map(listen).transpose(),
    ) {
        (Ok(p), Ok(i), Ok(m)) => (p, i, m),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            tracing::error!(kind = "start", error = %e, "a listener cannot listen");
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
            public_listener,
            public,
            Arrival::Public,
            ListenConfig::PUBLIC,
            move |p: SocketAddr| public_peer_allowed(p.ip(), &trusted, one_host),
            wait(public_stopped.clone()),
        ));
    }
    if let (Some(listener), Some(router)) = (metrics_listener, metrics) {
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
    let mut internal = tokio::spawn(serve_tls(
        internal_listener,
        identity,
        broker,
        internal_admit(fleet.limits().clone(), user_net),
        wait(internal_stopped),
    ));
    let reconcile = tokio::spawn(crate::reconcile::run(
        fleet.clone(),
        std::future::pending::<()>(),
    ));
    stop.await;
    tracing::info!(kind = "shutdown", "the router is stopping");
    // Stop accepting on the public and metrics listeners; their requests in flight go on.
    let _ = public_stop.send(true);
    // The loop starts nothing more; every step of a pass is safe to cut.
    reconcile.abort();
    let _ = reconcile.await;
    let deadline = tokio::time::Instant::now() + shutdown_wait;
    if one_host {
        park_everyone(&fleet, deadline).await;
    }
    // The requests in flight finish within the same wait.
    let drained = tokio::time::timeout_at(deadline, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!(kind = "shutdown", "requests still in flight were cut off");
    }
    tasks.shutdown().await;
    // The internal listener closes last, once the containers had their wait.
    let _ = internal_stop.send(true);
    if tokio::time::timeout(Duration::from_secs(5), &mut internal)
        .await
        .is_err()
    {
        internal.abort();
        let _ = internal.await;
    }
    fleet.shutdown().await;
    0
}

/// One host's stop: `park` with the reason `shutdown` to every claimed container, all at
/// once, then wait until their leases are parked, all before `deadline`.
async fn park_everyone(fleet: &Arc<Fleet>, deadline: tokio::time::Instant) {
    let all = async {
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
        futures_util::future::join_all(users.iter().map(|&user| async move {
            let Ok(Some(lease)) = fleet.leases.get(user).await else {
                return;
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
        }))
        .await;
        loop {
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
    };
    if tokio::time::timeout_at(deadline, all).await.is_err() {
        tracing::warn!(kind = "shutdown", "some containers did not park in time");
    }
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
        // An entry that does not parse counts as the peer, not as the address left of it.
        let mut odd = HeaderMap::new();
        odd.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, unknown"),
        );
        assert_eq!(client_address(proxy, &odd, &trusted), proxy);
        odd.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, garbage, 192.0.2.10"),
        );
        assert_eq!(client_address(proxy, &odd, &trusted), proxy);
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
