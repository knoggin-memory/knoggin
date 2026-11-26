// SPDX-License-Identifier: AGPL-3.0-only
//! The forwarder (tab "Server", How a store request is served): the router's store routes.
//! Each request waits for its user's container (claiming one if needed), goes to it over
//! the pinned link with the client's credentials and `Knoggin-` headers removed and the
//! internal headers added last, and comes back through the filter, so that a container
//! can never speak to the client directly. A failed or silent link is checked with the
//! container's `status`; a dead container is marked gone and the request sent once more
//! where that is safe.

use std::sync::{Arc, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use bytes::Bytes;
use futures_util::stream;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use knoggin_core::UserId;
use knoggin_http::ctx::headers;
use knoggin_http::error::Relayed;
use knoggin_http::{ApiError, RequestContext};
use knoggin_state::record::{LeaseState, StopReason};
use tokio::time::Instant;
use uuid::Uuid;

use crate::lease::{Caller, Fleet, Marked, POLL, db_error, peer_of};
use crate::link::api::paths;
use crate::link::{Peer, write_router_call};

/// Every store-route pattern of the store layer; only these are forwarded.
pub const STORE_ROUTES: &[&str] = &[
    "/v1/memories",
    "/v1/memories/batch",
    "/v1/memories/forget",
    "/v1/memories/move",
    "/v1/memories/{id}",
    "/v1/memories/{id}/reinforce",
    "/v1/memories/{id}/supersede",
    "/v1/memories/{id}/similar",
    "/v1/recall",
    "/v1/context",
    "/v1/namespaces",
    "/v1/namespaces/{name}",
    "/v1/namespaces/{name}/stats",
    "/v1/namespaces/{name}/policy",
    "/v1/tags",
    "/v1/export",
    "/v1/import",
    "/v1/ui/graph",
    "/v1/ui/memories",
    "/v1/ui/facets",
    "/v1/ui/memories/{id}/history",
    "/v1/ui/memories/{id}/neighbors",
    "/v1/ui/search",
    "/v1/ui/activity",
    "/v1/ui/health",
    "/v1/ui/system",
    "/v1/ui/events",
    "/mcp",
    "/mcp/plugin",
];

/// A request body of up to this many bytes is buffered so that it can be sent again.
const BUFFER_ONE: u64 = 1024 * 1024;
/// All buffered request bodies of one router.
const BUFFER_ALL: u64 = 256 * 1024 * 1024;
/// All buffered request bodies of one user.
const BUFFER_USER: u64 = 16 * 1024 * 1024;
/// The waits of "When the link fails or goes quiet"; tests pass shorter ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Silence on an open request before `status` is asked (10 seconds).
    pub silence: Duration,
    /// The same for the event stream, which sends a comment every 15 seconds (30).
    pub events_silence: Duration,
    /// The limit of one `status` call, and the wait before the second (5 seconds).
    pub status_limit: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            silence: Duration::from_secs(10),
            events_silence: Duration::from_secs(30),
            status_limit: Duration::from_secs(5),
        }
    }
}

/// The response headers a container may set.
const PASSED_ON: [HeaderName; 6] = [
    header::CONTENT_TYPE,
    header::CONTENT_LENGTH,
    header::RETRY_AFTER,
    headers::IDEMPOTENT_REPLAYED,
    HeaderName::from_static("mcp-session-id"),
    header::ALLOW,
];

/// Hop-by-hop headers, never forwarded.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// What the forwarder needs.
#[derive(Debug, Clone)]
pub struct Forwarder {
    fleet: Arc<Fleet>,
    /// `limits.request_timeout_ms` of an ordinary request.
    request_timeout: Duration,
    timing: Timing,
}

/// The router's store routes: every pattern of [`STORE_ROUTES`], forwarded.
pub fn forwarder(fleet: Arc<Fleet>, request_timeout: Duration, timing: Timing) -> Router {
    let mut router = Router::new();
    for pattern in STORE_ROUTES {
        router = router.route(pattern, any(forward));
    }
    router.with_state(Forwarder {
        fleet,
        request_timeout,
        timing,
    })
}

/// How long a route may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Ends with `timeout` at `limits.request_timeout_ms`.
    Ordinary,
    /// Export and import: no time limit.
    Long,
    /// The event stream: no time limit, a longer silence, never sent again.
    Events,
}

async fn forward(State(f): State<Forwarder>, request: Request) -> Response {
    match f.serve(request).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

impl Forwarder {
    async fn serve(&self, request: Request) -> Result<Response, ApiError> {
        let ctx = request
            .extensions()
            .get::<RequestContext>()
            .cloned()
            .ok_or_else(ApiError::internal)?;
        let user = ctx.user_id;
        let path = request.uri().path().to_owned();
        let method = request.method().clone();
        let mcp = path == "/mcp" || path == "/mcp/plugin";
        let kind = match path.as_str() {
            "/v1/ui/events" => Kind::Events,
            "/v1/export" | "/v1/import" => Kind::Long,
            _ => Kind::Ordinary,
        };
        // The cases the router answers itself, without waking a container.
        if mcp && method == Method::GET {
            let mut r = ApiError::method_not_allowed().into_response();
            r.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
            return Ok(r);
        }
        if (kind == Kind::Events && method == Method::GET) || (mcp && method == Method::DELETE) {
            let warm = self
                .fleet
                .leases
                .get(user)
                .await
                .map_err(db_error)?
                .is_some_and(|l| l.value.state == LeaseState::Warm);
            if !warm {
                return Ok(if mcp {
                    ApiError::not_found("No such MCP session").into_response()
                } else {
                    StatusCode::NO_CONTENT.into_response()
                });
            }
        }
        let resendable = !mcp
            && kind != Kind::Events
            && (method == Method::GET
                || method == Method::HEAD
                || request.headers().contains_key(headers::IDEMPOTENCY_KEY));
        let target = request
            .uri()
            .path_and_query()
            .map_or_else(|| path.clone(), |p| p.as_str().to_owned());
        let (parts, body) = request.into_parts();
        let outgoing = outgoing_headers(&parts.headers, &ctx)?;
        let mut payload = Payload::read(&self.fleet, user, body).await?;
        let resendable = resendable && payload.buffered();
        self.run(
            user,
            method,
            &target,
            outgoing,
            &mut payload,
            kind,
            resendable,
        )
        .await
    }

    #[expect(clippy::too_many_arguments, reason = "one forwarded request")]
    async fn run(
        &self,
        user: UserId,
        method: Method,
        target: &str,
        outgoing: HeaderMap,
        payload: &mut Payload,
        kind: Kind,
        resendable: bool,
    ) -> Result<Response, ApiError> {
        let wait = Duration::from_millis(self.fleet.config().containers.claim_wait_ms);
        let deadline = Instant::now() + wait;
        let timeout = (kind == Kind::Ordinary).then(|| Instant::now() + self.request_timeout);
        let silence = if kind == Kind::Events {
            self.timing.events_silence
        } else {
            self.timing.silence
        };
        let mut resent = false;
        loop {
            let lease = if kind == Kind::Events {
                match self.fleet.leases.get(user).await.map_err(db_error)? {
                    Some(l) if l.value.state == LeaseState::Warm => l,
                    _ => return Ok(StatusCode::NO_CONTENT.into_response()),
                }
            } else {
                self.fleet
                    .acquire(user, deadline.saturating_duration_since(Instant::now()))
                    .await?
            };
            let (Some(id), Some(peer)) = (
                lease.value.container_id,
                peer_of(
                    lease.value.address.as_deref(),
                    lease.value.cert_sha256.as_deref(),
                ),
            ) else {
                tracing::error!(kind = "lease", "a warm lease names no container");
                return Err(ApiError::internal());
            };
            self.fleet.peers.set(id, peer);
            let mut headers = outgoing.clone();
            write_router_call(
                &mut headers,
                &self.fleet.config().secret.launch_secret(id).router_token(),
            );
            let uri = format!("https://{id}:{}{target}", peer.address.port());
            let mut builder = http::Request::builder().method(method.clone()).uri(uri);
            if let Some(h) = builder.headers_mut() {
                *h = headers;
            }
            let request = builder
                .body(payload.take())
                .map_err(|_| ApiError::internal())?;
            match self.send(id, peer, request, silence, timeout).await {
                Sent::Answer(response) => {
                    let parking = response.status() == StatusCode::SERVICE_UNAVAILABLE
                        && response
                            .headers()
                            .get(headers::INTERNAL_STATUS)
                            .is_some_and(|v| v == paths::PARKING);
                    if !parking {
                        return Ok(self.relay(id, peer, response, silence));
                    }
                    // A parking container: wait and loop, while the body can be sent again.
                    if !payload.buffered() || Instant::now() + POLL >= deadline {
                        return Err(ApiError::unavailable(2));
                    }
                    tokio::time::sleep(POLL).await;
                    continue;
                }
                Sent::Timeout => return Err(ApiError::timeout()),
                Sent::LinkFailed => {
                    if self.alive(id, peer).await {
                        // Alive: nothing left to wait on; send once more on a new
                        // connection where that is safe.
                        if resendable && !resent {
                            resent = true;
                            continue;
                        }
                        return Err(ApiError::unavailable(2));
                    }
                }
                Sent::Dead => {}
            }
            // `status` did not answer twice: "Mark gone", then once more where safe.
            match self
                .fleet
                .mark_gone(id, Some(StopReason::Unresponsive), Caller::Request)
                .await
            {
                Ok(Marked::Gone) if resendable && !resent => resent = true,
                Ok(_) => return Err(ApiError::unavailable(2)),
                Err(e) => {
                    tracing::warn!(kind = "mark_gone", error = %e, "a dead container could not be marked gone");
                    return Err(ApiError::unavailable(2));
                }
            }
        }
    }

    /// Sends and waits for the response headers, asking `status` after each `silence`.
    async fn send(
        &self,
        id: Uuid,
        peer: Peer,
        request: http::Request<Body>,
        silence: Duration,
        timeout: Option<Instant>,
    ) -> Sent {
        let call = self.fleet.client.request(request);
        tokio::pin!(call);
        loop {
            let ends = async {
                match timeout {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                answered = &mut call => {
                    return match answered {
                        Ok(r) => Sent::Answer(r),
                        Err(_) => Sent::LinkFailed,
                    };
                }
                () = tokio::time::sleep(silence) => {
                    if !self.alive(id, peer).await {
                        return Sent::Dead;
                    }
                }
                () = ends => return Sent::Timeout,
            }
        }
    }

    /// Whether the container answers `status`: once, and once more after 5 seconds.
    async fn alive(&self, id: Uuid, peer: Peer) -> bool {
        alive(&self.fleet, id, peer, self.timing.status_limit).await
    }

    /// The filter on a container's answer, and its body streamed with the silence check.
    fn relay(
        &self,
        id: Uuid,
        peer: Peer,
        response: http::Response<Incoming>,
        silence: Duration,
    ) -> Response {
        let (parts, body) = response.into_parts();
        if let Some(why) = refused_answer(parts.status, &parts.headers) {
            tracing::error!(kind = "container_answer", container_id = %id, why, "a container's answer was replaced by internal");
            return ApiError::internal().into_response();
        }
        let mut out = Response::new(watched(
            self.fleet.clone(),
            id,
            peer,
            body,
            silence,
            self.timing.status_limit,
        ));
        *out.status_mut() = parts.status;
        for name in &PASSED_ON {
            for value in parts.headers.get_all(name) {
                out.headers_mut().append(name.clone(), value.clone());
            }
        }
        let h = out.headers_mut();
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        h.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; sandbox"),
        );
        out.extensions_mut().insert(Relayed);
        out
    }
}

/// Whether the container answers `status`: once, and once more after 5 seconds.
pub(crate) async fn alive(fleet: &Fleet, id: Uuid, peer: Peer, limit: Duration) -> bool {
    if fleet.status(id, peer, None, limit).await.is_some() {
        return true;
    }
    tokio::time::sleep(limit).await;
    fleet.status(id, peer, None, limit).await.is_some()
}

enum Sent {
    Answer(http::Response<Incoming>),
    LinkFailed,
    Dead,
    Timeout,
}

/// Why a container's answer may not reach the client, if it may not: a redirect, a 401,
/// or a body that is not JSON, JSON Lines or an event stream.
fn refused_answer(status: StatusCode, map: &HeaderMap) -> Option<&'static str> {
    if status.is_redirection() {
        return Some("redirect");
    }
    if status == StatusCode::UNAUTHORIZED {
        return Some("unauthorized");
    }
    match map.get(header::CONTENT_TYPE) {
        Some(value) => {
            let media = value
                .to_str()
                .ok()
                .and_then(|t| t.split(';').next())
                .map(|t| t.trim().to_ascii_lowercase());
            match media.as_deref() {
                Some("application/json" | "application/x-ndjson" | "text/event-stream") => None,
                _ => Some("content_type"),
            }
        }
        None => {
            let empty = status == StatusCode::NO_CONTENT
                || map
                    .get(header::CONTENT_LENGTH)
                    .is_some_and(|v| v.as_bytes() == b"0");
            (!empty).then_some("content_type")
        }
    }
}

/// The client's headers without the hop-by-hop ones (and those its `Connection` names),
/// `Authorization`, `Cookie` and every `Knoggin-` header, then the internal headers last,
/// so that nothing the client sent can remove or repeat one.
fn outgoing_headers(client: &HeaderMap, ctx: &RequestContext) -> Result<HeaderMap, ApiError> {
    let named: Vec<String> = client
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|n| n.trim().to_ascii_lowercase())
        .collect();
    let mut out = HeaderMap::new();
    for (name, value) in client {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n)
            || named.iter().any(|m| m == n)
            || name == header::AUTHORIZATION
            || name == header::COOKIE
            || name == header::HOST
            || n.starts_with("knoggin-")
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    // The client's `Knoggin-Namespace` travels in the context.
    ctx.write_internal_headers(&mut out)?;
    Ok(out)
}

/// A request body: buffered (it can be sent again) or streamed (once).
enum Payload {
    /// The bytes, and the share of the allowances they hold.
    Buffered {
        bytes: Bytes,
        _held: BufferGuard,
    },
    Streamed(Option<Body>),
}

impl Payload {
    /// Buffers a body of up to 1 MiB while the router's and the user's allowances last;
    /// streams anything else.
    async fn read(fleet: &Arc<Fleet>, user: UserId, body: Body) -> Result<Self, ApiError> {
        // The exact length, from `Content-Length` or a body that knows its size; an
        // HTTP/2 body may have none.
        let hint = axum::body::HttpBody::size_hint(&body);
        let len = match hint.exact() {
            Some(n) if n <= BUFFER_ONE => n,
            _ => return Ok(Self::Streamed(Some(body))),
        };
        let Some(guard) = BufferGuard::take(fleet, user, len) else {
            return Ok(Self::Streamed(Some(body)));
        };
        let limit = usize::try_from(len).unwrap_or(usize::MAX);
        let bytes = Limited::new(body, limit)
            .collect()
            .await
            .map_err(|_| ApiError::invalid_argument("body", "the request body could not be read"))?
            .to_bytes();
        Ok(Self::Buffered {
            bytes,
            _held: guard,
        })
    }

    fn buffered(&self) -> bool {
        matches!(self, Self::Buffered { .. })
    }

    /// The body for one send; a streamed body only once.
    fn take(&mut self) -> Body {
        match self {
            Self::Buffered { bytes, .. } => Body::from(bytes.clone()),
            Self::Streamed(body) => body.take().unwrap_or_else(Body::empty),
        }
    }
}

/// Holds a share of the buffer allowances until dropped.
struct BufferGuard {
    fleet: Arc<Fleet>,
    user: UserId,
    bytes: u64,
}

impl BufferGuard {
    fn take(fleet: &Arc<Fleet>, user: UserId, bytes: u64) -> Option<Self> {
        let mut b = fleet
            .buffered
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mine = b.1.get(&user).copied().unwrap_or(0);
        if b.0 + bytes > BUFFER_ALL || mine + bytes > BUFFER_USER {
            return None;
        }
        b.0 += bytes;
        b.1.insert(user, mine + bytes);
        Some(Self {
            fleet: fleet.clone(),
            user,
            bytes,
        })
    }
}

impl Drop for BufferGuard {
    fn drop(&mut self) {
        let mut b = self
            .fleet
            .buffered
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        b.0 = b.0.saturating_sub(self.bytes);
        if let Some(n) = b.1.get_mut(&self.user) {
            *n = n.saturating_sub(self.bytes);
            if *n == 0 {
                b.1.remove(&self.user);
            }
        }
    }
}

/// The container's response body, passed on as it comes. After each `silence` the
/// container's `status` is asked; a dead container is marked gone and the connection to
/// the client closed, since a started response is never started again.
fn watched(
    fleet: Arc<Fleet>,
    id: Uuid,
    peer: Peer,
    body: Incoming,
    silence: Duration,
    limit: Duration,
) -> Body {
    struct State {
        fleet: Arc<Fleet>,
        id: Uuid,
        peer: Peer,
        body: Incoming,
        silence: Duration,
        limit: Duration,
        done: bool,
    }
    let state = State {
        fleet,
        id,
        peer,
        body,
        silence,
        limit,
        done: false,
    };
    Body::from_stream(stream::unfold(state, |mut s| async move {
        if s.done {
            return None;
        }
        loop {
            match tokio::time::timeout(s.silence, s.body.frame()).await {
                Ok(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        return Some((Ok::<Bytes, std::io::Error>(data), s));
                    }
                }
                Ok(Some(Err(_))) => {
                    s.done = true;
                    return Some((
                        Err(std::io::Error::other("the container's answer broke")),
                        s,
                    ));
                }
                Ok(None) => return None,
                Err(_) => {
                    if !alive(&s.fleet, s.id, s.peer, s.limit).await {
                        let fleet = s.fleet.clone();
                        let id = s.id;
                        tokio::spawn(async move {
                            let _ = fleet
                                .mark_gone(id, Some(StopReason::Unresponsive), Caller::Request)
                                .await;
                        });
                        s.done = true;
                        return Some((
                            Err(std::io::Error::other("the container stopped answering")),
                            s,
                        ));
                    }
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use knoggin_core::StoreId;
    use knoggin_http::ctx::{CredentialKind, CredentialRef, Permissions};

    use super::*;

    fn ctx() -> RequestContext {
        RequestContext {
            request_id: Uuid::now_v7(),
            user_id: UserId::from_uuid(Uuid::now_v7()),
            store_id: StoreId::from_uuid(Uuid::now_v7()),
            permissions: Permissions::READ,
            credential: CredentialRef {
                kind: CredentialKind::ApiKey,
                id: Some(Uuid::now_v7()),
            },
            default_namespace: Some("work".into()),
            client: None,
        }
    }

    #[test]
    fn credentials_and_knoggin_headers_never_travel_and_connection_cannot_remove_one() {
        let mut map = HeaderMap::new();
        map.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        map.insert(header::COOKIE, HeaderValue::from_static("__Host-session=x"));
        map.insert(
            "knoggin-permissions",
            HeaderValue::from_static("memory:read,memory:write"),
        );
        map.insert("Knoggin-User-Id", HeaderValue::from_static("someone"));
        map.insert("knoggin-namespace", HeaderValue::from_static("other"));
        map.insert(
            header::CONNECTION,
            HeaderValue::from_static("knoggin-user-id, knoggin-permissions, x-drop"),
        );
        map.insert("x-drop", HeaderValue::from_static("1"));
        map.insert("x-keep", HeaderValue::from_static("1"));
        map.insert(header::TE, HeaderValue::from_static("trailers"));
        map.insert(headers::IDEMPOTENCY_KEY, HeaderValue::from_static("k1"));
        let c = ctx();
        let out = outgoing_headers(&map, &c).unwrap();
        assert!(out.get(header::AUTHORIZATION).is_none());
        assert!(out.get(header::COOKIE).is_none());
        assert!(out.get("x-drop").is_none() && out.get(header::TE).is_none());
        assert_eq!(out.get("x-keep").unwrap(), "1");
        assert_eq!(out.get(headers::IDEMPOTENCY_KEY).unwrap(), "k1");
        assert_eq!(out.get_all(headers::PERMISSIONS).iter().count(), 1);
        assert_eq!(out.get(headers::PERMISSIONS).unwrap(), "memory:read");
        assert_eq!(
            out.get(headers::USER_ID).unwrap().to_str().unwrap(),
            c.user_id.to_string()
        );
        assert_eq!(out.get(headers::NAMESPACE).unwrap(), "work");
        assert_eq!(out.get(headers::PROTOCOL).unwrap(), "1");
    }

    #[test]
    fn the_filter_refuses_redirects_401_and_other_types() {
        let typed = |t: &'static str| {
            let mut m = HeaderMap::new();
            m.insert(header::CONTENT_TYPE, HeaderValue::from_static(t));
            m
        };
        assert!(
            refused_answer(StatusCode::OK, &typed("application/json; charset=utf-8")).is_none()
        );
        assert!(refused_answer(StatusCode::OK, &typed("text/event-stream")).is_none());
        assert!(refused_answer(StatusCode::OK, &typed("application/x-ndjson")).is_none());
        assert!(refused_answer(StatusCode::OK, &typed("text/html")).is_some());
        assert!(refused_answer(StatusCode::FOUND, &typed("application/json")).is_some());
        assert!(refused_answer(StatusCode::UNAUTHORIZED, &typed("application/json")).is_some());
        assert!(refused_answer(StatusCode::NO_CONTENT, &HeaderMap::new()).is_none());
        assert!(refused_answer(StatusCode::OK, &HeaderMap::new()).is_some());
    }
}
