// SPDX-License-Identifier: AGPL-3.0-only
//! The edge layer: it checks the credential, resolves it to a user, one store and
//! permissions, applies rate limits and the browser checks, and serves the control routes.
//! The store layer behind it reads only the `RequestContext` the edge puts in the request
//! extensions.
//!
//! Both sign-in modes: the local socket, local web sessions, API keys, and with `oauth`
//! access tokens, web sign-in and cookie sessions (`oauth`, `signin`).
//!
//! Every request passes these steps in order; comments in this module and in `check`
//! name them by number:
//!
//! 1. The address buckets: the router's fixed ceiling per client address counts every
//!    request; the daemon's fixed bucket and the router's bucket per address count a
//!    request without a credential or with a refused one.
//! 2. The browser checks (`browser`).
//! 3. A malformed `Authorization` header (two of them, or no `Bearer` token) is refused.
//! 4. The local socket: the install's own user.
//! 5. A local web session, only on the daemon's local web listener.
//! 6. An API key: its form, then an active key record.
//! 7. An access token, in mode `oauth` only.
//! 8. The session cookie, in mode `oauth` only.
//! 9. On a server with sign-in off, a request without a credential.
//! 10. A deleted account: refused, except on the routes a deleted account may still use.
//! 11. The store: an API key's store must be live, and a store route selects its store.
//! 12. The bucket per credential and, on the router, the bucket per user.
//! 13. Control routes need the socket, a session, or sign-in off.

mod browser;
mod check;
mod control;
pub mod guard;
pub mod limits;
pub mod listen;
pub mod oauth;
pub mod openapi;
pub(crate) mod shapes;
mod signin;
pub(crate) mod ui;

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};
use knoggin_core::UserId;
use knoggin_crypto::KeyProvider;
use knoggin_state::{
    ApiKeys, Clock, FieldKeys, Leases, Sessions, StateDb, StateError, Stores, Users,
};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::RequestId;
use tower_http::timeout::TimeoutLayer;
use uuid::Uuid;

pub(crate) use self::check::state_error;
pub use self::check::{Caller, Credential};
pub use self::limits::Rate;
use self::limits::{Buckets, Caps, CredentialKey};
use self::oauth::Provider;
pub use self::oauth::{DiscoveryError, OauthConfig};
use crate::ctx::headers;
use crate::error::{ApiError, shape_answer};
use crate::host::StoreHost;
use crate::local::LocalSessions;
use crate::settings::{DEFAULT_BODY_BYTES, StoreLimits};

/// The client address of a request on the router's public listener: the peer, or behind
/// a trusted proxy the right-most forwarded address that is not one. The router's
/// listener puts it in the request extensions; the per-address limits key on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientAddr(pub IpAddr);

/// Which listener a request arrived on. Each listener tags its requests (see [`listen`]);
/// a request without a tag is refused as `internal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arrival {
    /// The daemon's Unix socket.
    Socket,
    /// The daemon's fixed loopback TCP listener (`server.bind`), with its port.
    Fixed {
        /// The bound port.
        port: u16,
    },
    /// The daemon's local web listener, with this run's web origin's host and port.
    Web {
        /// `<label>.localhost:<port>` or `127.0.0.1:<port>`.
        authority: String,
    },
    /// The router's public listener.
    Public,
}

/// Where the edge runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The local daemon.
    Local,
    /// The router.
    Server,
}

/// `auth.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// Sign-in off.
    None,
    /// Sign-in with an identity provider.
    Oauth,
}

impl AuthMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Oauth => "oauth",
        }
    }
}

/// The settings the edge reads.
#[derive(Debug, Clone)]
pub struct EdgeConfig {
    /// Daemon or router.
    pub role: Role,
    /// `auth.mode`.
    pub auth_mode: AuthMode,
    /// `server.public_url`, if set.
    pub public_url: Option<String>,
    /// The daemon's own fixed address (`http://127.0.0.1:7690`), the session's `publicUrl`
    /// when `server.public_url` is not set.
    pub own_url: String,
    /// `source_url`.
    pub source_url: String,
    /// The build's version.
    pub version: String,
    /// `retention.days`.
    pub retention_days: u32,
    /// `retention.soft_delete_days`.
    pub soft_delete_days: u32,
    /// `containers.idle_seconds`; `None` on a local install.
    pub idle_seconds: Option<u64>,
    /// `limits.per_credential_rps` and `_burst`.
    pub per_credential: Rate,
    /// The daemon's fixed bucket for requests that do not pass the credential check.
    pub unauthenticated: Rate,
    /// The router's `limits.per_address_rps` and `_burst`, charged on the public listener
    /// for a request without a credential or with a refused one, beside a fixed ceiling
    /// of ten times that rate and burst that counts every request. `None` on the daemon.
    pub per_address: Option<Rate>,
    /// The router's `limits.per_user_rps` and `_burst`, over all of a user's credentials
    /// (step 12). `None` on the daemon.
    pub per_user: Option<Rate>,
    /// The request timeout of the control routes.
    pub limits: StoreLimits,
    /// `KNOGGIN_TEST_MODE=1`: an `http` issuer and redirects from it are accepted.
    pub test_mode: bool,
    /// The `auth.*` settings; required with mode `oauth`.
    pub oauth: Option<OauthConfig>,
}

impl EdgeConfig {
    /// The local daemon's defaults in mode `none`.
    pub fn local(own_url: String, version: String) -> Self {
        Self {
            role: Role::Local,
            auth_mode: AuthMode::None,
            public_url: None,
            own_url,
            source_url: "https://github.com/knoggin-memory/knoggin".into(),
            version,
            retention_days: 7,
            soft_delete_days: 7,
            idle_seconds: None,
            per_credential: Rate {
                per_second: 20.0,
                burst: 40.0,
            },
            unauthenticated: Rate {
                per_second: 20.0,
                burst: 20.0,
            },
            per_address: None,
            per_user: None,
            limits: StoreLimits::default(),
            test_mode: false,
            oauth: None,
        }
    }
}

/// What the edge needs, shared by every listener of the process.
#[derive(Clone)]
pub struct EdgeState(Arc<Inner>);

pub(crate) struct Inner {
    pub(crate) config: EdgeConfig,
    pub(crate) public: Option<browser::Origin>,
    pub(crate) users: Users,
    pub(crate) stores: Stores,
    pub(crate) api_keys: ApiKeys,
    pub(crate) sessions: Sessions,
    pub(crate) leases: Leases,
    pub(crate) keys: Arc<dyn KeyProvider>,
    pub(crate) fields: Arc<FieldKeys>,
    pub(crate) oauth: Option<Arc<Provider>>,
    pub(crate) clock: Clock,
    pub(crate) host: Option<Arc<StoreHost>>,
    pub(crate) local: Option<Arc<LocalSessions>>,
    pub(crate) local_user: Option<UserId>,
    pub(crate) per_credential: Buckets<CredentialKey>,
    pub(crate) unauthenticated: Buckets<()>,
    pub(crate) per_address: Option<Buckets<IpAddr>>,
    pub(crate) address_ceiling: Option<Buckets<IpAddr>>,
    pub(crate) per_user: Option<Buckets<UserId>>,
    pub(crate) caps: Caps,
    pub(crate) ready: AtomicBool,
}

impl std::fmt::Debug for EdgeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeState")
            .field("role", &self.0.config.role)
            .finish_non_exhaustive()
    }
}

/// Everything [`EdgeState::new`] takes besides the settings.
#[derive(Clone)]
pub struct EdgeParts {
    /// The state database.
    pub db: Arc<dyn StateDb>,
    /// The field keys, unwrapped at start.
    pub field_keys: Arc<FieldKeys>,
    /// Makes the data key of a new store.
    pub keys: Arc<dyn KeyProvider>,
    /// Milliseconds since 1970; tests pass a clock they move.
    pub clock: Clock,
    /// The daemon's store host: deleted stores are dropped from it, and the Store shape
    /// reads its live counts. `None` on the router.
    pub host: Option<Arc<StoreHost>>,
    /// The daemon's local web sessions. `None` on the router.
    pub local: Option<Arc<LocalSessions>>,
}

impl std::fmt::Debug for EdgeParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeParts").finish_non_exhaustive()
    }
}

impl EdgeState {
    /// The edge over `parts`. On the daemon, and on a server with sign-in off, the one
    /// user with issuer and subject `local` must exist (see `local::ensure_local_user`).
    pub async fn new(config: EdgeConfig, parts: EdgeParts) -> Result<Self, StateError> {
        let users = Users::new(parts.db.clone(), parts.field_keys.clone());
        let local_user = if config.role == Role::Local || config.auth_mode == AuthMode::None {
            let user = users
                .find(
                    knoggin_state::users::LOCAL_IDENTITY,
                    knoggin_state::users::LOCAL_IDENTITY,
                )
                .await?
                .ok_or(StateError::NotFound)?;
            Some(user.value.id)
        } else {
            None
        };
        let public = config
            .public_url
            .as_deref()
            .and_then(browser::Origin::parse);
        let oauth = match (config.auth_mode, &config.oauth, &config.public_url) {
            (AuthMode::None, _, _) => None,
            (AuthMode::Oauth, Some(o), Some(url)) => Some(Arc::new(
                Provider::new(
                    o.clone(),
                    url.clone(),
                    config.test_mode,
                    parts.clock.clone(),
                )
                .map_err(|_| StateError::Invalid("the identity provider's client"))?,
            )),
            (AuthMode::Oauth, _, _) => {
                return Err(StateError::Invalid(
                    "auth.mode oauth needs the auth settings and server.public_url",
                ));
            }
        };
        Ok(Self(Arc::new(Inner {
            fields: parts.field_keys.clone(),
            oauth,
            public,
            stores: Stores::new(parts.db.clone(), parts.field_keys.clone()),
            api_keys: ApiKeys::new(parts.db.clone(), parts.field_keys.clone()),
            sessions: Sessions::new(parts.db.clone(), parts.field_keys.clone()),
            leases: Leases::new(parts.db.clone()),
            users,
            keys: parts.keys,
            clock: parts.clock,
            host: parts.host,
            local: parts.local,
            local_user,
            per_credential: Buckets::new(config.per_credential),
            unauthenticated: Buckets::new(config.unauthenticated),
            per_address: config.per_address.map(Buckets::new),
            address_ceiling: config.per_address.map(|r| {
                Buckets::new(Rate {
                    per_second: r.per_second * 10.0,
                    burst: r.burst * 10.0,
                })
            }),
            per_user: config.per_user.map(Buckets::new),
            caps: Caps::default(),
            ready: AtomicBool::new(true),
            config,
        })))
    }

    /// One discovery attempt at the identity provider (mode `oauth`; nothing otherwise).
    /// Until one works, the sign-in routes and access tokens answer `unavailable`.
    pub async fn discover(&self) -> Result<(), DiscoveryError> {
        match &self.0.oauth {
            Some(o) => o.discover().await,
            None => Ok(()),
        }
    }

    /// Whether discovery has worked (always true without sign-in).
    pub fn discovered(&self) -> bool {
        self.0
            .oauth
            .as_ref()
            .is_none_or(|o| o.discovery().is_some())
    }

    /// Sets what `/readyz` answers.
    pub fn set_ready(&self, ready: bool) {
        self.0.ready.store(ready, Ordering::Relaxed);
    }

    /// Step 1 on the listeners that have an address bucket: the daemon's fixed bucket, or
    /// the router's bucket per client address. Other listeners are not charged.
    pub(crate) fn charge_address(
        &self,
        arrival: &Arrival,
        client: Option<ClientAddr>,
    ) -> Result<(), ApiError> {
        match (arrival, &self.0.per_address, client) {
            (Arrival::Fixed { .. }, _, _) => self.0.unauthenticated.take(&(), "address"),
            (Arrival::Public, Some(b), Some(ClientAddr(ip))) => {
                b.take(&limits::address_key(ip), "address")
            }
            _ => Ok(()),
        }
    }

    /// Gives back what [`Self::charge_address`] took, for a sign-in that worked.
    pub(crate) fn give_back_address(&self, arrival: &Arrival, client: Option<ClientAddr>) {
        match (arrival, &self.0.per_address, client) {
            (Arrival::Fixed { .. }, _, _) => self.0.unauthenticated.give_back(&()),
            (Arrival::Public, Some(b), Some(ClientAddr(ip))) => {
                b.give_back(&limits::address_key(ip));
            }
            _ => {}
        }
    }

    pub(crate) fn now(&self) -> i64 {
        (self.0.clock)()
    }

    pub(crate) fn inner(&self) -> &Inner {
        &self.0
    }
}

/// The whole API of one process: `store_routes` (the store layer, or the router's
/// forwarder) and `mode_routes` (the three control routes built per mode, and on the
/// daemon the local web session routes) behind the credential check, beside the control
/// routes, with request IDs, the browser checks and the response headers around all.
pub fn edge_layer(edge: EdgeState, store_routes: Router, mode_routes: Router) -> Router {
    let limits = edge.0.config.limits.clone();
    let control = control::routes(edge.clone())
        .merge(signin::routes(edge.clone()))
        .merge(mode_routes)
        .layer(DefaultBodyLimit::max(DEFAULT_BODY_BYTES))
        .layer(RequestBodyLimitLayer::new(DEFAULT_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            Duration::from_millis(limits.request_timeout_ms),
        ))
        .layer(middleware::from_fn_with_state(edge.clone(), check::control))
        .layer(middleware::from_fn(crate::trace::observe));
    let store = store_routes.layer(middleware::from_fn_with_state(edge.clone(), check::store));
    control
        .merge(store)
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(edge, entry))
}

/// What no route takes: the web UI's files for a `GET` off the API's paths, else
/// `not_found`.
pub(crate) async fn fallback(request: Request) -> Response {
    if ui::is_ui(&request) {
        ui::serve(&request)
    } else {
        ApiError::not_found("No such route").into_response()
    }
}

/// The outermost step of every request: the request ID, the tag check, the daemon's fixed
/// bucket and the browser checks (steps 1 and 2), then the response headers and the error
/// shape on the way out.
async fn entry(State(edge): State<EdgeState>, mut request: Request, next: Next) -> Response {
    // An outer layer (the router's panic guard) may have made the ID already.
    let id = request
        .extensions()
        .get::<RequestId>()
        .and_then(|r| r.header_value().to_str().ok())
        .and_then(|t| Uuid::parse_str(t).ok())
        .unwrap_or_else(Uuid::now_v7);
    if let Ok(value) = HeaderValue::from_str(&id.to_string()) {
        request.extensions_mut().insert(RequestId::new(value));
    }
    let arrival = request.extensions().get::<Arrival>().cloned();
    let challenge = edge.challenge(&request);
    let response = match arrival {
        None => {
            tracing::error!(
                kind = "no_arrival",
                "a request arrived without its listener's tag"
            );
            ApiError::internal().into_response()
        }
        Some(arrival) => {
            // Step 1: the router's fixed ceiling per address counts every request; the
            // daemon's fixed bucket and the router's bucket per address count a request
            // without a credential (the UI's files are not counted). Then step 2, the
            // browser checks.
            let client = request.extensions().get::<ClientAddr>().copied();
            let ceiling = match (&arrival, &edge.0.address_ceiling, client) {
                (Arrival::Public, Some(b), Some(ClientAddr(ip))) => {
                    b.take(&limits::address_key(ip), "address")
                }
                _ => Ok(()),
            };
            let charged = ceiling.and_then(|()| {
                if !ui::is_ui(&request)
                    && !check::sends_credential(request.headers(), edge.session_cookie())
                {
                    edge.charge_address(&arrival, client)
                } else {
                    Ok(())
                }
            });
            match charged.and_then(|()| browser::check(&request, &arrival, edge.0.public.as_ref()))
            {
                Ok(()) => next.run(request).await,
                Err(e) => e.into_response(),
            }
        }
    };
    let mut response = finish(response, id);
    if let Some(value) = challenge
        && response.status() == StatusCode::UNAUTHORIZED
    {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

impl EdgeState {
    /// With mode `oauth`, the `WWW-Authenticate` value that an `unauthenticated` answer on
    /// `/mcp` and `/v1` carries: where the protected-resource metadata is, and
    /// `invalid_token` when a token was sent.
    fn challenge(&self, request: &Request) -> Option<HeaderValue> {
        let o = self.0.oauth.as_ref()?;
        let path = request.uri().path();
        let resource = if path == "/mcp/plugin" || path.starts_with("/mcp/plugin/") {
            "/mcp/plugin"
        } else if path == "/mcp" || path.starts_with("/mcp/") || path.starts_with("/v1/") {
            "/mcp"
        } else {
            return None;
        };
        let error = if request.headers().contains_key(header::AUTHORIZATION) {
            "error=\"invalid_token\", "
        } else {
            ""
        };
        HeaderValue::from_str(&format!(
            "Bearer {error}resource_metadata=\"{}/.well-known/oauth-protected-resource{resource}\"",
            o.public_url
        ))
        .ok()
    }
}

/// The browser checks without a configured public address, for the locked router.
pub(crate) fn browser_check(request: &Request, arrival: &Arrival) -> Result<(), ApiError> {
    browser::check(request, arrival, None)
}

/// The way out of every request: the store layer's error shape (shared with it), the
/// request ID header and the browser headers. A UI file keeps the `Cache-Control` it
/// asked for; every other answer is `no-store`.
pub(crate) fn finish(response: Response, id: Uuid) -> Response {
    let mut response = shape_answer(response, Some(id));
    if let Ok(value) = HeaderValue::from_str(&id.to_string()) {
        response.headers_mut().insert(headers::REQUEST_ID, value);
    }
    browser::response_headers(&mut response);
    if let Some(ui::CacheControl(cache)) = response.extensions().get::<ui::CacheControl>().cloned()
    {
        response.headers_mut().insert(header::CACHE_CONTROL, cache);
    }
    response
}

/// `GET /metrics` in the Prometheus text format, for the listener on `metrics.bind`. It
/// needs no credential, so that listener must never be published.
pub fn metrics_router(handle: metrics_exporter_prometheus::PrometheusHandle) -> Router {
    Router::new().route(
        "/metrics",
        axum::routing::get(move || {
            let text = handle.render();
            async move {
                let mut r = text.into_response();
                r.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; version=0.0.4"),
                );
                r
            }
        }),
    )
}
