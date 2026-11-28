// SPDX-License-Identifier: AGPL-3.0-only
//! The fleet's isolation tests and the internal link's checks, in one process: two users behind one
//! router, each with a container that runs the real container role. The test holds the internal
//! secret, so it can call the broker with a container's own token, which the real stack does not
//! allow from outside. Checks that need each container on its own address run on Linux only, where
//! each fake container listens on its own `127.0.0.x`; elsewhere they are skipped with a printed
//! reason.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod fleet_support;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use fleet_support::{
    Deployment, RouterHandle, eventually, json, local_user, peer, save, summaries,
};
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use knoggin_core::{StoreId, UserId};
use knoggin_http::ctx::{CredentialKind, CredentialRef, Permissions, RequestContext};
use knoggin_server::broker::api::paths;
use knoggin_server::link::{Peers, forwarding_client, write_container_call, write_router_call};
use knoggin_state::record::{Container, LeaseState, ParkReason, Slot};
use serde_json::json;
use uuid::Uuid;

const LIMIT: Duration = Duration::from_secs(20);

struct Two {
    d: Deployment,
    r: RouterHandle,
    alice: (String, UserId, StoreId, Container),
    bob: (String, UserId, StoreId, Container),
}

async fn two() -> Two {
    let d = Deployment::new(true).await;
    let r = d.router("0.1.0").await;
    let mut out = Vec::new();
    for sub in ["alice", "bob"] {
        let token = d.token(&r.url, sub);
        assert_eq!(
            save(&r, Some(&token), &format!("{sub}-secret")).await.0,
            201
        );
        let user = d.user_of(sub).await;
        let store = d
            .users()
            .await
            .get(user)
            .await
            .unwrap()
            .unwrap()
            .value
            .personal_store_id;
        let lease = d.lease(user).await;
        assert_eq!(lease.state, LeaseState::Warm);
        let c = d
            .containers()
            .get(lease.container_id.unwrap())
            .await
            .unwrap()
            .unwrap()
            .value;
        assert_eq!(c.slot, Some(Slot::User(user)));
        out.push((token, user, store, c));
    }
    let bob = out.pop().unwrap();
    let alice = out.pop().unwrap();
    Two { d, r, alice, bob }
}

/// A broker call as container `c` claims to be, for `user`, from `local`.
async fn broker_call(
    t: &Two,
    c: &Container,
    user: UserId,
    local: Option<IpAddr>,
    method: Method,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut req = http::Request::builder()
        .method(method)
        .uri(format!("https://127.0.0.1:{}{path}", t.r.internal.port()))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    write_container_call(
        req.headers_mut(),
        &t.d.launch_secret(c.id),
        Slot::User(user),
        c.id,
    );
    let answer = t.d.broker_client(local).request(req).await.unwrap();
    let status = answer.status();
    let bytes = answer.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// The local address a container's own calls come from.
fn own_address(c: &Container) -> Option<IpAddr> {
    cfg!(target_os = "linux").then(|| IpAddr::V4(*peer(c).address.ip()))
}

#[tokio::test]
async fn a_users_request_reaches_only_their_own_container_and_stores() {
    let t = two().await;
    let (a, b) = (&t.alice, &t.bob);
    assert_ne!(a.3.id, b.3.id, "one container per user");
    assert_eq!(
        summaries(&t.r, Some(&a.0)).await.1,
        vec!["alice-secret".to_owned()]
    );
    assert_eq!(
        summaries(&t.r, Some(&b.0)).await.1,
        vec!["bob-secret".to_owned()]
    );
    // Naming the other user's store is refused, and wakes nothing.
    let launched = t.d.launcher.handles().len();
    let r =
        t.r.req(reqwest::Method::GET, "/v1/memories")
            .bearer_auth(&a.0)
            .header("knoggin-store", b.2.to_string())
            .send()
            .await
            .unwrap();
    assert_ne!(r.status(), 200);
    assert!(!r.text().await.unwrap().contains("bob-secret"));
    assert_eq!(t.d.launcher.handles().len(), launched);
    // The same requests went to the same containers.
    assert_eq!(t.d.lease(a.1).await.container_id, Some(a.3.id));
    assert_eq!(t.d.lease(b.1).await.container_id, Some(b.3.id));
}

#[tokio::test]
async fn a_container_refuses_a_forwarded_request_for_another_user() {
    let t = two().await;
    let (a, b) = (&t.alice, &t.bob);
    let peers = Arc::new(Peers::default());
    peers.set(a.3.id, peer(&a.3));
    let client = forwarding_client::<Body>(peers).unwrap();
    let ctx = RequestContext {
        request_id: Uuid::now_v7(),
        user_id: b.1,
        store_id: b.2,
        permissions: Permissions::READ_WRITE,
        credential: CredentialRef {
            kind: CredentialKind::Oauth,
            id: Some(b.1.uuid()),
        },
        default_namespace: None,
        client: None,
    };
    let mut req = http::Request::get(format!(
        "https://{}:{}/v1/memories",
        a.3.id,
        peer(&a.3).address.port()
    ))
    .body(Body::empty())
    .unwrap();
    ctx.write_internal_headers(req.headers_mut()).unwrap();
    write_router_call(req.headers_mut(), &t.d.launch_secret(a.3.id).router_token());
    let answer = client.request(req).await.unwrap();
    assert!(!answer.status().is_success(), "{}", answer.status());
    let text = answer.into_body().collect().await.unwrap().to_bytes();
    assert!(!String::from_utf8_lossy(&text).contains("alice-secret"));
    // Alice is still served.
    assert_eq!(
        summaries(&t.r, Some(&a.0)).await.1,
        vec!["alice-secret".to_owned()]
    );
}

#[tokio::test]
async fn a_container_holding_bobs_token_cannot_reach_alices_stores() {
    let t = two().await;
    let (a, b) = (&t.alice, &t.bob);
    let from = own_address(&b.3);
    let calls = [
        (Method::POST, paths::open(a.2), json!({})),
        (
            Method::POST,
            paths::snapshots(a.2),
            json!({ "sequence": 1, "sizeBytes": 10 }),
        ),
        (
            Method::POST,
            paths::commit(a.2, 1),
            json!({ "memoryCount": 1, "namespaceCount": 1, "sizeBytes": 10 }),
        ),
    ];
    for (m, path, body) in calls {
        let (status, v) = broker_call(&t, &b.3, b.1, from, m, &path, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {v}");
    }
    // Bob's own store answers on the same identity, so the refusal is the owner check.
    let (status, _) = broker_call(
        &t,
        &b.3,
        b.1,
        from,
        Method::POST,
        &paths::open(b.2),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The blob route refuses one of Alice's objects.
    let key = format!("stores/{}/snap/0000000001-0000000001.kgs", a.2);
    let (status, _) = broker_call(
        &t,
        &b.3,
        b.1,
        from,
        Method::GET,
        &format!("{}{key}", paths::BLOB_PREFIX),
        json!(null),
    )
    .await;
    assert!(!status.is_success(), "{status}");
}

#[tokio::test]
async fn a_container_cannot_use_another_containers_identity() {
    let t = two().await;
    let (a, b) = (&t.alice, &t.bob);
    // Alice's user ID with Bob's token.
    let (status, v) = broker_call(
        &t,
        &b.3,
        a.1,
        own_address(&b.3),
        Method::POST,
        &paths::open(a.2),
        json!({}),
    )
    .await;
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("lease_lost"))
    );
    // Bob's whole valid identity from another address.
    if cfg!(target_os = "linux") {
        let other: IpAddr = "127.0.0.250".parse().unwrap();
        let (status, v) = broker_call(
            &t,
            &b.3,
            b.1,
            Some(other),
            Method::POST,
            &paths::open(b.2),
            json!({}),
        )
        .await;
        assert_eq!(
            (status, v["error"]["code"].as_str()),
            (StatusCode::CONFLICT, Some("lease_lost"))
        );
    } else {
        println!("skipped: the address check needs a container on its own 127.0.0.x (Linux)");
    }
}

#[tokio::test]
async fn a_replaced_container_gets_lease_lost() {
    let t = two().await;
    let b = &t.bob;
    let fleet =
        t.d.fleet("9.0.0", true, &format!("https://{}", t.r.internal))
            .await;
    assert!(fleet.park(b.3.id, peer(&b.3), ParkReason::Restore).await);
    assert!(
        eventually(LIMIT, || async {
            t.d.lease(b.1).await.state == LeaseState::Parked
        })
        .await
    );
    // Bob's next request claims a new container; the old identity is refused.
    assert_eq!(
        summaries(&t.r, Some(&b.0)).await.1,
        vec!["bob-secret".to_owned()]
    );
    assert_ne!(t.d.lease(b.1).await.container_id, Some(b.3.id));
    let (status, v) = broker_call(
        &t,
        &b.3,
        b.1,
        own_address(&b.3),
        Method::POST,
        &paths::open(b.2),
        json!({}),
    )
    .await;
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("lease_lost"))
    );
}

#[tokio::test]
async fn a_container_that_floods_the_internal_listener_is_cut_off_while_others_are_served() {
    if !cfg!(target_os = "linux") {
        println!("skipped: the flood needs a peer address of its own (Linux)");
        return;
    }
    let t = two().await;
    let (a, b) = (&t.alice, &t.bob);
    let flood: IpAddr = "127.0.0.251".parse().unwrap();
    for _ in 0..5 {
        let mut req = http::Request::post(format!(
            "https://127.0.0.1:{}{}",
            t.r.internal.port(),
            paths::open(b.2)
        ))
        .body(Body::from("{}"))
        .unwrap();
        // A token of another container: a failed check.
        write_container_call(
            req.headers_mut(),
            &t.d.launch_secret(a.3.id),
            Slot::User(b.1),
            b.3.id,
        );
        let _ = t.d.broker_client(Some(flood)).request(req).await;
    }
    let mut req = http::Request::post(format!(
        "https://127.0.0.1:{}{}",
        t.r.internal.port(),
        paths::open(b.2)
    ))
    .body(Body::from("{}"))
    .unwrap();
    write_container_call(
        req.headers_mut(),
        &t.d.launch_secret(b.3.id),
        Slot::User(b.1),
        b.3.id,
    );
    assert!(
        t.d.broker_client(Some(flood)).request(req).await.is_err(),
        "the address is refused"
    );
    // Alice's container still saves through the broker.
    let fleet =
        t.d.fleet("9.0.0", true, &format!("https://{}", t.r.internal))
            .await;
    assert!(fleet.park(a.3.id, peer(&a.3), ParkReason::Restore).await);
    assert!(
        eventually(LIMIT, || async {
            t.d.lease(a.1).await.state == LeaseState::Parked
        })
        .await
    );
    assert_eq!(
        summaries(&t.r, Some(&a.0)).await.1,
        vec!["alice-secret".to_owned()]
    );
}

#[tokio::test]
async fn a_container_launched_at_an_ended_containers_address_is_served_at_once() {
    let t = two().await;
    let a = &t.alice;
    let address = peer(&a.3).address;
    // Alice's container ends; her lease still names its address.
    t.d.launcher
        .kill(&t.d.launcher.handle_of(a.3.id).unwrap(), false);
    assert_eq!(t.d.lease(a.1).await.state, LeaseState::Warm);
    if cfg!(target_os = "linux") {
        // Bad tokens from that address would refuse it for a minute.
        for _ in 0..5 {
            let _ = broker_call(
                &t,
                &a.3,
                a.1,
                Some(IpAddr::V4(*address.ip())),
                Method::POST,
                &paths::open(a.2),
                json!({}),
            )
            .await;
        }
    }
    // A new user's container is launched at that very address.
    *t.d.launcher.fixed_address.lock().unwrap() = Some(address);
    let erin = t.d.token(&t.r.url, "erin");
    assert_eq!(save(&t.r, Some(&erin), "erin-note").await.0, 201);
    *t.d.launcher.fixed_address.lock().unwrap() = None;
    let e = t.d.user_of("erin").await;
    let c =
        t.d.containers()
            .get(t.d.lease(e).await.container_id.unwrap())
            .await
            .unwrap()
            .unwrap()
            .value;
    assert_eq!(peer(&c).address, address);
    // The older lease was ended in the same step, and the old token from that address is
    // refused.
    assert_eq!(t.d.lease(a.1).await.state, LeaseState::Parked);
    let (status, v) = broker_call(
        &t,
        &a.3,
        a.1,
        own_address(&c),
        Method::POST,
        &paths::open(a.2),
        json!({}),
    )
    .await;
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("lease_lost"))
    );
    // Erin's container saves through the broker at once.
    let fleet =
        t.d.fleet("9.0.0", true, &format!("https://{}", t.r.internal))
            .await;
    assert!(fleet.park(c.id, peer(&c), ParkReason::Restore).await);
    assert!(
        eventually(LIMIT, || async {
            t.d.lease(e).await.state == LeaseState::Parked
        })
        .await
    );
    assert_eq!(
        summaries(&t.r, Some(&erin)).await.1,
        vec!["erin-note".to_owned()]
    );
}

/// A request through the router with an API key of an app, or none (the personal store
/// of the one user of a deployment with sign-in off).
fn as_store(
    r: &RouterHandle,
    key: Option<&str>,
    method: reqwest::Method,
    path: &str,
) -> reqwest::RequestBuilder {
    let b = r.req(method, path);
    match key {
        Some(k) => b.bearer_auth(k),
        None => b,
    }
}

/// One MCP tool call of the current revision as client `client`; returns the tool's JSON.
async fn mcp(
    r: &RouterHandle,
    key: Option<&str>,
    client: &str,
    tool: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
        "name": tool, "arguments": args, "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": { "name": client, "version": "1" },
            "io.modelcontextprotocol/clientCapabilities": {} } } });
    let (status, v) = json(
        as_store(r, key, reqwest::Method::POST, "/mcp")
            .header("mcp-method", "tools/call")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-name", tool)
            .json(&body)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert!(v["result"]["isError"].is_null(), "{tool}: {v}");
    serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// Stores kept apart within one container, which holds every store of its user: an
/// `Idempotency-Key` is kept per store, app X's read models show nothing of the personal
/// store or of app Y, and X's event stream carries none of their events.
#[tokio::test]
async fn one_container_keeps_a_users_stores_apart() {
    let d = Deployment::new(false).await;
    let r = d.router("0.1.0").await;
    let mut keys = Vec::new();
    for name in ["X", "Y"] {
        let (status, app) = json(
            r.req(reqwest::Method::POST, "/v1/stores")
                .json(&json!({ "name": name }))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, 201, "{app}");
        let (status, key) = json(
            r.req(
                reqwest::Method::POST,
                &format!("/v1/stores/{}/keys", app["id"].as_str().unwrap()),
            )
            .json(&json!({ "name": name, "permissions": ["memory:read", "memory:write"] }))
            .send()
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(status, 201, "{key}");
        keys.push(key["secret"].as_str().unwrap().to_owned());
    }
    let (x, y) = (keys[0].as_str(), keys[1].as_str());

    // The same key and body to X twice (a replay), then to Y (a run of its own).
    let keyed = |key: &str| {
        as_store(&r, Some(key), reqwest::Method::POST, "/v1/memories")
            .header("idempotency-key", "k-shared")
            .json(&json!({ "summary": "same body" }))
            .send()
    };
    let first = keyed(x).await.unwrap();
    assert_eq!(first.status(), 201);
    assert!(first.headers().get("idempotent-replayed").is_none());
    let first = json(first).await.1;
    let again = keyed(x).await.unwrap();
    assert_eq!(again.headers()["idempotent-replayed"], "true");
    assert_eq!(json(again).await.1["id"], first["id"]);
    let other = keyed(y).await.unwrap();
    assert_eq!(other.status(), 201);
    assert!(
        other.headers().get("idempotent-replayed").is_none(),
        "Y answered from X's record"
    );
    assert_ne!(json(other).await.1["id"], first["id"]);

    // Distinct data and client names in each store, all in the one container.
    for (key, mark) in [(Some(x), "mkrx"), (None, "mkrpersonal"), (Some(y), "mkry")] {
        let client = format!("{mark}agent");
        mcp(
            &r,
            key,
            &client,
            "store_memory",
            json!({ "summary": format!("{mark}note"), "tags": [format!("{mark}tag")] }),
        )
        .await;
        mcp(
            &r,
            key,
            &client,
            "recall_memories",
            json!({ "query": format!("{mark}note") }),
        )
        .await;
    }
    let launched: Vec<_> = d
        .records()
        .await
        .into_iter()
        .filter(|c| c.slot.is_some())
        .collect();
    assert_eq!(launched.len(), 1, "one container holds the three stores");
    for path in [
        "/v1/ui/system",
        "/v1/ui/activity",
        "/v1/ui/facets",
        "/v1/ui/health",
    ] {
        let answer = as_store(&r, Some(x), reqwest::Method::GET, path)
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status(), 200, "{path}");
        let text = answer.text().await.unwrap();
        assert!(
            text.contains("mkrxnote") || text.contains("mkrxagent") || path == "/v1/ui/system",
            "{path}: {text}"
        );
        for other in ["mkrpersonal", "mkry"] {
            assert!(
                !text.contains(other),
                "{path} with X's key shows {other}: {text}"
            );
        }
    }
    let (_, s) = json(
        as_store(&r, Some(x), reqwest::Method::GET, "/v1/ui/system")
            .send()
            .await
            .unwrap(),
    )
    .await;
    let clients: Vec<&str> = s["clients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(clients, ["mkrxagent"], "{s}");
    let (_, h) = json(
        as_store(&r, Some(x), reqwest::Method::GET, "/v1/ui/health")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        h["overview"]["totalMemories"], 2,
        "the replayed save and the MCP one: {h}"
    );

    // X's event stream: a save, a recall and a forget in the personal store and in Y put
    // nothing on it; X's own next save comes first.
    let mut stream = as_store(&r, Some(x), reqwest::Method::GET, "/v1/ui/events")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    let mut foreign = Vec::new();
    for key in [None, Some(y)] {
        let (status, saved) = json(
            as_store(&r, key, reqwest::Method::POST, "/v1/memories")
                .json(&json!({ "summary": "mkrforeign" }))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, 201, "{saved}");
        let id = saved["id"].as_str().unwrap().to_owned();
        let (status, _) = json(
            as_store(&r, key, reqwest::Method::POST, "/v1/recall")
                .json(&json!({ "query": "mkrforeign" }))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, 200);
        let (status, _) = json(
            as_store(
                &r,
                key,
                reqwest::Method::DELETE,
                &format!("/v1/memories/{id}"),
            )
            .send()
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(status, 200);
        foreign.push(id);
    }
    let (status, mine) = json(
        as_store(&r, Some(x), reqwest::Method::POST, "/v1/memories")
            .json(&json!({ "summary": "mine" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, 201);
    let mut seen = String::new();
    let mine = format!("\"id\":\"{}\"", mine["id"].as_str().unwrap());
    // Everything up to X's own event, then whatever follows within half a second.
    while !seen.contains(&mine) {
        let chunk = tokio::time::timeout(LIMIT, stream.chunk())
            .await
            .unwrap()
            .unwrap()
            .expect("the stream ended");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    while let Ok(Ok(Some(chunk))) =
        tokio::time::timeout(Duration::from_millis(500), stream.chunk()).await
    {
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    let first_event = seen.split("\n\n").find(|b| b.contains("event: ")).unwrap();
    assert!(
        first_event.contains("event: store") && first_event.contains(&mine),
        "{seen}"
    );
    for id in &foreign {
        assert!(!seen.contains(id.as_str()), "{seen}");
    }
    assert!(
        !seen.contains("event: recall") && !seen.contains("event: forget"),
        "{seen}"
    );
}

/// An app that becomes isolated moves to a container of its own while it is written to:
/// the record changes first, the user's container is asked to park and saves what it
/// holds, and the app's own container opens it after that. Every write answered as saved,
/// before and during the move, is there afterwards.
#[tokio::test]
async fn turning_isolation_on_loses_no_write() {
    let d = Deployment::new(false).await;
    let r = d.router("0.1.0").await;
    let (user, _) = local_user(&d).await;
    let (status, app) = json(
        r.req(reqwest::Method::POST, "/v1/stores")
            .json(&json!({ "name": "moving" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, 201, "{app}");
    let app_id = StoreId::from_uuid(app["id"].as_str().unwrap().parse().unwrap());
    let (status, key) = json(
        r.req(reqwest::Method::POST, &format!("/v1/stores/{app_id}/keys"))
            .json(&json!({ "name": "k", "permissions": ["memory:read", "memory:write"] }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, 201, "{key}");
    let key = key["secret"].as_str().unwrap().to_owned();
    let mut written = Vec::new();
    for i in 0..3 {
        let text = format!("before{i}");
        assert_eq!(save(&r, Some(&key), &text).await.0, 201);
        written.push(text);
    }
    assert_eq!(save(&r, None, "personalnote").await.0, 201);
    let first = d.lease(user).await;
    assert_eq!(first.state, LeaseState::Warm);

    // Isolation on, through the state layer.
    d.stores()
        .await
        .update(user, app_id, |s| {
            s.isolated = true;
            Ok::<_, knoggin_state::StateError>(())
        })
        .await
        .unwrap();
    // Writes go on through the move; one that is refused for the wait is sent again.
    for i in 0..3 {
        let text = format!("during{i}");
        let saved = eventually(LIMIT, || async {
            let (status, _) = save(&r, Some(&key), &text).await;
            assert!(status == 201 || status == 503, "{status}");
            status == 201
        })
        .await;
        assert!(saved, "{text} was not saved");
        written.push(text);
    }
    let (status, mut found) = summaries(&r, Some(&key)).await;
    assert_eq!(status, 200);
    found.sort();
    written.sort();
    assert_eq!(found, written, "every write is in the app");
    // The app is served by its own container now, charged to its owner; the user's
    // own container saved and parked, and their personal store is intact.
    let own = d.lease_of(Slot::Store(app_id)).await;
    assert_eq!(own.state, LeaseState::Warm);
    let record = d
        .records()
        .await
        .into_iter()
        .find(|c| Some(c.id) == own.container_id)
        .unwrap();
    assert_eq!(
        (record.slot, record.charged_to),
        (Some(Slot::Store(app_id)), Some(user))
    );
    assert_ne!(own.container_id, first.container_id);
    assert_eq!(
        summaries(&r, None).await,
        (200, vec!["personalnote".to_owned()])
    );
}
