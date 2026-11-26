// SPDX-License-Identifier: AGPL-3.0-only
//! The panic hook, alone in its own test binary because a hook is process-wide (tab
//! "Engineering rules"): a router handler panics with a marker in its message, and the
//! hook's line names the place and not the marker.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use http::{Request, StatusCode};
use knoggin_http::panic::location_line;
use tower::ServiceExt;
use tower_http::catch_panic::CatchPanicLayer;

#[tokio::test]
async fn the_hook_logs_where_a_handler_panicked_and_not_its_message() {
    // The hook's own body is `log_location`, which writes `location_line` to stderr; the
    // test keeps the line instead, so that it can read it.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let out = seen.clone();
    std::panic::set_hook(Box::new(move |info| {
        out.lock().unwrap().push(location_line(info));
    }));
    let router: Router = Router::new()
        .route(
            "/boom",
            get(|| async { panic!("marker-secret-memory-text") }),
        )
        .layer(CatchPanicLayer::custom(|_payload| {
            http::Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap()
        }));
    let response = router
        .oneshot(Request::get("/boom").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let _ = std::panic::take_hook();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let lines = seen.lock().unwrap().clone();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("panic at ") && lines[0].contains("panic_hook.rs"),
        "{}",
        lines[0]
    );
    assert!(!lines[0].contains("marker"), "{}", lines[0]);
}
