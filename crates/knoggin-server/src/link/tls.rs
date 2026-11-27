// SPDX-License-Identifier: AGPL-3.0-only
//! TLS configs of the internal link (tab "Server", pinning code): TLS 1.3 only, the
//! `aws_lc_rs` provider named explicitly, no session resumption, and verifiers that trust
//! a pin and never an authority.

use std::fmt;
use std::sync::Arc;

use rustls::client::Resumption;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::NoServerSessionStorage;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, ServerConfig, SignatureScheme,
};
use uuid::Uuid;

use super::{CertPin, LinkError, Peers, ServerIdentity};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// The config of a listener on the link: the container's and the router's internal one.
/// It offers HTTP/2 and HTTP/1.1 by ALPN and keeps no sessions to resume.
pub fn server_config(identity: &ServerIdentity) -> Result<Arc<ServerConfig>, LinkError> {
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| LinkError::Tls)?
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert()], identity.key())
        .map_err(|_| LinkError::Tls)?;
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// A container's config for its calls to the router: the routers' certificate pinned.
pub fn pinned_client_config(pin: CertPin) -> Result<ClientConfig, LinkError> {
    client_config(Arc::new(PinnedVerifier::new(Pins::One(pin))))
}

/// The router's config for calls to containers once their pin is stored: the pin is
/// looked up by the server name, which is the container's ID.
pub(super) fn peers_client_config(peers: Arc<Peers>) -> Result<ClientConfig, LinkError> {
    client_config(Arc::new(PinnedVerifier::new(Pins::Peers(peers))))
}

/// The config of `hello` alone: any end-entity certificate, a checked handshake
/// signature. The caller trusts nothing until the proof matches the certificate.
pub fn hello_client_config() -> Result<ClientConfig, LinkError> {
    client_config(Arc::new(HelloVerifier {
        algs: provider().signature_verification_algorithms,
    }))
}

fn client_config(verifier: Arc<dyn ServerCertVerifier>) -> Result<ClientConfig, LinkError> {
    let mut config = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| LinkError::Tls)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    // A session begun under the any-certificate verifier must never be resumed by a
    // pinned client, and the two configs share nothing.
    config.resumption = Resumption::disabled();
    Ok(config)
}

#[derive(Debug)]
enum Pins {
    One(CertPin),
    Peers(Arc<Peers>),
}

/// Compares the end-entity certificate with the pin in constant time, ignores names, and
/// leaves the handshake signature to the provider.
struct PinnedVerifier {
    pins: Pins,
    algs: WebPkiSupportedAlgorithms,
}

impl PinnedVerifier {
    fn new(pins: Pins) -> Self {
        Self {
            pins,
            algs: provider().signature_verification_algorithms,
        }
    }

    fn pin_for(&self, name: &ServerName<'_>) -> Option<CertPin> {
        match &self.pins {
            Pins::One(pin) => Some(*pin),
            Pins::Peers(peers) => {
                let ServerName::DnsName(dns) = name else {
                    return None;
                };
                let id = Uuid::parse_str(dns.as_ref()).ok()?;
                peers.get(id).map(|p| p.pin)
            }
        }
    }
}

impl fmt::Debug for PinnedVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedVerifier")
            .field("pins", &self.pins)
            .finish_non_exhaustive()
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        match self.pin_for(server_name) {
            Some(pin) if pin.matches(end_entity) => Ok(ServerCertVerified::assertion()),
            _ => Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Accepts any end-entity certificate, still checks that the peer holds its key.
#[derive(Debug)]
struct HelloVerifier {
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for HelloVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use rustls::HandshakeKind;
    use rustls::server::{ClientHello, ResolvesServerCert};
    use rustls::sign::CertifiedKey;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::*;
    use crate::link::Peer;
    use crate::link::cert::tests::other_key;

    /// Serves one byte per connection with `config`, forever.
    async fn serve(config: Arc<ServerConfig>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(config);
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut tls) = acceptor.accept(tcp).await {
                        let _ = tls.write_all(b"x").await;
                        let _ = tls.flush().await;
                        let mut buf = [0u8; 1];
                        let _ = tls.read(&mut buf).await;
                    }
                });
            }
        });
        addr
    }

    async fn connect(
        config: ClientConfig,
        addr: SocketAddr,
        name: &str,
    ) -> Result<HandshakeKind, std::io::Error> {
        let tcp = TcpStream::connect(addr).await?;
        let name = ServerName::try_from(name.to_owned()).unwrap();
        let mut tls = TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await?;
        let mut buf = [0u8; 1];
        tls.read_exact(&mut buf).await?;
        Ok(tls.get_ref().1.handshake_kind().unwrap())
    }

    /// A server that shows `identity`'s certificate but signs with another key.
    #[derive(Debug)]
    struct Impostor(Arc<CertifiedKey>);

    impl ResolvesServerCert for Impostor {
        fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
    }

    fn impostor_config(identity: &ServerIdentity) -> Arc<ServerConfig> {
        let key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&other_key()).unwrap();
        let certified = Arc::new(CertifiedKey::new(vec![identity.cert()], key));
        let config = ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Impostor(certified)));
        Arc::new(config)
    }

    #[tokio::test]
    async fn the_pin_decides_and_names_do_not() {
        let id = Uuid::now_v7();
        let identity = ServerIdentity::for_container(id).unwrap();
        let addr = serve(server_config(&identity).unwrap()).await;
        let pinned = || pinned_client_config(identity.pin()).unwrap();
        connect(pinned(), addr, "any-name.example").await.unwrap();
        let other = ServerIdentity::for_container(id).unwrap();
        assert!(
            connect(pinned_client_config(other.pin()).unwrap(), addr, "x")
                .await
                .is_err()
        );
        // By name: the container ID selects the pin.
        let peers = Arc::new(Peers::default());
        let v4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, addr.port());
        let by_name = || peers_client_config(peers.clone()).unwrap();
        assert!(connect(by_name(), addr, &id.to_string()).await.is_err());
        peers.set(
            id,
            Peer {
                address: v4,
                pin: identity.pin(),
            },
        );
        connect(by_name(), addr, &id.to_string()).await.unwrap();
        let stranger = Uuid::now_v7().to_string();
        assert!(connect(by_name(), addr, &stranger).await.is_err());
        peers.set(
            id,
            Peer {
                address: v4,
                pin: other.pin(),
            },
        );
        assert!(connect(by_name(), addr, &id.to_string()).await.is_err());
    }

    #[tokio::test]
    async fn a_certificate_without_its_key_is_refused_by_every_verifier() {
        let id = Uuid::now_v7();
        let identity = ServerIdentity::for_container(id).unwrap();
        let addr = serve(impostor_config(&identity)).await;
        let peers = Arc::new(Peers::default());
        peers.set(
            id,
            Peer {
                address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, addr.port()),
                pin: identity.pin(),
            },
        );
        let name = id.to_string();
        for config in [
            pinned_client_config(identity.pin()).unwrap(),
            peers_client_config(peers).unwrap(),
            hello_client_config().unwrap(),
        ] {
            assert!(connect(config, addr, &name).await.is_err());
        }
        // The honest server passes the hello verifier with any certificate.
        let honest = serve(server_config(&identity).unwrap()).await;
        connect(hello_client_config().unwrap(), honest, &name)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn no_client_config_resumes_a_session() {
        let identity = ServerIdentity::for_container(Uuid::now_v7()).unwrap();
        // A server that would issue tickets and keep sessions, to tempt the client.
        let mut tempting = ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![identity.cert()], identity.key())
            .unwrap();
        tempting.send_tls13_tickets = 4;
        let addr = serve(Arc::new(tempting)).await;
        let pinned = Arc::new(pinned_client_config(identity.pin()).unwrap());
        let hello = Arc::new(hello_client_config().unwrap());
        for config in [pinned, hello] {
            for _ in 0..3 {
                let kind = connect((*config).clone(), addr, "n").await.unwrap();
                assert_eq!(kind, HandshakeKind::Full);
            }
        }
        // The control: the same client with resumption on does resume, so the loop above
        // would have seen it.
        let mut resuming = pinned_client_config(identity.pin()).unwrap();
        resuming.resumption = Resumption::default();
        let resuming = Arc::new(resuming);
        connect((*resuming).clone(), addr, "n").await.unwrap();
        let kind = connect((*resuming).clone(), addr, "n").await.unwrap();
        assert_eq!(kind, HandshakeKind::Resumed);
        // And our own servers issue none.
        let ours = server_config(&identity).unwrap();
        assert_eq!(ours.send_tls13_tickets, 0);
    }
}
