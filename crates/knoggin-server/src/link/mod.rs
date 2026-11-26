// SPDX-License-Identifier: AGPL-3.0-only
//! The internal link between routers and containers (tab "Server", internal link): TLS
//! 1.3 with certificates pinned by their SHA-256 and no authority trusted, launch secrets
//! and the two tokens derived from them, the `hello` proof, the clients and the listener,
//! and the bodies of the container's internal routes.

pub mod api;
mod calls;
mod cert;
mod client;
mod listen;
mod secret;
mod tls;

pub use calls::{
    CallRefused, ContainerCaller, verify_container_call, verify_router_call, write_container_call,
    write_router_call,
};
pub use cert::{INTERNAL_CERT_NAME, ServerIdentity, new_internal_cert};
pub use client::{
    CONNECT_TIMEOUT, ForwardClient, MAX_ANSWER_BYTES, Peer, PeerResolver, Peers, RouterClient,
    UnpinnedReply, fetch_unpinned, forwarding_client, fresh_client, router_client,
};
pub use listen::{HEADER_READ_TIMEOUT, PeerAddr, serve_tls};
pub use secret::{CertPin, InternalSecret, LaunchSecret, Nonce, Token};
pub use tls::{hello_client_config, pinned_client_config, server_config};

/// A snapshot transfer ends when no bytes moved for this long, in either direction.
pub(crate) const TRANSFER_IDLE: std::time::Duration = std::time::Duration::from_secs(60);

/// Ends a byte stream with an error when nothing arrives for [`TRANSFER_IDLE`], and keeps `guard`
/// for as long as the stream lives.
pub(crate) fn idle_limited<S, E, G>(
    stream: S,
    guard: G,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + 'static
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
    G: Send + 'static,
{
    futures_util::stream::unfold(Some((Box::pin(stream), guard)), |state| async move {
        let (mut stream, guard) = state?;
        match tokio::time::timeout(TRANSFER_IDLE, futures_util::StreamExt::next(&mut stream)).await
        {
            Ok(Some(Ok(bytes))) => Some((Ok(bytes), Some((stream, guard)))),
            Ok(None) => None,
            Ok(Some(Err(_))) => Some((Err(std::io::Error::other("the transfer broke")), None)),
            Err(_) => Some((
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "no bytes moved",
                )),
                None,
            )),
        }
    })
}

/// What can go wrong on the link. No variant carries a secret, an address or a URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    /// A secret in the config or the environment is not in its form.
    #[error("{0} is not in its expected form")]
    BadSecret(&'static str),
    /// A pin is not 64 hex characters.
    #[error("a certificate pin must be 64 hex characters")]
    BadPin,
    /// The system random source failed.
    #[error("no random bytes")]
    Random,
    /// A certificate could not be made or read.
    #[error("the certificate or key cannot be made or read")]
    Certificate,
    /// A TLS config could not be built, or the handshake failed.
    #[error("TLS failed")]
    Tls,
    /// The peer could not be reached, or the connection broke.
    #[error("the peer cannot be reached")]
    Unreachable,
    /// The answer was longer than the link allows.
    #[error("the answer is too large")]
    AnswerTooLarge,
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::{Extension, Query};
    use axum::http::{HeaderMap, Request, StatusCode};
    use axum::routing::get;
    use bytes::Bytes;
    use futures_util::StreamExt;
    use http_body_util::{BodyExt, Empty};
    use knoggin_core::UserId;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    use super::*;

    #[derive(serde::Deserialize)]
    struct HelloQuery {
        nonce: String,
    }

    /// A container's listener: `hello` with its proof, and `status` behind the router token.
    async fn container(id: Uuid, launch: LaunchSecret) -> (SocketAddr, oneshot::Sender<()>) {
        let identity = ServerIdentity::for_container(id).unwrap();
        let der = identity.cert_der().to_vec();
        let token = launch.router_token();
        let hello_secret = launch.clone();
        let router = Router::new()
            .route(
                api::paths::HELLO,
                get(move |Query(q): Query<HelloQuery>| {
                    let der = der.clone();
                    let launch = hello_secret.clone();
                    async move {
                        let nonce = Nonce::from_hex(&q.nonce).unwrap();
                        launch.hello_proof(&nonce, &der).expose().to_owned()
                    }
                }),
            )
            .route(
                api::paths::STATUS,
                get(
                    move |Extension(peer): Extension<PeerAddr>, headers: HeaderMap| {
                        let token = token.clone();
                        async move {
                            match verify_router_call(&token, &headers) {
                                Ok(()) => (StatusCode::OK, peer.ip().to_string()),
                                Err(_) => (StatusCode::UNAUTHORIZED, String::new()),
                            }
                        }
                    },
                ),
            );
        listen(server_config(&identity).unwrap(), router, |_| Some(())).await
    }

    async fn listen<G: Send + 'static>(
        tls: Arc<rustls::ServerConfig>,
        router: Router,
        admit: impl Fn(SocketAddr) -> Option<G> + Send + 'static,
    ) -> (SocketAddr, oneshot::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(serve_tls(listener, tls, router, admit, async {
            let _ = rx.await;
        }));
        (addr, tx)
    }

    #[tokio::test]
    async fn hello_then_pinned_calls_by_container_id() {
        let secret = InternalSecret::from_bytes([11u8; 32]);
        let id = Uuid::now_v7();
        let (addr, _stop) = container(id, secret.launch_secret(id)).await;

        // hello: one connection, the certificate of that connection, the proof checked
        // with the launch secret the router derives itself.
        let nonce = Nonce::random().unwrap();
        let req = Request::get(format!("{}?nonce={}", api::paths::HELLO, nonce.to_hex()))
            .header(http::header::HOST, id.to_string())
            .body(Empty::<Bytes>::new())
            .unwrap();
        let reply = fetch_unpinned(addr, &id.to_string(), req).await.unwrap();
        assert_eq!(reply.status, StatusCode::OK);
        let proof = std::str::from_utf8(&reply.body).unwrap();
        let launch = secret.launch_secret(id);
        assert!(launch.check_proof(&nonce, &reply.cert_der, proof));
        // Another container's secret does not match.
        let stranger = secret.launch_secret(Uuid::now_v7());
        assert!(!stranger.check_proof(&nonce, &reply.cert_der, proof));

        // The pooled client resolves the ID from the map and pins what hello proved.
        let peers = Arc::new(Peers::default());
        let client = forwarding_client::<Body>(peers.clone()).unwrap();
        let url = format!("https://{id}:{}{}", addr.port(), api::paths::STATUS);
        let call = |token: Option<Token>| {
            let mut req = Request::get(&url).body(Body::empty()).unwrap();
            if let Some(t) = token {
                write_router_call(req.headers_mut(), &t);
            }
            client.request(req)
        };
        assert!(
            call(Some(launch.router_token())).await.is_err(),
            "unknown ID"
        );
        let v4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, addr.port());
        peers.set(
            id,
            Peer {
                address: v4,
                pin: CertPin::of(&reply.cert_der),
            },
        );
        let ok = call(Some(launch.router_token())).await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(ok.version(), http::Version::HTTP_2);
        let seen = ok.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&seen[..], b"127.0.0.1");
        let wrong = call(Some(launch.container_token())).await.unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(call(None).await.unwrap().status(), StatusCode::UNAUTHORIZED);

        // A container that presents another certificate is refused before any token goes.
        let impostor = ServerIdentity::for_container(id).unwrap();
        peers.set(
            id,
            Peer {
                address: v4,
                pin: impostor.pin(),
            },
        );
        let fresh = forwarding_client::<Body>(peers.clone()).unwrap();
        let req = Request::get(&url).body(Body::empty()).unwrap();
        assert!(fresh.request(req).await.is_err());
    }

    #[tokio::test]
    async fn a_container_calls_the_router_it_pinned_and_no_other() {
        let routers = ServerIdentity::from_pem(
            &new_internal_cert(knoggin_core::Timestamp::from_millis(1_795_000_000_000))
                .unwrap()
                .0,
            "",
        );
        assert!(routers.is_err(), "a key is required");
        let (cert, key) =
            new_internal_cert(knoggin_core::Timestamp::from_millis(1_795_000_000_000)).unwrap();
        let identity = ServerIdentity::from_pem(&cert, &key).unwrap();
        let secret = Arc::new(InternalSecret::from_bytes([12u8; 32]));
        let checked = secret.clone();
        let router = Router::new().route(
            "/internal/v1/parked",
            axum::routing::post(move |headers: HeaderMap| {
                let secret = checked.clone();
                async move {
                    match verify_container_call(&secret, &headers) {
                        Ok(caller) => (StatusCode::OK, caller.container_id.to_string()),
                        Err(_) => (StatusCode::CONFLICT, String::new()),
                    }
                }
            }),
        );
        let refused = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = refused.clone();
        // Admit 127.0.0.1 only: the fleet tests' container addresses are 127.0.0.x.
        let (addr, stop) = listen(server_config(&identity).unwrap(), router, move |peer| {
            if peer.ip() == Ipv4Addr::LOCALHOST {
                Some(())
            } else {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                None
            }
        })
        .await;
        let id = Uuid::now_v7();
        let launch = secret.launch_secret(id);
        let user = UserId::from_uuid(Uuid::now_v7());
        let url = format!("https://127.0.0.1:{}/internal/v1/parked", addr.port());
        let post = |pin: CertPin, local: Option<std::net::IpAddr>| {
            let client = router_client::<Body>(pin, local).unwrap();
            let mut req = Request::post(&url).body(Body::empty()).unwrap();
            write_container_call(req.headers_mut(), &launch, user, id);
            async move { client.request(req).await }
        };
        let ok = post(identity.pin(), None).await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let body = ok.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, id.to_string());
        // The pin of another certificate: the connection is refused, no token is sent.
        let other = ServerIdentity::for_container(id).unwrap();
        assert!(post(other.pin(), None).await.is_err());
        // A peer the listener does not admit sees the connection close.
        if cfg!(target_os = "linux") {
            let local = Some(std::net::IpAddr::from([127, 0, 0, 2]));
            assert!(post(identity.pin(), local).await.is_err());
            assert_eq!(refused.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
        stop.send(()).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_transfer_ends_after_a_minute_without_bytes() {
        let started = tokio::time::Instant::now();
        let first =
            futures_util::stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"x")) });
        let mut s = Box::pin(idle_limited(
            first.chain(futures_util::stream::pending()),
            (),
        ));
        assert_eq!(s.next().await.unwrap().unwrap(), Bytes::from_static(b"x"));
        let err = s.next().await.unwrap().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= TRANSFER_IDLE);
        assert!(s.next().await.is_none(), "nothing after the error");
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_connection_is_closed() {
        let identity = ServerIdentity::for_container(Uuid::now_v7()).unwrap();
        let (addr, _stop) = listen(server_config(&identity).unwrap(), Router::new(), |_| {
            Some(())
        })
        .await;
        let mut silent = tokio::net::TcpStream::connect(addr).await.unwrap();
        let started = tokio::time::Instant::now();
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(
            HEADER_READ_TIMEOUT * 2,
            tokio::io::AsyncReadExt::read(&mut silent, &mut buf),
        )
        .await
        .unwrap();
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
        assert!(started.elapsed() >= HEADER_READ_TIMEOUT);
    }

    /// After the TLS handshake an HTTP/2 client that sends no request is closed within the
    /// same 30 seconds; HTTP/2 itself would wait for its first frames without a limit.
    #[tokio::test]
    async fn a_silent_http2_connection_is_closed() {
        let id = Uuid::now_v7();
        let identity = ServerIdentity::for_container(id).unwrap();
        let (addr, _stop) = listen(server_config(&identity).unwrap(), Router::new(), |_| {
            Some(())
        })
        .await;
        let mut config = hello_client_config().unwrap();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from(id.to_string()).unwrap();
        let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .unwrap();
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
        // The handshake ran in real time; the wait for a request runs on paused time.
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let mut buf = [0u8; 256];
        let closed = tokio::time::timeout(HEADER_READ_TIMEOUT * 2, async {
            // The server's own settings frame comes first; then the end.
            loop {
                match tokio::io::AsyncReadExt::read(&mut tls, &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "the connection stayed open");
        assert!(started.elapsed() >= HEADER_READ_TIMEOUT);
    }
}
