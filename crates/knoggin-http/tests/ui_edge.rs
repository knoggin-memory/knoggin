// SPDX-License-Identifier: AGPL-3.0-only
//! The web UI through the daemon's edge: the UI's files beside the API, the event stream's
//! cap per user, and the locked daemon (tab "Web UI", Serving and Live events).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod support;

use axum::body::Body;
use http::{Method, Request, StatusCode};
use knoggin_http::LockReason;
use knoggin_http::edge::Arrival;
use knoggin_http::error::Code;
use knoggin_http::local::locked_router;
use serde_json::json;
use tower::ServiceExt;

use support::edge::{EdgeEnv, PORT, fixed, tight};

/// Whether `ui/dist` is built in this checkout: debug builds read the UI from disk, so a
/// developer who built the UI gets its files where CI gets the "not built" answer.
fn ui_built() -> bool {
    std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../ui/dist/index.html"
    ))
    .exists()
}

fn get(uri: &str, arrival: Arrival) -> Request<Body> {
    let mut r = Request::builder()
        .uri(uri)
        .header("host", format!("127.0.0.1:{PORT}"))
        .body(Body::empty())
        .unwrap();
    r.extensions_mut().insert(arrival);
    r
}

#[tokio::test]
async fn the_ui_needs_no_credential_and_is_not_counted_by_the_fixed_bucket() {
    let env = EdgeEnv::with(tight).await;
    // The fixed bucket holds 3 requests without a credential; the UI's files take none.
    for path in [
        "/",
        "/map",
        "/explore?id=x",
        "/_app/immutable/x.js",
        "/favicon.svg",
    ] {
        let r = env.call(fixed(), Method::GET, path, None, &[]).await;
        if ui_built() {
            assert_ne!(r.status, StatusCode::UNAUTHORIZED, "{path}");
            assert_ne!(r.status, StatusCode::TOO_MANY_REQUESTS, "{path}");
        } else {
            assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert_eq!(r.bytes, b"The web UI is not built");
        }
        assert_eq!(r.headers["x-content-type-options"], "nosniff");
        assert_eq!(r.headers["x-frame-options"], "DENY");
        assert_eq!(r.headers["referrer-policy"], "no-referrer");
        assert!(r.headers.contains_key("knoggin-request-id"));
    }
    let r = env
        .call(fixed(), Method::GET, "/v1/stores", None, &[])
        .await;
    assert_eq!(
        r.error(),
        Code::Unauthenticated,
        "the bucket was not charged"
    );
    // The API's own paths never fall back to the UI.
    for path in [
        "/v1/nowhere",
        "/v1",
        "/mcp/nowhere",
        "/auth/nowhere",
        "/.well-known/x",
    ] {
        let r = env.local(Method::GET, path, None).await;
        assert_eq!(r.error(), Code::NotFound, "{path}");
    }
    let r = env.local(Method::POST, "/map", Some(json!({}))).await;
    assert_eq!(r.error(), Code::NotFound, "only GET reaches the UI");
    // The browser checks still run on the UI's paths.
    let r = env
        .call(fixed(), Method::GET, "/", None, &[("host", "evil.example")])
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
}

#[tokio::test]
async fn ten_event_streams_per_user_and_no_more() {
    let env = EdgeEnv::new().await;
    // No stream opens a store: until a store request, the answer is 204.
    let r = env.local(Method::GET, "/v1/ui/events", None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    env.local(Method::GET, "/v1/namespaces", None).await;
    let mut open = Vec::new();
    for _ in 0..10 {
        let r = env
            .app
            .clone()
            .oneshot(get("/v1/ui/events", Arrival::Socket))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["cache-control"], "no-store");
        assert_eq!(r.headers()["content-type"], "text/event-stream");
        open.push(r);
    }
    let r = env.local(Method::GET, "/v1/ui/events", None).await;
    assert_eq!(r.error(), Code::RateLimited);
    // The other store routes are not part of the cap.
    let r = env.local(Method::GET, "/v1/ui/health", None).await;
    assert_eq!(r.status, StatusCode::OK);
    open.pop();
    let r = env
        .app
        .clone()
        .oneshot(get("/v1/ui/events", Arrival::Socket))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "a closed stream frees its slot");
    open.push(r);
    // The cap is the user's, whatever the credential.
    let app = env.app("Work").await;
    let (secret, _) = env.key(app, json!(["memory:read"]), 90).await;
    let r = env
        .bearer(fixed(), &secret, Method::GET, "/v1/ui/events", None)
        .await;
    assert_eq!(r.error(), Code::RateLimited);
    drop(open);
    // An API key reaches its own store's stream; another user's store is not found.
    let r = env
        .bearer(fixed(), &secret, Method::GET, "/v1/ui/events", None)
        .await;
    assert_eq!(
        r.status,
        StatusCode::NO_CONTENT,
        "the app's store is not open"
    );
    let (_, theirs) = env.other_user("bob").await;
    let r = env
        .call(
            Arrival::Socket,
            Method::GET,
            "/v1/ui/events",
            None,
            &[("knoggin-store", &theirs.to_string())],
        )
        .await;
    assert_eq!(r.error(), Code::NotFound);
    let r = env
        .call(fixed(), Method::GET, "/v1/ui/events", None, &[])
        .await;
    assert_eq!(r.error(), Code::Unauthenticated);
}

#[tokio::test]
async fn the_locked_daemon_still_serves_the_ui_files() {
    let app = locked_router(
        LockReason::StateMissing,
        "0.1.0".into(),
        "https://docs".into(),
    );
    let r = app.clone().oneshot(get("/map", fixed())).await.unwrap();
    if ui_built() {
        assert_eq!(r.status(), StatusCode::OK);
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
    } else {
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.headers()["content-type"], "text/plain; charset=utf-8");
    }
    let r = app.oneshot(get("/v1/ui/graph", fixed())).await.unwrap();
    assert_eq!(r.status().as_u16(), 423);
}
