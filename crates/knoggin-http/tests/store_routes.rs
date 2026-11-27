// SPDX-License-Identifier: AGPL-3.0-only
//! The store routes against `store_layer` with an inserted context, the fake model and a
//! `LocalBackend` in a temporary folder.
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
use futures_util::StreamExt;
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

/// A memory of a new namespace `name` that has faded to a ghost: saved a month in the
/// past under thresholds that every sweep crosses, then swept twice (each open sweeps).
async fn ghost_in(env: &Env, name: &str) -> String {
    let ctx = env.t.ctx(RW);
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/namespaces",
            Some(json!({ "name": name })),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let mut policy = serde_json::to_value(knoggin_core::model::NamespacePolicy::default()).unwrap();
    policy["phaseThresholds"] =
        json!({ "fullToSummary": 0.99, "summaryToGhost": 0.98, "ghostToDelete": 0.000_001 });
    let r = env
        .call(
            Some(&ctx),
            Method::PUT,
            &format!("/v1/namespaces/{name}/policy"),
            Some(policy),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let created =
        serde_json::to_value(knoggin_core::Timestamp::from_millis(now - 30 * 86_400_000)).unwrap();
    let id = save(
        env,
        json!({ "summary": "fades", "namespace": name, "createdAt": created }),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for _ in 0..2 {
        env.t.host.close(env.t.store).await.unwrap();
        env.call(Some(&ctx), Method::GET, "/v1/namespaces", None, &[])
            .await;
    }
    let r = env
        .call(
            Some(&ctx),
            Method::GET,
            &format!("/v1/memories/{id}?recordAccess=false"),
            None,
            &[],
        )
        .await;
    assert_eq!(r.json["phase"], "ghost", "{}", r.json);
    id
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
    // A ghost can be neither reinforced nor moved.
    let ghost = ghost_in(&env, "fade").await;
    for (uri, body) in [
        (format!("/v1/memories/{ghost}/reinforce"), None),
        (
            "/v1/memories/move".to_owned(),
            Some(json!({ "ids": [ghost], "destinationNamespace": "default" })),
        ),
    ] {
        let r = env.call(Some(&ctx), Method::POST, &uri, body, &[]).await;
        assert_eq!(r.error(), Code::Conflict, "{uri}: {}", r.json);
    }
    // Supersede with content that fails a check.
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            &format!("/v1/memories/{a}/supersede"),
            Some(json!({ "summary": "" })),
            &[],
        )
        .await;
    assert_eq!(r.error(), Code::InvalidArgument, "{}", r.json);
    // Every idempotent route: the same key with another request.
    let policy = serde_json::to_value(knoggin_core::model::NamespacePolicy::default()).unwrap();
    let mut other_policy = policy.clone();
    other_policy["initialStability"] = json!(5.0);
    #[expect(
        clippy::type_complexity,
        reason = "one row per route: a request and its rival"
    )]
    let keyed: Vec<(Method, String, Option<Value>, String, Option<Value>)> = vec![
        (
            Method::POST,
            "/v1/memories".into(),
            Some(memory("k1 a")),
            "/v1/memories".into(),
            Some(memory("k1 b")),
        ),
        (
            Method::POST,
            "/v1/memories/batch".into(),
            Some(json!({ "memories": [memory("k2 a")] })),
            "/v1/memories/batch".into(),
            Some(json!({ "memories": [memory("k2 b")] })),
        ),
        (
            Method::DELETE,
            format!("/v1/memories/{missing}"),
            None,
            format!("/v1/memories/{a}"),
            None,
        ),
        (
            Method::POST,
            "/v1/memories/forget".into(),
            Some(json!({ "ids": [missing] })),
            "/v1/memories/forget".into(),
            Some(json!({ "ids": [a] })),
        ),
        (
            Method::POST,
            format!("/v1/memories/{a}/reinforce"),
            Some(json!({ "quality": 2 })),
            format!("/v1/memories/{a}/reinforce"),
            Some(json!({ "quality": 4 })),
        ),
        (
            Method::POST,
            format!("/v1/memories/{a}/supersede"),
            Some(memory("k6 a")),
            format!("/v1/memories/{a}/supersede"),
            Some(memory("k6 b")),
        ),
        (
            Method::POST,
            "/v1/memories/move".into(),
            Some(json!({ "ids": [a], "destinationNamespace": "full" })),
            "/v1/memories/move".into(),
            Some(json!({ "ids": [a], "destinationNamespace": "default" })),
        ),
        (
            Method::POST,
            "/v1/namespaces".into(),
            Some(json!({ "name": "k8a" })),
            "/v1/namespaces".into(),
            Some(json!({ "name": "k8b" })),
        ),
        (
            Method::PUT,
            "/v1/namespaces/full/policy".into(),
            Some(policy.clone()),
            "/v1/namespaces/full/policy".into(),
            Some(other_policy),
        ),
        (
            Method::DELETE,
            "/v1/namespaces/k8a".into(),
            None,
            "/v1/namespaces/nope".into(),
            None,
        ),
    ];
    for (i, (method, first, first_body, second, second_body)) in keyed.into_iter().enumerate() {
        let key = format!("conflict-{i}");
        let headers = [("idempotency-key", key.as_str())];
        let r = env
            .call(Some(&ctx), method.clone(), &first, first_body, &headers)
            .await;
        assert!(r.status.is_success(), "{method} {first}: {}", r.json);
        let r = env
            .call(Some(&ctx), method.clone(), &second, second_body, &headers)
            .await;
        assert_eq!(
            r.error(),
            Code::IdempotencyConflict,
            "{method} {second}: {}",
            r.json
        );
    }
    // An unknown body field, on every route that takes a body.
    let fresh = save(&env, memory("for the field checks")).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let with_bodies: Vec<(Method, String, Value)> = vec![
        (Method::POST, "/v1/memories".into(), memory("s")),
        (
            Method::POST,
            "/v1/memories/batch".into(),
            json!({ "memories": [memory("s")] }),
        ),
        (
            Method::POST,
            "/v1/memories/forget".into(),
            json!({ "ids": [missing] }),
        ),
        (
            Method::POST,
            format!("/v1/memories/{fresh}/reinforce"),
            json!({ "quality": 3 }),
        ),
        (
            Method::POST,
            format!("/v1/memories/{fresh}/supersede"),
            memory("s"),
        ),
        (
            Method::POST,
            "/v1/memories/move".into(),
            json!({ "ids": [fresh], "destinationNamespace": "default" }),
        ),
        (
            Method::POST,
            format!("/v1/memories/{fresh}/similar"),
            json!({ "limit": 3 }),
        ),
        (Method::POST, "/v1/recall".into(), json!({ "query": "q" })),
        (
            Method::POST,
            "/v1/namespaces".into(),
            json!({ "name": "fields" }),
        ),
        (Method::PUT, "/v1/namespaces/full/policy".into(), policy),
    ];
    for (method, uri, mut body) in with_bodies {
        body["colour"] = json!(1);
        let r = env
            .call(Some(&ctx), method.clone(), &uri, Some(body), &[])
            .await;
        assert_eq!(
            (r.error(), r.json["error"]["field"].as_str()),
            (Code::InvalidArgument, Some("colour")),
            "{method} {uri}: {}",
            r.json
        );
    }
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
    // The host call is what the MCP tool makes with the resolved request; the test over
    // the MCP endpoint itself is `mcp::the_same_key_over_rest_and_mcp_saves_once`.
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

/// A file of `tests/fixtures/export/`.
fn export_fixture(name: &str) -> Vec<u8> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/export");
    std::fs::read(dir.join(name)).unwrap()
}

/// Imports `body` into `store` and exports the store again, as parsed lines.
async fn import_then_export(
    env: &Env,
    store: knoggin_core::StoreId,
    query: &str,
    body: Vec<u8>,
) -> (Value, Vec<Value>) {
    let ctx = env.t.ctx_for(store, RW);
    let r = env
        .send(
            Request::post(format!("/v1/import{query}"))
                .header("content-type", "application/x-ndjson")
                .extension(ctx.clone())
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let e = env
        .call(Some(&ctx), Method::GET, "/v1/export", None, &[])
        .await;
    let lines = e
        .bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice(l).unwrap())
        .collect();
    (r.json, lines)
}

/// What an import keeps of an export line: no header, no automatic semantic link, the
/// saver replaced by `import`, and the `default` namespace's own creation time.
fn as_imported(lines: Vec<Value>) -> Vec<Value> {
    lines
        .into_iter()
        .filter_map(|mut v| match v["type"].as_str() {
            Some("header") => None,
            Some("edge") if v["autoCreated"] == true && v["edgeType"] == "associative" => None,
            Some("memory") => {
                v["savedBy"] = json!("import");
                v["client"] = Value::Null;
                Some(v)
            }
            Some("namespace") if v["name"] == "default" => {
                v["createdAt"] = Value::Null;
                Some(v)
            }
            _ => Some(v),
        })
        .collect()
}

#[tokio::test]
async fn the_good_export_fixture_imports_and_exports_back() {
    // No model: no vector lands, so no automatic link appears after the import.
    let env = Env::new(false).await;
    let good = export_fixture("good.jsonl");
    let file: Vec<Value> = good
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice(l).unwrap())
        .collect();
    let store = env.t.new_store("Good").await;
    let (r, exported) = import_then_export(&env, store, "", good.clone()).await;
    // 3 namespaces, 7 memories and 6 edges; the automatic semantic link is skipped.
    assert_eq!(r, json!({ "imported": 16, "skipped": 1, "errors": [] }));
    assert_eq!(as_imported(exported.clone()), as_imported(file));
    // A second time changes nothing.
    let (r, again) = import_then_export(&env, store, "", good).await;
    assert_eq!(r, json!({ "imported": 0, "skipped": 17, "errors": [] }));
    assert_eq!(as_imported(again), as_imported(exported));
}

#[tokio::test]
async fn the_bad_export_fixture_reports_each_bad_line_and_applies_the_rest() {
    let env = Env::new(false).await;
    let bad = export_fixture("bad.jsonl");
    let expected: Value = serde_json::from_slice(&export_fixture("bad-expected.json")).unwrap();
    // Too big to commit: one line over 4 MiB, after the file's last line.
    let mut body = bad.clone();
    body.extend_from_slice(&vec![b' '; 4 * 1024 * 1024 + 1]);
    body.push(b'\n');
    let last = bad.iter().filter(|b| **b == b'\n').count() as u64 + 1;
    let store = env.t.new_store("Bad").await;
    let (r, exported) = import_then_export(&env, store, "?onConflict=error", body).await;
    let mut want: Vec<(u64, String, String)> = expected["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let s = |k: &str| e[k].as_str().unwrap().to_owned();
            (e["line"].as_u64().unwrap(), s("code"), s("says"))
        })
        .collect();
    want.push((last, "invalid_argument".into(), "longer than 4 MiB".into()));
    let got = r["errors"].as_array().unwrap();
    assert_eq!(got.len(), want.len(), "{r:#}");
    for (g, (line, code, says)) in got.iter().zip(&want) {
        assert_eq!(
            (g["line"].as_u64(), g["code"].as_str()),
            (Some(*line), Some(code.as_str())),
            "{g}"
        );
        assert!(
            g["message"].as_str().unwrap().contains(says.as_str()),
            "{g} lacks {says:?}"
        );
    }
    assert_eq!(
        (&r["imported"], &r["skipped"]),
        (&expected["imported"], &expected["skipped"])
    );
    // The good lines are in: three namespaces, four memories and one edge.
    let names = |kind: &str, key: &str| -> Vec<String> {
        exported
            .iter()
            .filter(|v| v["type"] == kind)
            .map(|v| v[key].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(names("namespace", "name"), ["default", "later", "scratch"]);
    let id = |n: u32| format!("01a20000-0000-7000-8000-0000000b{n:04x}");
    assert_eq!(names("memory", "id"), [id(1), id(2), id(3), id(4)]);
    assert_eq!(names("edge", "target"), [id(4)]);
    assert_eq!(exported.last().unwrap()["weight"], 0.5, "the first of two");
    // A bad first line refuses the whole file.
    let rest = &bad[bad.iter().position(|b| *b == b'\n').unwrap()..];
    for (i, header) in [
        r#"{"type":"header","format":"knoggin-export","version":2}"#,
        r#"{"type":"header","format":"recalld-export","version":1}"#,
        r#"{"type":"header","format":"knoggin-export","version":1,"extra":1}"#,
        r#"{"type":"header","format":"knoggin-export"}"#,
        r#"{"type":"namespace","name":"first"}"#,
        "not json",
    ]
    .into_iter()
    .enumerate()
    {
        let other = env.t.new_store(&format!("Refused {i}")).await;
        let mut body = header.as_bytes().to_vec();
        body.extend_from_slice(rest);
        let r = env
            .send(
                Request::post("/v1/import")
                    .header("content-type", "application/x-ndjson")
                    .extension(env.t.ctx_for(other, RW))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await;
        assert_eq!(r.error(), Code::InvalidArgument, "{header}");
    }
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

#[tokio::test]
async fn the_65th_request_to_a_store_is_unavailable() {
    let env = Env::new(false).await;
    let ctx = env.t.ctx(RW);
    env.call(Some(&ctx), Method::GET, "/v1/namespaces", None, &[])
        .await;
    // Imports whose bodies never come each hold one of the store's request permits.
    let hold = || {
        let request = Request::post("/v1/import")
            .header("content-type", "application/x-ndjson")
            .extension(ctx.clone())
            .body(Body::from_stream(futures_util::stream::pending::<
                Result<Vec<u8>, std::io::Error>,
            >()))
            .unwrap();
        tokio::spawn(tower::ServiceExt::oneshot(env.app.clone(), request))
    };
    let settle = || async {
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
    };
    let mut held: Vec<_> = (0..63).map(|_| hold()).collect();
    settle().await;
    let r = env
        .call(Some(&ctx), Method::GET, "/v1/namespaces", None, &[])
        .await;
    assert_eq!(r.status, StatusCode::OK, "the 64th is served");
    held.push(hold());
    settle().await;
    let r = env
        .call(Some(&ctx), Method::GET, "/v1/namespaces", None, &[])
        .await;
    assert_eq!(r.error(), Code::Unavailable, "{}", r.json);
    assert_eq!(r.headers["retry-after"], "1");
    for task in held {
        task.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn a_request_past_its_time_limit_is_a_timeout() {
    let mut config = knoggin_http::testing::fake_config();
    config.limits.request_timeout_ms = 1_000;
    let env = Env::with_config(config, false).await;
    // Opening the store takes longer than the limit.
    *env.counting.open_delay.lock().unwrap() = std::time::Duration::from_secs(5);
    let ctx = env.t.ctx(RW);
    let r = env
        .call(Some(&ctx), Method::GET, "/v1/namespaces", None, &[])
        .await;
    assert_eq!(r.error(), Code::Timeout, "{}", r.json);
}

#[tokio::test(start_paused = true)]
async fn an_import_that_stops_sending_is_ended_after_60_seconds() {
    let env = Env::new(false).await;
    let ctx = env.t.ctx(RW);
    let record =
        knoggin_core::transfer::TransferRecord::Header(knoggin_core::transfer::ExportHeader {
            format: "knoggin-export".into(),
            version: 1,
            exported_at: None,
            model: None,
            counts: None,
        });
    let header = serde_json::to_string(&record).unwrap() + "\n";
    let body = futures_util::stream::iter([Ok::<_, std::io::Error>(header.into_bytes())])
        .chain(futures_util::stream::pending());
    let started = tokio::time::Instant::now();
    let r = env
        .send(
            Request::post("/v1/import")
                .header("content-type", "application/x-ndjson")
                .extension(ctx.clone())
                .body(Body::from_stream(body))
                .unwrap(),
        )
        .await;
    assert_eq!(r.error(), Code::Timeout, "{}", r.json);
    let waited = started.elapsed();
    assert!(
        waited >= std::time::Duration::from_secs(60) && waited < std::time::Duration::from_secs(61),
        "{waited:?}"
    );
}
