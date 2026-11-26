// SPDX-License-Identifier: AGPL-3.0-only
//! Sign-in (tab "Sign-in and credentials", Tests that must exist): access tokens, their
//! reach, web sign-in and cookie sessions, users and admins, a local install with
//! sign-in on, and the browser checks, all in one process against the edge layer with the
//! test issuer. A server is the edge in the router's role on its public listener; the
//! router itself (milestone 6) is not in these tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

mod support;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{HeaderValue, Method, Request, StatusCode, header};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use knoggin_core::Sensitive;
use knoggin_core::time::MILLIS_PER_DAY;
use knoggin_http::edge::{
    Arrival, AuthMode, EdgeConfig, EdgeParts, EdgeState, OauthConfig, Role, edge_layer,
};
use knoggin_http::error::Code;
use knoggin_http::local::{LocalState, mode_routes};
use knoggin_http::store_layer;
use knoggin_http::testing::issuer::{TEST_SECRET, TestIssuer};
use serde_json::{Value, json};
use support::Resp;
use support::edge::{EdgeEnv, PORT, send};

const PUBLIC: &str = "https://memory.example.test";
const CLIENT: &str = "web-client";
const HOUR: i64 = 3_600_000;

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A token made by hand: any header, any claims, a junk signature.
fn forged(header: Value, claims: &Value) -> String {
    let part = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    format!("{}.{}.c2ln", part(&header), part(claims))
}

/// The edge with sign-in on, in one role, beside a test issuer.
struct S {
    e: EdgeEnv,
    issuer: TestIssuer,
    config: EdgeConfig,
    arrival: Arrival,
    app: Router,
}

impl S {
    async fn new(role: Role, f: impl FnOnce(&mut OauthConfig)) -> Self {
        Self::at(role, PUBLIC, f).await
    }

    async fn at(role: Role, public: &str, f: impl FnOnce(&mut OauthConfig)) -> Self {
        let issuer = TestIssuer::start(true).await.unwrap();
        assert!(TestIssuer::start(false).await.is_err(), "test mode only");
        let mut oauth = OauthConfig {
            issuer: issuer.url().to_owned(),
            audiences: vec!["urn:extra".into()],
            provider_name: "Test IdP".into(),
            web_client_id: CLIENT.into(),
            web_client_secret: Sensitive::new(TEST_SECRET.into()),
            admin_emails: vec![],
            admin_subjects: vec![],
            allowed_emails: match role {
                Role::Local => vec!["alice@test.example".into()],
                Role::Server => vec![],
            },
            allowed_email_domains: vec![],
            email_claim: None,
            session_max_days: 7,
            session_idle_hours: 24,
        };
        f(&mut oauth);
        let mut config = None;
        let e = EdgeEnv::with(|c| {
            c.role = role;
            c.auth_mode = AuthMode::Oauth;
            c.test_mode = true;
            c.public_url = Some(public.into());
            c.oauth = Some(oauth);
            config = Some(c.clone());
        })
        .await;
        e.edge.discover().await.unwrap();
        let app = e.app.clone();
        Self {
            e,
            issuer,
            config: config.unwrap(),
            arrival: match role {
                Role::Local => Arrival::Fixed { port: PORT },
                Role::Server => Arrival::Public,
            },
            app,
        }
    }

    /// The same process restarted with other `auth.*` settings over the same records.
    async fn reconfigure(&mut self, f: impl FnOnce(&mut OauthConfig)) {
        let mut config = self.config.clone();
        f(config.oauth.as_mut().unwrap());
        let e = &self.e;
        let edge = EdgeState::new(
            config.clone(),
            EdgeParts {
                db: e.db.clone(),
                field_keys: e.fields.clone(),
                keys: e.keys.clone(),
                clock: e.clock.clock(),
                host: Some(e.host.clone()),
                local: Some(e.local.clone()),
            },
        )
        .await
        .unwrap();
        edge.discover().await.unwrap();
        let modes = mode_routes(LocalState {
            host: e.host.clone(),
            stores: e.stores.clone(),
            sessions: e.local.clone(),
            data_dir: e.dir.path().join("data"),
            key_provider: "key_file",
            version: "0.1.0".into(),
            clock: e.clock.clock(),
        });
        self.app = edge_layer(edge, store_layer(e.host.clone()), modes);
        self.config = config;
    }

    fn request(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            b = b.header("host", self.public().split_once("://").unwrap().1);
        }
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let mut r = b
            .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
            .unwrap();
        r.extensions_mut().insert(self.arrival.clone());
        r
    }

    async fn call(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Resp {
        send(&self.app, self.request(method, uri, body, headers)).await
    }

    async fn token(&self, token: &str, method: Method, uri: &str, body: Option<Value>) -> Resp {
        let auth = format!("Bearer {token}");
        self.call(method, uri, body, &[("authorization", &auth)])
            .await
    }

    /// A browser request with the session cookie and the UI header.
    async fn cookie(&self, value: &str, method: Method, uri: &str, body: Option<Value>) -> Resp {
        let cookie = format!("{}={value}", self.cookie_name());
        self.call(
            method,
            uri,
            body,
            &[("cookie", &cookie), ("x-knoggin-ui", "1")],
        )
        .await
    }

    fn cookie_name(&self) -> &'static str {
        if self.public().starts_with("https") {
            "__Host-session"
        } else {
            "knoggin_session"
        }
    }

    fn public(&self) -> &str {
        self.config.public_url.as_deref().unwrap()
    }

    /// An access token for person `sub`, for this server's MCP address.
    fn access(&self, sub: &str) -> String {
        self.issuer
            .mint(&self.issuer.claims(sub, &format!("{}/mcp", self.public())))
    }

    /// Starts a sign-in as `hint` and lets the provider approve it: the login cookie and
    /// the callback's path and query.
    async fn begin(&self, hint: &str, return_to: &str) -> (String, String, Resp) {
        let r = self
            .call(
                Method::GET,
                &format!("/auth/login?return_to={return_to}&login_hint={hint}"),
                None,
                &[],
            )
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.json);
        let login = cookie_from(
            &r,
            if self.public().starts_with("https") {
                "__Host-login"
            } else {
                "knoggin_login"
            },
        )
        .unwrap();
        let to = location(&r);
        assert!(to.starts_with(&format!("{}/authorize?", self.issuer.url())));
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let back = client.get(&to).send().await.unwrap();
        assert_eq!(back.status(), StatusCode::SEE_OTHER);
        let callback = back.headers()[header::LOCATION].to_str().unwrap();
        let path = callback
            .strip_prefix(self.public())
            .expect("back to the callback")
            .to_owned();
        (login, path, r)
    }

    async fn finish(&self, login: &str, path: &str) -> Resp {
        let name = if self.public().starts_with("https") {
            "__Host-login"
        } else {
            "knoggin_login"
        };
        self.call(
            Method::GET,
            path,
            None,
            &[("cookie", &format!("{name}={login}"))],
        )
        .await
    }

    /// A whole web sign-in: the session cookie's value, or the error code.
    async fn sign_in(&self, hint: &str) -> Result<String, String> {
        let (login, path, _) = self.begin(hint, "/map").await;
        let r = self.finish(&login, &path).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.json);
        let to = location(&r);
        if let Some(code) = to.strip_prefix("/signin?error=") {
            return Err(code.to_owned());
        }
        assert_eq!(to, "/map");
        Ok(cookie_from(&r, self.cookie_name()).unwrap())
    }

    async fn user_count(&self) -> usize {
        self.e.users.list(200, None).await.unwrap().len()
    }
}

fn location(r: &Resp) -> String {
    r.headers[header::LOCATION].to_str().unwrap().to_owned()
}

fn set_cookies(r: &Resp) -> Vec<String> {
    r.headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

fn cookie_from(r: &Resp, name: &str) -> Option<String> {
    set_cookies(r).into_iter().find_map(|c| {
        let (pair, _) = c.split_once(';')?;
        let (k, v) = pair.split_once('=')?;
        (k == name && !v.is_empty()).then(|| v.to_owned())
    })
}

fn reason(r: &Resp) -> Option<&str> {
    r.json["error"]["reason"].as_str()
}

/// Every control route but the session route, with the admin routes.
fn other_control_routes(s: &S) -> Vec<(Method, String)> {
    let p = s.e.personal;
    let id = uuid::Uuid::now_v7();
    vec![
        (Method::GET, "/v1/stores".into()),
        (Method::POST, "/v1/stores".into()),
        (Method::GET, format!("/v1/stores/{p}")),
        (Method::PATCH, format!("/v1/stores/{p}")),
        (Method::DELETE, format!("/v1/stores/{p}")),
        (Method::POST, format!("/v1/stores/{p}/undelete")),
        (Method::GET, format!("/v1/stores/{p}/status")),
        (Method::GET, format!("/v1/stores/{p}/keys")),
        (Method::POST, format!("/v1/stores/{p}/keys")),
        (Method::POST, format!("/v1/stores/{p}/keys/{id}/rotate")),
        (Method::DELETE, format!("/v1/stores/{p}/keys/{id}")),
        (Method::DELETE, "/v1/account".into()),
        (Method::POST, "/v1/account/undelete".into()),
        (Method::GET, "/v1/users".into()),
        (Method::GET, format!("/v1/users/{id}/stores")),
        (Method::DELETE, format!("/v1/users/{id}")),
        (Method::POST, format!("/v1/users/{id}/undelete")),
        (Method::GET, "/v1/admin/status".into()),
        (Method::GET, format!("/v1/admin/stores/{p}/snapshots")),
    ]
}

#[tokio::test]
async fn access_tokens_and_every_rule_of_the_check() {
    let s = S::new(Role::Server, |_| {}).await;
    let aud = format!("{PUBLIC}/mcp");
    let claims = |f: &dyn Fn(&mut Value)| {
        let mut c = s.issuer.claims("tess", &aud);
        f(&mut c);
        c
    };
    let ok = |r: &Resp| assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let refused = |r: &Resp, what: &str| {
        assert_eq!(r.error(), Code::Unauthenticated, "{what}: {}", r.json);
        let challenge = r.headers[header::WWW_AUTHENTICATE].to_str().unwrap();
        assert!(challenge.contains("error=\"invalid_token\""), "{what}");
    };
    // Valid with an RSA key and with a P-256 key, on a store route and the session route.
    let rsa = s.issuer.mint(&claims(&|_| {}));
    ok(&s.token(&rsa, Method::GET, "/v1/namespaces", None).await);
    let es = s.issuer.mint_with(Algorithm::ES256, None, &claims(&|_| {}));
    ok(&s.token(&es, Method::GET, "/v1/namespaces", None).await);
    let r = s.token(&es, Method::GET, "/v1/session", None).await;
    assert_eq!(r.json["credential"]["kind"], "oauth", "{}", r.json);
    assert_eq!(r.json["signedIn"], true);
    // Every accepted audience: the three addresses and `auth.audiences`, alone or in a list.
    for a in [
        json!(format!("{PUBLIC}/mcp/plugin")),
        json!(PUBLIC),
        json!("urn:extra"),
        json!(["https://other.example", format!("{PUBLIC}/mcp")]),
    ] {
        let t = s.issuer.mint(&claims(&|c| c["aud"] = a.clone()));
        ok(&s.token(&t, Method::GET, "/v1/namespaces", None).await);
    }
    let t = now_s();
    let rsa_kid = s.issuer.kids()[0].clone();
    for (what, token) in [
        (
            "expired",
            s.issuer.mint(&claims(&|c| c["exp"] = json!(t - 120))),
        ),
        (
            "not yet valid",
            s.issuer.mint(&claims(&|c| c["nbf"] = json!(t + 300))),
        ),
        (
            "wrong issuer",
            s.issuer
                .mint(&claims(&|c| c["iss"] = json!("https://evil.example"))),
        ),
        (
            "issuer in a list",
            s.issuer
                .mint(&claims(&|c| c["iss"] = json!([s.issuer.url()]))),
        ),
        (
            "wrong audience",
            s.issuer
                .mint(&claims(&|c| c["aud"] = json!("https://evil.example/mcp"))),
        ),
        (
            "missing audience",
            s.issuer.mint(&claims(&|c| {
                c.as_object_mut().unwrap().remove("aud");
            })),
        ),
        (
            "missing subject",
            s.issuer.mint(&claims(&|c| {
                c.as_object_mut().unwrap().remove("sub");
            })),
        ),
        (
            "missing expiry",
            s.issuer.mint(&claims(&|c| {
                c.as_object_mut().unwrap().remove("exp");
            })),
        ),
        (
            "the web client's ID",
            s.issuer
                .mint(&claims(&|c| c["aud"] = json!([aud.clone(), CLIENT]))),
        ),
        (
            "an ID token (nonce)",
            s.issuer.mint(&claims(&|c| c["nonce"] = json!("n"))),
        ),
        (
            "an ID token (at_hash)",
            s.issuer.mint(&claims(&|c| c["at_hash"] = json!("h"))),
        ),
        (
            "an ID token (c_hash)",
            s.issuer.mint(&claims(&|c| c["c_hash"] = json!("h"))),
        ),
        (
            "the ai_agent profile",
            s.issuer
                .mint(&claims(&|c| c["sub_profile"] = json!("ai_agent"))),
        ),
        (
            "alg none",
            forged(json!({ "alg": "none", "typ": "JWT" }), &claims(&|_| {})),
        ),
        (
            "no key ID",
            forged(json!({ "alg": "RS256", "typ": "JWT" }), &claims(&|_| {})),
        ),
        (
            "a key of the wrong type",
            forged(json!({ "alg": "ES256", "kid": rsa_kid }), &claims(&|_| {})),
        ),
        (
            "a bad signature",
            forged(json!({ "alg": "RS256", "kid": rsa_kid }), &claims(&|_| {})),
        ),
        ("an HMAC algorithm", {
            let mut h = Header::new(Algorithm::HS256);
            h.kid = Some(rsa_kid.clone());
            jsonwebtoken::encode(
                &h,
                &claims(&|_| {}),
                &EncodingKey::from_secret(b"x").unwrap(),
            )
            .unwrap()
        }),
        ("not a JWT", "abc.def".to_owned()),
    ] {
        refused(
            &s.token(&token, Method::GET, "/v1/namespaces", None).await,
            what,
        );
    }
    // A token in the query string is never read.
    let r = s
        .call(
            Method::GET,
            &format!("/v1/namespaces?access_token={rsa}"),
            None,
            &[],
        )
        .await;
    assert_eq!(r.error(), Code::Unauthenticated);
    let challenge = r.headers[header::WWW_AUTHENTICATE].to_str().unwrap();
    assert_eq!(
        challenge,
        format!("Bearer resource_metadata=\"{PUBLIC}/.well-known/oauth-protected-resource/mcp\"")
    );
}

async fn status(s: &S, token: &str) -> StatusCode {
    s.token(token, Method::GET, "/v1/namespaces", None)
        .await
        .status
}

#[tokio::test]
async fn signing_keys_rotate_refresh_and_go_stale() {
    let mut s = S::new(Role::Server, |_| {}).await;
    let claims = s.issuer.claims("kim", &format!("{PUBLIC}/mcp"));
    let old = s.issuer.mint(&claims);
    assert_eq!(status(&s, &old).await, StatusCode::OK);
    // A new key ID after a rotation fetches the set again, at most once per 30 seconds.
    s.issuer.rotate_keys();
    let new = s.issuer.mint(&claims);
    assert_eq!(status(&s, &new).await, StatusCode::UNAUTHORIZED);
    s.e.clock.advance(31_000);
    assert_eq!(status(&s, &new).await, StatusCode::OK);
    // The whole set was replaced: the old key is gone.
    assert_eq!(status(&s, &old).await, StatusCode::UNAUTHORIZED);
    // A key removed at the issuer works until the hourly refresh, then stops.
    let kid = s.issuer.kids()[0].clone();
    s.issuer.remove_key(&kid);
    assert_eq!(status(&s, &new).await, StatusCode::OK);
    s.e.clock.advance(HOUR + 1000);
    assert_eq!(status(&s, &new).await, StatusCode::UNAUTHORIZED);
    // With the issuer gone, the keys held are used for 24 hours, then nothing is accepted.
    let es = s.issuer.mint_with(Algorithm::ES256, None, &claims);
    s.e.clock.advance(HOUR);
    assert_eq!(status(&s, &es).await, StatusCode::OK);
    // The old issuer's address stops answering.
    drop(std::mem::replace(
        &mut s.issuer,
        TestIssuer::start(true).await.unwrap(),
    ));
    s.e.clock.advance(2 * HOUR);
    let r = s.token(&es, Method::GET, "/v1/namespaces", None).await;
    assert_eq!(r.status, StatusCode::OK, "kept after a failed refresh");
    s.e.clock.advance(23 * HOUR);
    let r = s.token(&es, Method::GET, "/v1/namespaces", None).await;
    assert_eq!(r.error(), Code::Unavailable);
    assert!(r.headers.contains_key(header::RETRY_AFTER));
}

#[tokio::test]
async fn a_token_reaches_memories_and_the_session_route_only() {
    for role in [Role::Server, Role::Local] {
        let s = S::new(role, |_| {}).await;
        let t = s.access("alice");
        let r = s
            .token(
                &t,
                Method::POST,
                "/v1/memories",
                Some(json!({ "summary": "hi" })),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{role:?}: {}", r.json);
        assert_eq!(
            s.token(&t, Method::GET, "/v1/session", None).await.status,
            StatusCode::OK
        );
        for (m, p) in other_control_routes(&s) {
            let body = matches!(m, Method::POST | Method::PATCH).then(|| json!({}));
            let r = s.token(&t, m.clone(), &p, body).await;
            assert_eq!(r.error(), Code::PermissionDenied, "{role:?} {m} {p}");
        }
        // The store is never named with a token.
        let auth = format!("Bearer {t}");
        let personal = s.e.personal.to_string();
        let r = s
            .call(
                Method::GET,
                "/v1/namespaces",
                None,
                &[("authorization", &auth), ("knoggin-store", &personal)],
            )
            .await;
        assert_eq!(r.error(), Code::InvalidArgument, "{role:?}");
        let r = s
            .token(
                &t,
                Method::GET,
                &format!("/v1/export?store={personal}"),
                None,
            )
            .await;
        assert_eq!(r.error(), Code::PermissionDenied, "{role:?}");
    }
}

#[tokio::test]
async fn the_challenge_and_the_protected_resource_metadata() {
    let s = S::new(Role::Server, |_| {}).await;
    for (path, resource) in [
        ("/v1/stores", "/mcp"),
        ("/mcp", "/mcp"),
        ("/mcp/plugin", "/mcp/plugin"),
    ] {
        let r = s.call(Method::POST, path, Some(json!({})), &[]).await;
        assert_eq!(r.error(), Code::Unauthenticated, "{path}");
        assert_eq!(
            r.headers[header::WWW_AUTHENTICATE],
            format!(
                "Bearer resource_metadata=\"{PUBLIC}/.well-known/oauth-protected-resource{resource}\""
            )
        );
    }
    for (path, resource) in [
        ("/.well-known/oauth-protected-resource", "/mcp"),
        ("/.well-known/oauth-protected-resource/mcp", "/mcp"),
        (
            "/.well-known/oauth-protected-resource/mcp/plugin",
            "/mcp/plugin",
        ),
    ] {
        let r = s.call(Method::GET, path, None, &[]).await;
        assert_eq!(r.status, StatusCode::OK, "{path}");
        assert_eq!(r.headers[header::CACHE_CONTROL], "max-age=3600");
        assert_eq!(
            r.json,
            json!({ "resource": format!("{PUBLIC}{resource}"),
                "authorization_servers": [s.issuer.url()],
                "bearer_methods_supported": ["header"], "resource_name": "Knoggin" })
        );
    }
    // With sign-in off there is no challenge, no metadata and no sign-in route; a cookie
    // is ignored and a bearer value that is no key is refused.
    let off = EdgeEnv::new().await;
    for p in [
        "/.well-known/oauth-protected-resource",
        "/auth/login",
        "/auth/callback?code=x&state=y",
    ] {
        let r = off
            .call(support::edge::fixed(), Method::GET, p, None, &[])
            .await;
        assert_eq!(r.error(), Code::NotFound, "{p}");
    }
    let r = off
        .call(
            support::edge::fixed(),
            Method::GET,
            "/v1/stores",
            None,
            &[("cookie", "__Host-session=abc")],
        )
        .await;
    assert_eq!(r.error(), Code::Unauthenticated);
    assert!(!r.headers.contains_key(header::WWW_AUTHENTICATE));
    let r = off
        .bearer(
            support::edge::fixed(),
            &s.access("alice"),
            Method::GET,
            "/v1/namespaces",
            None,
        )
        .await;
    assert_eq!(r.error(), Code::Unauthenticated);
}

#[tokio::test]
async fn web_sign_in_sessions_and_their_cookies() {
    let s = S::new(Role::Server, |_| {}).await;
    let (login, path, started) = s.begin("alice", "/explore?x=1").await;
    let set = set_cookies(&started);
    assert_eq!(set.len(), 1);
    assert!(set[0].starts_with("__Host-login=v1."));
    assert!(
        set[0].ends_with("; Secure; HttpOnly; SameSite=Lax; Path=/"),
        "{}",
        set[0]
    );
    // The provider got the PKCE challenge, the state, the nonce and the scope.
    let to = reqwest::Url::parse(&location(&started)).unwrap();
    let q: std::collections::HashMap<_, _> = to.query_pairs().into_owned().collect();
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], CLIENT);
    assert_eq!(q["redirect_uri"], format!("{PUBLIC}/auth/callback"));
    assert_eq!(q["scope"], "openid email profile");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["login_hint"], "alice");
    assert!(q.contains_key("state") && q.contains_key("nonce") && q.contains_key("code_challenge"));
    let done = s.finish(&login, &path).await;
    assert_eq!(location(&done), "/explore?x=1");
    let set = set_cookies(&done);
    let session = set
        .iter()
        .find(|c| c.starts_with("__Host-session="))
        .unwrap();
    assert!(
        session.ends_with("; Secure; HttpOnly; SameSite=Lax; Path=/"),
        "{session}"
    );
    let cleared = set
        .iter()
        .find(|c| c.starts_with("__Host-login=;"))
        .unwrap();
    assert!(cleared.ends_with("Max-Age=0"), "{cleared}");
    let value = cookie_from(&done, "__Host-session").unwrap();
    // The session reaches the control routes; the session route names the person.
    let r = s.cookie(&value, Method::GET, "/v1/session", None).await;
    assert_eq!(r.json["signedIn"], true);
    assert_eq!(r.json["credential"]["kind"], "session");
    assert_eq!(r.json["user"]["email"], "alice@test.example");
    assert_eq!(r.json["user"]["name"], "alice");
    assert_eq!(r.json["user"]["isAdmin"], false);
    assert_eq!(r.json["provider"]["name"], "Test IdP");
    assert_eq!(r.json["webSignIn"], true);
    assert_eq!(
        s.cookie(&value, Method::GET, "/v1/stores", None)
            .await
            .status,
        StatusCode::OK
    );
    // A replayed callback fails at the code exchange.
    let again = s.finish(&login, &path).await;
    assert_eq!(location(&again), "/signin?error=provider_error");
    // Writes need the UI header; a foreign Origin and a form body are refused.
    let cookie = format!("__Host-session={value}");
    let r = s
        .call(
            Method::POST,
            "/v1/stores",
            Some(json!({ "name": "x" })),
            &[("cookie", &cookie)],
        )
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
    let r = s
        .call(
            Method::POST,
            "/v1/stores",
            Some(json!({ "name": "x" })),
            &[
                ("cookie", &cookie),
                ("x-knoggin-ui", "1"),
                ("origin", "https://evil.example"),
            ],
        )
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
    let mut form = s.request(
        Method::POST,
        "/v1/stores",
        None,
        &[("cookie", &cookie), ("x-knoggin-ui", "1")],
    );
    *form.body_mut() = Body::from("name=x");
    form.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    assert_eq!(send(&s.app, form).await.error(), Code::InvalidArgument);
    let r = s
        .cookie(
            &value,
            Method::POST,
            "/v1/stores",
            Some(json!({ "name": "x" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    // A cookie is ignored when an Authorization header is sent.
    let r = s
        .call(
            Method::GET,
            "/v1/stores",
            None,
            &[("cookie", &cookie), ("authorization", "Bearer nope")],
        )
        .await;
    assert_eq!(r.error(), Code::Unauthenticated);
    // Logout ends it and clears the cookie.
    let r = s.cookie(&value, Method::POST, "/auth/logout", None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        set_cookies(&r),
        ["__Host-session=; Secure; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"]
    );
    let r = s.cookie(&value, Method::GET, "/v1/stores", None).await;
    assert_eq!(r.error(), Code::Unauthenticated);
    let r = s.cookie(&value, Method::GET, "/v1/session", None).await;
    assert_eq!(r.json["signedIn"], false);
    assert_eq!(r.json["signedOutReason"], "expired");
}

#[tokio::test]
async fn sessions_end_when_idle_and_after_their_lifetime() {
    let s = S::new(Role::Server, |_| {}).await;
    let idle = s.sign_in("alice").await.unwrap();
    s.e.clock.advance(24 * HOUR + 1000);
    let r = s.cookie(&idle, Method::GET, "/v1/stores", None).await;
    assert_eq!(r.error(), Code::Unauthenticated);
    // A session in use still ends 7 days after it was made.
    let busy = s.sign_in("alice").await.unwrap();
    for _ in 0..7 {
        s.e.clock.advance(23 * HOUR);
        let r = s.cookie(&busy, Method::GET, "/v1/stores", None).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    }
    s.e.clock.advance(23 * HOUR);
    let r = s.cookie(&busy, Method::GET, "/v1/stores", None).await;
    assert_eq!(r.error(), Code::Unauthenticated);
}

#[tokio::test]
async fn each_callback_error_code() {
    let s = S::new(Role::Server, |o| {
        o.allowed_emails = vec![
            "alice@test.example".into(),
            "bob-unverified@test.example".into(),
        ];
    })
    .await;
    let code_of = |r: &Resp| {
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.json);
        let to = location(r);
        // The login cookie is cleared on a failure too.
        assert!(
            set_cookies(r)
                .iter()
                .any(|c| c.starts_with("__Host-login=;"))
        );
        to.strip_prefix("/signin?error=").unwrap().to_owned()
    };
    // state_mismatch: no login cookie, one that does not open, another attempt's state,
    // and an attempt older than 10 minutes.
    let (login, path, _) = s.begin("alice", "/").await;
    assert_eq!(
        code_of(&s.call(Method::GET, &path, None, &[]).await),
        "state_mismatch"
    );
    assert_eq!(
        code_of(&s.finish("v1.AAAA.BBBB", &path).await),
        "state_mismatch"
    );
    let (other, _, _) = s.begin("alice", "/").await;
    assert_eq!(code_of(&s.finish(&other, &path).await), "state_mismatch");
    s.e.clock.advance(601_000);
    assert_eq!(code_of(&s.finish(&login, &path).await), "state_mismatch");
    // provider_error: the provider's error, an `iss` that differs, a bad code.
    let (login, path, _) = s.begin("alice", "/").await;
    let state = path
        .split("state=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let r = s
        .finish(
            &login,
            &format!("/auth/callback?error=access_denied&state={state}"),
        )
        .await;
    assert_eq!(code_of(&r), "provider_error");
    let evil = path.replace("iss=", "iss=https%3A%2F%2Fevil.example&x=");
    assert_eq!(code_of(&s.finish(&login, &evil).await), "provider_error");
    let r = s
        .finish(&login, &format!("/auth/callback?code=nope&state={state}"))
        .await;
    assert_eq!(code_of(&r), "provider_error");
    // email_unverified and not_allowed, with an allow-list.
    assert_eq!(
        s.sign_in("bob-unverified").await.unwrap_err(),
        "email_unverified"
    );
    assert_eq!(s.sign_in("mallory").await.unwrap_err(), "not_allowed");
    // A bad return_to becomes `/`.
    for bad in ["//evil.example", "/%5Cevil.example", "https://evil.example"] {
        let (login, path, _) = s.begin("alice", bad).await;
        assert_eq!(location(&s.finish(&login, &path).await), "/", "{bad}");
    }
}

#[tokio::test]
async fn users_are_made_by_web_sign_in_and_by_tokens_with_a_verified_email() {
    // Without an allow-list.
    let s = S::new(Role::Server, |o| o.email_claim = Some("work_email".into())).await;
    let before = s.user_count().await;
    s.sign_in("alice").await.unwrap();
    assert_eq!(s.user_count().await, before + 1);
    // A web sign-in with an unverified email makes a user whose email counts for nothing.
    s.sign_in("pat-unverified").await.unwrap();
    assert_eq!(s.user_count().await, before + 2);
    // An agent's token with a verified email makes a user; without one it makes none.
    let r = s
        .token(&s.access("dave"), Method::GET, "/v1/session", None)
        .await;
    assert_eq!(r.json["signedIn"], true, "{}", r.json);
    assert_eq!(s.user_count().await, before + 3);
    let r = s
        .token(
            &s.access("erin-unverified"),
            Method::GET,
            "/v1/namespaces",
            None,
        )
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
    assert!(
        r.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains(PUBLIC)
    );
    assert_eq!(s.user_count().await, before + 3);
    // A token without `email` but with the claim `auth.email_claim` names.
    let mut c = s.issuer.claims("gil", &format!("{PUBLIC}/mcp"));
    c["email_verified"] = json!(false);
    c["work_email"] = json!("gil@work.example");
    let r = s
        .token(&s.issuer.mint(&c), Method::GET, "/v1/namespaces", None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(s.user_count().await, before + 4);
    // Two first requests at once make one user.
    let t = s.access("twin");
    let (a, b) = tokio::join!(
        s.token(&t, Method::GET, "/v1/session", None),
        s.token(&t, Method::GET, "/v1/session", None)
    );
    assert_eq!(a.json["user"]["id"], b.json["user"]["id"]);
    assert_eq!(s.user_count().await, before + 5);

    // With an allow-list: only a verified, listed email makes a user.
    let s = S::new(Role::Server, |o| {
        o.allowed_email_domains = vec!["test.example".into()]
    })
    .await;
    let before = s.user_count().await;
    for sub in ["erin-unverified"] {
        let r = s
            .token(&s.access(sub), Method::GET, "/v1/namespaces", None)
            .await;
        assert_eq!(r.error(), Code::PermissionDenied);
    }
    let mut c = s.issuer.claims("zed", &format!("{PUBLIC}/mcp"));
    c["email"] = json!("zed@elsewhere.example");
    let r = s
        .token(&s.issuer.mint(&c), Method::GET, "/v1/namespaces", None)
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
    assert_eq!(s.user_count().await, before);
    s.token(&s.access("dave"), Method::GET, "/v1/namespaces", None)
        .await;
    s.sign_in("alice").await.unwrap();
    assert_eq!(s.user_count().await, before + 2);
}

#[tokio::test]
async fn admins_come_from_the_config_on_every_request() {
    let mut s = S::new(Role::Server, |o| {
        o.admin_emails = vec!["Boss@Test.Example".into()];
        o.admin_subjects = vec!["root".into()];
    })
    .await;
    let boss = s.sign_in("boss").await.unwrap();
    let root = s.sign_in("root").await.unwrap();
    let member = s.sign_in("member").await.unwrap();
    for (who, cookie, admin) in [
        ("boss", &boss, true),
        ("root", &root, true),
        ("member", &member, false),
    ] {
        let r = s.cookie(cookie, Method::GET, "/v1/session", None).await;
        assert_eq!(r.json["user"]["isAdmin"], admin, "{who}");
        let r = s.cookie(cookie, Method::GET, "/v1/users", None).await;
        assert_eq!(r.status == StatusCode::OK, admin, "{who}: {}", r.json);
    }
    // An unverified email never makes an admin.
    s.reconfigure(|o| o.admin_emails = vec!["pat-unverified@test.example".into()])
        .await;
    let pat = s.sign_in("pat-unverified").await.unwrap();
    assert_eq!(
        s.cookie(&pat, Method::GET, "/v1/users", None).await.error(),
        Code::PermissionDenied
    );
    // Taken out of the config, an admin loses the admin routes at the next request.
    assert_eq!(
        s.cookie(&boss, Method::GET, "/v1/users", None)
            .await
            .error(),
        Code::PermissionDenied
    );
    let r = s.cookie(&root, Method::GET, "/v1/users", None).await;
    assert_eq!(r.status, StatusCode::OK);
    let listed = r.json["items"].as_array().unwrap();
    assert!(listed.iter().any(|u| u["isAdmin"] == true));
}

#[tokio::test]
async fn a_removed_user_is_refused_everywhere_and_comes_back_new_after_the_erase() {
    let s = S::new(Role::Server, |o| o.admin_subjects = vec!["root".into()]).await;
    let root = s.sign_in("root").await.unwrap();
    let bob = s.sign_in("bob").await.unwrap();
    let me = s.cookie(&bob, Method::GET, "/v1/session", None).await;
    let bob_id = me.json["user"]["id"].as_str().unwrap().to_owned();
    let bob_store = me.json["personalStoreId"].as_str().unwrap().to_owned();
    let r = s
        .cookie(
            &bob,
            Method::POST,
            &format!("/v1/stores/{bob_store}/keys"),
            Some(json!({ "name": "laptop" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let key = r.json["secret"].as_str().unwrap().to_owned();
    let token = s.access("bob");
    assert_eq!(
        s.token(&token, Method::GET, "/v1/namespaces", None)
            .await
            .status,
        StatusCode::OK
    );
    let r = s
        .cookie(&root, Method::DELETE, &format!("/v1/users/{bob_id}"), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    // Every route but the session route, with the key and with the token.
    for credential in [&key, &token] {
        for (m, p) in [
            (Method::GET, "/v1/namespaces".to_owned()),
            (Method::POST, "/v1/account/undelete".to_owned()),
            (Method::GET, "/v1/stores".to_owned()),
        ] {
            let r = s.token(credential, m, &p, None).await;
            assert_eq!(r.error(), Code::PermissionDenied, "{p}");
            assert_eq!(reason(&r), Some("account_removed"), "{p}");
        }
        let r = s.token(credential, Method::GET, "/v1/session", None).await;
        assert_eq!(r.json["signedIn"], false);
        assert_eq!(r.json["signedOutReason"], "account_removed");
    }
    // The browser sessions ended with the removal.
    let r = s.cookie(&bob, Method::GET, "/v1/session", None).await;
    assert_eq!(r.json["signedOutReason"], "expired");
    assert_eq!(s.sign_in("bob").await.unwrap_err(), "account_removed");
    // After the erase the same person signs in again as a new user, with nothing old.
    s.e.clock.advance(8 * MILLIS_PER_DAY);
    let report = knoggin_state::erase(s.e.db.clone(), s.e.clock.now(), 7, |_| async {
        Ok::<(), ()>(())
    })
    .await
    .unwrap();
    assert_eq!(report.users_erased, 1);
    let again = s.sign_in("bob").await.unwrap();
    let me = s.cookie(&again, Method::GET, "/v1/session", None).await;
    assert_ne!(me.json["user"]["id"], json!(bob_id));
    let stores = s.cookie(&again, Method::GET, "/v1/stores", None).await;
    let items = stores.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_ne!(items[0]["id"], json!(bob_store));
    assert_eq!(
        s.token(&key, Method::GET, "/v1/namespaces", None)
            .await
            .error(),
        Code::Unauthenticated
    );
    assert_eq!(
        s.cookie(&bob, Method::GET, "/v1/stores", None)
            .await
            .error(),
        Code::Unauthenticated
    );
}

#[tokio::test]
async fn a_user_taken_off_the_allow_list_is_refused_on_the_next_request() {
    let mut s = S::new(Role::Server, |o| {
        o.allowed_emails = vec!["alice@test.example".into(), "bob@test.example".into()];
    })
    .await;
    let bob = s.sign_in("bob").await.unwrap();
    let me = s.cookie(&bob, Method::GET, "/v1/session", None).await;
    let store = me.json["personalStoreId"].as_str().unwrap().to_owned();
    let r = s
        .cookie(
            &bob,
            Method::POST,
            &format!("/v1/stores/{store}/keys"),
            Some(json!({ "name": "k" })),
        )
        .await;
    let key = r.json["secret"].as_str().unwrap().to_owned();
    s.reconfigure(|o| o.allowed_emails = vec!["alice@test.example".into()])
        .await;
    for credential in [key.as_str(), s.access("bob").as_str()] {
        let r = s
            .token(credential, Method::GET, "/v1/namespaces", None)
            .await;
        assert_eq!(reason(&r), Some("not_allowed"), "{}", r.json);
    }
    let r = s.cookie(&bob, Method::GET, "/v1/stores", None).await;
    assert_eq!(reason(&r), Some("not_allowed"));
    let r = s.cookie(&bob, Method::GET, "/v1/session", None).await;
    assert_eq!(r.json["signedIn"], false);
    assert_eq!(r.json["signedOutReason"], "not_allowed");
}

#[tokio::test]
async fn a_local_install_with_sign_in_on() {
    let mut s = S::new(Role::Local, |_| {}).await;
    // The first identity on the list signs in; a second one that is not on it is refused,
    // by web sign-in and by token.
    let alice = s.sign_in("alice").await.unwrap();
    assert_eq!(s.sign_in("mallory").await.unwrap_err(), "not_allowed");
    let r = s
        .token(&s.access("mallory"), Method::GET, "/v1/namespaces", None)
        .await;
    assert_eq!(reason(&r), Some("not_allowed"));
    // A token without a verified email is refused, even for a listed address.
    let mut c = s.issuer.claims("alice", &format!("{PUBLIC}/mcp"));
    c["email_verified"] = json!(false);
    let r = s
        .token(&s.issuer.mint(&c), Method::GET, "/v1/namespaces", None)
        .await;
    assert_eq!(reason(&r), Some("not_allowed"));
    // Both act as the one local user, on its personal store.
    let r = s
        .token(&s.access("alice"), Method::GET, "/v1/session", None)
        .await;
    assert_eq!(r.json["user"]["id"], json!(s.e.user.to_string()));
    assert_eq!(r.json["user"]["email"], Value::Null);
    let r = s.cookie(&alice, Method::GET, "/v1/session", None).await;
    assert_eq!(r.json["user"]["id"], json!(s.e.user.to_string()));
    assert_eq!(r.json["user"]["email"], "alice@test.example");
    assert_eq!(r.json["user"]["name"], "alice");
    assert_eq!(r.json["webSignIn"], true);
    // The session route on the loopback address of the fixed listener offers no sign-in.
    let r = s
        .call(
            Method::GET,
            "/v1/session",
            None,
            &[("host", "127.0.0.1:7690")],
        )
        .await;
    assert_eq!(r.json["webSignIn"], false);
    assert_eq!(r.json["auth"], "oauth");
    // A `knoggin open` session reports no email, and sign-in is not served on its listener.
    let code = s.e.local.new_code(s.e.clock.now()).unwrap();
    let local = s.e.local.redeem(&code, s.e.clock.now()).unwrap().unwrap();
    let web = support::edge::web();
    let r =
        s.e.bearer(web.clone(), &local, Method::GET, "/v1/session", None)
            .await;
    assert_eq!(r.json["user"]["email"], Value::Null);
    assert_eq!(r.json["webSignIn"], false);
    let r = s.e.call(web, Method::GET, "/auth/login", None, &[]).await;
    assert_eq!(r.error(), Code::NotFound);
    // A cookie session cannot make, rotate or revoke a key, delete a store or the data.
    let p = s.e.personal;
    let (_, key) = s.e.key(p, json!(["memory:read"]), 30).await;
    let key_id = key["id"].as_str().unwrap();
    let app = s
        .cookie(
            &alice,
            Method::POST,
            "/v1/stores",
            Some(json!({ "name": "notes" })),
        )
        .await;
    assert_eq!(app.status, StatusCode::CREATED);
    let app = app.json["id"].as_str().unwrap().to_owned();
    for (m, path, body) in [
        (
            Method::POST,
            format!("/v1/stores/{p}/keys"),
            Some(json!({ "name": "k" })),
        ),
        (
            Method::POST,
            format!("/v1/stores/{p}/keys/{key_id}/rotate"),
            Some(json!({})),
        ),
        (
            Method::DELETE,
            format!("/v1/stores/{p}/keys/{key_id}"),
            None,
        ),
        (Method::DELETE, format!("/v1/stores/{app}"), None),
        (
            Method::DELETE,
            "/v1/account".into(),
            Some(json!({ "confirm": "delete my data" })),
        ),
    ] {
        let r = s.cookie(&alice, m.clone(), &path, body).await;
        assert_eq!(r.error(), Code::PermissionDenied, "{m} {path}");
    }
    // Taken off the list, the identity is refused at its next request, by cookie and token.
    s.reconfigure(|o| o.allowed_emails = vec!["someone@test.example".into()])
        .await;
    let r = s.cookie(&alice, Method::GET, "/v1/stores", None).await;
    assert_eq!(reason(&r), Some("not_allowed"));
    let r = s.cookie(&alice, Method::GET, "/v1/session", None).await;
    assert_eq!(r.json["signedOutReason"], "not_allowed");
    let r = s
        .token(&s.access("alice"), Method::GET, "/v1/namespaces", None)
        .await;
    assert_eq!(reason(&r), Some("not_allowed"));
    // The socket is not subject to the list.
    assert_eq!(
        s.e.local(Method::GET, "/v1/stores", None).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn test_mode_cookies_on_a_loopback_address() {
    let s = S::at(Role::Local, "http://127.0.0.1:7690", |_| {}).await;
    let (_, _, started) = s.begin("alice", "/").await;
    let set = set_cookies(&started);
    assert!(set[0].starts_with("knoggin_login="));
    assert!(
        set[0].ends_with("; HttpOnly; SameSite=Lax; Path=/"),
        "{}",
        set[0]
    );
    assert!(!set[0].contains("Secure"));
    let value = s.sign_in("alice").await.unwrap();
    let r = s
        .call(
            Method::GET,
            "/v1/session",
            None,
            &[
                ("host", "127.0.0.1:7690"),
                ("cookie", &format!("knoggin_session={value}")),
            ],
        )
        .await;
    assert_eq!(r.json["signedIn"], true, "{}", r.json);
    assert_eq!(r.json["webSignIn"], true);
}

#[tokio::test]
async fn browser_checks_on_the_router() {
    let s = S::new(Role::Server, |_| {}).await;
    let t = s.access("alice");
    let auth = format!("Bearer {t}");
    let app = &s.app;
    let get = |headers: Vec<(&'static str, String)>, path: &'static str| {
        let mut h: Vec<(&str, &str)> = vec![("authorization", auth.as_str())];
        h.extend(headers.iter().map(|(k, v)| (*k, v.as_str())));
        let r = s.request(Method::GET, path, None, &h);
        async move { send(app, r).await }
    };
    assert_eq!(get(vec![], "/v1/namespaces").await.status, StatusCode::OK);
    // Host.
    for host in ["evil.example", "memory.example.test:8443", "127.0.0.1:7690"] {
        let r = get(vec![("host", host.into())], "/v1/namespaces").await;
        assert_eq!(r.error(), Code::PermissionDenied, "{host}");
    }
    for host in ["MEMORY.example.test.", "memory.example.test:443"] {
        let r = get(vec![("host", host.into())], "/v1/namespaces").await;
        assert_eq!(r.status, StatusCode::OK, "{host}");
    }
    for path in ["/healthz", "/readyz"] {
        let r = get(vec![("host", "anything.example".into())], path).await;
        assert_eq!(r.status, StatusCode::OK, "{path}");
    }
    // Internal headers, in any case and twice.
    for name in [
        "knoggin-permissions",
        "Knoggin-User-Id",
        "KNOGGIN-STORE-ID",
        "knoggin-credential-kind",
    ] {
        let r = get(vec![(name, "x".into())], "/v1/namespaces").await;
        assert_eq!(r.error(), Code::InvalidArgument, "{name}");
    }
    let mut twice = s.request(
        Method::GET,
        "/v1/namespaces",
        None,
        &[("authorization", &auth)],
    );
    twice
        .headers_mut()
        .append("knoggin-user-id", HeaderValue::from_static("a"));
    twice
        .headers_mut()
        .append("Knoggin-User-Id", HeaderValue::from_static("b"));
    assert_eq!(send(&s.app, twice).await.error(), Code::InvalidArgument);
    // Fetch site and Origin.
    let r = get(
        vec![("sec-fetch-site", "cross-site".into())],
        "/v1/namespaces",
    )
    .await;
    assert_eq!(r.error(), Code::PermissionDenied);
    let r = get(
        vec![("sec-fetch-site", "same-origin".into())],
        "/v1/namespaces",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    for origin in ["https://evil.example", "null", "http://memory.example.test"] {
        let r = get(vec![("origin", origin.into())], "/v1/namespaces").await;
        assert_eq!(r.error(), Code::PermissionDenied, "{origin}");
    }
    let r = get(vec![("origin", PUBLIC.into())], "/v1/namespaces").await;
    assert_eq!(r.status, StatusCode::OK);
    // Preflight.
    let r = send(
        &s.app,
        s.request(Method::OPTIONS, "/v1/namespaces", None, &[]),
    )
    .await;
    assert_eq!(r.error(), Code::PermissionDenied);
    assert!(!r.headers.contains_key("access-control-allow-origin"));
}
