// SPDX-License-Identifier: AGPL-3.0-only
//! The web UI through the daemon's edge: the UI's files beside the API, the event stream's
//! cap per user, the locked daemon, and what an app's key sees of the user's other stores
//! in the read models and the stream (an app's key reaches its own store only).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod support;

use std::time::Duration;

use axum::body::Body;
use http::{Method, Request, StatusCode};
use knoggin_http::LockReason;
use knoggin_http::edge::Arrival;
use knoggin_http::error::Code;
use knoggin_http::local::locked_router;
use serde_json::{Value, json};
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

/// A request of the read models or the event stream with an API key on the fixed
/// listener, or on the socket without one (the personal store).
fn keyed(uri: &str, secret: Option<&str>) -> Request<Body> {
    let arrival = if secret.is_some() {
        fixed()
    } else {
        Arrival::Socket
    };
    let mut r = get(uri, arrival);
    if let Some(s) = secret {
        r.headers_mut()
            .insert("authorization", format!("Bearer {s}").parse().unwrap());
    }
    r
}

/// One MCP tool call of the current revision as client `client`, with a key or (none)
/// over the socket; returns the tool's JSON.
async fn mcp(env: &EdgeEnv, secret: Option<&str>, client: &str, tool: &str, args: Value) -> Value {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
        "name": tool, "arguments": args, "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": { "name": client, "version": "1" },
            "io.modelcontextprotocol/clientCapabilities": {} } } });
    let auth = secret.map(|s| format!("Bearer {s}"));
    let mut headers = vec![
        ("mcp-method", "tools/call"),
        ("mcp-protocol-version", "2026-07-28"),
        ("mcp-name", tool),
    ];
    if let Some(a) = &auth {
        headers.push(("authorization", a));
    }
    let arrival = if secret.is_some() {
        fixed()
    } else {
        Arrival::Socket
    };
    let r = env
        .call(arrival, Method::POST, "/mcp", Some(body), &headers)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let result = &r.json["result"];
    assert!(result["isError"].is_null(), "{tool}: {}", r.json);
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// Each app's store is kept apart: with a key of app X, the read models show no client
/// name, event or count of the personal store or of app Y; only the figures of the process
/// itself are shared.
#[tokio::test]
async fn an_apps_read_models_show_nothing_of_the_users_other_stores() {
    let env = EdgeEnv::new().await;
    let rw = json!(["memory:read", "memory:write"]);
    let (x, _) = env.key(env.app("X").await, rw.clone(), 90).await;
    let (y, _) = env.key(env.app("Y").await, rw, 90).await;
    // Distinct data in each store: 1 memory in X, 2 in the personal store, 3 in Y, and
    // as many recalls, each by its own client.
    for (secret, mark, n) in [
        (Some(x.as_str()), "mkrx", 1),
        (None, "mkrpersonal", 2),
        (Some(y.as_str()), "mkry", 3),
    ] {
        let client = format!("{mark}agent");
        for i in 0..n {
            mcp(
                &env,
                secret,
                &client,
                "store_memory",
                json!({
                "summary": format!("{mark}note {i}"), "tags": [format!("{mark}tag")],
                "entities": [format!("{mark}entity")] }),
            )
            .await;
            mcp(
                &env,
                secret,
                &client,
                "recall_memories",
                json!({ "query": format!("{mark}note") }),
            )
            .await;
        }
    }
    let read = |uri: &'static str, secret: Option<String>| {
        let env = &env;
        async move {
            let r = support::edge::send(&env.app, keyed(uri, secret.as_deref())).await;
            assert_eq!(r.status, StatusCode::OK, "{uri}: {}", r.json);
            r
        }
    };
    for uri in [
        "/v1/ui/system",
        "/v1/ui/activity",
        "/v1/ui/facets",
        "/v1/ui/health",
    ] {
        let text = String::from_utf8(read(uri, Some(x.clone())).await.bytes).unwrap();
        assert!(
            text.contains("mkrxagent") || uri == "/v1/ui/health",
            "{uri}: {text}"
        );
        assert!(
            text.contains("mkrxnote") || uri != "/v1/ui/activity",
            "{uri}: {text}"
        );
        for other in ["mkrpersonal", "mkry"] {
            assert!(
                !text.contains(other),
                "{uri} with X's key shows {other}: {text}"
            );
        }
        // The other stores' own views hold their data, so its absence above means
        // something.
        let own = String::from_utf8(read(uri, None).await.bytes).unwrap();
        assert!(
            own.contains("mkrpersonal") || uri == "/v1/ui/health",
            "{uri}: {own}"
        );
        let own = String::from_utf8(read(uri, Some(y.clone())).await.bytes).unwrap();
        assert!(
            own.contains("mkry") || uri == "/v1/ui/health",
            "{uri}: {own}"
        );
    }
    let s = read("/v1/ui/system", Some(x.clone())).await.json;
    let clients: Vec<&str> = s["clients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(clients, ["mkrxagent"], "{s}");
    let f = read("/v1/ui/facets", Some(x.clone())).await.json;
    assert_eq!(
        f["clients"],
        json!([{ "name": "mkrxagent", "count": 1 }]),
        "{f}"
    );
    assert_eq!(f["tags"], json!([{ "name": "mkrxtag", "count": 1 }]), "{f}");
    let h = read("/v1/ui/health", Some(x.clone())).await.json;
    assert_eq!(h["overview"]["totalMemories"], 1, "{h}");
    let h = read("/v1/ui/health", Some(y.clone())).await.json;
    assert_eq!(h["overview"]["totalMemories"], 3, "{h}");
    let a = read("/v1/ui/activity", Some(x.clone())).await.json;
    let sum = |field: &str| {
        a["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b[field].as_u64().unwrap())
            .sum::<u64>()
    };
    assert_eq!(sum("full"), 1, "{a}");
    let rows = a["rows"].as_array().unwrap();
    assert_eq!(
        rows.iter().filter(|r| r["kind"] == "store").count(),
        1,
        "{a}"
    );
    assert_eq!(
        rows.iter().filter(|r| r["kind"] == "recall").count(),
        1,
        "{a}"
    );
}

/// The next named event of a stream, or `None` when none comes within `wait`.
async fn next_event(body: &mut Body, buf: &mut String, wait: Duration) -> Option<(String, Value)> {
    use http_body_util::BodyExt;
    loop {
        if let Some(end) = buf.find("\n\n") {
            let block: String = buf.drain(..end + 2).collect();
            let field = |name: &str| {
                block
                    .lines()
                    .find_map(|l| l.strip_prefix(name).map(str::to_owned))
            };
            if let (Some(name), Some(data)) = (field("event: "), field("data: ")) {
                return Some((name, serde_json::from_str(&data).unwrap()));
            }
            continue;
        }
        let frame = tokio::time::timeout(wait, body.frame()).await.ok()??.ok()?;
        if let Some(data) = frame.data_ref() {
            buf.push_str(std::str::from_utf8(data).unwrap());
        }
    }
}

/// Each app's store is kept apart: with the event stream of app X open, a save, a recall
/// and a forget in the personal store and in app Y put no event on it; a save in X then
/// does.
#[tokio::test]
async fn an_apps_event_stream_carries_nothing_of_the_users_other_stores() {
    let env = EdgeEnv::new().await;
    let rw = json!(["memory:read", "memory:write"]);
    let (x, _) = env.key(env.app("X").await, rw.clone(), 90).await;
    let (y, _) = env.key(env.app("Y").await, rw, 90).await;
    let call = |secret: Option<String>, method: Method, uri: String, body: Option<Value>| {
        let env = &env;
        async move {
            let r = match &secret {
                Some(s) => env.bearer(fixed(), s, method, &uri, body).await,
                None => env.local(method, &uri, body).await,
            };
            assert!(r.status.is_success(), "{uri}: {}", r.json);
            r.json
        }
    };
    // X's store is open, so its stream answers.
    call(Some(x.clone()), Method::GET, "/v1/namespaces".into(), None).await;
    let r = env
        .app
        .clone()
        .oneshot(keyed("/v1/ui/events", Some(&x)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let mut body = r.into_body();
    let mut buf = String::new();
    let mut foreign = Vec::new();
    for secret in [None, Some(y.clone())] {
        let saved = call(
            secret.clone(),
            Method::POST,
            "/v1/memories".into(),
            Some(json!({ "summary": "mkrforeign note" })),
        )
        .await;
        let id = saved["id"].as_str().unwrap().to_owned();
        call(
            secret.clone(),
            Method::POST,
            "/v1/recall".into(),
            Some(json!({ "query": "mkrforeign" })),
        )
        .await;
        call(
            secret.clone(),
            Method::DELETE,
            format!("/v1/memories/{id}"),
            None,
        )
        .await;
        foreign.push(id);
    }
    // The stream is live: X's own save is the first event on it.
    let mine = call(
        Some(x.clone()),
        Method::POST,
        "/v1/memories".into(),
        Some(json!({ "summary": "mine" })),
    )
    .await;
    let (name, data) = next_event(&mut body, &mut buf, Duration::from_secs(5))
        .await
        .expect("X's own event");
    assert_eq!(
        (name.as_str(), &data["id"]),
        ("store", &mine["id"]),
        "{data}"
    );
    // Nothing of the other stores follows, links made later included.
    while let Some((name, data)) = next_event(&mut body, &mut buf, Duration::from_millis(500)).await
    {
        let text = data.to_string();
        assert!(name != "recall" && name != "forget", "{name}: {text}");
        for id in &foreign {
            assert!(!text.contains(id.as_str()), "{name}: {text}");
        }
    }
}
