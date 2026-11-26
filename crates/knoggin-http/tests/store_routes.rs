// SPDX-License-Identifier: AGPL-3.0-only
//! The store routes against `store_layer` with an inserted context, the fake model and a
//! `LocalBackend` in a temporary folder (tab "REST API", Tests that must exist).
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
use knoggin_http::Permissions;
use knoggin_http::error::Code;
use serde_json::{Value, json};

use support::{Env, memory};

const RW: Permissions = Permissions::READ_WRITE;
const RO: Permissions = Permissions::READ;

async fn save(env: &Env, body: Value) -> Value {
    let ctx = env.t.ctx(RW);
    let r = env
        .call(Some(&ctx), Method::POST, "/v1/memories", Some(body), &[])
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    r.json
}

#[tokio::test]
async fn every_route_answers_success() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let a = save(
        &env,
        json!({ "summary": "Maya prefers window seats", "tags": ["travel"], "entities": ["Maya"] }),
    )
    .await;
    let b = save(&env, json!({ "summary": "Maya flies to Lisbon in May" })).await;
    let (a, b) = (
        a["id"].as_str().unwrap().to_owned(),
        b["id"].as_str().unwrap().to_owned(),
    );
    // The similar search needs `a`'s vector: wait for the queue rather than race it.
    for _ in 0..500 {
        let c = env
            .call(Some(&ctx), Method::GET, "/v1/context", None, &[])
            .await;
        if c.json["embedding"]["pending"] == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let cases: Vec<(Method, String, Option<Value>, StatusCode)> = vec![
        (
            Method::POST,
            "/v1/memories/batch".into(),
            Some(json!({ "memories": [memory("one"), memory("two")] })),
            StatusCode::OK,
        ),
        (
            Method::GET,
            "/v1/memories?tag=travel&includeFullText=true&limit=10".into(),
            None,
            StatusCode::OK,
        ),
        (
            Method::GET,
            format!("/v1/memories/{a}?includeHistory=true"),
            None,
            StatusCode::OK,
        ),
        (
            Method::POST,
            format!("/v1/memories/{a}/reinforce"),
            Some(json!({ "quality": 4 })),
            StatusCode::OK,
        ),
        (
            Method::POST,
            format!("/v1/memories/{a}/reinforce"),
            None,
            StatusCode::OK,
        ),
        (
            Method::POST,
            format!("/v1/memories/{a}/similar"),
            Some(json!({ "limit": 3 })),
            StatusCode::OK,
        ),
        (
            Method::POST,
            "/v1/recall".into(),
            Some(json!({ "query": "window seats", "detail": "full" })),
            StatusCode::OK,
        ),
        (
            Method::GET,
            "/v1/context?topic=Lisbon".into(),
            None,
            StatusCode::OK,
        ),
        (Method::GET, "/v1/context".into(), None, StatusCode::OK),
        (
            Method::POST,
            "/v1/namespaces".into(),
            Some(json!({ "name": "work" })),
            StatusCode::CREATED,
        ),
        (Method::GET, "/v1/namespaces".into(), None, StatusCode::OK),
        (
            Method::GET,
            "/v1/namespaces/default/stats".into(),
            None,
            StatusCode::OK,
        ),
        (
            Method::PUT,
            "/v1/namespaces/work/policy?dryRun=true".into(),
            Some(serde_json::to_value(knoggin_core::model::NamespacePolicy::default()).unwrap()),
            StatusCode::OK,
        ),
        (
            Method::PUT,
            "/v1/namespaces/work/policy".into(),
            Some(serde_json::to_value(knoggin_core::model::NamespacePolicy::default()).unwrap()),
            StatusCode::OK,
        ),
        (
            Method::GET,
            "/v1/tags?prefix=tr".into(),
            None,
            StatusCode::OK,
        ),
        (
            Method::POST,
            "/v1/memories/move".into(),
            Some(json!({ "ids": [b], "destinationNamespace": "work" })),
            StatusCode::OK,
        ),
        (
            Method::POST,
            format!("/v1/memories/{b}/supersede"),
            Some(json!({ "summary": "Maya flies to Lisbon in June" })),
            StatusCode::CREATED,
        ),
        (
            Method::POST,
            "/v1/memories/forget".into(),
            Some(json!({ "ids": [a], "dryRun": true })),
            StatusCode::OK,
        ),
        (
            Method::DELETE,
            format!("/v1/memories/{a}"),
            None,
            StatusCode::OK,
        ),
        (
            Method::DELETE,
            "/v1/namespaces/work?force=true".into(),
            None,
            StatusCode::OK,
        ),
    ];
    for (method, uri, body, status) in cases {
        let r = env.call(Some(&ctx), method.clone(), &uri, body, &[]).await;
        assert_eq!(r.status, status, "{method} {uri}: {}", r.json);
        assert_eq!(r.headers["cache-control"], "no-store");
    }
    let r = env
        .call(
            Some(&ctx),
            Method::GET,
            "/v1/memories/00000000-0000-7000-8000-000000000000",
            None,
            &[],
        )
        .await;
    assert_eq!(r.error(), Code::NotFound);
}

#[tokio::test]
async fn shapes_of_the_main_answers() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let saved = save(
        &env,
        json!({ "summary": "Maya prefers window seats", "fullText": "Lisbon trip" }),
    )
    .await;
    assert_eq!(saved["phase"], "full");
    assert_eq!(saved["namespace"], "default");
    assert!(saved.get("links").is_none());
    let id = saved["id"].as_str().unwrap();
    let got = env
        .call(
            Some(&ctx),
            Method::GET,
            &format!("/v1/memories/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(got.json["fullText"], "Lisbon trip");
    assert_eq!(
        got.json["accessCount"], 0,
        "the view is taken before access is recorded"
    );
    let again = env
        .call(
            Some(&ctx),
            Method::GET,
            &format!("/v1/memories/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(again.json["accessCount"], 1);
    let list = env
        .call(Some(&ctx), Method::GET, "/v1/memories", None, &[])
        .await;
    assert!(list.json["items"].is_array());
    assert!(list.json.get("nextCursor").is_some());
    let ns = env
        .call(Some(&ctx), Method::GET, "/v1/namespaces", None, &[])
        .await;
    assert!(ns.json.get("nextCursor").is_none());
    let c = env
        .call(Some(&ctx), Method::GET, "/v1/context", None, &[])
        .await;
    assert_eq!(c.json["defaultNamespace"], "default");
    assert_eq!(c.json["model"]["state"], "ready");
    assert_eq!(c.json["model"]["name"], "Fake model");
    assert!(c.json["webUrl"].is_null());
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/recall",
            Some(json!({ "query": "window" })),
            &[],
        )
        .await;
    assert!(
        r.json["memories"].is_array() && r.json["count"].is_number(),
        "{}",
        r.json
    );
}

#[tokio::test]
async fn listed_errors_per_route() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let a = save(&env, memory("one")).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let missing = "00000000-0000-7000-8000-000000000001";
    save(
        &env,
        json!({ "summary": "has a full text", "fullText": "x" }),
    )
    .await;
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/namespaces",
            Some(json!({ "name": "full" })),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    env.call(
        Some(&ctx),
        Method::POST,
        "/v1/memories",
        Some(json!({ "summary": "s", "namespace": "full" })),
        &[],
    )
    .await;
    let cases: Vec<(Method, String, Option<Value>, Code)> = vec![
        (
            Method::POST,
            "/v1/memories".into(),
            Some(json!({ "summary": "" })),
            Code::InvalidArgument,
        ),
        (
            Method::POST,
            "/v1/memories".into(),
            Some(json!({ "summary": "s", "namespace": "nope" })),
            Code::NotFound,
        ),
        (
            Method::POST,
            "/v1/memories".into(),
            Some(json!({ "summary": "s", "parentId": missing })),
            Code::NotFound,
        ),
        (
            Method::POST,
            "/v1/memories/batch".into(),
            Some(json!({ "memories": [] })),
            Code::InvalidArgument,
        ),
        (
            Method::GET,
            "/v1/memories?namespace=nope".into(),
            None,
            Code::NotFound,
        ),
        (
            Method::GET,
            "/v1/memories?limit=0".into(),
            None,
            Code::InvalidArgument,
        ),
        (
            Method::GET,
            "/v1/memories?phase=solid".into(),
            None,
            Code::InvalidArgument,
        ),
        (
            Method::GET,
            format!("/v1/memories/{missing}"),
            None,
            Code::NotFound,
        ),
        (
            Method::GET,
            "/v1/memories/not-a-uuid".into(),
            None,
            Code::InvalidArgument,
        ),
        (
            Method::POST,
            "/v1/memories/forget".into(),
            Some(json!({ "ids": [] })),
            Code::InvalidArgument,
        ),
        (
            Method::POST,
            format!("/v1/memories/{missing}/reinforce"),
            None,
            Code::NotFound,
        ),
        (
            Method::POST,
            format!("/v1/memories/{a}/reinforce"),
            Some(json!({ "quality": 5 })),
            Code::InvalidArgument,
        ),
        (
            Method::POST,
            format!("/v1/memories/{missing}/supersede"),
            Some(memory("s")),
            Code::NotFound,
        ),
        (
            Method::POST,
            "/v1/memories/move".into(),
            Some(json!({ "ids": [missing], "destinationNamespace": "full" })),
            Code::NotFound,
        ),
        (
            Method::POST,
            "/v1/memories/move".into(),
            Some(json!({ "ids": [a], "destinationNamespace": "full", "sourceNamespace": "full" })),
            Code::Conflict,
        ),
        (
            Method::POST,
            format!("/v1/memories/{missing}/similar"),
            None,
            Code::NotFound,
        ),
        (
            Method::POST,
            "/v1/recall".into(),
            Some(json!({ "query": "q", "limit": 3, "fullTextCount": 4 })),
            Code::InvalidArgument,
        ),
        (
            Method::POST,
            "/v1/recall".into(),
            Some(json!({ "query": "q", "namespace": "nope" })),
            Code::NotFound,
        ),
        (
            Method::GET,
            "/v1/context?namespace=nope".into(),
            None,
            Code::NotFound,
        ),
        (
            Method::POST,
            "/v1/namespaces".into(),
            Some(json!({ "name": "full" })),
            Code::AlreadyExists,
        ),
        (
            Method::POST,
            "/v1/namespaces".into(),
            Some(json!({ "name": "bad name" })),
            Code::InvalidArgument,
        ),
        (
            Method::GET,
            "/v1/namespaces/nope/stats".into(),
            None,
            Code::NotFound,
        ),
        (
            Method::PUT,
            "/v1/namespaces/nope/policy".into(),
            Some(serde_json::to_value(knoggin_core::model::NamespacePolicy::default()).unwrap()),
            Code::NotFound,
        ),
        (
            Method::PUT,
            "/v1/namespaces/full/policy".into(),
            Some(json!({ "initialStability": 1.0 })),
            Code::InvalidArgument,
        ),
        (
            Method::DELETE,
            "/v1/namespaces/nope".into(),
            None,
            Code::NotFound,
        ),
        (
            Method::DELETE,
            "/v1/namespaces/default".into(),
            None,
            Code::InvalidArgument,
        ),
        (
            Method::DELETE,
            "/v1/namespaces/full".into(),
            None,
            Code::NamespaceNotEmpty,
        ),
        (
            Method::GET,
            "/v1/tags?namespace=nope".into(),
            None,
            Code::NotFound,
        ),
        (
            Method::PATCH,
            "/v1/memories".into(),
            None,
            Code::InvalidArgument,
        ),
    ];
    for (method, uri, body, code) in cases {
        let r = env.call(Some(&ctx), method.clone(), &uri, body, &[]).await;
        assert_eq!(r.error(), code, "{method} {uri}: {}", r.json);
    }
    let r = env
        .call(Some(&ctx), Method::PATCH, "/v1/memories", None, &[])
        .await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(r.headers.contains_key("allow"));
    // A memory without a vector yet cannot be searched by similarity.
    let pending = Env::new(false).await;
    let id = save(&pending, memory("waits")).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let c = pending.t.ctx(RW);
    let r = pending
        .call(
            Some(&c),
            Method::POST,
            &format!("/v1/memories/{id}/similar"),
            None,
            &[],
        )
        .await;
    assert_eq!(r.error(), Code::Conflict);
}

#[tokio::test]
async fn a_read_only_key_cannot_write_and_records_no_access() {
    let env = Env::new(true).await;
    let id = save(&env, memory("remember me")).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let ro = env.t.ctx(RO);
    let writes: Vec<(Method, String, Option<Value>)> = vec![
        (Method::POST, "/v1/memories".into(), Some(memory("x"))),
        (
            Method::POST,
            "/v1/memories/batch".into(),
            Some(json!({ "memories": [memory("x")] })),
        ),
        (Method::DELETE, format!("/v1/memories/{id}"), None),
        (
            Method::POST,
            "/v1/memories/forget".into(),
            Some(json!({ "ids": [id] })),
        ),
        (Method::POST, format!("/v1/memories/{id}/reinforce"), None),
        (
            Method::POST,
            format!("/v1/memories/{id}/supersede"),
            Some(memory("x")),
        ),
        (
            Method::POST,
            "/v1/memories/move".into(),
            Some(json!({ "ids": [id], "destinationNamespace": "default" })),
        ),
        (
            Method::POST,
            "/v1/namespaces".into(),
            Some(json!({ "name": "w" })),
        ),
        (
            Method::PUT,
            "/v1/namespaces/default/policy".into(),
            Some(serde_json::to_value(knoggin_core::model::NamespacePolicy::default()).unwrap()),
        ),
        (Method::DELETE, "/v1/namespaces/w".into(), None),
    ];
    for (method, uri, body) in writes {
        let r = env.call(Some(&ro), method.clone(), &uri, body, &[]).await;
        assert_eq!(r.error(), Code::PermissionDenied, "{method} {uri}");
    }
    let r = env
        .send(
            Request::post("/v1/import?onConflict=skip")
                .header("content-type", "application/x-ndjson")
                .extension(ro.clone())
                .body(Body::from("{}\n"))
                .unwrap(),
        )
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
    // Reads with recording asked for record nothing.
    let uri = format!("/v1/memories/{id}?recordAccess=true");
    env.call(Some(&ro), Method::GET, &uri, None, &[]).await;
    env.call(
        Some(&ro),
        Method::POST,
        "/v1/recall",
        Some(json!({ "query": "remember me", "recordAccess": true })),
        &[],
    )
    .await;
    let rw = env.t.ctx(RW);
    let r = env
        .call(
            Some(&rw),
            Method::GET,
            &format!("/v1/memories/{id}?recordAccess=false"),
            None,
            &[],
        )
        .await;
    assert_eq!(r.json["accessCount"], 0, "{}", r.json);
    assert!(r.json["retrievedAt"].is_null());
}

#[tokio::test]
async fn unknown_fields_and_parameters_and_bounds() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories",
            Some(json!({ "summary": "s", "colour": 1 })),
            &[],
        )
        .await;
    assert_eq!(r.error(), Code::InvalidArgument);
    assert_eq!(r.json["error"]["field"], "colour");
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories?colour=1",
            Some(memory("s")),
            &[],
        )
        .await;
    assert_eq!(
        (r.error(), r.json["error"]["field"].as_str()),
        (Code::InvalidArgument, Some("colour"))
    );
    let r = env
        .call(Some(&ctx), Method::GET, "/v1/namespaces?x=1", None, &[])
        .await;
    assert_eq!(r.error(), Code::InvalidArgument);
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories",
            Some(json!({ "summary": 3 })),
            &[],
        )
        .await;
    assert_eq!(r.error(), Code::InvalidArgument, "no coercion");
    // A query of 4,097 bytes.
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/recall",
            Some(json!({ "query": "q".repeat(4097) })),
            &[],
        )
        .await;
    assert_eq!(
        (r.error(), r.json["error"]["field"].as_str()),
        (Code::InvalidArgument, Some("query"))
    );
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/recall",
            Some(json!({ "query": "q".repeat(4096) })),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    // Times before 1970 and after 9999.
    for (body, field) in [
        (
            json!({ "query": "q", "timeRangeStart": -1, "timeRangeEnd": 0 }),
            "timeRangeStart",
        ),
        (
            json!({ "query": "q", "timeRangeStart": 0, "timeRangeEnd": "10000-01-01T00:00:00.000Z" }),
            "timeRangeEnd",
        ),
    ] {
        let r = env
            .call(Some(&ctx), Method::POST, "/v1/recall", Some(body), &[])
            .await;
        assert_eq!(
            (r.error(), r.json["error"]["field"].as_str()),
            (Code::InvalidArgument, Some(field)),
            "{}",
            r.json
        );
    }
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories",
            Some(json!({ "summary": "s", "createdAt": "1969-12-31T23:59:59.999Z" })),
            &[],
        )
        .await;
    assert_eq!(r.error(), Code::InvalidArgument);
    let r = env
        .call(
            Some(&ctx),
            Method::GET,
            "/v1/memories?createdAfter=10000-01-01T00:00:00Z",
            None,
            &[],
        )
        .await;
    assert_eq!(r.error(), Code::InvalidArgument);
    // The wrong content type.
    let r = env
        .send(
            Request::post("/v1/memories")
                .header("content-type", "text/plain")
                .extension(ctx.clone())
                .body(Body::from(memory("s").to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(
        (r.error(), r.json["error"]["field"].as_str()),
        (Code::InvalidArgument, Some("Content-Type"))
    );
    let r = env
        .send(
            Request::post("/v1/import")
                .header("content-type", "application/json")
                .extension(ctx.clone())
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
    assert_eq!(r.error(), Code::InvalidArgument);
    // Over 256 KiB on an ordinary route, by length and streamed; the batch route takes it.
    let big = json!({ "summary": "s", "fullText": "x".repeat(300 * 1024) });
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories",
            Some(big.clone()),
            &[],
        )
        .await;
    assert_eq!(r.error(), Code::PayloadTooLarge);
    let chunks =
        futures_util::stream::iter(vec![Ok::<_, std::io::Error>(big.to_string().into_bytes())]);
    let r = env
        .send(
            Request::post("/v1/recall")
                .header("content-type", "application/json")
                .extension(ctx.clone())
                .body(Body::from_stream(chunks))
                .unwrap(),
        )
        .await;
    assert_eq!(r.error(), Code::PayloadTooLarge);
    let items: Vec<Value> = (0..5)
        .map(|_| json!({ "summary": "s", "fullText": "y".repeat(60_000) }))
        .collect();
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories/batch",
            Some(json!({ "memories": items })),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["stored"], 5);
}

#[tokio::test]
async fn idempotency() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let key = [("idempotency-key", "k-1")];
    let first = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories",
            Some(json!({ "summary": "once", "tags": ["a"] })),
            &key,
        )
        .await;
    assert_eq!(first.status, StatusCode::CREATED);
    assert!(!first.headers.contains_key("idempotent-replayed"));
    // Same request, keys in another order and a default sent explicitly: a replay.
    let raw = r#"{"tags":["a"],"checkDuplicates":false,"summary":"once"}"#;
    let again = env
        .send(
            Request::post("/v1/memories")
                .header("content-type", "application/json")
                .header("idempotency-key", "k-1")
                .extension(ctx.clone())
                .body(Body::from(raw))
                .unwrap(),
        )
        .await;
    assert_eq!(again.status, StatusCode::CREATED);
    assert_eq!(again.headers["idempotent-replayed"], "true");
    assert_eq!(again.bytes, first.bytes);
    let list = env
        .call(Some(&ctx), Method::GET, "/v1/memories", None, &[])
        .await;
    assert_eq!(
        list.json["items"].as_array().unwrap().len(),
        1,
        "no second memory"
    );
    // The header's namespace is folded in: the same body under another namespace differs.
    let mut other = ctx.clone();
    other.default_namespace = Some("elsewhere".into());
    let r = env
        .call(
            Some(&other),
            Method::POST,
            "/v1/memories",
            Some(json!({ "summary": "once", "tags": ["a"] })),
            &key,
        )
        .await;
    assert_eq!(r.error(), Code::IdempotencyConflict);
    // A different body with the same key.
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories",
            Some(memory("twice")),
            &key,
        )
        .await;
    assert_eq!(r.error(), Code::IdempotencyConflict);
    // The same key with a different query parameter.
    let id = first.json["id"].as_str().unwrap();
    let ns = [("idempotency-key", "k-2")];
    env.call(
        Some(&ctx),
        Method::POST,
        "/v1/namespaces",
        Some(json!({ "name": "w" })),
        &[],
    )
    .await;
    let r = env
        .call(Some(&ctx), Method::DELETE, "/v1/namespaces/w", None, &ns)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = env
        .call(
            Some(&ctx),
            Method::DELETE,
            "/v1/namespaces/w?force=true",
            None,
            &ns,
        )
        .await;
    assert_eq!(r.error(), Code::IdempotencyConflict);
    // A dry run is not recorded: the same key then runs for real.
    let dry = [("idempotency-key", "k-3")];
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories/forget",
            Some(json!({ "ids": [id], "dryRun": true })),
            &dry,
        )
        .await;
    assert_eq!(r.json["deleted"], 1);
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories/forget",
            Some(json!({ "ids": [id] })),
            &dry,
        )
        .await;
    assert!(!r.headers.contains_key("idempotent-replayed"));
    assert_eq!(r.json["results"][0]["deleted"], true);
    // A route without the mark ignores the header.
    let k = [("idempotency-key", "k-4")];
    let a = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/recall",
            Some(json!({ "query": "a" })),
            &k,
        )
        .await;
    let b = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/recall",
            Some(json!({ "query": "b" })),
            &k,
        )
        .await;
    assert_eq!((a.status, b.status), (StatusCode::OK, StatusCode::OK));
    assert!(!b.headers.contains_key("idempotent-replayed"));
    // A key that is not printable ASCII.
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories",
            Some(memory("x")),
            &[("idempotency-key", "")],
        )
        .await;
    assert_eq!(r.error(), Code::InvalidArgument);
}

#[tokio::test]
async fn the_same_save_over_rest_and_over_the_host_happens_once() {
    // MCP calls the same host operation with the same resolved request; this stands in
    // for it until the MCP layer exists.
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let key = [("idempotency-key", "shared")];
    let rest = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories",
            Some(memory("once")),
            &key,
        )
        .await;
    let req = serde_json::from_value(json!({ "summary": "once", "namespace": "default" })).unwrap();
    let reply = env
        .t
        .host
        .store_memory(&ctx, req, Some("shared".into()))
        .await
        .unwrap();
    assert!(reply.is_replayed());
    assert_eq!(reply.into_json().unwrap(), rest.bytes);
}

#[tokio::test]
async fn a_duplicate_check_without_the_model_is_skipped_and_recall_degrades() {
    let env = Env::new(false).await;
    let ctx = env.t.ctx(RW);
    let r = save(&env, json!({ "summary": "s", "checkDuplicates": true })).await;
    assert_eq!(r["duplicateCheck"], "skipped");
    assert_eq!(r["embedding"], "pending");
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/recall",
            Some(json!({ "query": "s" })),
            &[],
        )
        .await;
    assert_eq!(r.json["degraded"], "model_unavailable");
    let r = env
        .call(Some(&ctx), Method::GET, "/v1/context", None, &[])
        .await;
    assert_eq!(r.json["model"]["state"], "loading");
    assert_eq!(r.json["embedding"]["pending"], 1);
}

#[tokio::test]
async fn the_queue_embeds_and_the_duplicate_check_reports() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    save(&env, memory("Maya prefers window seats on long flights")).await;
    // The worker embeds it in the background.
    let mut pending = 1;
    for _ in 0..500 {
        let c = env
            .call(Some(&ctx), Method::GET, "/v1/context", None, &[])
            .await;
        pending = c.json["embedding"]["pending"].as_u64().unwrap();
        if pending == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(pending, 0);
    let r = save(
        &env,
        json!({ "summary": "Maya prefers window seats on long flights", "checkDuplicates": true }),
    )
    .await;
    assert!(r.get("duplicateCheck").is_none(), "{r}");
    assert_eq!(r["embedding"], "ready");
    assert_eq!(
        r["nearDuplicates"]["matches"].as_array().unwrap().len(),
        1,
        "{r}"
    );
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/recall",
            Some(json!({ "query": "window seats" })),
            &[],
        )
        .await;
    assert!(r.json.get("degraded").is_none(), "{}", r.json);
}

#[tokio::test]
async fn export_and_import_round_trip() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let items: Vec<Value> = (0..100)
        .map(|i| json!({ "summary": format!("memory {i}"), "tags": ["t"] }))
        .collect();
    for _ in 0..12 {
        env.call(
            Some(&ctx),
            Method::POST,
            "/v1/memories/batch",
            Some(json!({ "memories": items })),
            &[],
        )
        .await;
    }
    let r = env
        .call(Some(&ctx), Method::GET, "/v1/export?store=x", None, &[])
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "application/x-ndjson");
    let text = String::from_utf8(r.bytes.clone()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let count = |kind: &str| {
        lines
            .iter()
            .filter(|l| l.contains(&format!("\"type\":\"{kind}\"")))
            .count()
    };
    // Every memory, over two pages, then the edges.
    assert_eq!(
        (count("header"), count("namespace"), count("memory")),
        (1, 1, 1200)
    );
    let records = (lines.len() - 1) as u64;
    assert!(lines[0].contains("\"type\":\"header\""));
    // Into an empty store, then a second time: nothing changes the second time.
    let other = env.t.new_store("Copy").await;
    let octx = env.t.ctx_for(other, RW);
    let import = |body: String| {
        Request::post("/v1/import")
            .header("content-type", "application/x-ndjson")
            .extension(octx.clone())
            .body(Body::from(body))
            .unwrap()
    };
    let r = env.send(import(text.clone())).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["errors"], json!([]), "{}", r.json);
    let imported = r.json["imported"].as_u64().unwrap();
    assert!(imported >= 1201, "{}", r.json);
    assert_eq!(imported + r.json["skipped"].as_u64().unwrap(), records);
    let r = env.send(import(text.clone())).await;
    assert_eq!(
        (r.json["imported"].as_u64(), r.json["skipped"].as_u64()),
        (Some(0), Some(records))
    );
    // A bad first line refuses the whole import; a bad later line is reported.
    let r = env.send(import("{\"type\":\"memory\"}\n".into())).await;
    assert_eq!(r.error(), Code::InvalidArgument);
    let long = format!(
        "{}\n{{\"type\":\"namespace\",\"name\":\"n\",\"x\":1}}\n{}\n",
        lines[0],
        "z".repeat(4 * 1024 * 1024 + 1)
    );
    let r = env.send(import(long)).await;
    let errors = r.json["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2, "{}", r.json);
    assert_eq!(
        (errors[0]["line"].as_u64(), errors[1]["line"].as_u64()),
        (Some(2), Some(3))
    );
    assert_eq!(errors[0]["code"], "invalid_argument");
    // A line of exactly 4 MiB is accepted (and fails only as JSON).
    let exact = format!("{}\n{}\n", lines[0], "z".repeat(4 * 1024 * 1024));
    let r = env.send(import(exact)).await;
    assert!(
        r.json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("line 2:"),
        "{}",
        r.json
    );
}

#[tokio::test]
async fn a_route_without_a_context_is_internal() {
    let env = Env::new(true).await;
    let r = env
        .call(None, Method::GET, "/v1/namespaces", None, &[])
        .await;
    // Without a context there is no request ID to put in the body.
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.json["error"]["code"], "internal");
    assert_eq!(r.json["error"]["message"], "Internal error");
}

#[tokio::test]
async fn the_default_namespace_header_is_used() {
    let env = Env::new(true).await;
    let mut ctx = env.t.ctx(RW);
    env.call(
        Some(&ctx),
        Method::POST,
        "/v1/namespaces",
        Some(json!({ "name": "proj" })),
        &[],
    )
    .await;
    ctx.default_namespace = Some("proj".into());
    let saved = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories",
            Some(memory("s")),
            &[],
        )
        .await;
    assert_eq!(saved.json["namespace"], "proj");
    let id = saved.json["id"].as_str().unwrap();
    // A namespace that is only a check is never filled in from the header.
    let mut plain = env.t.ctx(RW);
    plain.default_namespace = Some("default".into());
    let r = env
        .call(
            Some(&plain),
            Method::GET,
            &format!("/v1/memories/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = env
        .call(
            Some(&plain),
            Method::GET,
            &format!("/v1/memories/{id}?namespace=default"),
            None,
            &[],
        )
        .await;
    assert_eq!(r.error(), Code::NotFound);
}
