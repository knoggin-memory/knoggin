// SPDX-License-Identifier: AGPL-3.0-only
//! The test issuer (tab "Sign-in and credentials", The test issuer): a small OpenID
//! Connect provider for tests, in test mode only. It publishes a discovery document and
//! one RSA and one P-256 key, approves every sign-in at once, exchanges codes, and mints
//! tokens with any claims a test wants. Never use it for real users.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der};
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use http::{HeaderMap, StatusCode, header};
use jsonwebtoken::jwk::{Jwk, PublicKeyUse};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use knoggin_crypto::token::random_token;
use reqwest::Url;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// The one client secret the token endpoint takes.
pub const TEST_SECRET: &str = "test-secret";

/// Tokens live this many seconds.
const LIFETIME: u64 = 300;

struct SigningKey {
    kid: String,
    alg: Algorithm,
    encoding: EncodingKey,
    jwk: Jwk,
    published: bool,
}

struct Grant {
    subject: String,
    nonce: Option<String>,
    challenge: String,
    client_id: String,
    redirect_uri: String,
}

struct Shared {
    url: String,
    keys: Mutex<Vec<SigningKey>>,
    codes: Mutex<HashMap<String, Grant>>,
    /// Every secret the issuer saw or made, for the log-marker test.
    seen: Mutex<Vec<String>>,
}

/// A running test issuer. Dropping it stops it.
pub struct TestIssuer {
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for TestIssuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestIssuer")
            .field("url", &self.shared.url)
            .finish_non_exhaustive()
    }
}

impl Drop for TestIssuer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_kid() -> String {
    random_token().unwrap()[..16].to_owned()
}

fn rsa_key() -> SigningKey {
    let pair = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
    let der: Pkcs8V1Der<'static> = pair.as_der().unwrap();
    // `jsonwebtoken` reads a PKCS#8 RSA key only as PEM.
    let body = STANDARD.encode(der.as_ref());
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        lines.join("\n")
    );
    signing_key(
        EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
        Algorithm::RS256,
    )
}

fn ec_key() -> SigningKey {
    let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap();
    let der = pair.to_pkcs8v1().unwrap();
    signing_key(EncodingKey::from_ec_der(der.as_ref()), Algorithm::ES256)
}

fn signing_key(encoding: EncodingKey, alg: Algorithm) -> SigningKey {
    let kid = new_kid();
    let mut jwk = Jwk::from_encoding_key(&encoding, alg).unwrap();
    jwk.common.key_id = Some(kid.clone());
    jwk.common.public_key_use = Some(PublicKeyUse::Signature);
    SigningKey {
        kid,
        alg,
        encoding,
        jwk,
        published: true,
    }
}

impl Shared {
    fn sign(&self, alg: Algorithm, kid: Option<&str>, claims: &Value) -> String {
        let keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        let key = keys
            .iter()
            .rev()
            .find(|k| k.alg == alg && kid.is_none_or(|id| id == k.kid))
            .expect("no such signing key");
        let mut h = Header::new(alg);
        h.kid = Some(key.kid.clone());
        let token = jsonwebtoken::encode(&h, claims, &key.encoding).unwrap();
        self.remember(&token);
        token
    }

    fn remember(&self, secret: &str) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(secret.to_owned());
    }

    /// The claims of a person `sub`, as the issuer writes them.
    fn person(&self, sub: &str, aud: &str) -> Value {
        let t = now();
        json!({ "iss": self.url, "sub": sub, "aud": aud, "iat": t, "exp": t + LIFETIME,
            "email": format!("{sub}@test.example"),
            "email_verified": !sub.ends_with("-unverified"), "name": sub })
    }
}

impl TestIssuer {
    /// Starts an issuer on `127.0.0.1:0`. Refused outside test mode.
    pub async fn start(test_mode: bool) -> Result<Self, &'static str> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "could not bind 127.0.0.1:0")?;
        Self::serve(listener, None, test_mode)
    }

    /// Serves on `listener`, as `issuer_url` (default: `http://` and the listener's
    /// address). Refused outside test mode.
    pub fn serve(
        listener: TcpListener,
        issuer_url: Option<String>,
        test_mode: bool,
    ) -> Result<Self, &'static str> {
        if !test_mode {
            return Err("the test issuer runs in test mode only");
        }
        tracing::warn!(
            kind = "test_issuer",
            "the test issuer is running: it approves every sign-in and must never be used for real users"
        );
        let addr = listener.local_addr().map_err(|_| "no local address")?;
        let url = issuer_url
            .unwrap_or_else(|| format!("http://{addr}"))
            .trim_end_matches('/')
            .to_owned();
        let shared = Arc::new(Shared {
            url,
            keys: Mutex::new(vec![rsa_key(), ec_key()]),
            codes: Mutex::new(HashMap::new()),
            seen: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/authorize", get(authorize))
            .route("/token", post(token))
            .route("/mint", post(mint_http))
            .with_state(shared.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { shared, task })
    }

    /// The issuer URL, `auth.issuer` for the tests.
    pub fn url(&self) -> &str {
        &self.shared.url
    }

    /// A token signed with the current RSA key, with exactly `claims`.
    pub fn mint(&self, claims: &Value) -> String {
        self.shared.sign(Algorithm::RS256, None, claims)
    }

    /// A token signed with the current key of `alg` (`RS256` or `ES256`), or the key
    /// `kid` when given.
    pub fn mint_with(&self, alg: Algorithm, kid: Option<&str>, claims: &Value) -> String {
        self.shared.sign(alg, kid, claims)
    }

    /// The claims of person `sub` for audience `aud`: `iss`, `sub`, `aud`, `iat`, `exp` in
    /// 5 minutes, `email` (`<sub>@test.example`, verified unless `sub` ends in
    /// `-unverified`) and `name`.
    pub fn claims(&self, sub: &str, aud: &str) -> Value {
        self.shared.person(sub, aud)
    }

    /// The key IDs published now.
    pub fn kids(&self) -> Vec<String> {
        let keys = self
            .shared
            .keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        keys.iter()
            .filter(|k| k.published)
            .map(|k| k.kid.clone())
            .collect()
    }

    /// Replaces both keys with new ones under new key IDs. The old keys are no longer
    /// published, but `mint_with` can still sign with them.
    pub fn rotate_keys(&self) {
        let (rsa, ec) = (rsa_key(), ec_key());
        let mut keys = self
            .shared
            .keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for k in keys.iter_mut() {
            k.published = false;
        }
        keys.extend([rsa, ec]);
    }

    /// Stops publishing the key `kid`.
    pub fn remove_key(&self, kid: &str) {
        let mut keys = self
            .shared
            .keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for k in keys.iter_mut().filter(|k| k.kid == kid) {
            k.published = false;
        }
    }

    /// Every code, PKCE verifier and challenge, nonce, state and token it saw or made.
    pub fn secrets(&self) -> Vec<String> {
        self.shared
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

async fn discovery(State(s): State<Arc<Shared>>) -> Json<Value> {
    let u = &s.url;
    Json(json!({
        "issuer": u,
        "authorization_endpoint": format!("{u}/authorize"),
        "token_endpoint": format!("{u}/token"),
        "jwks_uri": format!("{u}/jwks"),
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
    }))
}

async fn jwks(State(s): State<Arc<Shared>>) -> Json<Value> {
    let keys = s.keys.lock().unwrap_or_else(PoisonError::into_inner);
    let published: Vec<&Jwk> = keys
        .iter()
        .filter(|k| k.published)
        .map(|k| &k.jwk)
        .collect();
    Json(json!({ "keys": published }))
}

fn bad(what: &'static str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "invalid_request", "error_description": what })),
    )
        .into_response()
}

/// Approves at once: signs in `login_hint`, else `alice`.
async fn authorize(
    State(s): State<Arc<Shared>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let field = |k: &str| q.get(k).cloned();
    let (Some(client_id), Some(redirect_uri), Some(challenge)) = (
        field("client_id"),
        field("redirect_uri"),
        field("code_challenge"),
    ) else {
        return bad("client_id, redirect_uri and code_challenge are required");
    };
    if field("response_type").as_deref() != Some("code")
        || field("code_challenge_method").as_deref() != Some("S256")
    {
        return bad("response_type=code and code_challenge_method=S256 are required");
    }
    let Ok(mut back) = Url::parse(&redirect_uri) else {
        return bad("redirect_uri is not a URL");
    };
    let code = random_token().unwrap();
    for secret in [
        Some(&code),
        q.get("state"),
        q.get("nonce"),
        Some(&challenge),
    ]
    .into_iter()
    .flatten()
    {
        s.remember(secret);
    }
    s.codes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            code.clone(),
            Grant {
                subject: field("login_hint").unwrap_or_else(|| "alice".into()),
                nonce: field("nonce"),
                challenge,
                client_id,
                redirect_uri,
            },
        );
    {
        let mut pairs = back.query_pairs_mut();
        pairs.append_pair("code", &code);
        if let Some(state) = q.get("state") {
            pairs.append_pair("state", state);
        }
        pairs.append_pair("iss", &s.url);
    }
    Redirect::to(back.as_str()).into_response()
}

/// `application/x-www-form-urlencoded` decoding of one value.
fn form_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// The client ID and secret from `client_secret_basic`.
fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let raw = STANDARD.decode(value.strip_prefix("Basic ")?).ok()?;
    let (id, secret) = std::str::from_utf8(&raw).ok()?.split_once(':')?;
    Some((form_decode(id)?, form_decode(secret)?))
}

async fn token(
    State(s): State<Arc<Shared>>,
    headers: HeaderMap,
    Form(f): Form<HashMap<String, String>>,
) -> Response {
    let (client_id, secret) = match (basic(&headers), f.get("client_id"), f.get("client_secret")) {
        (Some(pair), _, _) => pair,
        (None, Some(id), Some(secret)) => (id.clone(), secret.clone()),
        _ => return bad("no client authentication"),
    };
    if secret != TEST_SECRET
        || f.get("grant_type").map(String::as_str) != Some("authorization_code")
    {
        return bad("wrong client secret or grant type");
    }
    let Some(code) = f.get("code") else {
        return bad("no code");
    };
    // A code works once.
    let Some(grant) = s
        .codes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(code)
    else {
        return bad("unknown or used code");
    };
    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
    s.remember(&verifier);
    let challenge = URL_SAFE_NO_PAD.encode(
        aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, verifier.as_bytes()).as_ref(),
    );
    if grant.client_id != client_id
        || f.get("redirect_uri") != Some(&grant.redirect_uri)
        || challenge != grant.challenge
    {
        return bad("the client, the redirect address or the verifier does not fit the code");
    }
    let mut id = s.person(&grant.subject, &client_id);
    if let Some(nonce) = &grant.nonce {
        id["nonce"] = json!(nonce);
    }
    let aud = f.get("resource").cloned().unwrap_or(client_id);
    let access = s.person(&grant.subject, &aud);
    Json(json!({
        "token_type": "Bearer",
        "expires_in": LIFETIME,
        "id_token": s.sign(Algorithm::RS256, None, &id),
        "access_token": s.sign(Algorithm::RS256, None, &access),
    }))
    .into_response()
}

async fn mint_http(State(s): State<Arc<Shared>>, Json(claims): Json<Value>) -> String {
    s.sign(Algorithm::RS256, None, &claims)
}
