// SPDX-License-Identifier: AGPL-3.0-only
//! Listeners built with hyper-util's automatic HTTP/1 and HTTP/2 builder, with the HTTP
//! limits: timers set, the header read timeout, HTTP/2 keep-alive and stream limits, and a
//! first-byte timeout, because the automatic builder waits for the first bytes without one.
//! Each connection's requests carry the [`Arrival`] of their listener.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::middleware::AddExtension;
use axum::{Extension, Router};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::server::graceful::{GracefulShutdown, Watcher};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};
use tokio::task::JoinSet;
use tower::Layer;

use super::Arrival;

/// The timeouts of one listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenConfig {
    /// The header read timeout, which also bounds the wait for a connection's first bytes
    /// and closes an idle HTTP/1 connection.
    pub header_read_timeout: Duration,
}

impl ListenConfig {
    /// 30 seconds: every listener but the router's public one.
    pub const DEFAULT: Self = Self {
        header_read_timeout: Duration::from_secs(30),
    };
    /// 310 seconds: the router's public listener, longer than a proxy's 300-second idle.
    pub const PUBLIC: Self = Self {
        header_read_timeout: Duration::from_secs(310),
    };
}

fn builder(config: ListenConfig) -> Builder<TokioExecutor> {
    let mut b = Builder::new(TokioExecutor::new());
    b.http1()
        .timer(TokioTimer::new())
        .header_read_timeout(config.header_read_timeout);
    b.http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(20))
        .max_concurrent_streams(100);
    b
}

/// Serves `router` on a TCP listener until `shutdown` resolves, then waits for the
/// requests in flight (the caller bounds that wait).
pub async fn serve_tcp(
    listener: TcpListener,
    router: Router,
    arrival: Arrival,
    config: ListenConfig,
    shutdown: impl Future<Output = ()>,
) {
    let accept = || async {
        let (s, peer) = listener.accept().await?;
        let peer = SocketAddr::new(peer.ip().to_canonical(), peer.port());
        Ok((Stream::Tcp(s), Some(Peer(peer))))
    };
    run(accept, router, arrival, config, shutdown).await;
}

/// The peer of a connection on a listener served by [`serve_tcp_admitted`], in every
/// request's extensions, with its IP made canonical (`IpAddr::to_canonical`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer(pub SocketAddr);

/// Like [`serve_tcp`], for the router's public and metrics listeners: `admit` runs for
/// every accepted connection before anything is read and closes a refused peer's
/// connection without an answer, and each request carries the connection's [`Peer`].
pub async fn serve_tcp_admitted(
    listener: TcpListener,
    router: Router,
    arrival: Arrival,
    config: ListenConfig,
    admit: impl Fn(SocketAddr) -> bool,
    shutdown: impl Future<Output = ()>,
) {
    let accept = || async {
        let (stream, peer) = listener.accept().await?;
        let peer = SocketAddr::new(peer.ip().to_canonical(), peer.port());
        let allowed = admit(peer);
        Ok((Stream::Tcp(stream), allowed.then_some(Peer(peer))))
    };
    run(accept, router, arrival, config, shutdown).await;
}

/// Serves `router` on the daemon's Unix socket. A connection whose peer is not this
/// process's own user ID is closed before it is read.
pub async fn serve_unix(
    listener: UnixListener,
    router: Router,
    config: ListenConfig,
    shutdown: impl Future<Output = ()>,
) {
    let own = rustix::process::getuid().as_raw();
    let accept = || async {
        let (stream, _) = listener.accept().await?;
        let allowed = stream
            .peer_cred()
            .is_ok_and(|cred| peer_allowed(cred.uid(), own));
        if !allowed {
            tracing::warn!(
                kind = "foreign_peer",
                "closed a socket connection from another user"
            );
        }
        Ok((Stream::Unix(stream), allowed.then_some(Peer(UNIX_PEER))))
    };
    run(accept, router, Arrival::Socket, config, shutdown).await;
}

/// The [`Peer`] of a Unix socket connection, which has no address.
const UNIX_PEER: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);

/// Whether a socket peer may be served: only this process's own user.
pub fn peer_allowed(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid
}

/// An accepted connection of either listener kind.
enum Stream {
    Tcp(tokio::net::TcpStream),
    Unix(tokio::net::UnixStream),
}

async fn run<A, F>(
    accept: A,
    router: Router,
    arrival: Arrival,
    config: ListenConfig,
    shutdown: impl Future<Output = ()>,
) where
    A: Fn() -> F,
    F: Future<Output = io::Result<(Stream, Option<Peer>)>>,
{
    let builder = Arc::new(builder(config));
    let graceful = GracefulShutdown::new();
    let mut connections = JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        while connections.try_join_next().is_some() {}
        tokio::select! {
            accepted = accept() => match accepted {
                Ok((stream, Some(peer))) => {
                    let service = service(router.clone(), arrival.clone(), peer);
                    connections.spawn(connection(
                        stream,
                        service,
                        builder.clone(),
                        graceful.watcher(),
                        config.header_read_timeout,
                    ));
                }
                Ok((_, None)) => {}
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

type Service = TowerToHyperService<AddExtension<AddExtension<Router, Peer>, Arrival>>;

fn service(router: Router, arrival: Arrival, peer: Peer) -> Service {
    TowerToHyperService::new(Extension(arrival).layer(Extension(peer).layer(router)))
}

async fn connection(
    stream: Stream,
    service: Service,
    builder: Arc<Builder<TokioExecutor>>,
    watcher: Watcher,
    first_byte: Duration,
) {
    // The automatic HTTP/1-or-2 choice reads the first bytes without a time limit. Wait
    // for a byte with a peek: `readable()` alone can report a fresh socket ready at once.
    let ready = match &stream {
        Stream::Tcp(s) => tokio::time::timeout(first_byte, s.peek(&mut [0u8; 1]))
            .await
            .map(|_| Ok(())),
        Stream::Unix(s) => tokio::time::timeout(first_byte, unix_first_byte(s)).await,
    };
    if ready.is_err() {
        return;
    }
    match stream {
        Stream::Tcp(s) => serve_io(s, service, &builder, watcher).await,
        Stream::Unix(s) => serve_io(s, service, &builder, watcher).await,
    }
}

/// Waits until the socket has a byte to read, or is closed, without taking it.
async fn unix_first_byte(s: &tokio::net::UnixStream) -> io::Result<()> {
    loop {
        s.readable().await?;
        let peeked = s.try_io(tokio::io::Interest::READABLE, || {
            rustix::net::recv(s, &mut [0u8; 1], rustix::net::RecvFlags::PEEK)
                .map_err(io::Error::from)
        });
        match peeked {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            _ => return Ok(()),
        }
    }
}

async fn serve_io<S>(
    stream: S,
    service: Service,
    builder: &Builder<TokioExecutor>,
    watcher: Watcher,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let conn = builder
        .serve_connection_with_upgrades(TokioIo::new(stream), service)
        .into_owned();
    if watcher.watch(conn).await.is_err() {
        tracing::debug!(kind = "connection", "a connection ended with an error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_own_user_is_served() {
        assert!(peer_allowed(501, 501));
        assert!(!peer_allowed(502, 501));
        assert!(!peer_allowed(0, 501));
    }
}
