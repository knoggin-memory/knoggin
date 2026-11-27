// SPDX-License-Identifier: AGPL-3.0-only
//! The secrets of the internal link and their exact bytes:
//! `internal.secret`, a container's launch secret, the router and container tokens, the
//! `hello` proof, nonces and certificate pins.

use std::fmt;
use std::str::FromStr;

use aws_lc_rs::constant_time::verify_slices_are_equal;
use aws_lc_rs::digest::{SHA256, digest};
use aws_lc_rs::hmac;
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use super::LinkError;
use crate::hex;

const LAUNCH_LABEL: &[u8] = b"knoggin-launch-v1";
const ROUTER_TOKEN_LABEL: &[u8] = b"knoggin-router-token-v1";
const CONTAINER_TOKEN_LABEL: &[u8] = b"knoggin-container-token-v1";
const HELLO_LABEL: &[u8] = b"knoggin-hello-v1";

/// `internal.secret`: 32 random bytes shared by all routers. Every launch secret derives
/// from it.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct InternalSecret([u8; 32]);

impl InternalSecret {
    /// Wraps 32 bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Reads the config form: standard base64 with padding of exactly 32 bytes. Surrounding
    /// whitespace (a file's trailing newline) is ignored.
    pub fn from_base64(text: &str) -> Result<Self, LinkError> {
        let bytes = Zeroizing::new(
            STANDARD
                .decode(text.trim())
                .map_err(|_| LinkError::BadSecret("internal.secret"))?,
        );
        let array: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| LinkError::BadSecret("internal.secret"))?;
        Ok(Self(array))
    }

    /// The launch secret of one container: HMAC-SHA-256 under this secret over
    /// `knoggin-launch-v1` and the container ID as its lower-case hyphenated string.
    pub fn launch_secret(&self, container_id: Uuid) -> LaunchSecret {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.0);
        let mut ctx = hmac::Context::with_key(&key);
        ctx.update(LAUNCH_LABEL);
        ctx.update(container_id.hyphenated().to_string().as_bytes());
        LaunchSecret(tag32(ctx.sign()))
    }
}

impl fmt::Debug for InternalSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InternalSecret([redacted])")
    }
}

impl<'de> Deserialize<'de> for InternalSecret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = Zeroizing::new(String::deserialize(d)?);
        Self::from_base64(&text).map_err(serde::de::Error::custom)
    }
}

/// A container's launch secret: 32 bytes that any router can recompute and nothing
/// stores. The container gets it as hex in `KNOGGIN_LAUNCH_SECRET`.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct LaunchSecret([u8; 32]);

impl LaunchSecret {
    /// Reads the environment form: 64 hex characters.
    pub fn from_hex(text: &str) -> Result<Self, LinkError> {
        let bytes = hex::decode::<32>(text.trim()).ok_or(LinkError::BadSecret("launch secret"))?;
        Ok(Self(*bytes))
    }

    /// The environment form: lower-case hex.
    pub fn to_hex(&self) -> Zeroizing<String> {
        hex::encode(&self.0)
    }

    /// The token a router sends on every call to this container except `hello`.
    pub fn router_token(&self) -> Token {
        Token(hex::encode(&self.mac(&[ROUTER_TOKEN_LABEL])[..]))
    }

    /// The token this container sends on every call to a router.
    pub fn container_token(&self) -> Token {
        Token(hex::encode(&self.mac(&[CONTAINER_TOKEN_LABEL])[..]))
    }

    /// The `hello` proof: lower-case hex of HMAC-SHA-256 under this secret over
    /// `knoggin-hello-v1`, the 32 nonce bytes and the SHA-256 of the certificate's DER.
    pub fn hello_proof(&self, nonce: &Nonce, cert_der: &[u8]) -> Token {
        let pin = CertPin::of(cert_der);
        Token(hex::encode(&self.mac(&[HELLO_LABEL, &nonce.0, &pin.0])[..]))
    }

    /// Whether `proof` is the proof for this nonce and the certificate of this very
    /// connection. Constant time.
    pub fn check_proof(&self, nonce: &Nonce, cert_der: &[u8], proof: &str) -> bool {
        self.hello_proof(nonce, cert_der).matches(proof)
    }

    fn mac(&self, parts: &[&[u8]]) -> Zeroizing<[u8; 32]> {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.0);
        let mut ctx = hmac::Context::with_key(&key);
        for part in parts {
            ctx.update(part);
        }
        Zeroizing::new(tag32(ctx.sign()))
    }
}

impl fmt::Debug for LaunchSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LaunchSecret([redacted])")
    }
}

fn tag32(tag: hmac::Tag) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

/// A token or a proof in its lower-case hex form. Compared in constant time only.
#[derive(Clone)]
pub struct Token(Zeroizing<String>);

impl Token {
    /// The hex text, to put in a header or a body.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison with what a peer presented.
    pub fn matches(&self, presented: &str) -> bool {
        verify_slices_are_equal(self.0.as_bytes(), presented.as_bytes()).is_ok()
    }

    /// The `Authorization` value `Bearer <token>`, marked sensitive.
    pub fn bearer(&self) -> http::HeaderValue {
        let text = Zeroizing::new(format!("Bearer {}", self.0.as_str()));
        #[expect(
            clippy::expect_used,
            reason = "hex digits and one space are always a valid header value"
        )]
        let mut value = http::HeaderValue::from_str(&text).expect("hex is a valid header value");
        value.set_sensitive(true);
        value
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token([redacted])")
    }
}

/// The 32 random bytes of one `hello` call; sent as 64 hex characters.
#[derive(Clone, PartialEq, Eq)]
pub struct Nonce([u8; 32]);

impl Nonce {
    /// A new random nonce.
    pub fn random() -> Result<Self, LinkError> {
        let mut bytes = [0u8; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| LinkError::Random)?;
        Ok(Self(bytes))
    }

    /// The query form: 64 lower-case hex characters.
    pub fn to_hex(&self) -> String {
        hex::encode(&self.0).to_string()
    }

    /// Reads the query form; anything but 64 hex characters is `None`.
    pub fn from_hex(text: &str) -> Option<Self> {
        hex::decode::<32>(text).map(|b| Self(*b))
    }
}

impl fmt::Debug for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Nonce(..)")
    }
}

/// The pin of a certificate: the SHA-256 of its DER bytes, written as lower-case hex.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CertPin([u8; 32]);

impl CertPin {
    /// The pin of these DER bytes.
    pub fn of(cert_der: &[u8]) -> Self {
        let mut out = [0u8; 32];
        out.copy_from_slice(digest(&SHA256, cert_der).as_ref());
        Self(out)
    }

    /// Whether `cert_der` is the pinned certificate. Constant time.
    pub fn matches(&self, cert_der: &[u8]) -> bool {
        verify_slices_are_equal(&Self::of(cert_der).0, &self.0).is_ok()
    }

    /// The hex form, as stored in the lease and passed in `KNOGGIN_ROUTER_CERT_SHA256`.
    pub fn to_hex(&self) -> String {
        hex::encode(&self.0).to_string()
    }
}

impl FromStr for CertPin {
    type Err = LinkError;

    /// 64 hex characters; upper case is accepted and written back in lower case.
    fn from_str(text: &str) -> Result<Self, LinkError> {
        hex::decode::<32>(text.trim())
            .map(|b| Self(*b))
            .ok_or(LinkError::BadPin)
    }
}

impl fmt::Display for CertPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for CertPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CertPin({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> InternalSecret {
        InternalSecret::from_bytes([7u8; 32])
    }

    fn id() -> Uuid {
        Uuid::parse_str("0190a5c4-1111-7222-8333-444455556666").unwrap()
    }

    // The exact bytes, computed independently: HMAC-SHA-256 written out with SHA-256 alone.
    fn reference_hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
        let mut k = [0u8; 64];
        k[..key.len()].copy_from_slice(key);
        let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
        let mut inner = pad(0x36);
        inner.extend_from_slice(msg);
        let mut outer = pad(0x5c);
        outer.extend_from_slice(digest(&SHA256, &inner).as_ref());
        digest(&SHA256, &outer).as_ref().try_into().unwrap()
    }

    #[test]
    fn launch_secret_and_tokens_have_the_specified_bytes() {
        let launch = secret().launch_secret(id());
        let expected = reference_hmac(
            &[7u8; 32],
            b"knoggin-launch-v10190a5c4-1111-7222-8333-444455556666",
        );
        assert_eq!(launch.0, expected);
        assert_eq!(
            launch.router_token().expose(),
            hex::encode(&reference_hmac(&expected, b"knoggin-router-token-v1")).as_str()
        );
        assert_eq!(
            launch.container_token().expose(),
            hex::encode(&reference_hmac(&expected, b"knoggin-container-token-v1")).as_str()
        );
        assert_ne!(
            launch.router_token().expose(),
            launch.container_token().expose()
        );
        // Another container gets other values.
        let other = secret().launch_secret(Uuid::now_v7());
        assert!(!other.router_token().matches(launch.router_token().expose()));
    }

    #[test]
    fn launch_secret_hex_round_trip() {
        let launch = secret().launch_secret(id());
        let hex = launch.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_eq!(LaunchSecret::from_hex(&hex).unwrap().0, launch.0);
        assert!(LaunchSecret::from_hex("abc").is_err());
        assert!(!format!("{launch:?}").contains(&hex[..8]));
    }

    #[test]
    fn hello_proof_binds_nonce_and_certificate() {
        let launch = secret().launch_secret(id());
        let nonce = Nonce::from_hex(&"ab".repeat(32)).unwrap();
        let cert = b"certificate der bytes";
        let mut msg = b"knoggin-hello-v1".to_vec();
        msg.extend_from_slice(&[0xab; 32]);
        msg.extend_from_slice(digest(&SHA256, cert).as_ref());
        let proof = launch.hello_proof(&nonce, cert);
        assert_eq!(
            proof.expose(),
            hex::encode(&reference_hmac(&launch.0, &msg)).as_str()
        );
        assert!(launch.check_proof(&nonce, cert, proof.expose()));
        assert!(!launch.check_proof(&nonce, b"another certificate", proof.expose()));
        let other = Nonce::random().unwrap();
        assert!(!launch.check_proof(&other, cert, proof.expose()));
        assert!(!launch.check_proof(&nonce, cert, &proof.expose().to_uppercase()));
        assert!(!launch.check_proof(&nonce, cert, ""));
    }

    #[test]
    fn internal_secret_reads_base64_of_32_bytes() {
        let text = STANDARD.encode([9u8; 32]);
        let s = InternalSecret::from_base64(&format!("{text}\n")).unwrap();
        assert_eq!(s.0, [9u8; 32]);
        assert!(InternalSecret::from_base64(&STANDARD.encode([9u8; 31])).is_err());
        assert!(InternalSecret::from_base64("not base64!").is_err());
        let s: InternalSecret = serde_json::from_str(&format!("\"{text}\"")).unwrap();
        assert_eq!(format!("{s:?}"), "InternalSecret([redacted])");
    }

    #[test]
    fn pins() {
        let pin = CertPin::of(b"der");
        let text = pin.to_hex();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<CertPin>().unwrap(), pin);
        assert_eq!(text.to_uppercase().parse::<CertPin>().unwrap(), pin);
        assert!(pin.matches(b"der"));
        assert!(!pin.matches(b"de"));
        assert!("00".parse::<CertPin>().is_err());
    }

    #[test]
    fn bearer_is_sensitive() {
        let token = secret().launch_secret(id()).router_token();
        let value = token.bearer();
        assert!(value.is_sensitive());
        assert_eq!(
            value.to_str().unwrap(),
            format!("Bearer {}", token.expose())
        );
        assert_eq!(format!("{token:?}"), "Token([redacted])");
    }
}
