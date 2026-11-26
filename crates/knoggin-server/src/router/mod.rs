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
pub mod internal;
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

use crate::lease::{Fleet, peer_of};
use crate::link::{ServerIdentity, serve_tls, server_config};
use crate::settings::{Cidr, RouterSettings};

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

/// The pieces of a running router.
#[derive(Debug)]
pub struct RouterRole {
    /// The fleet.
    pub fleet: Arc<Fleet>,
    /// The public API ([`public_router`]).
    pub public: Router,
    /// The broker's routes, served on the internal listener.
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
        internal::internal_router(broker),
        internal::admit(fleet.bounds().clone(), user_net),
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
