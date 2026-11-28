// SPDX-License-Identifier: AGPL-3.0-only
//! Web sign-in and its routes: the authorization-code flow with PKCE, the sealed login
//! cookie, the callback's checks and user rules, cookie sessions and logout, and the
//! protected-resource metadata.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use knoggin_core::Sensitive;
use knoggin_crypto::token::{hash_secret, random_token};
use knoggin_state::record::DeletedBy;
use serde::{Deserialize, Serialize};
use serde_json::json;
use zeroize::Zeroizing;

use super::check::{Caller, Credential, cookie_value, query_pairs, state_error};
use super::oauth::{Identity, Provider};
use super::ui::CacheControl;
use super::{Arrival, EdgeState, Role};
use crate::error::ApiError;

/// A sign-in attempt lives this many seconds.
const LOGIN_SECONDS: i64 = 600;
/// `returnTo` is cut off at this many bytes.
const RETURN_TO_BYTES: usize = 1024;
/// A longer `login_hint` is left out.
const HINT_BYTES: usize = 256;

pub(crate) fn routes(edge: EdgeState) -> Router {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
        .route("/.well-known/oauth-protected-resource", get(metadata))
        .route("/.well-known/oauth-protected-resource/mcp", get(metadata))
        .route(
            "/.well-known/oauth-protected-resource/mcp/plugin",
            get(metadata),
        )
        .with_state(edge)
}

/// What the login cookie carries, sealed: nothing of an attempt is stored.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Login {
    state: String,
    nonce: String,
    verifier: String,
    return_to: String,
    exp: i64,
}

impl EdgeState {
    /// The provider, where the sign-in routes are served: mode `oauth`, on the router's
    /// public listener, or on the daemon's fixed listener at the host and port of
    /// `server.public_url` only, so that no cookie is set for a loopback address unless
    /// that address is the public one (test mode).
    fn sign_in_here(&self, arrival: &Arrival, headers: &HeaderMap) -> Result<&Provider, ApiError> {
        match (&self.inner().oauth, arrival) {
            (Some(o), Arrival::Public) => Ok(o),
            (Some(o), Arrival::Fixed { .. }) if self.web_sign_in(arrival, headers) => Ok(o),
            _ => Err(ApiError::not_found("No such route")),
        }
    }

    /// The session's `webSignIn`: true on a server with sign-in on; on a local install
    /// only on the fixed listener, at the host and port of `server.public_url`.
    pub(crate) fn web_sign_in(&self, arrival: &Arrival, headers: &HeaderMap) -> bool {
        let inner = self.inner();
        match (inner.config.role, &inner.oauth) {
            (_, None) => false,
            (Role::Server, Some(_)) => true,
            (Role::Local, Some(_)) => {
                matches!(arrival, Arrival::Fixed { .. })
                    && inner.public.as_ref().is_some_and(|p| {
                        headers
                            .get(header::HOST)
                            .and_then(|h| h.to_str().ok())
                            .is_some_and(|h| p.is_host(h))
                    })
            }
        }
    }
}

/// `return_to` if it is a path on this site, else `/`: it starts with `/`, its second
/// character is neither `/` nor a backslash, and it holds no backslash and no control
/// character.
fn safe_return_to(path: Option<&str>) -> String {
    match path {
        Some(p)
            if p.starts_with('/')
                && !matches!(p.as_bytes().get(1), Some(b'/' | b'\\'))
                && !p.contains('\\')
                && !p.chars().any(char::is_control) =>
        {
            let mut end = p.len().min(RETURN_TO_BYTES);
            while !p.is_char_boundary(end) {
                end -= 1;
            }
            p[..end].to_owned()
        }
        _ => "/".into(),
    }
}

fn set_cookie(name: &str, value: &str, attributes: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!("{name}={value}{attributes}")).ok()
}

fn clear_cookie(name: &str, attributes: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!("{name}={attributes}; Max-Age=0")).ok()
}

/// A `303 See Other` to `location` that sets `cookies`.
fn redirect(location: &str, cookies: impl IntoIterator<Item = Option<HeaderValue>>) -> Response {
    let mut r = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(location) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    for c in cookies.into_iter().flatten() {
        r.headers_mut().append(header::SET_COOKIE, c);
    }
    r
}

fn random() -> Result<String, ApiError> {
    random_token().map_err(|_| ApiError::internal())
}

/// The first value of query parameter `name`.
fn param(pairs: &[(String, String)], name: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

/// `GET /auth/login?return_to=<path>`: a new attempt in the sealed login cookie, and a
/// redirect to the provider with the state, the nonce and the PKCE challenge.
async fn login(
    State(edge): State<EdgeState>,
    Extension(arrival): Extension<Arrival>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, ApiError> {
    let o = edge.sign_in_here(&arrival, &headers)?;
    let d = o.discovery().ok_or_else(|| ApiError::unavailable(60))?;
    let pairs = match uri.query() {
        Some(q) => query_pairs(q)?,
        None => Vec::new(),
    };
    let login = Login {
        state: random()?,
        nonce: random()?,
        verifier: random()?,
        return_to: safe_return_to(param(&pairs, "return_to").as_deref()),
        exp: edge.now() / 1000 + LOGIN_SECONDS,
    };
    let challenge = URL_SAFE_NO_PAD.encode(
        aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, login.verifier.as_bytes()).as_ref(),
    );
    let plain = Zeroizing::new(serde_json::to_vec(&login).map_err(|_| ApiError::internal())?);
    let sealed = edge
        .inner()
        .fields
        .seal_login(&plain)
        .map_err(state_error)?;
    let mut to = d.authorization_endpoint.clone();
    {
        let mut q = to.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &o.config.web_client_id)
            .append_pair("redirect_uri", &format!("{}/auth/callback", o.public_url))
            .append_pair("scope", "openid email profile")
            .append_pair("state", &login.state)
            .append_pair("nonce", &login.nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        // One percent-encoded value, so that it cannot add a parameter of its own.
        if let Some(hint) = param(&pairs, "login_hint").filter(|h| h.len() <= HINT_BYTES) {
            q.append_pair("login_hint", &hint);
        }
    }
    let c = o.cookies();
    Ok(redirect(
        to.as_str(),
        [set_cookie(c.login, &sealed, c.attributes)],
    ))
}

/// Why a callback failed, as the code `/signin?error=` carries, or an error answer.
enum Failed {
    Code(&'static str),
    Api(ApiError),
}

impl From<&'static str> for Failed {
    fn from(code: &'static str) -> Self {
        Self::Code(code)
    }
}

/// `GET /auth/callback?code=...&state=...`.
async fn callback(
    State(edge): State<EdgeState>,
    Extension(arrival): Extension<Arrival>,
    request: Request,
) -> Response {
    let client = request.extensions().get::<super::ClientAddr>().copied();
    let o = match edge.sign_in_here(&arrival, request.headers()) {
        Ok(o) => o,
        Err(e) => return e.into_response(),
    };
    let c = o.cookies();
    // The login cookie is cleared whatever happens.
    let clear = clear_cookie(c.login, c.attributes);
    match finish_sign_in(&edge, o, request.uri(), request.headers()).await {
        Ok((session, to)) => {
            // Only a callback that does not sign in counts against the fixed listener's
            // bucket, which charged every callback before its code exchange.
            edge.give_back_address(&arrival, client);
            redirect(
                &to,
                [set_cookie(c.session, session.expose(), c.attributes), clear],
            )
        }
        Err(Failed::Code(code)) => {
            tracing::info!(kind = "sign_in_failed", code, "a web sign-in failed");
            redirect(&format!("/signin?error={code}"), [clear])
        }
        Err(Failed::Api(e)) => {
            let mut r = e.into_response();
            if let Some(v) = clear {
                r.headers_mut().append(header::SET_COOKIE, v);
            }
            r
        }
    }
}

/// The callback's checks, the code exchange, the user rules and the new session: the
/// cookie value and where to go.
async fn finish_sign_in(
    edge: &EdgeState,
    o: &Provider,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<(Sensitive<String>, String), Failed> {
    let inner = edge.inner();
    let pairs = match uri.query() {
        Some(q) => query_pairs(q).map_err(|_| "provider_error")?,
        None => Vec::new(),
    };
    let sealed = cookie_value(headers, o.cookies().login).ok_or("state_mismatch")?;
    let plain = inner
        .fields
        .open_login(&sealed)
        .map_err(|_| "state_mismatch")?;
    let login: Login = serde_json::from_slice(&plain).map_err(|_| "state_mismatch")?;
    let state = param(&pairs, "state").unwrap_or_default();
    if login.exp <= edge.now() / 1000
        || !knoggin_crypto::token::eq(state.as_bytes(), login.state.as_bytes())
    {
        return Err("state_mismatch".into());
    }
    if param(&pairs, "error").is_some() {
        return Err("provider_error".into());
    }
    if param(&pairs, "iss").is_some_and(|iss| iss != o.config.issuer) {
        return Err("provider_error".into());
    }
    let code = param(&pairs, "code").ok_or("provider_error")?;
    let redirect_uri = format!("{}/auth/callback", o.public_url);
    let id_token = o
        .exchange(&code, &login.verifier, &redirect_uri)
        .await
        .map_err(|()| "provider_error")?;
    let id = o
        .check_id_token(id_token.expose(), &login.nonce)
        .await
        .map_err(|_| "provider_error")?;
    let user = sign_in_user(edge, o, &id).await?;
    // A new session; only its hash is stored.
    let value = Sensitive::new(random().map_err(Failed::Api)?);
    let hash = hash_secret(value.expose());
    let local = inner.config.role == Role::Local;
    let max_ms =
        i64::from(o.config.session_max_days).saturating_mul(knoggin_core::time::MILLIS_PER_DAY);
    inner
        .sessions
        .create(
            &hash,
            user,
            edge.now(),
            max_ms,
            if local { id.email.clone() } else { None },
            if local { id.name.clone() } else { None },
        )
        .await
        .map_err(|e| Failed::Api(state_error(e)))?;
    Ok((value, safe_return_to(Some(&login.return_to))))
}

/// The user rules of a web sign-in: the allow-list on the ID token's verified email,
/// then the user of that identity (on a local install the one user).
async fn sign_in_user(
    edge: &EdgeState,
    o: &Provider,
    id: &Identity,
) -> Result<knoggin_core::UserId, Failed> {
    let inner = edge.inner();
    if o.config.allow_list_set() {
        let email = id.verified_email().ok_or("email_unverified")?;
        if !o.config.allowed(email) {
            return Err("not_allowed".into());
        }
    }
    if inner.config.role == Role::Local {
        return inner
            .local_user
            .ok_or_else(|| Failed::Api(ApiError::internal()));
    }
    let api = |e| Failed::Api(state_error(e));
    match inner
        .users
        .find(&o.config.issuer, id.subject.expose())
        .await
        .map_err(api)?
    {
        Some(u) if u.value.deleted_by == Some(DeletedBy::Admin) => Err("account_removed".into()),
        Some(u) => {
            // A web sign-in refreshes the stored email, its flag and the name.
            let (email, verified, name) = (id.email.clone(), id.email_verified, id.name.clone());
            inner
                .users
                .update(u.value.id, |user| {
                    user.email = email.clone();
                    user.email_verified = verified;
                    user.name = name.clone();
                    Ok::<_, knoggin_state::StateError>(())
                })
                .await
                .map_err(api)?;
            Ok(u.value.id)
        }
        None => edge
            .create_user(&o.config.issuer, id)
            .await
            .map(|u| u.value.id)
            .map_err(Failed::Api),
    }
}

#[utoipa::path(post, path = "/auth/logout", tag = "session",
    responses((status = 204, description = "The session is ended"), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
/// Ends a browser session or a local web session.
// A cookie session is deleted and its cookie cleared; on the daemon's local web listener a
// local web session is forgotten. Served on a local install in both modes; on a server
// with sign-in off it does not exist.
pub(crate) async fn logout(
    State(edge): State<EdgeState>,
    Extension(arrival): Extension<Arrival>,
    Extension(c): Extension<Caller>,
) -> Result<Response, ApiError> {
    let inner = edge.inner();
    if inner.config.role == Role::Server && inner.oauth.is_none() {
        return Err(ApiError::not_found("No such route"));
    }
    match (&c.credential, &arrival, &inner.local) {
        (Credential::LocalSession(hash), Arrival::Web { .. }, Some(local)) => {
            local.forget(hash);
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        (Credential::Cookie(hash, _), _, _) => {
            inner.sessions.delete(hash).await.map_err(state_error)?;
            let mut r = StatusCode::NO_CONTENT.into_response();
            if let Some(cookies) = inner.oauth.as_ref().map(|o| o.cookies())
                && let Some(v) = clear_cookie(cookies.session, cookies.attributes)
            {
                r.headers_mut().insert(header::SET_COOKIE, v);
            }
            Ok(r)
        }
        _ => Err(ApiError::permission_denied(
            "Logout ends a browser session only",
        )),
    }
}

/// The protected-resource metadata (RFC 9728) at its three addresses; `resource` is the
/// address the client connected to.
async fn metadata(State(edge): State<EdgeState>, uri: Uri) -> Result<Response, ApiError> {
    let o = edge
        .inner()
        .oauth
        .as_ref()
        .ok_or_else(|| ApiError::not_found("No such route"))?;
    let resource = if uri.path().ends_with("/mcp/plugin") {
        "/mcp/plugin"
    } else {
        "/mcp"
    };
    let mut r = Json(json!({
        "resource": format!("{}{resource}", o.public_url),
        "authorization_servers": [o.config.issuer],
        "bearer_methods_supported": ["header"],
        "resource_name": "Knoggin",
    }))
    .into_response();
    r.extensions_mut()
        .insert(CacheControl(HeaderValue::from_static("max-age=3600")));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_to_is_a_path_on_this_site_or_slash() {
        for ok in ["/", "/map", "/explore?id=1&x=%2F", "/a/b/c"] {
            assert_eq!(safe_return_to(Some(ok)), ok);
        }
        for bad in [
            "",
            "map",
            "//evil.example",
            "/\\evil.example",
            "/a\\b",
            "https://evil.example",
            "/a\nb",
            "/a\u{7f}",
        ] {
            assert_eq!(safe_return_to(Some(bad)), "/", "{bad:?}");
        }
        assert_eq!(safe_return_to(None), "/");
        let long = format!("/{}", "é".repeat(600));
        let cut = safe_return_to(Some(&long));
        assert!(cut.len() <= RETURN_TO_BYTES && long.starts_with(&cut));
    }
}
