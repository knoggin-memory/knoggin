// SPDX-License-Identifier: AGPL-3.0-only
//! The clients of the internal link: the router's pooled forwarding client (each
//! container's ID is the host of the address and is resolved from an in-memory map, never
//! by DNS), the container's client to the router, and the one-connection `hello` call.

use std::collections::HashMap;
use std::future::{Ready, ready};
use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::pin::pin;
use std::sync::{Arc, PoisonError, RwLock};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Empty, Limited};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use uuid::Uuid;

use super::tls::{hello_client_config, peers_client_config, pinned_client_config};
use super::{CertPin, LinkError};

/// How long a client waits for a TCP connection on the link.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The most bytes the router reads of an answer it reads in full (`hello`, `claim`, `park`,
/// `status`), and of a broker call's body.
pub const MAX_ANSWER_BYTES: usize = 1024 * 1024;

/// Where a container listens and the pin of its certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    /// The container's address: an IPv4 address and a port.
    pub address: SocketAddrV4,
    /// The SHA-256 of the certificate its `hello` proved.
    pub pin: CertPin,
}

/// The router's map from container ID to address and pin. The router fills it from the
/// lease or container record it read before every call it makes to a container, and drops
/// an entry once that record is `gone`. The forwarding client resolves and pins with it.
#[derive(Debug, Default)]
pub struct Peers(RwLock<HashMap<Uuid, Peer>>);

impl Peers {
    /// Records (or replaces) a container's address and pin.
    pub fn set(&self, container_id: Uuid, peer: Peer) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(container_id, peer);
    }

    /// Forgets a container.
    pub fn remove(&self, container_id: Uuid) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&container_id);
    }

    /// The container's address and pin, if recorded.
    pub fn get(&self, container_id: Uuid) -> Option<Peer> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&container_id)
            .copied()
    }
}

/// Resolves a container ID to its recorded address. Any other name is not found.
#[derive(Debug, Clone)]
pub struct PeerResolver(Arc<Peers>);

impl tower::Service<Name> for PeerResolver {
    type Response = std::iter::Once<SocketAddr>;
    type Error = std::io::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let found = Uuid::parse_str(name.as_str())
            .ok()
            .and_then(|id| self.0.get(id));
        ready(match found {
            Some(peer) => Ok(std::iter::once(SocketAddr::V4(peer.address))),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such container",
            )),
        })
    }
}

/// The router's pooled client to containers. Send requests to
/// `https://<container id>:<port>/...` after recording the container in [`Peers`].
pub type ForwardClient<B> = Client<HttpsConnector<HttpConnector<PeerResolver>>, B>;

/// A container's client to the router, with the routers' certificate pinned.
pub type RouterClient<B> = Client<HttpsConnector<HttpConnector>, B>;

/// Builds the forwarding client over `peers`.
pub fn forwarding_client<B>(peers: Arc<Peers>) -> Result<ForwardClient<B>, LinkError>
where
    B: hyper::body::Body + Send + 'static,
    B::Data: Send,
{
    peers_client(peers, true)
}

/// The forwarding client's twin that keeps no idle connection, so that every call opens
/// a new one: the router's `status` call after a link failure or a silence.
pub fn fresh_client<B>(peers: Arc<Peers>) -> Result<ForwardClient<B>, LinkError>
where
    B: hyper::body::Body + Send + 'static,
    B::Data: Send,
{
    peers_client(peers, false)
}

fn peers_client<B>(peers: Arc<Peers>, pool: bool) -> Result<ForwardClient<B>, LinkError>
where
    B: hyper::body::Body + Send + 'static,
    B::Data: Send,
{
    let mut http = HttpConnector::new_with_resolver(PeerResolver(peers.clone()));
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(CONNECT_TIMEOUT));
    let https = HttpsConnectorBuilder::new()
        .with_tls_config(peers_client_config(peers)?)
        .https_only()
        .enable_all_versions()
        .wrap_connector(http);
    if pool {
        Ok(pooled(https))
    } else {
        Ok(Client::builder(TokioExecutor::new())
            .pool_max_idle_per_host(0)
            .timer(TokioTimer::new())
            .build(https))
    }
}

/// Builds a container's client to the router. `local_address` binds its connections to
/// one address; only the fleet tests set it, so that each fake container calls from its
/// own `127.0.0.x`.
pub fn router_client<B>(
    router_pin: CertPin,
    local_address: Option<IpAddr>,
) -> Result<RouterClient<B>, LinkError>
where
    B: hyper::body::Body + Send + 'static,
    B::Data: Send,
{
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(CONNECT_TIMEOUT));
    http.set_local_address(local_address);
    let https = HttpsConnectorBuilder::new()
        .with_tls_config(pinned_client_config(router_pin)?)
        .https_only()
        .enable_all_versions()
        .wrap_connector(http);
    Ok(pooled(https))
}

fn pooled<C, B>(connector: C) -> Client<C, B>
where
    C: hyper_util::client::legacy::connect::Connect + Clone,
    B: hyper::body::Body + Send + 'static,
    B::Data: Send,
{
    Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .timer(TokioTimer::new())
        .build(connector)
}

/// What one unpinned call answered, and the certificate of the very connection it came on.
#[derive(Debug)]
pub struct UnpinnedReply {
    /// The status.
    pub status: StatusCode,
    /// The whole body, at most [`MAX_ANSWER_BYTES`].
    pub body: Bytes,
    /// The DER bytes of the end-entity certificate the peer presented.
    pub cert_der: Vec<u8>,
}

/// Sends one request on a connection of its own, with the `hello` verifier, reads the
/// answer once (at most [`MAX_ANSWER_BYTES`]) and closes. The certificate is read from
/// the same stream, so two calls at once cannot mix theirs up. `server_name` is the
/// container's ID. The caller bounds the whole call with its time limit.
pub async fn fetch_unpinned(
    address: SocketAddr,
    server_name: &str,
    mut request: Request<Empty<Bytes>>,
) -> Result<UnpinnedReply, LinkError> {
    let name = ServerName::try_from(server_name.to_owned()).map_err(|_| LinkError::Tls)?;
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(|_| LinkError::Unreachable)?
        .map_err(|_| LinkError::Unreachable)?;
    let tls = TlsConnector::from(Arc::new(hello_client_config()?))
        .connect(name, tcp)
        .await
        .map_err(|_| LinkError::Tls)?;
    let cert_der = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|chain| chain.first())
        .map(|c| c.to_vec())
        .ok_or(LinkError::Tls)?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(|_| LinkError::Unreachable)?;
    request.headers_mut().insert(
        http::header::CONNECTION,
        http::HeaderValue::from_static("close"),
    );
    let exchange = async {
        let response = sender
            .send_request(request)
            .await
            .map_err(|_| LinkError::Unreachable)?;
        let status = response.status();
        let body = Limited::new(response.into_body(), MAX_ANSWER_BYTES)
            .collect()
            .await
            .map_err(|_| LinkError::AnswerTooLarge)?
            .to_bytes();
        Ok::<_, LinkError>((status, body))
    };
    let mut exchange = pin!(exchange);
    let mut conn = pin!(conn);
    let (status, body) = tokio::select! {
        done = &mut exchange => done?,
        ended = &mut conn => match ended {
            // The connection closed cleanly: what it delivered is still to be read.
            Ok(()) => exchange.await?,
            Err(_) => return Err(LinkError::Unreachable),
        },
    };
    Ok(UnpinnedReply {
        status,
        body,
        cert_der,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_resolver_knows_only_recorded_containers() {
        use tower::ServiceExt as _;
        let peers = Arc::new(Peers::default());
        let id = Uuid::now_v7();
        let address: SocketAddrV4 = "10.201.0.3:8443".parse().unwrap();
        peers.set(
            id,
            Peer {
                address,
                pin: CertPin::of(b"c"),
            },
        );
        let resolver = PeerResolver(peers.clone());
        let name: Name = id.to_string().parse().unwrap();
        let found: Vec<_> = resolver
            .clone()
            .oneshot(name.clone())
            .await
            .unwrap()
            .collect();
        assert_eq!(found, vec![SocketAddr::V4(address)]);
        for other in ["router", "localhost", &Uuid::now_v7().to_string()] {
            let n: Name = other.parse().unwrap();
            assert!(resolver.clone().oneshot(n).await.is_err(), "{other}");
        }
        peers.remove(id);
        assert!(resolver.oneshot(name).await.is_err());
    }
}
