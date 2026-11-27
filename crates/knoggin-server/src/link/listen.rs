// SPDX-License-Identifier: AGPL-3.0-only
//! The TLS listener of the link: the container's on `0.0.0.0:8443` and the router's
//! internal one. Built with hyper-util and tokio-rustls (tab "Engineering rules", HTTP
//! limits): timers set, a 30-second header read timeout, the TLS handshake bounded by the
//! same time, HTTP/2 keep-alive and stream limits. The protocol comes from ALPN, so the
//! server never waits unbounded to guess it.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::{Extension, Router};
use hyper::Request;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::server::graceful::{GracefulShutdown, Watcher};
use hyper_util::service::TowerToHyperService;
use rustls::ServerConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tower::{Layer, ServiceExt};

/// The header read timeout of every link listener, which also bounds the TLS handshake.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The peer of a connection on a link listener, in every request's extensions. The IP is
/// canonical (`IpAddr::to_canonical`), as every comparison with a lease address needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerAddr(pub SocketAddr);

impl PeerAddr {
    /// The canonical IP address of the peer.
    pub fn ip(self) -> IpAddr {
        self.0.ip()
    }
}

fn builder(http2: bool) -> Builder<TokioExecutor> {
    let mut b = Builder::new(TokioExecutor::new());
    b.http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    b.http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(20))
        .max_concurrent_streams(100);
    if http2 {
        b.http2_only()
    } else {
        b.http1_only()
    }
}

/// Serves `router` over TLS until `shutdown` resolves, then waits for the requests in
/// flight (the caller bounds that wait).
///
/// `admit` runs for every accepted connection before anything is read: `None` closes it
/// at once without an answer. What it returns is held for as long as the connection lives,
/// so the router's per-address counts can use a guard. The container admits everyone.
pub async fn serve_tls<G, A>(
    listener: TcpListener,
    tls: Arc<ServerConfig>,
    router: Router,
    admit: A,
    shutdown: impl Future<Output = ()>,
) where
    G: Send + 'static,
    A: Fn(SocketAddr) -> Option<G>,
{
    let acceptor = TlsAcceptor::from(tls);
    let builders = Arc::new([builder(false), builder(true)]);
    let graceful = GracefulShutdown::new();
    let mut connections = JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        while connections.try_join_next().is_some() {}
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let peer = SocketAddr::new(peer.ip().to_canonical(), peer.port());
                    let Some(guard) = admit(peer) else { continue };
                    connections.spawn(connection(
                        stream,
                        acceptor.clone(),
                        router.clone(),
                        PeerAddr(peer),
                        builders.clone(),
                        graceful.watcher(),
                        guard,
                    ));
                }
                Err(e) => {
                    tracing::warn!(kind = ?e.kind(), "accept failed");
                    // A full descriptor table would otherwise spin.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            () = &mut shutdown => break,
        }
    }
    graceful.shutdown().await;
    connections.abort_all();
}

async fn connection<G>(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    router: Router,
    peer: PeerAddr,
    builders: Arc<[Builder<TokioExecutor>; 2]>,
    watcher: Watcher,
    guard: G,
) {
    let _ = stream.set_nodelay(true);
    let Ok(Ok(tls)) = tokio::time::timeout(HEADER_READ_TIMEOUT, acceptor.accept(stream)).await
    else {
        return;
    };
    let http2 = tls.get_ref().1.alpn_protocol() == Some(b"h2");
    // HTTP/1 bounds its first request with the header read timeout; HTTP/2 has no such
    // timer before the client's first frames, so the first request must come within it.
    let started = Arc::new(AtomicBool::new(false));
    let seen = started.clone();
    let service = TowerToHyperService::new(Extension(peer).layer(router).map_request(
        move |req: Request<Incoming>| {
            seen.store(true, Ordering::Relaxed);
            req
        },
    ));
    let conn = builders[usize::from(http2)]
        .serve_connection_with_upgrades(TokioIo::new(tls), service)
        .into_owned();
    let mut conn = std::pin::pin!(watcher.watch(conn));
    let ended = tokio::select! {
        ended = &mut conn => ended,
        () = tokio::time::sleep(HEADER_READ_TIMEOUT), if http2 => {
            if !started.load(Ordering::Relaxed) {
                // Dropping the connection closes it.
                return;
            }
            conn.await
        }
    };
    if ended.is_err() {
        tracing::debug!(kind = "connection", "a link connection ended with an error");
    }
    drop(guard);
}
