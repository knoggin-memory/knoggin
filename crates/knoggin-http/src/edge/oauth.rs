// SPDX-License-Identifier: AGPL-3.0-only
//! Sign-in with one OpenID Connect identity provider (tab "Sign-in and credentials", OAuth
//! tokens and Web sign-in): discovery, the cached signing keys, the checks of access and
//! ID tokens, the code exchange, and the allow-list. Knoggin is a resource server: it
//! checks tokens and issues none.

use std::collections::HashSet;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use knoggin_core::Sensitive;
use knoggin_state::Clock;
use reqwest::Url;
use serde_json::Value;

/// The signing keys are fetched again this often.
const REFRESH_MS: i64 = 3_600_000;
/// An unknown key ID fetches the keys again at most this often.
const REFETCH_MS: i64 = 30_000;
/// Keys whose last good fetch is older than this are not used.
const STALE_MS: i64 = 24 * REFRESH_MS;
/// The largest answer read from the identity provider.
const MAX_BODY: usize = 64 * 1024;
/// Every call to the identity provider.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Leeway on `exp` and `nbf`, in seconds.
const LEEWAY: u64 = 60;

/// The `auth.*` settings of mode `oauth` (tab "Config, CLI, local install").
#[derive(Clone)]
pub struct OauthConfig {
    /// `auth.issuer`.
    pub issuer: String,
    /// `auth.audiences`, accepted beside `<public_url>/mcp`, `/mcp/plugin` and `<public_url>`.
    pub audiences: Vec<String>,
    /// `auth.provider_name`.
    pub provider_name: String,
    /// `auth.web_client_id`.
    pub web_client_id: String,
    /// `auth.web_client_secret`.
    pub web_client_secret: Sensitive<String>,
    /// `auth.admin_emails`.
    pub admin_emails: Vec<String>,
    /// `auth.admin_subjects`.
    pub admin_subjects: Vec<String>,
    /// `auth.allowed_emails`.
    pub allowed_emails: Vec<String>,
    /// `auth.allowed_email_domains`.
    pub allowed_email_domains: Vec<String>,
    /// `auth.email_claim`.
    pub email_claim: Option<String>,
    /// `auth.session_max_days`.
    pub session_max_days: u32,
    /// `auth.session_idle_hours`.
    pub session_idle_hours: u32,
}

/// Counts only: the lists hold people's emails and subjects.
impl std::fmt::Debug for OauthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OauthConfig")
            .field("admin_emails", &self.admin_emails.len())
            .field("admin_subjects", &self.admin_subjects.len())
            .field("allowed_emails", &self.allowed_emails.len())
            .field("allowed_email_domains", &self.allowed_email_domains.len())
            .finish_non_exhaustive()
    }
}

impl OauthConfig {
    /// Whether either allow-list is set.
    pub fn allow_list_set(&self) -> bool {
        !self.allowed_emails.is_empty() || !self.allowed_email_domains.is_empty()
    }

    /// Whether a verified `email` passes the allow-list (true when no list is set).
    pub fn allowed(&self, email: &str) -> bool {
        !self.allow_list_set()
            || email_matches(email, &self.allowed_emails, &self.allowed_email_domains)
    }
}

/// An address matches an equal listed address, or a listed domain equal to the text after
/// its last `@`. Both ignore the case of ASCII letters only, and an address with a
/// character outside ASCII matches nothing, so no look-alike can stand in for a letter.
pub fn email_matches(email: &str, emails: &[String], domains: &[String]) -> bool {
    if !email.is_ascii() {
        return false;
    }
    emails.iter().any(|e| e.eq_ignore_ascii_case(email))
        || email
            .rsplit_once('@')
            .is_some_and(|(_, d)| domains.iter().any(|x| x.eq_ignore_ascii_case(d)))
}

/// Why the provider's discovery document cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// Neither discovery address answered with a document; try again.
    #[error("the identity provider's discovery document could not be fetched")]
    Unreachable,
    /// The document names another issuer.
    #[error("the discovery document's issuer {found:?} is not auth.issuer {expected:?}")]
    IssuerMismatch {
        /// `auth.issuer`.
        expected: String,
        /// The document's `issuer`.
        found: String,
    },
    /// An address that must be `https` is not.
    #[error("{0} is not https")]
    Insecure(&'static str),
}

/// The endpoints that discovery named.
#[derive(Debug, Clone)]
pub(crate) struct Discovery {
    pub(crate) authorization_endpoint: Url,
    token_endpoint: Url,
    jwks_uri: Url,
    client_secret_post: bool,
}

/// Reads a discovery document: its `issuer` must be `issuer` exactly, and every address
/// must be `https` outside test mode.
pub(crate) fn parse_discovery(
    doc: &Value,
    issuer: &str,
    test_mode: bool,
) -> Result<Discovery, DiscoveryError> {
    let found = doc
        .get("issuer")
        .and_then(Value::as_str)
        .ok_or(DiscoveryError::Unreachable)?;
    if found != issuer {
        return Err(DiscoveryError::IssuerMismatch {
            expected: issuer.to_owned(),
            found: found.to_owned(),
        });
    }
    let secure = |name: &'static str, text: &str| -> Result<Url, DiscoveryError> {
        let url = Url::parse(text).map_err(|_| DiscoveryError::Unreachable)?;
        if url.scheme() != "https" && !(test_mode && url.scheme() == "http") {
            return Err(DiscoveryError::Insecure(name));
        }
        Ok(url)
    };
    secure("auth.issuer", issuer)?;
    let endpoint = |name: &'static str| {
        doc.get(name)
            .and_then(Value::as_str)
            .ok_or(DiscoveryError::Unreachable)
            .and_then(|t| secure(name, t))
    };
    Ok(Discovery {
        authorization_endpoint: endpoint("authorization_endpoint")?,
        token_endpoint: endpoint("token_endpoint")?,
        jwks_uri: endpoint("jwks_uri")?,
        client_secret_post: doc
            .get("token_endpoint_auth_methods_supported")
            .and_then(Value::as_array)
            .is_some_and(|m| m.iter().any(|v| v == "client_secret_post")),
    })
}

/// Why a token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A check failed; the kind is logged, never the token.
    Invalid(&'static str),
    /// The provider or its keys cannot be used now.
    Unavailable,
}

/// Who a token names.
pub(crate) struct Identity {
    pub(crate) subject: Sensitive<String>,
    pub(crate) email: Option<Sensitive<String>>,
    pub(crate) email_verified: bool,
    pub(crate) name: Option<Sensitive<String>>,
}

impl Identity {
    /// The email, when the provider said it is verified.
    pub(crate) fn verified_email(&self) -> Option<&str> {
        self.email
            .as_ref()
            .filter(|_| self.email_verified)
            .map(|e| e.expose().as_str())
    }
}

#[derive(Default)]
struct KeyCache {
    keys: Vec<Jwk>,
    ok_at: Option<i64>,
    tried_at: Option<i64>,
}

/// The provider as this process sees it: settings, discovery, keys.
pub(crate) struct Provider {
    pub(crate) config: OauthConfig,
    pub(crate) public_url: String,
    test_mode: bool,
    client: reqwest::Client,
    discovery: RwLock<Option<Arc<Discovery>>>,
    keys: tokio::sync::Mutex<KeyCache>,
    clock: Clock,
}

impl Provider {
    pub(crate) fn new(
        config: OauthConfig,
        public_url: String,
        test_mode: bool,
        clock: Clock,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: client(test_mode, !test_mode)?,
            config,
            public_url: public_url.trim_end_matches('/').to_owned(),
            test_mode,
            discovery: RwLock::new(None),
            keys: tokio::sync::Mutex::new(KeyCache::default()),
            clock,
        })
    }

    /// The endpoints, once discovery has worked.
    pub(crate) fn discovery(&self) -> Option<Arc<Discovery>> {
        self.discovery
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// One discovery attempt: the OpenID document, else the OAuth one.
    pub(crate) async fn discover(&self) -> Result<(), DiscoveryError> {
        let base = self.config.issuer.trim_end_matches('/');
        for path in [
            "/.well-known/openid-configuration",
            "/.well-known/oauth-authorization-server",
        ] {
            let Ok(url) = Url::parse(&format!("{base}{path}")) else {
                return Err(DiscoveryError::Unreachable);
            };
            if let Ok(doc) = get_json(&self.client, url).await {
                let d = parse_discovery(&doc, &self.config.issuer, self.test_mode)?;
                *self
                    .discovery
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(d));
                return Ok(());
            }
        }
        Err(DiscoveryError::Unreachable)
    }

    /// The cookie names and attributes for `public_url` (tab "Sign-in and credentials",
    /// Sessions): `__Host-` cookies over `https`, plain names on a test-mode loopback.
    pub(crate) fn cookies(&self) -> Cookies {
        if self.public_url.starts_with("https://") {
            Cookies {
                session: "__Host-session",
                login: "__Host-login",
                attributes: "; Secure; HttpOnly; SameSite=Lax; Path=/",
            }
        } else {
            Cookies {
                session: "knoggin_session",
                login: "knoggin_login",
                attributes: "; HttpOnly; SameSite=Lax; Path=/",
            }
        }
    }

    /// The key for `kid`, fetching the set when it is an hour old or does not hold the
    /// ID (at most once per 30 seconds). Keys older than 24 hours are not used.
    async fn decoding_key(&self, kid: &str, alg: Algorithm) -> Result<DecodingKey, Refusal> {
        let discovery = self.discovery().ok_or(Refusal::Unavailable)?;
        let now = (self.clock)();
        let mut cache = self.keys.lock().await;
        let may_fetch = |c: &KeyCache| c.tried_at.is_none_or(|t| now - t >= REFETCH_MS);
        if cache.ok_at.is_none_or(|t| now - t >= REFRESH_MS) && may_fetch(&cache) {
            self.refresh(&mut cache, &discovery, now).await;
        }
        let known = |c: &KeyCache| {
            c.keys
                .iter()
                .any(|k| k.common.key_id.as_deref() == Some(kid))
        };
        if !known(&cache) && may_fetch(&cache) {
            self.refresh(&mut cache, &discovery, now).await;
        }
        if cache.ok_at.is_none_or(|t| now - t > STALE_MS) {
            return Err(Refusal::Unavailable);
        }
        select_key(&cache.keys, kid, alg).map_err(Refusal::Invalid)
    }

    /// Replaces the whole set on success; keeps the old one on failure.
    async fn refresh(&self, cache: &mut KeyCache, d: &Discovery, now: i64) {
        cache.tried_at = Some(now);
        let list = match get_json(&self.client, d.jwks_uri.clone()).await {
            Ok(doc) => doc.get("keys").and_then(Value::as_array).cloned(),
            Err(()) => None,
        };
        let Some(list) = list else {
            tracing::warn!(
                kind = "jwks_fetch_failed",
                "the identity provider's signing keys could not be fetched"
            );
            return;
        };
        // One key at a time: a key the library does not know is skipped, not fatal.
        cache.keys = list
            .into_iter()
            .filter_map(|k| serde_json::from_value(k).ok())
            .collect();
        cache.ok_at = Some(now);
    }

    /// Checks a JWT's signature and its `exp`, `nbf`, `iss`, `aud` and `sub` (rules 1 to
    /// 5), with every field of the `Validation` set, and returns its claims.
    async fn verify(&self, token: &str, audiences: HashSet<String>) -> Result<Value, Refusal> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| Refusal::Invalid("header"))?;
        if !matches!(header.alg, Algorithm::RS256 | Algorithm::ES256) {
            return Err(Refusal::Invalid("alg"));
        }
        let kid = header.kid.ok_or(Refusal::Invalid("kid"))?;
        let key = self.decoding_key(&kid, header.alg).await?;
        let mut v = Validation::new(header.alg);
        v.algorithms = vec![header.alg];
        v.required_spec_claims = ["exp", "iss", "aud", "sub"].map(String::from).into();
        v.leeway = LEEWAY;
        v.reject_tokens_expiring_in_less_than = 0;
        v.validate_exp = true;
        v.validate_nbf = true;
        v.validate_aud = true;
        v.aud = Some(audiences);
        v.iss = Some(HashSet::from([self.config.issuer.clone()]));
        v.sub = None;
        let claims = jsonwebtoken::decode::<Value>(token, &key, &v)
            .map_err(|_| Refusal::Invalid("claims"))?
            .claims;
        // `iss` is the one string, not a list that holds it.
        if claims.get("iss").and_then(Value::as_str) != Some(self.config.issuer.as_str()) {
            return Err(Refusal::Invalid("iss"));
        }
        if !claims.get("sub").is_some_and(Value::is_string) {
            return Err(Refusal::Invalid("sub"));
        }
        Ok(claims)
    }

    /// An agent's access token (rules 1 to 7).
    pub(crate) async fn check_access(&self, token: &str) -> Result<Identity, Refusal> {
        let p = &self.public_url;
        let mut audiences: HashSet<String> =
            [format!("{p}/mcp"), format!("{p}/mcp/plugin"), p.clone()].into();
        audiences.extend(self.config.audiences.iter().cloned());
        let claims = self.verify(token, audiences).await?;
        // Rule 6: signs of an ID token from the web sign-in.
        let aud_has_client = match claims.get("aud") {
            Some(Value::String(a)) => *a == self.config.web_client_id,
            Some(Value::Array(list)) => list.iter().any(|a| *a == *self.config.web_client_id),
            _ => false,
        };
        if aud_has_client
            || ["nonce", "at_hash", "c_hash"]
                .iter()
                .any(|c| claims.get(c).is_some())
        {
            return Err(Refusal::Invalid("id_token"));
        }
        // Rule 7: AuthKit's own agents.
        if claims.get("sub_profile").and_then(Value::as_str) == Some("ai_agent") {
            return Err(Refusal::Invalid("ai_agent"));
        }
        Ok(identity(&claims, self.config.email_claim.as_deref()))
    }

    /// The ID token of a web sign-in: rules 1 to 4, `aud` the client ID, and the nonce.
    pub(crate) async fn check_id_token(
        &self,
        token: &str,
        nonce: &str,
    ) -> Result<Identity, Refusal> {
        let claims = self
            .verify(token, HashSet::from([self.config.web_client_id.clone()]))
            .await?;
        let sent = claims
            .get("nonce")
            .and_then(Value::as_str)
            .ok_or(Refusal::Invalid("nonce"))?;
        if !knoggin_crypto::token::eq(sent.as_bytes(), nonce.as_bytes()) {
            return Err(Refusal::Invalid("nonce"));
        }
        Ok(identity(&claims, None))
    }

    /// The code exchange of web sign-in; returns the ID token and discards the rest.
    pub(crate) async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<Sensitive<String>, ()> {
        let d = self.discovery().ok_or(())?;
        let id = self.config.web_client_id.as_str();
        let secret = self.config.web_client_secret.expose().as_str();
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ];
        let mut request = self
            .client
            .post(d.token_endpoint.clone())
            .header(http::header::ACCEPT, "application/json");
        if d.client_secret_post {
            form.extend([("client_id", id), ("client_secret", secret)]);
        } else {
            // RFC 6749, section 2.3.1: both are form-encoded before Basic.
            request = request.basic_auth(form_encode(id), Some(form_encode(secret)));
        }
        let mut response = request.form(&form).send().await.map_err(|_| ())?;
        if !response.status().is_success() {
            return Err(());
        }
        let body = capped(&mut response).await?;
        let doc: Value = serde_json::from_slice(&body).map_err(|_| ())?;
        doc.get("id_token")
            .and_then(Value::as_str)
            .map(|t| Sensitive::new(t.to_owned()))
            .ok_or(())
    }
}

/// The session and login cookies of one deployment.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cookies {
    pub(crate) session: &'static str,
    pub(crate) login: &'static str,
    pub(crate) attributes: &'static str,
}

/// The client for the provider: no redirects and `https` only, outside test mode.
fn client(follow_redirects: bool, https_only: bool) -> Result<reqwest::Client, reqwest::Error> {
    client_builder(follow_redirects, https_only).build()
}

fn client_builder(follow_redirects: bool, https_only: bool) -> reqwest::ClientBuilder {
    // No pooled connections: the calls are rare, and a pooled one could outlive the
    // provider's address.
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .pool_max_idle_per_host(0)
        .redirect(if follow_redirects {
            reqwest::redirect::Policy::limited(5)
        } else {
            reqwest::redirect::Policy::none()
        })
        .https_only(https_only)
}

async fn get_json(client: &reqwest::Client, url: Url) -> Result<Value, ()> {
    let mut response = client
        .get(url)
        .header(http::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    let body = capped(&mut response).await?;
    serde_json::from_slice(&body).map_err(|_| ())
}

/// The body, refused past 64 KB.
async fn capped(response: &mut reqwest::Response) -> Result<Vec<u8>, ()> {
    let mut out = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if out.len() + chunk.len() > MAX_BODY {
            return Err(());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// `application/x-www-form-urlencoded` of one value.
fn form_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._*".contains(&b) {
            out.push(char::from(b));
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The identity in checked claims. An email counts as verified when `email_verified` is
/// true, or when it comes from `email_claim`, which the deployer vouches for.
fn identity(claims: &Value, email_claim: Option<&str>) -> Identity {
    let text = |name: &str| {
        claims
            .get(name)
            .and_then(Value::as_str)
            .map(|s| Sensitive::new(s.to_owned()))
    };
    let mut email = text("email");
    let mut verified = claims.get("email_verified").and_then(Value::as_bool) == Some(true);
    if !(verified && email.is_some())
        && let Some(claimed) = email_claim.and_then(text)
    {
        email = Some(claimed);
        verified = true;
    }
    Identity {
        subject: text("sub").unwrap_or_else(|| Sensitive::new(String::new())),
        email_verified: verified && email.is_some(),
        email,
        name: text("name"),
    }
}

/// The key with ID `kid`, if it fits `alg` (rule 2): an RSA key for `RS256`, a P-256 key
/// for `ES256`; a stated `use` must be `sig` and a stated `alg` the token's.
pub(crate) fn select_key(
    keys: &[Jwk],
    kid: &str,
    alg: Algorithm,
) -> Result<DecodingKey, &'static str> {
    let jwk = keys
        .iter()
        .find(|k| k.common.key_id.as_deref() == Some(kid))
        .ok_or("unknown_key")?;
    if jwk
        .common
        .public_key_use
        .as_ref()
        .is_some_and(|u| *u != PublicKeyUse::Signature)
    {
        return Err("key_use");
    }
    if jwk
        .common
        .key_algorithm
        .is_some_and(|a| a != KeyAlgorithm::from(alg))
    {
        return Err("key_alg");
    }
    let fits = match (&jwk.algorithm, alg) {
        (AlgorithmParameters::RSA(_), Algorithm::RS256) => true,
        // The library does not check the curve.
        (AlgorithmParameters::EllipticCurve(p), Algorithm::ES256) => p.curve == EllipticCurve::P256,
        _ => false,
    };
    if !fits {
        return Err("key_type");
    }
    DecodingKey::from_jwk(jwk).map_err(|_| "key_unreadable")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const N: &str = "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw";

    fn rsa(extra: Value) -> Value {
        let mut k = json!({ "kty": "RSA", "kid": "r1", "n": N, "e": "AQAB" });
        k.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        k
    }

    fn ec(crv: &str) -> Value {
        json!({ "kty": "EC", "kid": "e1", "crv": crv,
            "x": "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
            "y": "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0" })
    }

    fn set(keys: &[Value]) -> Vec<Jwk> {
        keys.iter()
            .filter_map(|k| serde_json::from_value(k.clone()).ok())
            .collect()
    }

    #[test]
    fn a_key_must_fit_the_algorithm() {
        let keys = set(&[rsa(json!({})), ec("P-256")]);
        assert!(select_key(&keys, "r1", Algorithm::RS256).is_ok());
        assert!(select_key(&keys, "e1", Algorithm::ES256).is_ok());
        assert_eq!(
            select_key(&keys, "r1", Algorithm::ES256).err(),
            Some("key_type")
        );
        assert_eq!(
            select_key(&keys, "e1", Algorithm::RS256).err(),
            Some("key_type")
        );
        assert_eq!(
            select_key(&keys, "zz", Algorithm::RS256).err(),
            Some("unknown_key")
        );
    }

    #[test]
    fn a_key_for_encryption_another_curve_or_another_alg_is_refused() {
        let enc = set(&[rsa(json!({ "use": "enc" }))]);
        assert_eq!(
            select_key(&enc, "r1", Algorithm::RS256).err(),
            Some("key_use")
        );
        let sig = set(&[rsa(json!({ "use": "sig", "alg": "RS256" }))]);
        assert!(select_key(&sig, "r1", Algorithm::RS256).is_ok());
        let other_alg = set(&[rsa(json!({ "alg": "RS512" }))]);
        assert_eq!(
            select_key(&other_alg, "r1", Algorithm::RS256).err(),
            Some("key_alg")
        );
        let p384 = set(&[ec("P-384")]);
        assert_eq!(
            select_key(&p384, "e1", Algorithm::ES256).err(),
            Some("key_type")
        );
    }

    #[test]
    fn a_key_the_library_does_not_know_spoils_only_itself() {
        let keys = set(&[
            json!({ "kty": "EC", "kid": "odd", "crv": "P-999", "x": "a", "y": "b" }),
            json!({ "kty": "RSA", "kid": "bad", "n": 5 }),
            rsa(json!({})),
        ]);
        assert!(select_key(&keys, "r1", Algorithm::RS256).is_ok());
        assert!(select_key(&keys, "odd", Algorithm::ES256).is_err());
        assert!(select_key(&keys, "bad", Algorithm::RS256).is_err());
    }

    #[test]
    fn the_discovery_document_must_name_the_issuer_and_https_addresses() {
        let doc = |iss: &str, scheme: &str| {
            json!({ "issuer": iss,
                "authorization_endpoint": format!("{scheme}://idp.example/authorize"),
                "token_endpoint": format!("{scheme}://idp.example/token"),
                "jwks_uri": format!("{scheme}://idp.example/jwks"),
                "token_endpoint_auth_methods_supported": ["client_secret_post"] })
        };
        let issuer = "https://idp.example";
        let d = parse_discovery(&doc(issuer, "https"), issuer, false).unwrap();
        assert!(d.client_secret_post);
        assert!(matches!(
            parse_discovery(&doc("https://idp.example/", "https"), issuer, false),
            Err(DiscoveryError::IssuerMismatch { .. })
        ));
        assert!(matches!(
            parse_discovery(&doc(issuer, "http"), issuer, false),
            Err(DiscoveryError::Insecure("authorization_endpoint"))
        ));
        assert!(parse_discovery(&doc(issuer, "http"), issuer, true).is_ok());
        let http = "http://127.0.0.1:1";
        assert!(matches!(
            parse_discovery(&doc(http, "https"), http, false),
            Err(DiscoveryError::Insecure("auth.issuer"))
        ));
        let mut basic = doc(issuer, "https");
        basic["token_endpoint_auth_methods_supported"] = json!(["client_secret_basic"]);
        assert!(
            !parse_discovery(&basic, issuer, false)
                .unwrap()
                .client_secret_post
        );
    }

    fn oauth_config(issuer: &str) -> OauthConfig {
        OauthConfig {
            issuer: issuer.to_owned(),
            audiences: vec![],
            provider_name: "x".into(),
            web_client_id: "c".into(),
            web_client_secret: Sensitive::new("s".into()),
            admin_emails: vec![],
            admin_subjects: vec![],
            allowed_emails: vec![],
            allowed_email_domains: vec![],
            email_claim: None,
            session_max_days: 7,
            session_idle_hours: 24,
        }
    }

    /// A local stub over `https` on 127.0.0.1, with a certificate from a test CA that
    /// only these tests trust (`testdata/`, made once with `openssl`; the CA's key was
    /// not kept). `make` gets the stub's base address. Returns that address.
    async fn https_stub(make: impl FnOnce(&str) -> axum::Router) -> String {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let cert = CertificateDer::from_pem_slice(include_bytes!("testdata/stub-leaf.pem"));
        let key = PrivateKeyDer::from_pem_slice(include_bytes!("testdata/stub-leaf.key"));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.unwrap()], key.unwrap())
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("https://{}", listener.local_addr().unwrap());
        let app = make(&base);
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (acceptor, app) = (acceptor.clone(), app.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let _ = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection(
                        hyper_util::rt::TokioIo::new(tls),
                        hyper_util::service::TowerToHyperService::new(app),
                    )
                    .await;
                });
            }
        });
        base
    }

    /// A provider for `issuer` as a process runs it: outside test mode its client follows
    /// no redirect and takes only `https`; here it also trusts the stub's CA. In test
    /// mode (the control) it follows redirects.
    fn provider(issuer: &str, test_mode: bool, clock: Clock) -> Provider {
        let mut p = Provider::new(
            oauth_config(issuer),
            "https://memory.example".into(),
            test_mode,
            clock,
        )
        .unwrap();
        let ca = reqwest::Certificate::from_pem(include_bytes!("testdata/stub-ca.pem")).unwrap();
        p.client = client_builder(test_mode, !test_mode)
            .tls_certs_only([ca])
            .build()
            .unwrap();
        p
    }

    /// A discovery document at `issuer`, its token endpoint with `scheme`.
    fn document(issuer: &str, scheme: &str) -> Value {
        let token = format!("{issuer}/token").replacen("https", scheme, 1);
        json!({ "issuer": issuer, "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": token, "jwks_uri": format!("{issuer}/jwks") })
    }

    fn serve_json(v: Value) -> axum::routing::MethodRouter {
        axum::routing::get(move || {
            let v = v.clone();
            async move { axum::Json(v) }
        })
    }

    /// The discovery cases with `test_mode` false against a stub that serves crafted
    /// documents over `https`: another issuer, an `http` endpoint, and a redirect on the
    /// discovery fetch or on the key fetch are each refused. A sound document works.
    #[tokio::test]
    async fn discovery_outside_test_mode_against_a_stub() {
        let base = https_stub(|base| {
            let wk = "/.well-known/openid-configuration";
            let keys = json!({ "keys": [rsa(json!({}))] });
            axum::Router::new()
                .route(
                    &format!("/good{wk}"),
                    serve_json(document(&format!("{base}/good"), "https")),
                )
                .route("/good/jwks", serve_json(keys.clone()))
                .route(
                    &format!("/mismatch{wk}"),
                    serve_json(document(&format!("{base}/mismatch/"), "https")),
                )
                .route(
                    &format!("/plain{wk}"),
                    serve_json(document(&format!("{base}/plain"), "http")),
                )
                .route(
                    &format!("/there/moved{wk}"),
                    serve_json(document(&format!("{base}/moved"), "https")),
                )
                .route("/there/moved/jwks", serve_json(keys.clone()))
                .route(
                    &format!("/keys-moved{wk}"),
                    serve_json(document(&format!("{base}/keys-moved"), "https")),
                )
                .route("/there/keys-moved/jwks", serve_json(keys))
                .fallback(|uri: http::Uri| async move {
                    axum::response::Redirect::temporary(&format!("/there{}", uri.path()))
                })
        })
        .await;
        let clock: Clock = Arc::new(|| 0);
        let off = |path: &str| provider(&format!("{base}{path}"), false, clock.clone());
        let good = off("/good");
        good.discover().await.unwrap();
        assert!(good.decoding_key("r1", Algorithm::RS256).await.is_ok());
        match off("/mismatch").discover().await {
            Err(DiscoveryError::IssuerMismatch { expected, found }) => {
                assert_eq!(found, format!("{expected}/"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            off("/plain").discover().await,
            Err(DiscoveryError::Insecure("token_endpoint"))
        ));
        // A redirect on the discovery fetch is not followed.
        assert!(matches!(
            off("/moved").discover().await,
            Err(DiscoveryError::Unreachable)
        ));
        // Nor on the key fetch: no key, so no token is accepted.
        let keys_moved = off("/keys-moved");
        keys_moved.discover().await.unwrap();
        assert_eq!(
            keys_moved.decoding_key("r1", Algorithm::RS256).await.err(),
            Some(Refusal::Unavailable)
        );
        // The control: in test mode the same redirects are followed.
        let on = |path: &str| provider(&format!("{base}{path}"), true, clock.clone());
        on("/moved").discover().await.unwrap();
        let keys_moved = on("/keys-moved");
        keys_moved.discover().await.unwrap();
        assert!(
            keys_moved
                .decoding_key("r1", Algorithm::RS256)
                .await
                .is_ok()
        );
    }

    #[test]
    fn allow_list_matching() {
        let emails = vec!["Maya@Example.com".to_owned()];
        let domains = vec!["corp.example".to_owned()];
        assert!(email_matches("maya@example.COM", &emails, &domains));
        assert!(email_matches("x@CORP.example", &emails, &domains));
        assert!(!email_matches("x@sub.corp.example", &emails, &domains));
        assert!(!email_matches("other@example.com", &emails, &domains));
        // A Cyrillic "а" in place of the Latin "a" matches nothing.
        assert!(!email_matches("m\u{0430}ya@example.com", &emails, &domains));
        assert!(!email_matches("x@corp.ex\u{0430}mple", &emails, &domains));
        assert_eq!(form_encode("a b+c/é"), "a+b%2Bc%2F%C3%A9");
    }
}
