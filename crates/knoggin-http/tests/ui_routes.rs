// SPDX-License-Identifier: AGPL-3.0-only
//! The web UI's read models and event stream against `store_layer` with an inserted
//! context (tab "Web UI", Read-model routes and Live events; tab "REST API", Tests that
//! must exist).
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
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use knoggin_http::error::Code;
use knoggin_http::{Permissions, RequestContext};
use serde_json::{Value, json};
use tower::ServiceExt;

use support::Env;

const RW: Permissions = Permissions::READ_WRITE;
const RO: Permissions = Permissions::READ;

async fn post(env: &Env, ctx: &RequestContext, uri: &str, body: Value) -> Value {
    let r = env
        .call(Some(ctx), Method::POST, uri, Some(body), &[])
        .await;
    assert!(r.status.is_success(), "{uri}: {}", r.json);
    r.json
}

async fn get(env: &Env, ctx: &RequestContext, uri: &str) -> Value {
    let r = env.call(Some(ctx), Method::GET, uri, None, &[]).await;
    assert_eq!(r.status, StatusCode::OK, "{uri}: {}", r.json);
    r.json
}

async fn settle(env: &Env, ctx: &RequestContext) {
    for _ in 0..500 {
        if get(env, ctx, "/v1/context").await["embedding"]["pending"] == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the embedding queue did not drain");
}

/// Three memories: two in `default` (a parent and its child, sharing an entity) and one in
/// `work`. Returns their IDs.
async fn seed(env: &Env, ctx: &RequestContext) -> (String, String, String) {
    post(env, ctx, "/v1/namespaces", json!({ "name": "work" })).await;
    let a = post(
        env,
        ctx,
        "/v1/memories",
        json!({ "summary": "Maya prefers window seats", "tags": ["travel"], "entities": ["Maya"] }),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let b = post(
        env,
        ctx,
        "/v1/memories",
        json!({ "summary": "Maya flies to Lisbon in May", "entities": ["maya"], "parentId": a }),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let c = post(
        env,
        ctx,
        "/v1/memories",
        json!({ "summary": "The quarterly plan is due Friday", "namespace": "work", "tags": ["plan"] }),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    settle(env, ctx).await;
    (a, b, c)
}

#[tokio::test]
async fn every_read_model_answers_and_none_records_access() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let (a, b, c) = seed(&env, &ctx).await;
    // A read-write credential, which could record access, and a read-only one may read
    // them all; the namespace header is not read.
    let mut rw = env.t.ctx(RW);
    rw.default_namespace = Some("work".into());
    let mut ro = env.t.ctx(RO);
    ro.default_namespace = Some("work".into());
    let uris = [
        "/v1/ui/graph".to_owned(),
        "/v1/ui/graph?namespace=work&edgeOffset=0".into(),
        "/v1/ui/memories?sort=strength&desc=true&limit=10&offset=0".into(),
        format!("/v1/ui/memories?id={a}&id={c}&phase=full&phase=summary"),
        "/v1/ui/facets".into(),
        format!("/v1/ui/memories/{a}/history?limit=5"),
        format!("/v1/ui/memories/{a}/neighbors?depth=2"),
        "/v1/ui/search?q=Maya%20seats&limit=5".into(),
        "/v1/ui/search?q=plan&namespace=work".into(),
        "/v1/ui/activity?since=2020-01-01T00:00:00.000Z&limit=10".into(),
        "/v1/ui/health".into(),
        "/v1/ui/health?namespace=default".into(),
        "/v1/ui/system".into(),
    ];
    for reader in [&rw, &ro] {
        for uri in &uris {
            get(&env, reader, uri).await;
        }
    }
    let all = get(&env, &ro, "/v1/ui/memories").await;
    assert_eq!(all["total"], 3, "no namespace means every namespace: {all}");
    for id in [&a, &b, &c] {
        let m = get(&env, &ctx, &format!("/v1/memories/{id}?recordAccess=false")).await;
        assert_eq!(
            (m["accessCount"].clone(), m["retrievedAt"].clone()),
            (json!(0), Value::Null),
            "{m}"
        );
    }
}

#[tokio::test]
async fn the_answers_use_names_and_times_except_the_graph_columns() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let (a, b, _) = seed(&env, &ctx).await;

    let page = get(&env, &ctx, &format!("/v1/ui/memories?id={a}")).await;
    let row = &page["rows"][0];
    assert_eq!(page["total"], 1);
    assert_eq!(row["phase"], "full");
    assert_eq!(row["namespace"], "default");
    assert_eq!(row["nextChange"]["to"], "summary");
    assert!(row["createdAt"].as_str().unwrap().ends_with('Z'));
    assert!(row["nextChange"]["at"].as_str().unwrap() > row["createdAt"].as_str().unwrap());
    assert_eq!(row["retrievedAt"], Value::Null);
    assert_eq!(row["tags"], json!(["travel"]));
    assert!(row["degree"].as_u64().unwrap() >= 1);
    let filtered = get(&env, &ctx, "/v1/ui/memories?entity=MAYA&tag=travel").await;
    assert_eq!(
        filtered["total"], 1,
        "entities match without case: {filtered}"
    );
    let by_created = get(&env, &ctx, "/v1/ui/memories?sort=created").await;
    let times: Vec<&str> = by_created["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["createdAt"].as_str().unwrap())
        .collect();
    assert!(
        times.windows(2).all(|w| w[0] >= w[1]),
        "newest first by default"
    );

    let facets = get(&env, &ctx, "/v1/ui/facets?namespace=default").await;
    assert_eq!(facets["tags"], json!([{ "name": "travel", "count": 1 }]));
    assert_eq!(facets["clients"], json!([]));

    let n = get(&env, &ctx, &format!("/v1/ui/memories/{a}/neighbors")).await;
    assert_eq!(n["center"], a.as_str());
    let edge = n["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "parent_child")
        .unwrap_or_else(|| panic!("no parent link: {n}"));
    assert_eq!(
        (edge["src"].as_str(), edge["dst"].as_str()),
        (Some(a.as_str()), Some(b.as_str()))
    );
    let near = n["nodes"].as_array().unwrap();
    assert!(
        near.iter()
            .any(|x| x["id"] == b.as_str() && x["hop"] == 1 && x["phase"] == "full")
    );

    let g = get(&env, &ctx, "/v1/ui/graph").await;
    assert_eq!(g["nodeCount"], 3);
    assert!(g["generatedAt"].is_string());
    let ns: Vec<Value> = g["namespaces"].as_array().unwrap().clone();
    assert!(
        ns.iter()
            .any(|x| x["name"] == "work" && x["thresholds"] == json!([0.7, 0.3, 0.05]))
    );
    let bytes = |k: &str, v: &Value| STANDARD.decode(v[k].as_str().unwrap()).unwrap();
    assert_eq!(bytes("phase", &g["nodes"]), vec![1, 1, 1]);
    assert_eq!(bytes("permastore", &g["nodes"]), vec![0, 0, 0]);
    assert_eq!(bytes("ns", &g["nodes"]).len(), 12);
    assert_eq!(bytes("createdAt", &g["nodes"]).len(), 24);
    let last = bytes("lastAccess", &g["nodes"]);
    assert!(last.iter().all(|b| *b == 0), "never accessed is 0");
    let types = bytes("type", &g["edges"]);
    assert_eq!(types.len() as u64, g["edgeTotal"].as_u64().unwrap());
    assert!(types.contains(&1) && types.iter().all(|t| [1, 2, 5, 6, 7].contains(t)));
    assert_eq!(g["nextEdgeOffset"], Value::Null);
    let later = get(&env, &ctx, "/v1/ui/graph?edgeOffset=1").await;
    assert_eq!(
        later["nodes"],
        Value::Null,
        "nodes come on the first page only"
    );
    assert_eq!(
        bytes("type", &later["edges"]).len() as u64,
        g["edgeTotal"].as_u64().unwrap() - 1
    );

    let s = get(&env, &ctx, "/v1/ui/search?q=Maya").await;
    assert!(s.get("query").is_none(), "the query is not echoed");
    let hits = s["results"].as_array().unwrap();
    assert!(!hits.is_empty(), "{s}");
    assert!(
        hits.iter()
            .all(|h| h["phase"] == "full" && h["score"].is_number())
    );
    assert!(
        hits.windows(2)
            .all(|w| w[0]["score"].as_f64() >= w[1]["score"].as_f64())
    );
    assert!(
        hits.iter()
            .any(|h| h["text"].is_number() || h["vector"].is_number()),
        "{s}"
    );
    let both = get(
        &env,
        &ctx,
        "/v1/ui/search?q=Maya%20plan%20Friday%20Lisbon&limit=50",
    )
    .await;
    assert_eq!(both["merged"], true, "{both}");
    let one = get(&env, &ctx, "/v1/ui/search?q=plan&namespace=work").await;
    assert_eq!(one["merged"], false);

    post(
        &env,
        &ctx,
        &format!("/v1/memories/{a}/reinforce"),
        json!({ "quality": 4 }),
    )
    .await;
    let h = get(&env, &ctx, &format!("/v1/ui/memories/{a}/history")).await;
    assert_eq!(h["events"][0]["kind"], "reinforcement", "{h}");
    assert!(h["events"][0]["at"].is_string());
    assert_eq!(h["total"], 1);
}

#[tokio::test]
async fn bad_parameters_are_refused() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let (a, _, _) = seed(&env, &ctx).await;
    let missing = "0193a1f2-7c3e-7b1a-9f00-2b6f3c1d9e10";
    for (uri, code) in [
        ("/v1/ui/graph?bogus=1".to_owned(), Code::InvalidArgument),
        ("/v1/ui/graph?edgeOffset=x".into(), Code::InvalidArgument),
        ("/v1/ui/memories?limit=201".into(), Code::InvalidArgument),
        ("/v1/ui/memories?limit=0".into(), Code::InvalidArgument),
        ("/v1/ui/memories?phase=2".into(), Code::InvalidArgument),
        ("/v1/ui/memories?sort=size".into(), Code::InvalidArgument),
        ("/v1/ui/memories?desc=yes".into(), Code::InvalidArgument),
        ("/v1/ui/memories?id=nope".into(), Code::InvalidArgument),
        ("/v1/ui/memories?edgeLimit=5".into(), Code::InvalidArgument),
        (
            format!("/v1/ui/memories/{a}/neighbors?depth=3"),
            Code::InvalidArgument,
        ),
        (
            format!("/v1/ui/memories/{a}/history?limit=101"),
            Code::InvalidArgument,
        ),
        (format!("/v1/ui/memories/{missing}/history"), Code::NotFound),
        (
            format!("/v1/ui/memories/{missing}/neighbors"),
            Code::NotFound,
        ),
        ("/v1/ui/memories/nope/history".into(), Code::InvalidArgument),
        ("/v1/ui/search".into(), Code::InvalidArgument),
        ("/v1/ui/search?q=x&limit=51".into(), Code::InvalidArgument),
        ("/v1/ui/search?q=%20".into(), Code::InvalidArgument),
        (
            "/v1/ui/activity?since=yesterday".into(),
            Code::InvalidArgument,
        ),
        ("/v1/ui/activity?limit=501".into(), Code::InvalidArgument),
        (
            "/v1/ui/system?namespace=default".into(),
            Code::InvalidArgument,
        ),
        ("/v1/ui/events?x=1".into(), Code::InvalidArgument),
    ] {
        let r = env.call(Some(&ctx), Method::GET, &uri, None, &[]).await;
        assert_eq!(r.error(), code, "{uri}: {}", r.json);
    }
    for route in [
        "graph",
        "memories",
        "facets",
        "search?q=x&",
        "activity",
        "health",
    ] {
        let sep = if route.ends_with('&') { "" } else { "?" };
        let uri = format!("/v1/ui/{route}{sep}namespace=nowhere");
        let r = env.call(Some(&ctx), Method::GET, &uri, None, &[]).await;
        assert_eq!(r.error(), Code::NotFound, "{uri}");
    }
    // Without a context (the edge's job) a route answers internal, as every store route.
    let r = env.call(None, Method::GET, "/v1/ui/graph", None, &[]).await;
    assert_eq!(
        (r.status, &r.json["error"]["code"]),
        (StatusCode::INTERNAL_SERVER_ERROR, &json!("internal"))
    );
    // A credential without memory:read.
    let none = env.t.ctx(Permissions {
        read: false,
        write: false,
    });
    let r = env
        .call(Some(&none), Method::GET, "/v1/ui/health", None, &[])
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
}

#[tokio::test]
async fn the_activity_joins_history_with_the_live_events() {
    let env = Env::new(true).await;
    let mut ctx = env.t.ctx(RW);
    ctx.client = Some("codex".into());
    let (a, b, _) = seed(&env, &ctx).await;
    post(
        &env,
        &ctx,
        "/v1/recall",
        json!({ "query": "Maya window seats", "limit": 2 }),
    )
    .await;
    let plain = env.t.ctx(RW);
    post(
        &env,
        &plain,
        &format!("/v1/memories/{b}/reinforce"),
        json!({}),
    )
    .await;
    let act = get(&env, &ctx, "/v1/ui/activity").await;
    assert_eq!(act["bucketMs"], 7 * 86_400_000_i64);
    let buckets = act["buckets"].as_array().unwrap();
    assert!(!buckets.is_empty() && buckets[0]["start"].is_string());
    assert_eq!(
        buckets
            .iter()
            .map(|x| x["full"].as_u64().unwrap())
            .sum::<u64>(),
        3
    );
    assert!(
        buckets
            .iter()
            .map(|x| x["recalls"].as_u64().unwrap())
            .sum::<u64>()
            >= 1
    );
    let rows = act["rows"].as_array().unwrap();
    let kind = |k: &str| rows.iter().filter(|r| r["kind"] == k).collect::<Vec<_>>();
    let recall = kind("recall");
    assert_eq!(recall.len(), 1, "one recall is one row: {act}");
    assert_eq!(recall[0]["client"], "codex");
    assert!(recall[0]["count"].as_u64().unwrap() >= 1);
    assert!(recall[0]["id"].is_string() && recall[0]["summary"].is_string());
    let reinforce = kind("reinforce");
    assert_eq!(reinforce.len(), 1);
    assert_eq!(
        (reinforce[0]["id"].as_str(), reinforce[0]["count"].clone()),
        (Some(b.as_str()), json!(1))
    );
    assert_eq!(
        reinforce[0]["client"],
        Value::Null,
        "a REST request has no client name"
    );
    let stored = kind("store");
    assert_eq!(stored.len(), 3);
    let first = stored.iter().find(|r| r["id"] == a.as_str()).unwrap();
    assert_eq!(first["client"], "codex");
    assert!(
        first["count"].is_number(),
        "a save with its event has a link count"
    );
    assert!(
        rows.windows(2)
            .all(|w| w[0]["at"].as_str() >= w[1]["at"].as_str())
    );
    // A namespace scope keeps only its own rows.
    let work = get(&env, &ctx, "/v1/ui/activity?namespace=work&limit=1").await;
    assert_eq!(work["rows"].as_array().unwrap().len(), 1);
    assert_eq!(work["rows"][0]["namespace"], "work");
}

#[tokio::test]
async fn health_and_system_report_the_store_and_the_process() {
    let env = Env::new(true).await;
    let mut ctx = env.t.ctx(RW);
    ctx.client = Some("claude-code".into());
    seed(&env, &ctx).await;
    let h = get(&env, &ctx, "/v1/ui/health").await;
    assert_eq!(h["scope"], "all");
    assert_eq!(h["overview"]["totalMemories"], 3);
    assert_eq!(
        h["overview"]["phaseCounts"],
        json!({ "full": 3, "summary": 0, "ghost": 0 })
    );
    assert_eq!(
        h["decayForecast"]["transitions90d"]["fullToSummary"], 3,
        "{h}"
    );
    assert_eq!(h["decayForecast"]["transitions7d"]["fullToSummary"], 0);
    let risk = h["atRisk"].as_array().unwrap();
    assert_eq!(risk.len(), 3);
    assert_eq!(risk[0]["to"], "summary");
    assert!(risk[0]["id"].as_str().unwrap().len() == 36 && risk[0]["summary"].is_string());
    assert!(h["storage"]["textBytes"].as_u64().unwrap() > 50);
    assert_eq!(h["storage"]["vectorBytes"], 3 * 64 * 4);
    let scoped = get(&env, &ctx, "/v1/ui/health?namespace=work").await;
    assert_eq!(scoped["overview"]["totalMemories"], 1);
    assert_eq!(
        scoped["storage"], h["storage"],
        "storage is for the whole store"
    );
    // The last snapshot's size, once one is written.
    let mut size = Value::Null;
    for _ in 0..100 {
        size = get(&env, &ctx, "/v1/ui/health").await["storage"]["lastSnapshotBytes"].clone();
        if size.is_number() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(size.as_u64().unwrap() > 0, "{size}");

    post(&env, &ctx, "/v1/recall", json!({ "query": "plan" })).await;
    let s = get(&env, &ctx, "/v1/ui/system").await;
    assert_eq!(s["version"], env!("CARGO_PKG_VERSION"));
    assert!(s["startedAt"].is_string() && s["uptimeSecs"].is_number());
    assert_eq!(s["health"]["status"], "ok", "{s}");
    let names: Vec<&str> = s["health"]["subsystems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["store", "embedding", "sweeper"]);
    assert_eq!(s["model"]["state"], "ready");
    assert!(
        s["recallLatencyMs"].is_number() && s["embedLatencyMs"].is_number(),
        "{s}"
    );
    assert!(s["sweep"]["lastAt"].as_str() < s["sweep"]["nextAt"].as_str());
    assert_eq!(s["clients"][0]["name"], "claude-code");
    assert!(s["clients"][0]["lastSeenAt"].is_string());
    assert!(s["process"].get("rssBytes").is_some());
    for gone in ["pid", "dataDir", "socket", "protocolVersion"] {
        assert!(s.get(gone).is_none(), "{gone}");
    }
}

/// Opens the event stream of `ctx`'s store.
async fn stream(env: &Env, ctx: &RequestContext) -> http::Response<Body> {
    let mut request = Request::builder()
        .uri("/v1/ui/events")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ctx.clone());
    env.app.clone().oneshot(request).await.unwrap()
}

/// The next named event of an event stream, or `None` when none comes within `wait`.
async fn next_event(body: &mut Body, buf: &mut String, wait: Duration) -> Option<(String, Value)> {
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

#[tokio::test]
async fn the_event_stream_carries_the_stores_events_and_opens_nothing() {
    let env = Env::new(true).await;
    let mut ctx = env.t.ctx(RO);
    // The store is not open: 204, and the stream does not open it.
    let r = stream(&env, &ctx).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert!(!env.t.host.is_open(ctx.store_id));
    let writer = {
        let mut w = env.t.ctx(RW);
        w.client = Some("codex".into());
        w
    };
    get(&env, &writer, "/v1/namespaces").await;
    let r = stream(&env, &ctx).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "text/event-stream");
    let mut body = r.into_body();
    let mut buf = String::new();
    let wait = Duration::from_secs(5);
    let saved = post(
        &env,
        &writer,
        "/v1/memories",
        json!({ "summary": "mkrsecretsummary about Maya", "entities": ["Maya"] }),
    )
    .await;
    let id = saved["id"].as_str().unwrap();
    let (name, data) = next_event(&mut body, &mut buf, wait).await.unwrap();
    assert_eq!(name, "store");
    assert_eq!(data["kind"], "store");
    assert_eq!(data["id"], id);
    assert_eq!(
        (data["namespace"].as_str(), data["phase"].as_str()),
        (Some("default"), Some("full"))
    );
    assert_eq!(data["client"], "codex");
    assert!(data["at"].is_i64() && data["seq"].is_u64());
    assert!(data["edges"].is_array());
    post(
        &env,
        &writer,
        "/v1/recall",
        json!({ "query": "mkrqueryword Maya" }),
    )
    .await;
    let mut seen = Vec::new();
    while let Some((name, data)) = next_event(&mut body, &mut buf, Duration::from_millis(500)).await
    {
        let text = data.to_string();
        assert!(
            !text.contains("mkrsecretsummary") && !text.contains("mkrqueryword"),
            "{text}"
        );
        if name == "recall" {
            assert_eq!(data["returned"][0], id, "{data}");
            assert_eq!(data["namespace"], "default");
            assert!(
                data["seeds"].is_array()
                    && data["spreads"].is_array()
                    && data["suppressed"].is_array()
            );
            assert!(data.get("query").is_none() && data.get("trace").is_none());
        }
        seen.push(name);
    }
    assert!(seen.contains(&"recall".to_owned()), "{seen:?}");
    post(
        &env,
        &writer,
        &format!("/v1/memories/{id}/reinforce"),
        json!({ "quality": 1 }),
    )
    .await;
    let (name, data) = next_event(&mut body, &mut buf, wait).await.unwrap();
    assert_eq!(
        (name.as_str(), data["rating"].clone()),
        ("reinforce", json!(1))
    );
    assert!(data["stabilityAfter"].as_f64() < data["stabilityBefore"].as_f64());
    let r = env
        .call(
            Some(&writer),
            Method::DELETE,
            &format!("/v1/memories/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let (name, data) = next_event(&mut body, &mut buf, wait).await.unwrap();
    assert_eq!((name.as_str(), data["id"].as_str()), ("forget", Some(id)));

    // Another store's events go to its own streams only.
    let app = env.t.new_store("other").await;
    let other = env.t.ctx_for(app, RW);
    get(&env, &other, "/v1/namespaces").await;
    post(
        &env,
        &other,
        "/v1/memories",
        json!({ "summary": "elsewhere" }),
    )
    .await;
    assert!(
        next_event(&mut body, &mut buf, Duration::from_millis(300))
            .await
            .is_none()
    );

    // The stream ends when its store closes.
    env.t.host.close(ctx.store_id).await.unwrap();
    let end = tokio::time::timeout(wait, async {
        while let Some(frame) = body.frame().await {
            if frame.is_err() {
                break;
            }
        }
    })
    .await;
    assert!(end.is_ok(), "the stream did not end with its store");
    ctx.store_id = app;
    assert_eq!(stream(&env, &ctx).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_recall_names_the_namespace_as_stored_whatever_its_spelling() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let (_, _, c) = seed(&env, &ctx).await;
    let r = stream(&env, &ctx).await;
    let mut body = r.into_body();
    let mut buf = String::new();
    // No access recorded, so only the event can make the Timeline's row.
    post(
        &env,
        &ctx,
        "/v1/recall",
        json!({ "query": "quarterly plan", "namespace": "WORK", "recordAccess": false }),
    )
    .await;
    let (name, data) = next_event(&mut body, &mut buf, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(name, "recall");
    assert_eq!(data["namespace"], "work", "{data}");
    for scope in ["work", "Work"] {
        let act = get(&env, &ctx, &format!("/v1/ui/activity?namespace={scope}")).await;
        let recalls: Vec<&Value> = act["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == "recall")
            .collect();
        assert_eq!(recalls.len(), 1, "{scope}: {act}");
        assert_eq!(recalls[0]["id"], c.as_str());
    }
    let other = get(&env, &ctx, "/v1/ui/activity?namespace=default").await;
    assert!(
        other["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["kind"] != "recall"),
        "{other}"
    );
}

#[tokio::test]
async fn a_stop_or_park_ends_every_stream_at_once_and_later_ones_get_204() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    get(&env, &ctx, "/v1/namespaces").await;
    let r = stream(&env, &ctx).await;
    assert_eq!(r.status(), StatusCode::OK);
    let mut body = r.into_body();
    env.t.host.end_event_streams();
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = body.frame().await {
            if frame.is_err() {
                break;
            }
        }
    })
    .await;
    assert!(end.is_ok(), "the stream did not end");
    assert!(env.t.host.is_open(ctx.store_id));
    assert_eq!(stream(&env, &ctx).await.status(), StatusCode::NO_CONTENT);
}
