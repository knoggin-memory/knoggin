// SPDX-License-Identifier: AGPL-3.0-only
//! The router's panic guard, alone in its own test binary: a
//! handler panics with a marker in its message; the request gets `internal` with its
//! request ID, the log line is fixed, and the marker is in no log output.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use knoggin_http::panic::location_line;
use knoggin_server::router::guarded;
use tower::ServiceExt;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_panicking_handler_answers_internal_and_its_message_reaches_no_log() {
    let sink = Sink::default();
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    // The hook's body, as `main` installs it, kept in memory instead of stderr.
    let hook = sink.clone();
    std::panic::set_hook(Box::new(move |info| {
        let mut h = hook.clone();
        let _ = writeln!(h, "{}", location_line(info));
    }));
    let app = guarded(Router::new().route(
        "/boom",
        get(|| async { panic!("marker-secret-memory-text") }),
    ));
    let r = app
        .oneshot(Request::get("/boom").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let _ = std::panic::take_hook();
    assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let id = r
        .headers()
        .get("knoggin-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "internal");
    assert_eq!(v["error"]["requestId"], id);
    let log = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("a request handler panicked"), "{log}");
    assert!(log.contains(&id), "the line carries the request ID: {log}");
    assert!(!log.contains("marker"), "{log}");
}
