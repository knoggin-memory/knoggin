// SPDX-License-Identifier: AGPL-3.0-only
//! The container role in process against the broker: hello, claim, the park sequence, the
//! memory budget, the 64 MiB of bodies in flight, `lease_lost`, and, as far as they run in
//! one process, that a container serves only its claimed user and stops once replaced.
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
use bytes::Bytes;
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use knoggin_core::snapshot::SnapshotName;
use knoggin_server::container::{EndReason, ServeTimings};
use knoggin_server::link::api::{ContainerPhase, paths};
use knoggin_server::link::{CertPin, Nonce, Peer, ServerIdentity};
use knoggin_server::storage::{Fault, names};
use knoggin_state::record::ParkReason;
use support::{Options, Rig, eventually, quick, settings, snapshots};

const GIB: u64 = 1 << 30;
const SECONDS: Duration = Duration::from_secs(10);

fn memories_of(body: &Bytes) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap();
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["summary"].as_str().unwrap().to_owned())
        .collect()
}

fn ndjson(req: &mut http::Request<Body>) {
    req.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/x-ndjson"),
    );
}

#[tokio::test]
async fn hello_claim_and_the_router_token() {
    let rig = Rig::start(Options::default()).await;
    let (a, b) = (rig.account().await, rig.account().await);
    let c = rig.container(quick()).await;

    // A nonce is answered once; a recorded proof does not fit a new nonce.
    let nonce = Nonce::random().unwrap();
    let (first, cert) = c.hello(&nonce).await;
    let first = first.unwrap();
    assert!(first.test_mode);
    assert_eq!(first.protocols, [1]);
    assert!(c.hello(&nonce).await.0.is_none(), "a replayed nonce");
    let other = Nonce::random().unwrap();
    assert!(!c.launch.check_proof(&other, &cert, &first.proof));
    // A proof made with another container's secret does not match.
    let stranger = rig.secret.launch_secret(uuid::Uuid::now_v7());
    assert!(!stranger.check_proof(&nonce, &cert, &first.proof));

    // Every route but hello needs the router token.
    let wrong = c.launch.container_token();
    let (status, _, _) = c
        .call(Method::GET, paths::STATUS, Body::empty(), Some(&wrong))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let mut no_protocol = http::Request::get(format!(
        "https://{}:{}{}",
        c.id,
        c.addr.port(),
        paths::STATUS
    ))
    .body(Body::empty())
    .unwrap();
    knoggin_server::link::write_router_call(no_protocol.headers_mut(), &c.launch.router_token());
    no_protocol.headers_mut().remove("knoggin-protocol");
    assert_eq!(
        c.send(no_protocol).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let mut stolen = c.request(a.user, a.store, Method::GET, "/v1/memories", Body::empty());
    knoggin_server::link::write_router_call(stolen.headers_mut(), &wrong);
    assert_eq!(c.send(stolen).await.0, StatusCode::UNAUTHORIZED);

    // A pool container serves no one.
    let (status, _, _) = c
        .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(c.status().await.state, ContainerPhase::Ready);

    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    assert_eq!(c.claim(a.user, g, settings(600, GIB)).await, StatusCode::OK);
    // A second claim, for another user or the same one, is refused.
    assert_eq!(
        c.claim(b.user, g, settings(600, GIB)).await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        c.claim(a.user, g, settings(600, GIB)).await,
        StatusCode::CONFLICT
    );
    let status = c.status().await;
    assert_eq!(status.state, ContainerPhase::Claimed);
    assert_eq!(status.user_id, Some(a.user));
    assert_eq!(status.generation, Some(g));

    // A request for another user is refused; the claimed user is served.
    let (status, _, _) = c
        .forward(b.user, b.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(c.save(a, "hello").await, StatusCode::CREATED);
    let (status, _, body) = c
        .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(memories_of(&body), ["hello"]);

    // An internal route is never served to a forwarded request.
    let req = c.request(
        a.user,
        a.store,
        Method::POST,
        paths::PARK,
        r#"{"reason":"drain"}"#,
    );
    assert_eq!(c.send(req).await.0, StatusCode::NOT_FOUND);
    let req = c.request(
        a.user,
        a.store,
        Method::GET,
        "/internal/v1/status",
        Body::empty(),
    );
    assert_eq!(c.send(req).await.0, StatusCode::NOT_FOUND);
    assert_eq!(c.status().await.state, ContainerPhase::Claimed);

    // A required internal header missing or twice is `internal`.
    let mut missing = c.request(a.user, a.store, Method::GET, "/v1/memories", Body::empty());
    missing.headers_mut().remove("knoggin-store-id");
    assert_eq!(c.send(missing).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    let mut twice = c.request(a.user, a.store, Method::GET, "/v1/memories", Body::empty());
    twice
        .headers_mut()
        .append("knoggin-user-id", b.user.to_string().parse().unwrap());
    assert_eq!(c.send(twice).await.0, StatusCode::INTERNAL_SERVER_ERROR);

    // A router that pins another certificate never gets a connection.
    let std::net::SocketAddr::V4(v4) = c.addr else {
        unreachable!()
    };
    let impostor = ServerIdentity::for_container(c.id).unwrap();
    c.peers.set(
        c.id,
        Peer {
            address: v4,
            pin: impostor.pin(),
        },
    );
    let fresh = knoggin_server::link::forwarding_client::<Body>(c.peers.clone()).unwrap();
    let req = c.request(a.user, a.store, Method::GET, "/v1/memories", Body::empty());
    assert!(fresh.request(req).await.is_err());
    let _: CertPin = impostor.pin();
}

#[tokio::test]
async fn a_store_is_saved_on_park_and_opened_by_the_next_container() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let app = rig.app(a.user, "notes").await;
    let mut c1 = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c1, g).await;
    assert_eq!(
        c1.claim(a.user, g, settings(600, GIB)).await,
        StatusCode::OK
    );
    assert_eq!(c1.save(a, "first").await, StatusCode::CREATED);
    let in_app = support::Account {
        user: a.user,
        store: app,
    };
    assert_eq!(c1.save(in_app, "in the app").await, StatusCode::CREATED);
    assert_eq!(c1.park(ParkReason::Restore).await, StatusCode::ACCEPTED);
    let end = c1.end(SECONDS).await;
    assert_eq!((end.reason, end.exit_code), (EndReason::Parked, 0));
    assert_eq!(*rig.parked.lock().unwrap(), [c1.id]);
    let name = SnapshotName::new(g, 1).unwrap();
    assert_eq!(
        snapshots(&rig, a.store),
        [names::snapshot_key(a.store, name)]
    );
    let record = rig
        .stores
        .get_of(a.user, a.store)
        .await
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(
        record.newest.map(|n| (n.generation, n.sequence)),
        Some((g, 1))
    );
    assert_eq!(record.memory_count, 1);
    assert!(record.last_write_ms.is_some());
    // The bucket saw the signed headers and nothing of the link's identity.
    for seen in rig.storage.seen() {
        assert!(seen.status < 300, "{seen:?}");
        for name in &seen.header_names {
            assert!(
                !name.starts_with("knoggin-") && name != "authorization",
                "{name} reached the bucket"
            );
        }
    }

    // The next container, with a higher generation, loads what the first saved.
    rig.clock.advance(5_000);
    let c2 = rig.container(quick()).await;
    let g2 = rig.generation();
    rig.lease(a.user, &c2, g2).await;
    assert_eq!(
        c2.claim(a.user, g2, settings(600, GIB)).await,
        StatusCode::OK
    );
    let (status, _, body) = c2
        .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(memories_of(&body), ["first"]);
    let (_, _, body) = c2
        .forward(a.user, app, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(memories_of(&body), ["in the app"]);
}

#[tokio::test]
async fn links_point_straight_at_the_bucket_without_one_host() {
    let rig = Rig::start(Options {
        one_host: false,
        ..Options::default()
    })
    .await;
    let a = rig.account().await;
    let mut c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    assert_eq!(c.save(a, "direct").await, StatusCode::CREATED);
    c.stop();
    let end = c.end(SECONDS).await;
    assert_eq!(end.reason, EndReason::Parked);
    assert_eq!(snapshots(&rig, a.store).len(), 1);
    assert!(
        !rig.broker_calls
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.starts_with(knoggin_server::broker::api::paths::BLOB_PREFIX))
    );
}

#[tokio::test]
async fn an_unreadable_newest_upload_falls_back_and_a_missing_newest_locks() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let mut c1 = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c1, g).await;
    c1.claim(a.user, g, settings(600, GIB)).await;
    assert_eq!(c1.save(a, "kept").await, StatusCode::CREATED);
    c1.park(ParkReason::Drain).await;
    assert_eq!(c1.end(SECONDS).await.reason, EndReason::Parked);
    // An upload above the recorded newest that never committed, and is junk.
    let junk = names::snapshot_key(a.store, SnapshotName::new(g, 2).unwrap());
    rig.storage.insert(
        &junk,
        vec![b'x'; 4096],
        knoggin_core::Timestamp::from_millis(rig.clock.now()),
    );
    rig.clock.advance(5_000);
    let c2 = rig.container(quick()).await;
    let g2 = rig.generation();
    rig.lease(a.user, &c2, g2).await;
    c2.claim(a.user, g2, settings(600, GIB)).await;
    let (status, _, body) = c2
        .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(memories_of(&body), ["kept"]);

    // With the recorded newest itself gone, the store is locked and nothing older or
    // empty is served.
    let b = rig.account().await;
    let mut c3 = rig.container(quick()).await;
    rig.lease(b.user, &c3, g2).await;
    c3.claim(b.user, g2, settings(600, GIB)).await;
    assert_eq!(c3.save(b, "lost").await, StatusCode::CREATED);
    c3.stop();
    assert_eq!(c3.end(SECONDS).await.reason, EndReason::Parked);
    for key in snapshots(&rig, b.store) {
        rig.storage.remove(&key);
    }
    rig.storage.insert(
        &names::snapshot_key(b.store, SnapshotName::new(g2, 9).unwrap()),
        vec![b'x'; 4096],
        knoggin_core::Timestamp::from_millis(rig.clock.now()),
    );
    rig.clock.advance(5_000);
    let c4 = rig.container(quick()).await;
    let g4 = rig.generation();
    rig.lease(b.user, &c4, g4).await;
    c4.claim(b.user, g4, settings(600, GIB)).await;
    let (status, _, body) = c4
        .forward(b.user, b.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::LOCKED);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["reason"], "snapshot_unreadable");
}

#[tokio::test]
async fn a_taken_name_moves_to_the_next_sequence() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let mut c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    assert_eq!(c.save(a, "one").await, StatusCode::CREATED);
    // Someone took (g, 1) after the open.
    let taken = names::snapshot_key(a.store, SnapshotName::new(g, 1).unwrap());
    rig.storage.insert(
        &taken,
        Bytes::from_static(b"other"),
        knoggin_core::Timestamp::from_millis(rig.clock.now()),
    );
    c.stop();
    assert_eq!(c.end(SECONDS).await.reason, EndReason::Parked);
    assert_eq!(
        rig.storage.get(&taken).unwrap(),
        Bytes::from_static(b"other")
    );
    let next = names::snapshot_key(a.store, SnapshotName::new(g, 2).unwrap());
    assert!(rig.storage.get(&next).is_some());
    let record = rig
        .stores
        .get_of(a.user, a.store)
        .await
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(record.newest.map(|n| n.sequence), Some(2));
    assert!(
        rig.storage
            .seen()
            .iter()
            .any(|s| s.method == "PUT" && s.status == 412)
    );
}

#[tokio::test]
async fn over_the_upload_budget_writes_wait_and_upload_again_later() {
    let rig = Rig::start(Options {
        upload_bytes_per_hour: 1 << 20,
        ..Options::default()
    })
    .await;
    let a = rig.account().await;
    let mut c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    assert_eq!(c.save(a, "before").await, StatusCode::CREATED);
    // The user's containers used the hour's budget.
    let body = serde_json::json!({"sequence": 99, "sizeBytes": 1 << 20}).to_string();
    let (status, _, _) = rig
        .broker_call(
            c.id,
            a.user,
            Method::POST,
            &knoggin_server::broker::api::paths::snapshots(a.store),
            body,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(c.park(ParkReason::Upgrade).await, StatusCode::ACCEPTED);
    // The park fails: the container serves again, takes no writes, and reads go on.
    let refused = eventually(SECONDS, || async {
        let (status, headers, _) = c
            .forward(
                a.user,
                a.store,
                Method::POST,
                "/v1/memories",
                r#"{"summary":"blocked"}"#,
            )
            .await;
        status == StatusCode::TOO_MANY_REQUESTS
            && headers.get("retry-after").is_some_and(|v| v == "60")
    })
    .await;
    assert!(refused);
    let (status, _, body) = c
        .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(memories_of(&body), ["before"]);
    assert!(!c.ended());
    // An hour later the standing park request goes through.
    rig.clock.advance(3_600_000);
    let end = c.end(SECONDS).await;
    assert_eq!(end.reason, EndReason::Parked);
    assert_eq!(snapshots(&rig, a.store).len(), 1);
}

#[tokio::test]
async fn a_lost_lease_stops_everything_before_any_further_broker_call() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let mut c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    assert_eq!(c.save(a, "unsaved").await, StatusCode::CREATED);
    // The lease moves on to another container.
    let mut lease = rig.lease_of(a.user).await;
    lease.container_id = Some(uuid::Uuid::now_v7());
    rig.leases
        .put(a.user, &lease, knoggin_state::Cond::Any)
        .await
        .unwrap();
    let before = rig.broker_calls.lock().unwrap().len();
    c.park(ParkReason::Drain).await;
    let end = c.end(SECONDS).await;
    assert_eq!((end.reason, end.exit_code), (EndReason::LeaseLost, 1));
    let calls = rig.broker_calls.lock().unwrap().clone();
    // One call was refused with lease_lost, and nothing followed it.
    assert_eq!(calls.len(), before + 1, "{calls:?}");
    assert!(calls[before].ends_with("/snapshots"));
    assert!(rig.parked.lock().unwrap().is_empty());
    assert!(snapshots(&rig, a.store).is_empty());
    assert!(!rig.storage.seen().iter().any(|s| s.method == "PUT"));
}

/// `parked` answered `lease_lost`: the lease is lost, the container ends as such and
/// does not try again.
#[tokio::test]
async fn a_lease_lost_answer_to_parked_ends_the_container_as_lease_lost() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let mut c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    // Nothing to save; the lease moves on before the container reports.
    let mut lease = rig.lease_of(a.user).await;
    lease.container_id = Some(uuid::Uuid::now_v7());
    rig.leases
        .put(a.user, &lease, knoggin_state::Cond::Any)
        .await
        .unwrap();
    c.park(ParkReason::Drain).await;
    let end = c.end(SECONDS).await;
    assert_eq!((end.reason, end.exit_code), (EndReason::LeaseLost, 1));
    let calls = rig.broker_calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        [knoggin_server::broker::api::paths::PARKED],
        "one try, not three"
    );
    assert!(rig.parked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_deleted_store_is_dropped_and_the_container_still_parks() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let app = rig.app(a.user, "gone soon").await;
    let in_app = support::Account {
        user: a.user,
        store: app,
    };
    let mut c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    assert_eq!(c.save(a, "personal").await, StatusCode::CREATED);
    assert_eq!(c.save(in_app, "app").await, StatusCode::CREATED);
    let now = rig.clock.now();
    rig.stores
        .update(a.user, app, |r| {
            r.deleted_at_ms = Some(now);
            Ok::<(), knoggin_state::StateError>(())
        })
        .await
        .unwrap();
    c.park(ParkReason::Drain).await;
    assert_eq!(c.end(SECONDS).await.reason, EndReason::Parked);
    assert_eq!(snapshots(&rig, a.store).len(), 1);
    assert!(snapshots(&rig, app).is_empty());

    // A container asked for a deleted store answers store_deleting.
    rig.clock.advance(5_000);
    let c2 = rig.container(quick()).await;
    let g2 = rig.generation();
    rig.lease(a.user, &c2, g2).await;
    c2.claim(a.user, g2, settings(600, GIB)).await;
    let (status, _, body) = c2
        .forward(a.user, app, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::GONE);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "store_deleting");
    let (status, _, _) = c2
        .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::OK);
}

/// A deleted store that its owner's container still holds is erased by the lifecycle
/// task; the container's next save is refused by the broker, it drops the store, and
/// nothing of the store is left.
#[tokio::test]
async fn erasing_a_store_that_a_container_still_holds_leaves_nothing() {
    const DAY: i64 = 24 * 3600 * 1000;
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let app = rig.app(a.user, "erased").await;
    let in_app = support::Account {
        user: a.user,
        store: app,
    };
    let mut c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    assert_eq!(
        c.save(in_app, "unsaved in the app").await,
        StatusCode::CREATED
    );
    // An older snapshot of the app in the bucket.
    rig.storage.insert(
        &names::snapshot_key(app, SnapshotName::new(g - 1, 1).unwrap()),
        vec![1u8; 4],
        knoggin_core::Timestamp::from_millis(rig.clock.now()),
    );
    let now = rig.clock.now();
    rig.stores
        .update(a.user, app, |r| {
            r.deleted_at_ms = Some(now);
            Ok::<(), knoggin_state::StateError>(())
        })
        .await
        .unwrap();
    // Past the undo period and its ten minutes: the lifecycle task erases the app.
    rig.clock.advance(7 * DAY + 11 * 60_000);
    let lifecycle = knoggin_server::lifecycle::Lifecycle {
        db: rig.db.clone(),
        postgres: None,
        storage: rig.storage.clone(),
        retention: knoggin_server::settings::RetentionSettings {
            days: 3,
            soft_delete_days: 7,
        },
    };
    let report = lifecycle
        .run_once(knoggin_core::Timestamp::from_millis(rig.clock.now()))
        .await
        .unwrap();
    assert_eq!(report.stores_erased, 1);
    assert!(
        c.status()
            .await
            .stores
            .iter()
            .any(|s| s.store_id == app && s.unsaved),
        "the container still holds the app"
    );
    c.park(ParkReason::Drain).await;
    let end = c.end(SECONDS).await;
    assert_eq!((end.reason, end.exit_code), (EndReason::Parked, 0));
    let prefix = names::store_prefix(app);
    assert!(rig.storage.keys().iter().all(|k| !k.starts_with(&prefix)));
    assert!(
        !rig.storage
            .seen()
            .iter()
            .any(|s| s.method == "PUT" && s.key.starts_with(&prefix)),
        "nothing of the app was uploaded"
    );
    assert!(rig.stores.get(app).await.unwrap().is_none());
}

#[tokio::test]
async fn a_park_refuses_new_requests_ends_streams_and_cuts_what_outlives_the_wait() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let mut c = rig
        .container(ServeTimings {
            drain: Duration::from_millis(800),
            park_retry: Duration::from_millis(300),
        })
        .await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    assert_eq!(c.save(a, "open").await, StatusCode::CREATED);

    // An open event stream, and an import whose body stalls.
    let events = c.request(a.user, a.store, Method::GET, "/v1/ui/events", Body::empty());
    let stream = c.client.request(events).await.unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    let stream_end = tokio::spawn(async move { stream.into_body().collect().await.is_ok() });
    let stalled = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
    let mut import = c.request(
        a.user,
        a.store,
        Method::POST,
        "/v1/import",
        Body::from_stream(stalled),
    );
    ndjson(&mut import);
    let client = c.client.clone();
    let import = tokio::spawn(async move {
        match client.request(import).await {
            Ok(r) => Some(r.status()),
            Err(_) => None,
        }
    });
    // The import is in flight.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(c.status().await.idle_seconds, 0);

    assert_eq!(c.park(ParkReason::Upgrade).await, StatusCode::ACCEPTED);
    assert!(
        eventually(SECONDS, || async {
            c.status().await.state == ContainerPhase::Parking
        })
        .await
    );
    let (status, headers, _) = c
        .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        headers.get("knoggin-internal-status").unwrap(),
        paths::PARKING
    );
    // The event stream ended at once; the import is cut off after the wait.
    tokio::time::timeout(SECONDS, stream_end)
        .await
        .unwrap()
        .unwrap();
    let cut = tokio::time::timeout(SECONDS, import)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(cut, None | Some(StatusCode::SERVICE_UNAVAILABLE)),
        "{cut:?}"
    );
    let end = c.end(SECONDS).await;
    assert_eq!(end.reason, EndReason::Parked);
    assert_eq!(*rig.parked.lock().unwrap(), [c.id]);
}

/// Writes that race a park: every write the container acknowledged is in the snapshot the
/// park saved, and none is acknowledged after it.
#[tokio::test]
async fn a_write_during_a_park_is_saved_or_refused() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    assert_eq!(c.save(a, "before").await, StatusCode::CREATED);
    let c = std::sync::Arc::new(tokio::sync::Mutex::new(c));
    let writers: Vec<_> = (0..8)
        .map(|w| {
            let c = c.clone();
            tokio::spawn(async move {
                let mut acknowledged = Vec::new();
                for i in 0.. {
                    let summary = format!("w{w}-{i}");
                    let guard = c.lock().await;
                    if guard.ended() {
                        break;
                    }
                    let req = guard.request(
                        a.user,
                        a.store,
                        Method::POST,
                        "/v1/memories",
                        serde_json::json!({ "summary": summary }).to_string(),
                    );
                    let client = guard.client.clone();
                    drop(guard);
                    match client.request(req).await {
                        Ok(r) if r.status() == StatusCode::CREATED => acknowledged.push(summary),
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                acknowledged
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    c.lock().await.park(ParkReason::Drain).await;
    let end = c.lock().await.end(SECONDS).await;
    assert_eq!(end.reason, EndReason::Parked);
    let mut acknowledged = vec!["before".to_owned()];
    for w in writers {
        acknowledged.extend(w.await.unwrap());
    }
    assert!(
        acknowledged.len() > 1,
        "writes went through before the park"
    );
    // The saved snapshot holds exactly the acknowledged writes.
    let record = rig
        .stores
        .get_of(a.user, a.store)
        .await
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(record.memory_count, acknowledged.len() as u64);
}

#[tokio::test]
async fn an_idle_container_parks_but_not_while_the_bucket_is_down() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let mut c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(1, GIB)).await;
    // The store opens; then the bucket fails.
    assert_eq!(
        c.save(a, "kept through an outage").await,
        StatusCode::CREATED
    );
    rig.storage.set_fault(Fault::Fail).await.unwrap();
    // Idle parks are tried and fail: the container keeps serving, and the write stays.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(!c.ended());
    let status = c.status().await;
    assert_eq!(status.state, ContainerPhase::Claimed);
    assert!(status.snapshot_failures > 0);
    assert!(status.stores.iter().any(|s| s.unsaved));
    // A router's park request stands; once the bucket answers again it goes through.
    c.park(ParkReason::Restore).await;
    rig.storage.set_fault(Fault::None).await.unwrap();
    assert_eq!(c.end(SECONDS).await.reason, EndReason::Parked);
    assert_eq!(snapshots(&rig, a.store).len(), 1);

    // An idle container with nothing unsaved parks by itself.
    let b = rig.account().await;
    let mut idle = rig.container(quick()).await;
    rig.lease(b.user, &idle, g).await;
    idle.claim(b.user, g, settings(1, GIB)).await;
    assert_eq!(idle.end(SECONDS).await.reason, EndReason::Parked);
}

/// Only the broker's own answers mean deleted or lease lost. A bucket's 404, 410 or 409
/// through the blob route is an outage: the store stays in memory, the lease stays, and
/// the store is saved once the bucket answers again.
#[tokio::test]
async fn a_bucket_status_through_the_blob_route_is_only_an_outage() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let mut c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    assert_eq!(c.save(a, "acknowledged").await, StatusCode::CREATED);
    for code in [404, 410, 409] {
        rig.storage.set_fault(Fault::Answer(code)).await.unwrap();
        let failures = c.status().await.snapshot_failures;
        // The request stands: the park is tried again every 300 ms.
        c.park(ParkReason::Restore).await;
        let failed = || async { c.ended() || c.status().await.snapshot_failures > failures };
        assert!(eventually(SECONDS, failed).await, "{code}");
        assert!(!c.ended(), "{code}: the container ended");
        let status = c.status().await;
        assert!(
            status
                .stores
                .iter()
                .any(|s| s.store_id == a.store && s.unsaved),
            "{code}: the store was dropped"
        );
    }
    assert!(rig.parked.lock().unwrap().is_empty());
    assert!(snapshots(&rig, a.store).is_empty());
    rig.storage.set_fault(Fault::None).await.unwrap();
    let end = c.end(SECONDS).await;
    assert_eq!((end.reason, end.exit_code), (EndReason::Parked, 0));
    assert_eq!(snapshots(&rig, a.store).len(), 1);

    // A download answered 404 by the bucket is `unavailable`, not a deleted store.
    rig.clock.advance(5_000);
    let c2 = rig.container(quick()).await;
    let g2 = rig.generation();
    rig.lease(a.user, &c2, g2).await;
    c2.claim(a.user, g2, settings(600, GIB)).await;
    rig.storage.set_fault(Fault::Answer(404)).await.unwrap();
    let (status, _, body) = c2
        .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body:?}");
    rig.storage.set_fault(Fault::None).await.unwrap();
    let loaded = || async {
        let (status, _, body) = c2
            .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
            .await;
        status == StatusCode::OK && memories_of(&body) == ["acknowledged"]
    };
    assert!(eventually(SECONDS, loaded).await);
    assert_eq!(rig.lease_of(a.user).await.container_id, Some(c2.id));
}

#[tokio::test]
async fn a_stop_signal_parks_a_claimed_container_and_ends_a_pool_one_at_once() {
    let rig = Rig::start(Options::default()).await;
    let mut pool = rig.container(quick()).await;
    pool.stop();
    let end = pool.end(SECONDS).await;
    assert_eq!((end.reason, end.exit_code), (EndReason::Stopped, 0));
    assert!(rig.broker_calls.lock().unwrap().is_empty());

    let a = rig.account().await;
    let mut c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    // The bucket is down when the stop comes: the saves are tried until they work.
    assert_eq!(c.save(a, "saved late").await, StatusCode::CREATED);
    rig.storage.set_fault(Fault::Fail).await.unwrap();
    c.stop();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!c.ended());
    rig.storage.set_fault(Fault::None).await.unwrap();
    assert_eq!(c.end(SECONDS).await.reason, EndReason::Parked);
    assert_eq!(snapshots(&rig, a.store).len(), 1);
}

#[tokio::test]
async fn the_memory_budget_refuses_writes_and_locks_a_store_that_cannot_fit() {
    const MODEL: u64 = 928 * 1024 * 1024;
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let mut c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    // A budget of 40,000 bytes.
    c.claim(a.user, g, settings(600, MODEL + 100_000)).await;
    let mut refused = false;
    for i in 0..400 {
        let summary = format!("memory number {i} with some words to take room in the store");
        match c.save(a, &summary).await {
            StatusCode::CREATED => {}
            StatusCode::INSUFFICIENT_STORAGE => {
                refused = true;
                break;
            }
            other => panic!("{other}"),
        }
    }
    assert!(refused, "the budget never refused a write");
    let status = c.status().await;
    assert!(
        status
            .stores
            .iter()
            .any(|s| s.store_id == a.store && s.full)
    );
    // Reads keep working.
    let (status, _, _) = c
        .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::OK);
    c.stop();
    assert_eq!(c.end(SECONDS).await.reason, EndReason::Parked);

    // A container whose whole budget is smaller than the snapshot does not load it.
    rig.clock.advance(5_000);
    let small = rig.container(quick()).await;
    let g2 = rig.generation();
    rig.lease(a.user, &small, g2).await;
    small.claim(a.user, g2, settings(600, MODEL + 5_000)).await;
    let (status, _, body) = small
        .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::LOCKED);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["reason"], "out_of_memory");
    let status = small.status().await;
    assert_eq!(status.locks.len(), 1);
}

#[tokio::test]
async fn request_bodies_in_flight_are_capped_at_64_mib() {
    const MIB: u64 = 1024 * 1024;
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    let stalled =
        || Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
    let batch = |len: u64| {
        let mut req = c.request(
            a.user,
            a.store,
            Method::POST,
            "/v1/memories/batch",
            stalled(),
        );
        req.headers_mut().insert("content-length", len.into());
        req
    };
    // One body alone beyond the cap.
    let (status, headers, _) = c.send(batch(64 * MIB + 1)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers.get("retry-after").unwrap(), "1");
    // 60 MiB held by six slow batches, then 5 MiB more is refused, and a small one passes.
    let holders: Vec<_> = (0..6)
        .map(|_| {
            let client = c.client.clone();
            let req = batch(10 * MIB);
            tokio::spawn(async move { client.request(req).await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        holders.iter().all(|h| !h.is_finished()),
        "the batches hold their bodies"
    );
    let (status, headers, _) = c.send(batch(5 * MIB)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers.get("retry-after").unwrap(), "1");
    assert_eq!(c.save(a, "small").await, StatusCode::CREATED);
    for h in holders {
        h.abort();
    }
}

/// An import streams: no limit on the whole body. One of more than 64 MiB goes through.
#[tokio::test]
async fn an_import_larger_than_the_cap_is_streamed() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    let header = serde_json::json!({
        "type": "header", "format": "knoggin-export", "version": 1,
    });
    let mut body = format!("{header}\n").into_bytes();
    // 17 lines just under 4 MiB each, every one of them an error line: 68 MB in all.
    let junk = vec![b'x'; 4_000_000];
    for _ in 0..17 {
        body.extend_from_slice(&junk);
        body.push(b'\n');
    }
    let len = body.len() as u64;
    assert!(len > 64 * 1024 * 1024);
    let mut req = c.request(
        a.user,
        a.store,
        Method::POST,
        "/v1/import",
        Body::from(body),
    );
    ndjson(&mut req);
    req.headers_mut().insert("content-length", len.into());
    let (status, _, answer) = c.send(req).await;
    assert_eq!(status, StatusCode::OK, "{answer:?}");
    let v: serde_json::Value = serde_json::from_slice(&answer).unwrap();
    assert_eq!(v["errors"].as_array().unwrap().len(), 17, "{v}");
}

#[tokio::test]
async fn a_failed_commit_is_tried_again_without_a_new_upload() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let mut c = rig.container(quick()).await;
    let g = rig.generation();
    rig.lease(a.user, &c, g).await;
    c.claim(a.user, g, settings(600, GIB)).await;
    assert_eq!(c.save(a, "committed late").await, StatusCode::CREATED);
    rig.fail_commits
        .store(1, std::sync::atomic::Ordering::SeqCst);
    c.stop();
    assert_eq!(c.end(SECONDS).await.reason, EndReason::Parked);
    let calls = rig.broker_calls.lock().unwrap().clone();
    let commits: Vec<_> = calls.iter().filter(|p| p.ends_with("/commit")).collect();
    let first = format!("/snapshots/{}/commit", 1);
    // (g, 1) committed twice (the failure, then the retry), then (g, 2) for the changes.
    assert_eq!(commits.len(), 3, "{calls:?}");
    assert!(commits[0].ends_with(&first) && commits[1].ends_with(&first));
    let puts = rig
        .storage
        .seen()
        .iter()
        .filter(|s| s.method == "PUT")
        .count();
    assert_eq!(puts, 2, "the failed commit's object is not uploaded again");
    let record = rig
        .stores
        .get_of(a.user, a.store)
        .await
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(
        record.newest.map(|n| (n.generation, n.sequence)),
        Some((g, 2))
    );
}
