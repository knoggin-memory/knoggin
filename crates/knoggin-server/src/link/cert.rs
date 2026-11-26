// SPDX-License-Identifier: AGPL-3.0-only
//! The certificates of the internal link. A container makes its own at start and never
//! writes it to disk; the routers share one, `internal.cert` with `internal.key`. Both are
//! self-signed with an ECDSA P-256 key: nothing on the link trusts an authority, the pin
//! decides.

use std::fmt;

use knoggin_core::Timestamp;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{CertPin, LinkError};

/// The one name of the routers' shared certificate.
pub const INTERNAL_CERT_NAME: &str = "knoggin-internal";

/// How long `knoggin admin internal-cert` makes the routers' certificate valid.
const INTERNAL_CERT_YEARS: i32 = 10;

/// A certificate with its private key, as a TLS listener serves it.
pub struct ServerIdentity {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    pin: CertPin,
}

impl ServerIdentity {
    /// A container's own certificate: a new P-256 key and one subject alternative name,
    /// the container ID as a DNS name. Kept in memory only.
    pub fn for_container(container_id: Uuid) -> Result<Self, LinkError> {
        let params = CertificateParams::new(vec![container_id.hyphenated().to_string()])
            .map_err(|_| LinkError::Certificate)?;
        let (cert, key) = self_signed(params)?;
        Ok(Self::new(cert, key))
    }

    /// The routers' identity from `internal.cert` and `internal.key` (PEM text).
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self, LinkError> {
        let cert = CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .map_err(|_| LinkError::Certificate)?;
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
            .map_err(|_| LinkError::Certificate)?;
        Ok(Self::new(cert, key))
    }

    fn new(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> Self {
        let pin = CertPin::of(&cert);
        Self { cert, key, pin }
    }

    /// The pin of this certificate.
    pub fn pin(&self) -> CertPin {
        self.pin
    }

    /// The certificate's DER bytes.
    pub fn cert_der(&self) -> &[u8] {
        &self.cert
    }

    pub(super) fn cert(&self) -> CertificateDer<'static> {
        self.cert.clone()
    }

    pub(super) fn key(&self) -> PrivateKeyDer<'static> {
        self.key.clone_key()
    }
}

impl fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("pin", &self.pin)
            .finish_non_exhaustive()
    }
}

/// A new pair for the routers (`knoggin admin internal-cert`): an ECDSA P-256 key and a
/// self-signed certificate named `knoggin-internal`, valid for 10 years from `now`.
/// Returns the certificate's PEM and the key's PEM.
pub fn new_internal_cert(now: Timestamp) -> Result<(String, Zeroizing<String>), LinkError> {
    let start = OffsetDateTime::from_unix_timestamp(now.millis().div_euclid(1000))
        .map_err(|_| LinkError::Certificate)?;
    let end = start
        .replace_year(start.year() + INTERNAL_CERT_YEARS)
        .map_err(|_| LinkError::Certificate)?;
    let mut params = CertificateParams::new(vec![INTERNAL_CERT_NAME.to_owned()])
        .map_err(|_| LinkError::Certificate)?;
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, INTERNAL_CERT_NAME);
    params.distinguished_name = name;
    params.not_before = start;
    params.not_after = end;
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|_| LinkError::Certificate)?;
    let cert = params
        .self_signed(&key)
        .map_err(|_| LinkError::Certificate)?;
    Ok((cert.pem(), Zeroizing::new(key.serialize_pem())))
}

fn self_signed(
    params: CertificateParams,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), LinkError> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|_| LinkError::Certificate)?;
    let cert = params
        .self_signed(&key)
        .map_err(|_| LinkError::Certificate)?;
    let der = Zeroizing::new(key.serialize_der());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der.to_vec()));
    Ok((cert.der().clone(), key))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A second key in front of a certificate it did not sign: for the pinning tests.
    pub(crate) fn other_key() -> PrivateKeyDer<'static> {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()))
    }

    #[test]
    fn container_certificates_are_new_each_time_and_name_the_container() {
        let id = Uuid::now_v7();
        let a = ServerIdentity::for_container(id).unwrap();
        let b = ServerIdentity::for_container(id).unwrap();
        assert_ne!(a.pin(), b.pin());
        assert_eq!(a.pin(), CertPin::of(a.cert_der()));
        let der = a.cert_der();
        let name = id.hyphenated().to_string();
        assert!(der.windows(name.len()).any(|w| w == name.as_bytes()));
        assert!(format!("{a:?}").contains(&a.pin().to_hex()));
    }

    #[test]
    fn internal_pair_reads_back() {
        let now = Timestamp::from_millis(1_795_000_000_000);
        let (cert, key) = new_internal_cert(now).unwrap();
        assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(key.contains("PRIVATE KEY"));
        let id = ServerIdentity::from_pem(&cert, &key).unwrap();
        assert!(
            id.cert_der()
                .windows(INTERNAL_CERT_NAME.len())
                .any(|w| w == INTERNAL_CERT_NAME.as_bytes())
        );
        assert!(ServerIdentity::from_pem("junk", &key).is_err());
        assert!(ServerIdentity::from_pem(&cert, "junk").is_err());
    }
}
