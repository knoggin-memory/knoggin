// SPDX-License-Identifier: AGPL-3.0-only
//! Request IDs, the request span and the request log line and metrics.
//!
//! The span records the matched route pattern and the request ID, never the URI: a query
//! string can carry a one-time code. Apply [`observe`] with `Router::layer` (not around
//! the whole router), so that the matched route is known.

use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use http::HeaderValue;
use tower_http::request_id::{MakeRequestId, RequestId};
use tracing::Instrument;
use uuid::Uuid;

use crate::ctx::RequestContext;
use crate::error::ApiError;

/// Makes the request ID: a UUIDv7 (tower-http's own generator makes version 4).
#[derive(Debug, Clone, Copy, Default)]
pub struct MakeRequestUuidV7;

impl MakeRequestId for MakeRequestUuidV7 {
    fn make_request_id<B>(&mut self, _request: &http::Request<B>) -> Option<RequestId> {
        HeaderValue::from_str(&Uuid::now_v7().to_string())
            .ok()
            .map(RequestId::new)
    }
}

/// The request's ID: from its `RequestContext`, else from tower-http's `RequestId`.
pub fn request_id<B>(request: &http::Request<B>) -> Option<Uuid> {
    if let Some(ctx) = request.extensions().get::<RequestContext>() {
        return Some(ctx.request_id);
    }
    request
        .extensions()
        .get::<RequestId>()
        .and_then(|id| id.header_value().to_str().ok())
        .and_then(|text| Uuid::parse_str(text).ok())
}

/// The matched route pattern, or `unmatched`.
pub fn route<B>(request: &http::Request<B>) -> String {
    request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |p| p.as_str().to_owned())
}

/// The span of one request: route pattern and request ID only.
pub fn request_span<B>(request: &http::Request<B>) -> tracing::Span {
    let id = request_id(request)
        .map(|id| id.to_string())
        .unwrap_or_default();
    tracing::info_span!("request", route = %route(request), request_id = %id)
}

/// Runs the request inside its span, then logs one line (status, code, duration) and
/// counts it in `knoggin_requests_total` and `knoggin_request_seconds`.
pub async fn observe(request: Request, next: Next) -> Response {
    let route = route(&request);
    let span = request_span(&request);
    let start = Instant::now();
    let response = next.run(request).instrument(span.clone()).await;
    let code = response
        .extensions()
        .get::<ApiError>()
        .map_or("ok", |e| e.code.as_str());
    let elapsed = start.elapsed();
    span.in_scope(|| {
        tracing::info!(
            status = response.status().as_u16(),
            code,
            duration_ms = elapsed.as_millis() as u64,
            "request"
        );
    });
    metrics::counter!("knoggin_requests_total", "route" => route.clone(), "code" => code)
        .increment(1);
    metrics::histogram!("knoggin_request_seconds", "route" => route).record(elapsed.as_secs_f64());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_version_7() {
        let req = http::Request::new(());
        let id = MakeRequestUuidV7.make_request_id(&req).unwrap();
        let id = Uuid::parse_str(id.header_value().to_str().unwrap()).unwrap();
        assert_eq!(id.get_version_num(), 7);
    }
}
