// SPDX-License-Identifier: AGPL-3.0-only
//! The container's routes: `hello`, `claim`,
//! `park` and `status` for the router, and every forwarded store request, which passes
//! the router token, the claim's user, the parking state and the 64 MiB of bodies in
//! flight before the store layer sees it with a `RequestContext` rebuilt from the
//! internal headers.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, HttpBody};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use hyper::body::Frame;
use knoggin_core::StoreId;
use knoggin_http::ctx::{self, RequestContext, headers};
use knoggin_http::error::{ApiError, Code};
use knoggin_http::host::{EmbedWaits, Embedder, HostConfig, SnapshotTiming, StoreHost};
use knoggin_http::mcp::McpConfig;
use knoggin_http::settings::{IMPORT_LINE_BYTES, StoreLimits};
use knoggin_http::store_api::store_layer;
use serde::Deserialize;
use tower::ServiceExt;
use uuid::Uuid;

use super::{
    ActivityGuard, BODY_BYTES_IN_FLIGHT, Claim, EMBED_ALLOWANCE, Phase, RouterBackend, Shared,
};
use crate::link::api::{
    ClaimRequest, ContainerPhase, Counters, HelloAnswer, PROTOCOLS, ParkRequest, StatusAnswer,
    StoreLock, StoreReport, paths,
};
use crate::link::{CallRefused, MAX_ANSWER_BYTES, Nonce, verify_router_call};
use crate::settings::store_budget;
use crate::{json, lock};

/// How many `hello` nonces are remembered.
const NONCES_KEPT: usize = 64;

type S = State<Arc<Shared>>;

/// Every route of the container.
pub(super) fn app(shared: Arc<Shared>) -> Router {
    let internal = Router::new()
        .route(paths::CLAIM, post(claim))
        .route(paths::PARK, post(park))
        .route(paths::STATUS, get(status))
        .route_layer(middleware::from_fn_with_state(shared.clone(), router_call));
    Router::new()
        .route(paths::HELLO, get(hello))
        .merge(internal)
        .fallback(forward)
        .layer(middleware::from_fn(no_user_on_internal))
        .with_state(shared)
}

fn refused(e: CallRefused) -> Response {
    match e {
        CallRefused::Protocol => ApiError::internal().into_response(),
        CallRefused::Identity | CallRefused::Token => ApiError::new(
            Code::Unauthenticated,
            "The router token is missing or wrong",
        )
        .into_response(),
    }
}

/// No client path reaches an internal route: a request that carries `Knoggin-User-Id`
/// (a forwarded one) is never served under `/internal/`.
async fn no_user_on_internal(req: Request, next: Next) -> Response {
    if req.uri().path().starts_with("/internal/") && req.headers().contains_key(headers::USER_ID) {
        return ApiError::not_found("No such route").into_response();
    }
    next.run(req).await
}

/// The router token on every route but `hello`.
async fn router_call(State(s): S, req: Request, next: Next) -> Response {
    match verify_router_call(&s.router_token, req.headers()) {
        Ok(()) => next.run(req).await,
        Err(e) => refused(e),
    }
}

async fn json_body<T: serde::de::DeserializeOwned>(body: Body) -> Result<T, ApiError> {
    let bytes = axum::body::to_bytes(body, MAX_ANSWER_BYTES)
        .await
        .map_err(|_| ApiError::invalid_argument("body", "The body is too large"))?;
    serde_json::from_slice(&bytes).map_err(|_| ApiError::invalid_argument("body", "Bad body"))
}

#[derive(Deserialize)]
struct HelloQuery {
    nonce: Option<String>,
}

async fn hello(State(s): S, headers: HeaderMap, Query(query): Query<HelloQuery>) -> Response {
    if !ctx::link_protocol_ok(&headers) {
        return ApiError::internal().into_response();
    }
    let Some(nonce) = query.nonce.as_deref().and_then(Nonce::from_hex) else {
        return ApiError::invalid_argument("nonce", "The nonce must be 64 hex characters")
            .into_response();
    };
    let model = s.model.state.borrow().clone();
    if model.state != knoggin_core::ops::ModelStatus::Ready {
        return ApiError::unavailable(1).into_response();
    }
    {
        let mut seen = lock(&s.nonces);
        let hex = nonce.to_hex();
        if seen.contains(&hex) {
            return ApiError::new(Code::Conflict, "This nonce was used before").into_response();
        }
        if seen.len() >= NONCES_KEPT {
            seen.pop_front();
        }
        seen.push_back(hex);
    }
    json(&HelloAnswer {
        container_id: s.env.container_id,
        build: s.build.version.clone(),
        protocols: PROTOCOLS.to_vec(),
        model,
        test_mode: s.test_mode,
        proof: s
            .env
            .launch_secret
            .hello_proof(&nonce, &s.cert_der)
            .expose()
            .to_owned(),
    })
}

async fn claim(State(s): S, body: Body) -> Response {
    if s.claim.get().is_some() {
        return ApiError::new(Code::Conflict, "The container is claimed").into_response();
    }
    let request: ClaimRequest = match json_body(body).await {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let settings = &request.settings;
    let limits = StoreLimits {
        body_bytes: usize::try_from(settings.limits.body_bytes).unwrap_or(usize::MAX),
        request_timeout_ms: settings.limits.request_timeout_ms,
        store_concurrency: usize::try_from(settings.limits.store_concurrency).unwrap_or(usize::MAX),
    };
    let idle = Duration::from_secs(settings.idle_seconds);
    let config = HostConfig {
        timing: SnapshotTiming::SERVER,
        model: s.model.identity.clone(),
        limits,
        memory_budget: Some(store_budget(settings.memory_limit_bytes)),
        idle_close: Some(idle),
        embed_allowance: Some(EMBED_ALLOWANCE),
        web_url: Some(settings.public_url.clone()),
        mcp: McpConfig {
            source_url: settings.source_url.clone(),
        },
    };
    let backend = Arc::new(RouterBackend::new(s.link.clone(), request.user_id));
    let embedder = Embedder::new(
        s.model.state.clone(),
        s.model.providers.clone(),
        EmbedWaits::default(),
    );
    let host = StoreHost::new(config, backend, embedder);
    let claim = Arc::new(Claim {
        user: request.user_id,
        generation: request.generation,
        store: store_layer(host.clone()),
        host,
        idle,
        body_bytes: settings.limits.body_bytes,
    });
    if let Err(extra) = s.claim.set(claim) {
        // Another claim won the race.
        extra.host.shutdown();
        return ApiError::new(Code::Conflict, "The container is claimed").into_response();
    }
    s.set_phase(Phase::Claimed);
    s.wake.notify_one();
    tracing::info!(
        container_id = %s.env.container_id,
        user_id = %request.user_id,
        generation = request.generation,
        "claimed"
    );
    StatusCode::OK.into_response()
}

async fn park(State(s): S, body: Body) -> Response {
    let request: ParkRequest = match json_body(body).await {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    tracing::info!(container_id = %s.env.container_id, reason = ?request.reason, "park requested");
    s.park_stands.store(true, Ordering::SeqCst);
    s.wake.notify_one();
    StatusCode::ACCEPTED.into_response()
}

#[derive(Deserialize)]
struct StatusQuery {
    store: Option<String>,
}

async fn status(State(s): S, Query(query): Query<StatusQuery>) -> Response {
    let claim = s.claim.get();
    if let (Some(claim), Some(id)) = (claim, query.store)
        && let Ok(id) = Uuid::parse_str(&id)
    {
        // Retries a stale lock in the background; not waited for.
        claim.host.store_status(StoreId::from_uuid(id));
    }
    let report = claim.map(|c| c.host.report()).unwrap_or_default();
    let counters = claim.map(|c| {
        let c = &c.host.counters;
        let get = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        (
            get(&c.snapshot_failures),
            Counters {
                requests: get(&c.requests),
                embed_count: get(&c.embed_count),
                embed_millis: get(&c.embed_millis),
                snapshot_count: get(&c.snapshot_count),
                snapshot_millis: get(&c.snapshot_millis),
                snapshot_bytes: get(&c.snapshot_bytes),
            },
        )
    });
    let (snapshot_failures, counters) = counters.unwrap_or_default();
    json(&StatusAnswer {
        state: match s.phase() {
            Phase::Ready => ContainerPhase::Ready,
            Phase::Claimed => ContainerPhase::Claimed,
            Phase::Parking => ContainerPhase::Parking,
        },
        user_id: claim.map(|c| c.user),
        generation: claim.map(|c| c.generation),
        build: s.build.version.clone(),
        idle_seconds: s.idle_seconds(),
        model: s.model.state.borrow().clone(),
        stores: report
            .stores
            .iter()
            .map(|r| StoreReport {
                store_id: r.store_id,
                memory_count: r.memory_count,
                estimated_bytes: r.estimated_bytes,
                unsaved: r.unsaved,
                last_sequence: r.last_sequence,
                pending_embeddings: r.pending_embeddings,
                total_embeddable: r.total_embeddable,
                full: r.full,
            })
            .collect(),
        opening: report.opening,
        locks: report
            .locks
            .iter()
            .map(|&(store_id, reason)| StoreLock { store_id, reason })
            .collect(),
        snapshot_failures,
        counters,
    })
}

/// The 503 of a parking container: the router waits and tries again.
fn parking() -> Response {
    let mut response = ApiError::unavailable(2).into_response();
    response.headers_mut().insert(
        headers::INTERNAL_STATUS,
        HeaderValue::from_static(paths::PARKING),
    );
    response
}

/// What a request's body takes of the 64 MiB: its length, nothing for no body, and for a
/// body of unknown length the largest body the store layer buffers. An import streams
/// ("no limit on the whole body"), so it takes at most the 4 MiB of one line, whatever
/// its length.
fn reservation(headers: &HeaderMap, empty: bool, body_bytes: u64, streamed: bool) -> Option<u64> {
    let mut lengths = headers.get_all(header::CONTENT_LENGTH).iter();
    let bytes = match (lengths.next(), lengths.next()) {
        (Some(v), None) => v.to_str().ok()?.parse().ok()?,
        (None, _) if empty => 0,
        (None, _) => body_bytes,
        _ => return None,
    };
    Some(if streamed {
        bytes.min(IMPORT_LINE_BYTES as u64)
    } else {
        bytes
    })
}

/// A forwarded store request.
async fn forward(State(s): S, req: Request) -> Response {
    if req.uri().path().starts_with("/internal/") {
        return ApiError::not_found("No such route").into_response();
    }
    if let Err(e) = verify_router_call(&s.router_token, req.headers()) {
        return refused(e);
    }
    if s.lease_lost() {
        return ApiError::unavailable(2).into_response();
    }
    // A pool container serves no one.
    let Some(claim) = s.claim.get().cloned() else {
        return ApiError::internal().into_response();
    };
    // Counted before the phase is read: a park that sees no request in flight has set
    // `Parking` first, so a request that slips in after its count sees `Parking` here.
    // An open event stream is not activity.
    let events = req.method() == Method::GET && req.uri().path() == "/v1/ui/events";
    let guard = (!events).then(|| s.begin());
    if s.phase() == Phase::Parking {
        return parking();
    }
    let ctx = match RequestContext::from_internal_headers(req.headers()) {
        Ok(ctx) => ctx,
        Err(e) => return e.into_response(),
    };
    if ctx.user_id != claim.user {
        tracing::warn!(container_id = %s.env.container_id, "a request for another user was refused");
        return ApiError::internal()
            .with_request_id(ctx.request_id)
            .into_response();
    }
    let busy = || {
        ApiError::unavailable(1)
            .with_request_id(ctx.request_id)
            .into_response()
    };
    let streamed = req.method() == Method::POST && req.uri().path() == "/v1/import";
    let Some(bytes) = reservation(
        req.headers(),
        req.body().is_end_stream(),
        claim.body_bytes,
        streamed,
    ) else {
        return ApiError::invalid_argument("Content-Length", "Bad Content-Length").into_response();
    };
    let permit = if bytes == 0 {
        None
    } else {
        let Some(permits) = u32::try_from(bytes)
            .ok()
            .filter(|n| *n <= BODY_BYTES_IN_FLIGHT)
        else {
            return busy();
        };
        match s.bodies.clone().try_acquire_many_owned(permits) {
            Ok(p) => Some(p),
            Err(_) => return busy(),
        }
    };
    let mut req = req;
    let request_id = ctx.request_id;
    req.extensions_mut().insert(ctx);
    let mut cut = s.cut.subscribe();
    let response = tokio::select! {
        r = claim.store.clone().oneshot(req) => r.into_response(),
        _ = cut.wait_for(|c| *c) => ApiError::unavailable(2).with_request_id(request_id).into_response(),
    };
    drop(permit);
    let (parts, body) = response.into_parts();
    let cut = s.cut.subscribe();
    Response::from_parts(
        parts,
        Body::new(Watched {
            inner: body,
            cut: Box::pin(async move {
                let mut cut = cut;
                if cut.wait_for(|c| *c).await.is_err() {
                    std::future::pending::<()>().await;
                }
            }),
            _guard: guard,
        }),
    )
}

/// A response body that keeps its request counted as activity until it ends, and breaks
/// off when a park cuts the requests that outlived its wait (a long export).
struct Watched {
    inner: Body,
    cut: Pin<Box<dyn Future<Output = ()> + Send>>,
    _guard: Option<ActivityGuard>,
}

impl HttpBody for Watched {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        if self.cut.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Some(Err(axum::Error::new("cut off by a park"))));
        }
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_reserves_its_length_or_the_batch_limit() {
        let mut h = HeaderMap::new();
        assert_eq!(reservation(&h, true, 10, false), Some(0));
        assert_eq!(reservation(&h, false, 10, false), Some(10));
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("7"));
        assert_eq!(reservation(&h, false, 10, false), Some(7));
        let line = IMPORT_LINE_BYTES as u64;
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(1u64 << 30));
        assert_eq!(
            reservation(&h, false, 10, true),
            Some(line),
            "an import streams"
        );
        assert_eq!(reservation(&h, false, 10, false), Some(1 << 30));
        h.append(header::CONTENT_LENGTH, HeaderValue::from_static("7"));
        assert_eq!(reservation(&h, false, 10, false), None);
    }
}
