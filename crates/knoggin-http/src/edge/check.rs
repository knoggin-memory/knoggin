// SPDX-License-Identifier: AGPL-3.0-only
//! The credential check, steps 3 to 13 (tab "Sign-in and credentials", Credential check,
//! in order), the store selection of the store routes, the per-credential bucket, the
//! per-user caps, the bookkeeping writes, and the 60-second re-check of long transfers.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use knoggin_core::time::{MILLIS_PER_DAY, Timestamp};
use knoggin_core::{Sensitive, StoreId};
use knoggin_crypto::token::{API_KEY_PREFIX, hash_secret};
use knoggin_state::record::{ApiKey, DeletedBy, Permission, StoreRecord, User};
use knoggin_state::users::LOCAL_IDENTITY;
use knoggin_state::{NewStore, StateError, Versioned};
use uuid::Uuid;

use super::limits::{Cap, CredentialKey};
use super::{Arrival, AuthMode, EdgeState, Role};
use crate::ctx::{
    CredentialKind, CredentialRef, Permissions, RequestContext, headers, namespace_header,
};
use crate::error::{ApiError, Code};
use crate::local::SESSION_PREFIX;
use crate::store_api::export_file_name;

/// How often an export or import re-runs the credential check.
const RECHECK_EVERY: Duration = Duration::from_secs(60);

/// A key that expires within this many milliseconds gets `Knoggin-Key-Expires`.
const EXPIRY_WARNING_MS: i64 = 14 * MILLIS_PER_DAY;

/// The credential a request was resolved by.
#[derive(Clone)]
pub enum Credential {
    /// The daemon's Unix socket without an `Authorization` header.
    Local,
    /// A local web session, by the SHA-256 of its token.
    LocalSession([u8; 32]),
    /// An API key, with the SHA-256 of its whole string.
    ApiKey(Box<Versioned<ApiKey>>, [u8; 32]),
    /// A server with sign-in off and no credential.
    None,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Local => "local",
            Self::LocalSession(_) => "local_session",
            Self::ApiKey(..) => "api_key",
            Self::None => "none",
        };
        f.write_str(kind)
    }
}

impl Credential {
    /// The kind and ID for logs, rate limits and `savedBy`.
    pub fn reference(&self, user: knoggin_core::UserId) -> CredentialRef {
        match self {
            Self::Local => CredentialRef {
                kind: CredentialKind::Local,
                id: None,
            },
            Self::LocalSession(_) => CredentialRef {
                kind: CredentialKind::Session,
                id: Some(user.uuid()),
            },
            Self::ApiKey(k, _) => CredentialRef {
                kind: CredentialKind::ApiKey,
                id: Some(k.value.id),
            },
            Self::None => CredentialRef {
                kind: CredentialKind::None,
                id: None,
            },
        }
    }

    /// Whether this credential reaches the control routes: the socket, a session, or none.
    pub fn is_control(&self) -> bool {
        match self {
            Self::Local | Self::LocalSession(_) | Self::None => true,
            Self::ApiKey(..) => false,
        }
    }

    /// The API key and its hash, for the steps that only an API key takes.
    fn api_key(&self) -> Option<(&Versioned<ApiKey>, &[u8; 32])> {
        match self {
            Self::ApiKey(k, hash) => Some((k, hash)),
            Self::Local | Self::LocalSession(_) | Self::None => None,
        }
    }

    fn bucket(&self) -> Option<CredentialKey> {
        match self {
            Self::Local => None,
            Self::LocalSession(h) => Some(CredentialKey::LocalSession(*h)),
            Self::ApiKey(k, _) => Some(CredentialKey::Key(k.value.id)),
            Self::None => Some(CredentialKey::None),
        }
    }
}

/// Who a request is, after the credential check. Control routes read it from the request
/// extensions.
#[derive(Debug, Clone)]
pub struct Caller {
    /// The user's record as read for this request.
    pub user: Versioned<User>,
    /// The credential.
    pub credential: Credential,
    /// Worked out for this request.
    pub is_admin: bool,
}

/// Whether a request carries a credential that this mode reads: an `Authorization` header,
/// or in mode `oauth` a cookie (step 8). In mode `none` a cookie is ignored, so a request
/// with only a cookie is charged to the fixed listener's bucket at step 1.
pub(super) fn sends_credential(map: &HeaderMap, mode: AuthMode) -> bool {
    map.contains_key(header::AUTHORIZATION)
        || match mode {
            AuthMode::Oauth => map.contains_key(header::COOKIE),
            AuthMode::None => false,
        }
}

/// What a request presented, kept so that a transfer can be checked again.
#[derive(Clone)]
struct Presented {
    arrival: Arrival,
    bearer: Option<Sensitive<String>>,
    malformed: bool,
    /// See [`sends_credential`].
    sent: bool,
}

impl Presented {
    fn read(arrival: Arrival, map: &HeaderMap, mode: AuthMode) -> Self {
        let mut values = map.get_all(header::AUTHORIZATION).iter();
        let (bearer, malformed) = match (values.next(), values.next()) {
            (None, _) => (None, false),
            (Some(v), None) => match v.to_str().ok().and_then(|t| t.strip_prefix("Bearer ")) {
                Some(t) if !t.is_empty() => (Some(Sensitive::new(t.trim().to_owned())), false),
                _ => (None, true),
            },
            (Some(_), Some(_)) => (None, true),
        };
        Self {
            arrival,
            bearer,
            malformed,
            sent: sends_credential(map, mode),
        }
    }
}

/// How a control route may be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    /// Needs no credential.
    Open,
    /// `GET /v1/session`: runs the later steps when a credential is sent.
    Session,
    /// Works while the account is scheduled for deletion by its owner.
    WhileDeleted,
    /// Needs an admin.
    Admin,
    /// Every other control route.
    Control,
}

fn access(method: &Method, path: &str) -> Access {
    match (method, path) {
        (&Method::GET, "/healthz" | "/readyz" | "/v1/openapi.json")
        | (&Method::POST, "/v1/local/session") => Access::Open,
        (&Method::GET, "/v1/session") => Access::Session,
        (&Method::POST, "/v1/account/undelete" | "/auth/logout" | "/v1/local/browser-code")
        | (&Method::DELETE, "/v1/account") => Access::WhileDeleted,
        _ if path.starts_with("/v1/users") || path.starts_with("/v1/admin/") => Access::Admin,
        _ => Access::Control,
    }
}

fn unauthenticated() -> ApiError {
    ApiError::new(
        Code::Unauthenticated,
        "The credential is missing, unknown, expired or revoked",
    )
}

/// The answer to a state database error: `unavailable`, or `internal` for a fault.
pub(crate) fn state_error(e: StateError) -> ApiError {
    match e {
        StateError::Unavailable(_) | StateError::Conflict => ApiError::unavailable(1),
        other => {
            let kind = match other {
                StateError::Corrupt(what) | StateError::Invalid(what) => what,
                _ => "state",
            };
            tracing::error!(kind, "a state database call failed");
            ApiError::internal()
        }
    }
}

impl EdgeState {
    /// Steps 3 to 11 for one request. `Ok(None)` is a request without any credential.
    /// `touch` is false for the re-check of an open transfer, which must not keep a local
    /// web session alive.
    async fn resolve(
        &self,
        p: &Presented,
        access: Access,
        touch: bool,
    ) -> Result<Option<Caller>, ApiError> {
        let inner = self.inner();
        let now = self.now();
        // Steps 3 to 9.
        let credential = if p.malformed {
            return Err(unauthenticated());
        } else if let Some(bearer) = &p.bearer {
            let value = bearer.expose();
            if value.starts_with(SESSION_PREFIX) {
                // Step 5: only on the daemon's local web listener.
                let local = inner
                    .local
                    .as_ref()
                    .filter(|_| matches!(p.arrival, Arrival::Web { .. }));
                let hash = local
                    .and_then(|l| l.check(value, now, touch))
                    .ok_or_else(unauthenticated)?;
                Credential::LocalSession(hash)
            } else if let Some(random) = value.strip_prefix(API_KEY_PREFIX) {
                // Step 6.
                if random.len() != 43
                    || !random
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err(unauthenticated());
                }
                let hash = hash_secret(value);
                let key = inner
                    .api_keys
                    .get(&hash)
                    .await
                    .map_err(state_error)?
                    .filter(|k| k.value.is_active(now))
                    .ok_or_else(unauthenticated)?;
                Credential::ApiKey(Box::new(key), hash)
            } else {
                // Step 7. Mode `none` has nothing to check such a value against.
                // Milestone 5: in mode `oauth`, check it as a JWT here.
                return Err(unauthenticated());
            }
        } else if p.arrival == Arrival::Socket {
            // Step 4.
            Credential::Local
        } else if inner.config.role == Role::Server && inner.config.auth_mode == AuthMode::None {
            // Step 9. (Step 8, the session cookie, exists in mode `oauth` only: milestone 5.)
            Credential::None
        } else {
            return Ok(None);
        };
        let user_id = match &credential {
            Credential::ApiKey(k, _) => k.value.user_id,
            Credential::Local | Credential::LocalSession(_) | Credential::None => {
                inner.local_user.ok_or_else(ApiError::internal)?
            }
        };
        let mut user = inner
            .users
            .get(user_id)
            .await
            .map_err(state_error)?
            .ok_or_else(unauthenticated)?;
        if user.value.issuer == LOCAL_IDENTITY {
            user = self.keep_personal_store(user).await?;
        }
        // Step 10.
        if user.value.deleted_at_ms.is_some() {
            match user.value.deleted_by {
                Some(DeletedBy::Admin) => {
                    return Err(ApiError::permission_denied(
                        "This account was removed by an admin",
                    )
                    .with_reason("account_removed"));
                }
                _ if access != Access::WhileDeleted && access != Access::Session => {
                    return Err(ApiError::store_deleting());
                }
                _ => {}
            }
        }
        // Step 11 for an API key: its store must be live.
        if let Some((k, _)) = credential.api_key() {
            let store = inner
                .stores
                .get_of(user_id, k.value.store_id)
                .await
                .map_err(state_error)?
                .ok_or_else(unauthenticated)?;
            if store.value.deleted_at_ms.is_some() {
                return Err(ApiError::store_deleting());
            }
        }
        // On a local install, and on a server with sign-in off, the one user is an admin.
        // Milestone 5: on a server with sign-in on, decide from `auth.admin_*` here.
        let is_admin = user.value.issuer == LOCAL_IDENTITY;
        Ok(Some(Caller {
            user,
            credential,
            is_admin,
        }))
    }

    /// The daemon's erase step removes a personal store whose user deleted their data; the
    /// next request makes a new, empty one, which also clears the deletion mark.
    async fn keep_personal_store(
        &self,
        user: Versioned<User>,
    ) -> Result<Versioned<User>, ApiError> {
        let inner = self.inner();
        let personal = inner
            .stores
            .get_of(user.value.id, user.value.personal_store_id)
            .await
            .map_err(state_error)?;
        if personal.is_some() {
            return Ok(user);
        }
        let id = StoreId::from_uuid(Uuid::now_v7());
        let (_, wrapped) = inner.keys.generate(id).await.map_err(|_| {
            tracing::error!(kind = "key", "could not make a data key");
            ApiError::internal()
        })?;
        let store = NewStore {
            id,
            name: Sensitive::new("Personal".into()),
            wrapped,
        };
        match inner
            .users
            .replace_personal_store(&user, store, self.now())
            .await
        {
            Ok(user) => Ok(user),
            // Another request replaced it first.
            Err(StateError::Conflict) => inner
                .users
                .get(user.value.id)
                .await
                .map_err(state_error)?
                .ok_or_else(unauthenticated),
            Err(e) => Err(state_error(e)),
        }
    }

    /// Charges a refused credential to the daemon's fixed bucket (step 1).
    fn refused(&self, p: &Presented, e: ApiError) -> ApiError {
        if matches!(p.arrival, Arrival::Fixed { .. })
            && p.sent
            && let Err(limited) = self.inner().unauthenticated.take(&(), "address")
        {
            return limited;
        }
        e
    }

    /// Step 6's `Knoggin-Key-Expires` value, for an API key that expires within 14 days.
    fn key_expires(&self, credential: &Credential) -> Option<HeaderValue> {
        let (k, _) = credential.api_key()?;
        if k.value.key_expires_ms - self.now() > EXPIRY_WARNING_MS {
            return None;
        }
        let text = Timestamp::from_millis(k.value.key_expires_ms).to_rfc3339()?;
        HeaderValue::from_str(&text).ok()
    }

    /// Step 12, the bucket per credential.
    fn rate_limit(&self, caller: &Caller) -> Result<(), ApiError> {
        match caller.credential.bucket() {
            Some(key) => self.inner().per_credential.take(&key, "credential"),
            None => Ok(()),
        }
    }

    /// The bookkeeping writes of a request that passed the whole check; conflicts and
    /// failures are ignored.
    async fn bookkeeping(&self, caller: &Caller) {
        let inner = self.inner();
        let now = self.now();
        if inner.users.touch_active(&caller.user, now).await.is_err() {
            tracing::debug!(kind = "bookkeeping", "last_active_day not written");
        }
        if let Some((k, hash)) = caller.credential.api_key()
            && inner.api_keys.touch_used(k, hash, now).await.is_err()
        {
            tracing::debug!(kind = "bookkeeping", "last_used_day not written");
        }
    }

    /// The store a store route acts on (step 11), with the export's `store` parameter.
    async fn select_store(
        &self,
        caller: &Caller,
        map: &HeaderMap,
        store_param: Option<&str>,
    ) -> Result<Versioned<StoreRecord>, ApiError> {
        let inner = self.inner();
        let header = {
            let mut values = map.get_all(&headers::STORE).iter();
            match (values.next(), values.next()) {
                (None, _) => None,
                (Some(v), None) => Some(v.to_str().unwrap_or("").to_owned()),
                (Some(_), Some(_)) => {
                    return Err(ApiError::invalid_argument(
                        "Knoggin-Store",
                        "the Knoggin-Store header may be sent once",
                    ));
                }
            }
        };
        let user = caller.user.value.id;
        let named = match &caller.credential {
            Credential::ApiKey(k, _) => match (header, store_param) {
                (Some(_), _) => {
                    return Err(ApiError::invalid_argument(
                        "Knoggin-Store",
                        "an API key reaches its own store only; do not send Knoggin-Store",
                    ));
                }
                (None, Some(_)) => {
                    return Err(ApiError::permission_denied(
                        "the store parameter is accepted only from a session",
                    ));
                }
                (None, None) => k.value.store_id,
            },
            Credential::Local | Credential::LocalSession(_) | Credential::None => {
                match (header, store_param) {
                    (Some(_), Some(_)) => {
                        return Err(ApiError::invalid_argument(
                            "store",
                            "send the store parameter or the Knoggin-Store header, not both",
                        ));
                    }
                    (Some(text), None) => parse_store(&text, "Knoggin-Store")?,
                    (None, Some(text)) => parse_store(text, "store")?,
                    (None, None) => caller.user.value.personal_store_id,
                }
            }
        };
        let store = inner
            .stores
            .get_of(user, named)
            .await
            .map_err(state_error)?
            .ok_or_else(|| ApiError::not_found("No such store"))?;
        if store.value.deleted_at_ms.is_some() {
            return Err(ApiError::store_deleting());
        }
        Ok(store)
    }

    /// Steps 5 to 11 once more, for a transfer that is still open.
    async fn recheck(&self, p: &Presented, store: StoreId) -> Result<(), ApiError> {
        let caller = self
            .resolve(p, Access::Control, false)
            .await?
            .ok_or_else(unauthenticated)?;
        let record = self
            .inner()
            .stores
            .get_of(caller.user.value.id, store)
            .await
            .map_err(state_error)?;
        match record {
            Some(r) if r.value.deleted_at_ms.is_none() => Ok(()),
            _ => Err(ApiError::store_deleting()),
        }
    }
}

fn parse_store(text: &str, field: &str) -> Result<StoreId, ApiError> {
    Uuid::parse_str(text)
        .map(StoreId::from_uuid)
        .map_err(|_| ApiError::invalid_argument(field, "the store must be named by its ID"))
}

fn arrival(request: &Request) -> Result<Arrival, ApiError> {
    request
        .extensions()
        .get::<Arrival>()
        .cloned()
        .ok_or_else(ApiError::internal)
}

fn route(request: &Request) -> String {
    request.extensions().get::<MatchedPath>().map_or_else(
        || request.uri().path().to_owned(),
        |m| m.as_str().to_owned(),
    )
}

/// The check in front of the control routes and the mode routes.
pub(crate) async fn control(
    State(edge): State<EdgeState>,
    request: Request,
    next: Next,
) -> Response {
    let arrival = match arrival(&request) {
        Ok(a) => a,
        Err(e) => return e.into_response(),
    };
    let access = access(request.method(), &route(&request));
    if access == Access::Open {
        return next.run(request).await;
    }
    let p = Presented::read(arrival, request.headers(), edge.inner().config.auth_mode);
    let resolved = edge.resolve(&p, access, true).await;
    let expires = match &resolved {
        Ok(Some(caller)) => edge.key_expires(&caller.credential),
        _ => None,
    };
    let mut response = control_steps(&edge, request, next, p, access, resolved).await;
    add_key_expires(&mut response, expires);
    response
}

/// The control check after step 11.
async fn control_steps(
    edge: &EdgeState,
    mut request: Request,
    next: Next,
    p: Presented,
    access: Access,
    resolved: Result<Option<Caller>, ApiError>,
) -> Response {
    if access == Access::Session {
        // The session route answers `signedIn: false` where a step would refuse.
        let seen = match resolved {
            Ok(Some(caller)) => match edge.rate_limit(&caller) {
                Ok(()) => {
                    edge.bookkeeping(&caller).await;
                    SessionSeen::Caller(Box::new(caller))
                }
                Err(e) => return e.into_response(),
            },
            Ok(None) => SessionSeen::Nobody,
            Err(e) => match signed_out_reason(&e) {
                Some(reason) => {
                    let charged = edge.refused(&p, e);
                    if charged.code == Code::RateLimited {
                        return charged.into_response();
                    }
                    SessionSeen::SignedOut(reason)
                }
                None => return e.into_response(),
            },
        };
        request.extensions_mut().insert(seen);
        return next.run(request).await;
    }
    let caller = match resolved {
        Ok(Some(c)) => c,
        Ok(None) => return edge.refused(&p, unauthenticated()).into_response(),
        Err(e) => return edge.refused(&p, e).into_response(),
    };
    // Step 12, by credential and by user.
    if let Err(e) = edge.rate_limit(&caller) {
        return e.into_response();
    }
    let _in_flight = match edge.inner().caps.take(caller.user.value.id, Cap::InFlight) {
        Ok(g) => g,
        Err(e) => return e.into_response(),
    };
    // Step 13: control routes need the socket, a session, or sign-in off.
    if !caller.credential.is_control() {
        return ApiError::permission_denied("This credential reaches memories only")
            .into_response();
    }
    if access == Access::Admin && !caller.is_admin {
        return ApiError::permission_denied("This route needs an admin").into_response();
    }
    edge.bookkeeping(&caller).await;
    request.extensions_mut().insert(caller);
    next.run(request).await
}

/// Puts `Knoggin-Key-Expires` on every answer to a request whose API key expires soon.
fn add_key_expires(response: &mut Response, expires: Option<HeaderValue>) {
    if let Some(value) = expires {
        response.headers_mut().insert("knoggin-key-expires", value);
    }
}

/// What the session route learned about the request's credential.
#[derive(Debug, Clone)]
pub(crate) enum SessionSeen {
    /// A credential that passed.
    Caller(Box<Caller>),
    /// No credential.
    Nobody,
    /// A credential that was refused, with the `signedOutReason`.
    SignedOut(&'static str),
}

fn signed_out_reason(e: &ApiError) -> Option<&'static str> {
    match (e.code, e.reason) {
        (Code::Unauthenticated | Code::StoreDeleting, _) => Some("expired"),
        (Code::PermissionDenied, Some(r @ ("account_removed" | "not_allowed"))) => Some(r),
        _ => None,
    }
}

/// The check in front of the store routes: the credential, the store, the context.
pub(crate) async fn store(
    State(edge): State<EdgeState>,
    mut request: Request,
    next: Next,
) -> Response {
    let started = std::time::Instant::now();
    let route = route(&request);
    let mut expires = None;
    let prepared = prepare(&edge, &mut request, &route, &mut expires).await;
    let mut response = match prepared {
        Ok(prepared) => {
            let mut response = run(&edge, request, next, prepared).await;
            add_key_expires(&mut response, expires);
            return response;
        }
        Err(e) => e.into_response(),
    };
    add_key_expires(&mut response, expires);
    // The store layer logs and counts the requests it runs; these never reach it.
    let code = response
        .extensions()
        .get::<ApiError>()
        .map_or("ok", |e| e.code.as_str());
    tracing::info!(
        route = %route,
        status = response.status().as_u16(),
        code,
        duration_ms = started.elapsed().as_millis() as u64,
        "request"
    );
    metrics::counter!("knoggin_requests_total", "route" => route.clone(), "code" => code)
        .increment(1);
    metrics::histogram!("knoggin_request_seconds", "route" => route)
        .record(started.elapsed().as_secs_f64());
    response
}

struct Prepared {
    store: Versioned<StoreRecord>,
    presented: Presented,
    guards: Vec<super::limits::CapGuard>,
    transfer: Option<Cap>,
}

/// Steps 3 to 12 of a store route; `expires` is set once an API key passed.
async fn prepare(
    edge: &EdgeState,
    request: &mut Request,
    route: &str,
    expires: &mut Option<HeaderValue>,
) -> Result<Prepared, ApiError> {
    let arrival = arrival(request)?;
    let p = Presented::read(arrival, request.headers(), edge.inner().config.auth_mode);
    let caller = match edge.resolve(&p, Access::Control, true).await {
        Ok(Some(c)) => c,
        Ok(None) => return Err(edge.refused(&p, unauthenticated())),
        Err(e) => return Err(edge.refused(&p, e)),
    };
    *expires = edge.key_expires(&caller.credential);
    let transfer = match route {
        "/v1/export" => Some(Cap::Export),
        "/v1/import" => Some(Cap::Import),
        _ => None,
    };
    let store_param = match transfer {
        Some(Cap::Export) => export_store_param(request.uri().query())?,
        _ => None,
    };
    let store = edge
        .select_store(&caller, request.headers(), store_param.as_deref())
        .await?;
    edge.rate_limit(&caller)?;
    let user = caller.user.value.id;
    let mut guards = vec![edge.inner().caps.take(user, Cap::InFlight)?];
    if let Some(cap) = transfer {
        guards.push(edge.inner().caps.take(user, cap)?);
    }
    let permissions = match &caller.credential {
        Credential::ApiKey(k, _) if !k.value.permissions.contains(&Permission::Write) => {
            Permissions::READ
        }
        Credential::ApiKey(..)
        | Credential::Local
        | Credential::LocalSession(_)
        | Credential::None => Permissions::READ_WRITE,
    };
    let ctx = RequestContext {
        request_id: crate::trace::request_id(request).unwrap_or_else(Uuid::now_v7),
        user_id: user,
        store_id: store.value.id,
        permissions,
        credential: caller.credential.reference(user),
        default_namespace: namespace_header(request.headers())?,
        client: None,
    };
    edge.bookkeeping(&caller).await;
    request.extensions_mut().insert(ctx);
    Ok(Prepared {
        store,
        presented: p,
        guards,
        transfer,
    })
}

/// The export's `store` parameter, read without consuming the query (the store layer
/// checks the rest).
fn export_store_param(query: Option<&str>) -> Result<Option<String>, ApiError> {
    let Some(query) = query else {
        return Ok(None);
    };
    let pairs: Vec<(String, String)> = query_pairs(query)?;
    let mut values = pairs
        .into_iter()
        .filter(|(k, _)| k == "store")
        .map(|(_, v)| v);
    match (values.next(), values.next()) {
        (v, None) => Ok(v),
        (_, Some(_)) => Err(ApiError::invalid_argument(
            "store",
            "the query parameter store may be given once",
        )),
    }
}

fn query_pairs(query: &str) -> Result<Vec<(String, String)>, ApiError> {
    let uri: http::Uri = format!("/?{query}")
        .parse()
        .map_err(|_| ApiError::invalid_argument("query", "the query string is malformed"))?;
    axum::extract::Query::<Vec<(String, String)>>::try_from_uri(&uri)
        .map(|q| q.0)
        .map_err(|_| ApiError::invalid_argument("query", "the query string is malformed"))
}

async fn run(edge: &EdgeState, mut request: Request, next: Next, prepared: Prepared) -> Response {
    let Prepared {
        store,
        presented,
        guards,
        transfer,
    } = prepared;
    let store_id = store.value.id;
    let recheck = {
        let edge = edge.clone();
        let p = presented.clone();
        move || -> BoxFuture<'static, bool> {
            let edge = edge.clone();
            let p = p.clone();
            Box::pin(async move { edge.recheck(&p, store_id).await.is_ok() })
        }
    };
    if transfer == Some(Cap::Import) {
        let body = std::mem::take(request.body_mut());
        *request.body_mut() = rechecked(body, recheck.clone(), RECHECK_EVERY);
    }
    let mut response = next.run(request).await;
    let now = edge.now();
    if transfer == Some(Cap::Export) && response.status() == StatusCode::OK {
        let date = Timestamp::from_millis(now).utc_date().unwrap_or_default();
        let name = export_file_name(store.value.name.expose(), &date);
        if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
            response
                .headers_mut()
                .insert(header::CONTENT_DISPOSITION, value);
        }
        // The caps stay taken while the body streams, and the credential is checked again.
        let (parts, body) = response.into_parts();
        let body = rechecked(body, recheck, RECHECK_EVERY);
        let body = Body::from_stream(body.into_data_stream().map(move |frame| {
            let _held = &guards;
            frame
        }));
        return Response::from_parts(parts, body);
    }
    drop(guards);
    response
}

/// `body`, checked again every `every` by `check`; when a check fails the body ends with
/// an error, which closes the connection.
pub(crate) fn rechecked<F>(body: Body, check: F, every: Duration) -> Body
where
    F: Fn() -> BoxFuture<'static, bool> + Send + Sync + 'static,
{
    let check = Arc::new(check);
    let start = tokio::time::Instant::now() + every;
    let stream = futures_util::stream::unfold(
        (body.into_data_stream(), start, check, false),
        move |(mut inner, mut next, check, done)| async move {
            if done {
                return None;
            }
            loop {
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(next) => {
                        if !check().await {
                            tracing::info!(kind = "recheck", "a transfer's credential no longer passes");
                            let e = axum::Error::new(std::io::Error::other("the credential no longer passes"));
                            return Some((Err(e), (inner, next, check, true)));
                        }
                        next += every;
                    }
                    item = inner.next() => {
                        return item.map(|i| (i, (inner, next, check, false)));
                    }
                }
            }
        },
    );
    Body::from_stream(stream)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use bytes::Bytes;

    use super::*;

    #[test]
    fn route_access() {
        assert_eq!(access(&Method::GET, "/healthz"), Access::Open);
        assert_eq!(access(&Method::POST, "/v1/local/session"), Access::Open);
        assert_eq!(access(&Method::GET, "/v1/session"), Access::Session);
        assert_eq!(access(&Method::DELETE, "/v1/account"), Access::WhileDeleted);
        assert_eq!(access(&Method::GET, "/v1/users/{id}/stores"), Access::Admin);
        assert_eq!(access(&Method::GET, "/v1/admin/status"), Access::Admin);
        assert_eq!(access(&Method::POST, "/v1/stores"), Access::Control);
    }

    #[test]
    fn authorization_forms() {
        let mut map = HeaderMap::new();
        map.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert!(Presented::read(Arrival::Public, &map, AuthMode::None).malformed);
        map.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        let p = Presented::read(Arrival::Public, &map, AuthMode::None);
        assert_eq!(p.bearer.unwrap().expose(), "abc");
        map.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer def"),
        );
        assert!(Presented::read(Arrival::Public, &map, AuthMode::None).malformed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_transfer_ends_when_its_credential_stops_passing() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
        let body = Body::from_stream(tokio_stream_from(rx));
        let ok = Arc::new(AtomicBool::new(true));
        let flag = ok.clone();
        let body = rechecked(
            body,
            move || {
                let flag = flag.clone();
                Box::pin(async move { flag.load(Ordering::SeqCst) })
            },
            RECHECK_EVERY,
        );
        let mut body = body.into_data_stream();
        tx.send(Ok(Bytes::from_static(b"one"))).await.unwrap();
        assert_eq!(body.next().await.unwrap().unwrap(), "one");
        // A check that passes lets the transfer go on.
        tokio::time::advance(Duration::from_secs(61)).await;
        tx.send(Ok(Bytes::from_static(b"two"))).await.unwrap();
        assert_eq!(body.next().await.unwrap().unwrap(), "two");
        // A revoked credential ends it at the next check, within a minute.
        ok.store(false, Ordering::SeqCst);
        let next = body.next().await.unwrap();
        assert!(next.is_err());
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn the_transfer_recheck_does_not_keep_a_local_session_alive() {
        use knoggin_crypto::KeyProvider;
        use knoggin_state::testing::{TestClock, test_field_keys};
        use knoggin_state::{LocalFileDb, StateDb, Users};

        use crate::edge::{EdgeConfig, EdgeParts};
        use crate::local::LocalSessions;

        let clock = TestClock::new();
        let db: Arc<dyn StateDb> = Arc::new(LocalFileDb::in_memory(clock.clock()));
        let fields = Arc::new(test_field_keys());
        let keys: Arc<dyn KeyProvider> = Arc::new(knoggin_crypto::testing::test_provider());
        let users = Users::new(db.clone(), fields.clone());
        let user = crate::local::ensure_local_user(&users, &*keys, clock.now())
            .await
            .unwrap();
        let personal = users
            .get(user)
            .await
            .unwrap()
            .unwrap()
            .value
            .personal_store_id;
        let local = Arc::new(LocalSessions::new("http://127.0.0.1:1".into()));
        let parts = EdgeParts {
            db,
            field_keys: fields,
            keys,
            clock: clock.clock(),
            host: None,
            local: Some(local.clone()),
        };
        let config = EdgeConfig::local("http://127.0.0.1:7690".into(), "0.1.0".into());
        let edge = EdgeState::new(config, parts).await.unwrap();
        let code = local.new_code(clock.now()).unwrap();
        let token = local.redeem(&code, clock.now()).unwrap().unwrap();
        let mut map = HeaderMap::new();
        map.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        let web = Arrival::Web {
            authority: "127.0.0.1:1".into(),
        };
        let p = Presented::read(web, &map, AuthMode::None);
        clock.advance(23 * MILLIS_PER_DAY / 24);
        assert!(edge.recheck(&p, personal).await.is_ok());
        clock.advance(2 * MILLIS_PER_DAY / 24);
        assert!(
            edge.recheck(&p, personal).await.is_err(),
            "24 hours after the last request the session is idle, re-checks or not"
        );
    }

    fn tokio_stream_from(
        rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
    ) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
        futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|i| (i, rx)) })
    }
}
