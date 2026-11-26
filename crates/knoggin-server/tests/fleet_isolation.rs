// SPDX-License-Identifier: AGPL-3.0-only
//! The fleet's isolation tests (tab "Security", rows I1, I4 to I7, and the internal
//! link's checks of tab "Server"), in one process: two users behind one router, each with
//! a container that runs the real container role. The test holds the internal secret, so
//! it can call the broker with a container's own token, which the real stack does not
//! allow from outside. Checks that need each container on its own address run on Linux
//! only, where each fake container listens on its own `127.0.0.x`; elsewhere they are
//! skipped with a printed reason.
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
use fleet_support::{Deployment, RouterHandle, eventually, peer, save, summaries};
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use knoggin_core::{StoreId, UserId};
use knoggin_http::ctx::{CredentialKind, CredentialRef, Permissions, RequestContext};
use knoggin_server::broker::api::paths;
use knoggin_server::link::{Peers, forwarding_client, write_container_call, write_router_call};
use knoggin_state::record::{Container, LeaseState, ParkReason};
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
        assert_eq!(c.user_id, Some(user));
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
    write_container_call(req.headers_mut(), &t.d.launch_secret(c.id), user, c.id);
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
async fn i1_a_users_request_reaches_only_their_own_container_and_stores() {
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
async fn i4_a_container_refuses_a_forwarded_request_for_another_user() {
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
async fn i5_a_container_holding_bobs_token_cannot_reach_alices_stores() {
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
async fn i6_a_container_cannot_use_another_containers_identity() {
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
async fn i7_a_replaced_container_gets_lease_lost() {
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
        write_container_call(req.headers_mut(), &t.d.launch_secret(a.3.id), b.1, b.3.id);
        let _ = t.d.broker_client(Some(flood)).request(req).await;
    }
    let mut req = http::Request::post(format!(
        "https://127.0.0.1:{}{}",
        t.r.internal.port(),
        paths::open(b.2)
    ))
    .body(Body::from("{}"))
    .unwrap();
    write_container_call(req.headers_mut(), &t.d.launch_secret(b.3.id), b.1, b.3.id);
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
