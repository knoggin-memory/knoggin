// SPDX-License-Identifier: AGPL-3.0-only
//! The broker in process, called the way a container calls it (tab "Server": the broker,
//! its limits and the one-host blob route; tab "Security": rows I5, I6 and I7). The test
//! holds containers' tokens itself, as the fleet tests may.
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
use bytes::Bytes;
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use knoggin_core::snapshot::SnapshotName;
use knoggin_core::{StoreId, Timestamp, UserId};
use knoggin_crypto::WrappedKey;
use knoggin_server::broker::api::{OpenAnswer, UploadLink, paths};
use knoggin_server::link::write_container_call;
use knoggin_server::storage::names;
use knoggin_state::record::{Lease, LeaseState, ParkReason};
use knoggin_state::{Cond, StateError};
use support::{Account, Options, Rig};
use uuid::Uuid;

/// A container identity with a warm lease of `user` at 127.0.0.1, the test's own address.
async fn holder(rig: &Rig, user: UserId, generation: u64) -> Uuid {
    let id = Uuid::now_v7();
    let old = rig.leases.get(user).await.unwrap();
    let mut lease = old.as_ref().map(|l| l.value.clone()).unwrap_or_default();
    lease.generation = generation;
    lease.container_id = Some(id);
    lease.address = Some("127.0.0.1:8443".into());
    lease.set_state(LeaseState::Warm, rig.clock.now());
    let cond = old.map_or(Cond::Absent, |l| l.cond());
    rig.leases.put(user, &lease, cond).await.unwrap();
    id
}

fn put(rig: &Rig, store: StoreId, g: u64, s: u64, bytes: &'static [u8]) -> String {
    let key = names::snapshot_key(store, SnapshotName::new(g, s).unwrap());
    rig.storage.insert(
        &key,
        Bytes::from_static(bytes),
        Timestamp::from_millis(rig.clock.now()),
    );
    key
}

fn code(body: &Bytes) -> (String, Option<String>) {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap();
    (
        v["error"]["code"].as_str().unwrap().to_owned(),
        v["error"]["reason"].as_str().map(str::to_owned),
    )
}

async fn open(rig: &Rig, id: Uuid, a: Account, body: &str) -> (StatusCode, Bytes) {
    let (status, _, bytes) = rig
        .broker_call(
            id,
            a.user,
            Method::POST,
            &paths::open(a.store),
            body.to_owned(),
        )
        .await;
    (status, bytes)
}

#[tokio::test]
async fn every_call_proves_its_container_and_its_lease() {
    let rig = Rig::start(Options::default()).await;
    let (a, b) = (rig.account().await, rig.account().await);
    let g = rig.generation();
    let id_a = holder(&rig, a.user, g).await;
    let id_b = holder(&rig, b.user, g).await;
    let (status, _) = open(&rig, id_a, a, "").await;
    assert_eq!(status, StatusCode::OK);

    let send = |launch_of: Uuid, user: UserId, named: Uuid, path: String| {
        let rig = &rig;
        async move {
            let mut req = http::Request::post(format!("{}{path}", rig.router_url))
                .body(Body::from("{}"))
                .unwrap();
            write_container_call(
                req.headers_mut(),
                &rig.secret.launch_secret(launch_of),
                user,
                named,
            );
            let answer = rig.broker_client().request(req).await.unwrap();
            let status = answer.status();
            let body = answer.into_body().collect().await.unwrap().to_bytes();
            (status, body)
        }
    };
    // A wrong token: B's launch secret naming A's container.
    let (status, body) = send(id_b, a.user, id_a, paths::open(a.store)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&body).0, "lease_lost");
    // I6: A's user with B's whole valid identity; A's lease names another container.
    let (status, body) = send(id_b, a.user, id_b, paths::open(a.store)).await;
    assert_eq!(
        (status, code(&body).0.as_str()),
        (StatusCode::CONFLICT, "lease_lost")
    );
    // I5: B's container asks for A's store on every store route.
    for path in [
        paths::open(a.store),
        paths::snapshots(a.store),
        paths::commit(a.store, 1),
    ] {
        let (status, _, body) = rig
            .broker_call(
                id_b,
                b.user,
                Method::POST,
                &path,
                r#"{"sequence":1,"sizeBytes":1}"#,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(code(&body).0, "not_found");
    }
    // No protocol header: `internal`.
    let mut bare = http::Request::post(format!("{}{}", rig.router_url, paths::open(a.store)))
        .body(Body::empty())
        .unwrap();
    write_container_call(
        bare.headers_mut(),
        &rig.secret.launch_secret(id_a),
        a.user,
        id_a,
    );
    bare.headers_mut().remove("knoggin-protocol");
    let answer = rig.broker_client().request(bare).await.unwrap();
    assert_eq!(answer.status(), StatusCode::INTERNAL_SERVER_ERROR);
    // Malformed requests.
    let bad = |path: String, body: &'static str| {
        let rig = &rig;
        async move {
            rig.broker_call(id_a, a.user, Method::POST, &path, body)
                .await
                .0
        }
    };
    assert_eq!(
        bad("/internal/v1/stores/not-a-uuid/open".into(), "").await,
        StatusCode::BAD_REQUEST
    );
    let huge = r#"{"sequence":1,"sizeBytes":1000000000000}"#;
    assert_eq!(
        bad(paths::snapshots(a.store), huge).await,
        StatusCode::BAD_REQUEST
    );
    let far = r#"{"sequence":10000000000,"sizeBytes":1}"#;
    assert_eq!(
        bad(paths::snapshots(a.store), far).await,
        StatusCode::BAD_REQUEST
    );
    let commit = r#"{"memoryCount":1,"namespaceCount":1,"sizeBytes":1}"#;
    assert_eq!(
        bad(
            format!(
                "/internal/v1/stores/{}/snapshots/10000000000/commit",
                a.store
            ),
            commit
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        bad(paths::open(a.store), r#"{"unknown":1}"#).await,
        StatusCode::BAD_REQUEST
    );
    // A public route on the internal listener.
    let (status, _, _) = rig
        .broker_call(id_a, a.user, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The blob route is off without one host... and on here.
    assert_ne!(
        rig.broker_call(
            id_a,
            a.user,
            Method::GET,
            "/internal/v1/blob/x",
            Body::empty()
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    // I7: once the lease names another container, the old one gets lease_lost.
    let _new = holder(&rig, a.user, g + 1).await;
    let (status, body) = open(&rig, id_a, a, "").await;
    assert_eq!(
        (status, code(&body).0.as_str()),
        (StatusCode::CONFLICT, "lease_lost")
    );
    // And a parked lease names no one.
    let mut lease: Lease = rig.lease_of(b.user).await;
    lease.set_state(LeaseState::Parked, rig.clock.now());
    rig.leases.put(b.user, &lease, Cond::Any).await.unwrap();
    let b_open = rig
        .broker_call(id_b, b.user, Method::POST, &paths::open(b.store), "")
        .await;
    assert_eq!(b_open.0, StatusCode::CONFLICT);

    // A valid identity from another address than the lease's.
    if cfg!(target_os = "linux") {
        let id_c = holder(&rig, b.user, g).await;
        let client = knoggin_server::link::router_client::<Body>(
            rig.router_pin,
            Some(std::net::IpAddr::from([127, 0, 0, 2])),
        )
        .unwrap();
        let mut req = http::Request::post(format!("{}{}", rig.router_url, paths::open(b.store)))
            .body(Body::empty())
            .unwrap();
        write_container_call(
            req.headers_mut(),
            &rig.secret.launch_secret(id_c),
            b.user,
            id_c,
        );
        let answer = client.request(req).await.unwrap();
        assert_eq!(answer.status(), StatusCode::CONFLICT);
    } else {
        println!("skipped: only Linux gives every 127.0.0.x to a test without setup");
    }
}

#[tokio::test]
async fn open_picks_the_snapshot_the_rules_name() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let g = rig.generation();
    let id = holder(&rig, a.user, g).await;
    // A new store.
    let (status, body) = open(&rig, id, a, "").await;
    assert_eq!(status, StatusCode::OK);
    let answer: OpenAnswer = serde_json::from_slice(&body).unwrap();
    assert_eq!((answer.generation, answer.next_sequence), (g, 1));
    assert!(answer.snapshot.is_none() && !answer.restore);
    let record = rig
        .stores
        .get_of(a.user, a.store)
        .await
        .unwrap()
        .unwrap()
        .value;
    let key = rig
        .keys
        .unwrap(a.store, &record.wrapped().unwrap())
        .await
        .unwrap();
    assert_eq!(*answer.data_key.0, *key.to_base64url());
    assert_eq!(answer.wrapped_key, record.wrapped_key);

    // The newest, the next sequence under the lease's generation, and the fall-back.
    put(&rig, a.store, g - 1, 5, b"old");
    put(&rig, a.store, g, 1, b"one");
    put(&rig, a.store, g, 3, b"three");
    let (_, body) = open(&rig, id, a, "").await;
    let answer: OpenAnswer = serde_json::from_slice(&body).unwrap();
    let link = answer.snapshot.unwrap();
    assert_eq!((link.generation, link.sequence, link.size_bytes), (g, 3, 5));
    assert_eq!(answer.next_sequence, 4);
    assert!(
        link.url
            .starts_with(&format!("{}{}", rig.router_url, paths::BLOB_PREFIX))
    );
    let after = format!(r#"{{"after":{{"generation":{g},"sequence":3}}}}"#);
    let (_, body) = open(&rig, id, a, &after).await;
    let answer: OpenAnswer = serde_json::from_slice(&body).unwrap();
    assert_eq!(answer.snapshot.unwrap().sequence, 1);

    // Never below the recorded newest.
    rig.stores
        .update(a.user, a.store, |r| {
            r.newest = Some(knoggin_state::record::Newest {
                generation: g,
                sequence: 3,
            });
            Ok::<(), StateError>(())
        })
        .await
        .unwrap();
    let (status, body) = open(&rig, id, a, &after).await;
    assert_eq!(status, StatusCode::LOCKED);
    assert_eq!(code(&body).1.as_deref(), Some("snapshot_unreadable"));

    // A restore below the recorded newest opens, with `restore`.
    let restore = SnapshotName::new(g, 1).unwrap().to_string();
    let chosen = restore.clone();
    rig.stores
        .update(a.user, a.store, move |r| {
            r.restore_from = Some(chosen.clone());
            Ok::<(), StateError>(())
        })
        .await
        .unwrap();
    let (_, body) = open(&rig, id, a, "").await;
    let answer: OpenAnswer = serde_json::from_slice(&body).unwrap();
    assert!(answer.restore);
    assert_eq!(answer.snapshot.unwrap().sequence, 1);
    rig.stores
        .update(a.user, a.store, |r| {
            r.restore_from = None;
            Ok::<(), StateError>(())
        })
        .await
        .unwrap();

    // The recorded newest is missing: locked, nothing older is served.
    rig.storage.remove(&names::snapshot_key(
        a.store,
        SnapshotName::new(g, 3).unwrap(),
    ));
    let (status, body) = open(&rig, id, a, "").await;
    assert_eq!(status, StatusCode::LOCKED);
    assert_eq!(code(&body).1.as_deref(), Some("snapshot_unreadable"));

    // A name above the lease's generation locks the store; once real time has passed it,
    // the lease is asked to park for `lifetime`.
    let b = rig.account().await;
    let id_b = holder(&rig, b.user, g).await;
    put(&rig, b.store, g + 5, 1, b"ahead");
    let (status, body) = open(&rig, id_b, b, "").await;
    assert_eq!(
        (status, code(&body).1.as_deref()),
        (StatusCode::LOCKED, Some("snapshot_unreadable"))
    );
    assert_eq!(rig.lease_of(b.user).await.park_requested(), None);
    rig.clock.advance(6_000);
    let (status, _) = open(&rig, id_b, b, "").await;
    assert_eq!(status, StatusCode::LOCKED);
    assert_eq!(
        rig.lease_of(b.user).await.park_requested(),
        Some(ParkReason::Lifetime)
    );

    // A data key the master key set cannot unwrap.
    let c = rig.account().await;
    let id_c = holder(&rig, c.user, g).await;
    rig.stores
        .update(c.user, c.store, |r| {
            r.set_wrapped(&WrappedKey {
                kek_id: "local:00000000000000ff".into(),
                blob: vec![0; 60],
            });
            Ok::<(), StateError>(())
        })
        .await
        .unwrap();
    let (status, body) = open(&rig, id_c, c, "").await;
    assert_eq!(
        (status, code(&body).1.as_deref()),
        (StatusCode::LOCKED, Some("master_key_missing"))
    );
}

async fn upload_link(
    rig: &Rig,
    id: Uuid,
    a: Account,
    sequence: u64,
    len: u64,
) -> (StatusCode, Bytes) {
    let body = format!(r#"{{"sequence":{sequence},"sizeBytes":{len}}}"#);
    let (status, _, bytes) = rig
        .broker_call(id, a.user, Method::POST, &paths::snapshots(a.store), body)
        .await;
    (status, bytes)
}

/// Sends a signed request over the link with `id`'s identity, as a container does.
async fn send_link(
    rig: &Rig,
    id: Uuid,
    user: UserId,
    method: Method,
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    body: &[u8],
) -> (StatusCode, Bytes) {
    let mut req = http::Request::builder()
        .method(method)
        .uri(url)
        .body(Body::from(body.to_vec()))
        .unwrap();
    for (n, v) in headers {
        req.headers_mut().insert(
            http::HeaderName::from_bytes(n.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    write_container_call(req.headers_mut(), &rig.secret.launch_secret(id), user, id);
    let answer = rig.broker_client().request(req).await.unwrap();
    let status = answer.status();
    (
        status,
        answer.into_body().collect().await.unwrap().to_bytes(),
    )
}

#[tokio::test]
async fn uploads_are_create_only_commits_only_raise_newest_and_the_budget_holds() {
    const BUDGET: u64 = 100;
    let rig = Rig::start(Options {
        upload_bytes_per_hour: BUDGET,
        ..Options::default()
    })
    .await;
    let a = rig.account().await;
    let g = rig.generation();
    let id = holder(&rig, a.user, g).await;
    let (status, body) = upload_link(&rig, id, a, 2, 5).await;
    assert_eq!(status, StatusCode::OK);
    let link: UploadLink = serde_json::from_slice(&body).unwrap();
    assert_eq!(link.method, "PUT");
    assert_eq!(link.headers["content-length"], "5");
    assert_eq!(link.headers["if-none-match"], "*");
    assert!(link.url.contains("x-id=PutObject"));
    assert!(link.expires_at.ends_with('Z'));
    let (status, _) = send_link(
        &rig,
        id,
        a.user,
        Method::PUT,
        &link.url,
        &link.headers,
        b"hello",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let key = names::snapshot_key(a.store, SnapshotName::new(g, 2).unwrap());
    assert_eq!(rig.storage.get(&key).unwrap(), Bytes::from_static(b"hello"));
    // The same link again: the name is taken.
    let (status, _) = send_link(
        &rig,
        id,
        a.user,
        Method::PUT,
        &link.url,
        &link.headers,
        b"again",
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);

    // Commits.
    let restore = SnapshotName::new(g - 1, 1).unwrap().to_string();
    let chosen = restore.clone();
    rig.stores
        .update(a.user, a.store, move |r| {
            r.restore_from = Some(chosen.clone());
            Ok::<(), StateError>(())
        })
        .await
        .unwrap();
    let commit = |sequence: u64, body: String| {
        let rig = &rig;
        async move {
            rig.broker_call(
                id,
                a.user,
                Method::POST,
                &paths::commit(a.store, sequence),
                body,
            )
            .await
        }
    };
    let body = format!(
        r#"{{"memoryCount":7,"namespaceCount":2,"lastWriteAt":"2026-11-23T10:15:30.123Z","lastWriteClient":"claude","sizeBytes":5,"restored":"{restore}"}}"#
    );
    assert_eq!(commit(2, body).await.0, StatusCode::OK);
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
    assert_eq!((record.memory_count, record.namespace_count), (7, 2));
    assert_eq!(record.last_write_client.unwrap().expose(), "claude");
    assert_eq!(record.restore_from, None);
    // A lower name is answered as done and changes nothing.
    let lower = r#"{"memoryCount":1,"namespaceCount":1,"sizeBytes":5}"#.to_owned();
    assert_eq!(commit(1, lower).await.0, StatusCode::OK);
    let record = rig
        .stores
        .get_of(a.user, a.store)
        .await
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(
        (record.newest.unwrap().sequence, record.memory_count),
        (2, 7)
    );
    // Bounds.
    let many = r#"{"memoryCount":1000000001,"namespaceCount":1,"sizeBytes":5}"#.to_owned();
    assert_eq!(commit(3, many).await.0, StatusCode::BAD_REQUEST);
    let long = format!(
        r#"{{"memoryCount":1,"namespaceCount":1,"sizeBytes":5,"lastWriteAt":"2026-11-23T10:15:30.123Z","lastWriteClient":"{}"}}"#,
        "c".repeat(65)
    );
    assert_eq!(commit(3, long).await.0, StatusCode::BAD_REQUEST);

    // The hourly upload budget: 5 of 100 bytes used.
    let (status, body) = upload_link(&rig, id, a, 3, 96).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        code(&body),
        ("rate_limited".into(), Some("upload_budget".into()))
    );
    assert_eq!(upload_link(&rig, id, a, 3, 95).await.0, StatusCode::OK);
    rig.clock.advance(3_600_000);
    assert_eq!(upload_link(&rig, id, a, 4, 96).await.0, StatusCode::OK);

    // A deleted store: 410 on every route.
    let now = rig.clock.now();
    rig.stores
        .update(a.user, a.store, |r| {
            r.deleted_at_ms = Some(now);
            Ok::<(), StateError>(())
        })
        .await
        .unwrap();
    let body = r#"{"memoryCount":1,"namespaceCount":1,"sizeBytes":5}"#.to_owned();
    let (status, _, body) = commit(5, body).await;
    assert_eq!(
        (status, code(&body).0.as_str()),
        (StatusCode::GONE, "store_deleting")
    );
    assert_eq!(open(&rig, id, a, "").await.0, StatusCode::GONE);
}

#[tokio::test]
async fn the_blob_route_forwards_only_the_callers_own_objects_and_signed_headers() {
    let rig = Rig::start(Options {
        upload_bytes_per_hour: 1_000,
        ..Options::default()
    })
    .await;
    let (a, b) = (rig.account().await, rig.account().await);
    let g = rig.generation();
    let id_a = holder(&rig, a.user, g).await;
    let id_b = holder(&rig, b.user, g).await;
    let key = put(&rig, a.store, g, 1, b"snapshot bytes");
    let (_, body) = open(&rig, id_a, a, "").await;
    let link = serde_json::from_slice::<OpenAnswer>(&body)
        .unwrap()
        .snapshot
        .unwrap();
    let mut extra = link.headers.clone();
    extra.insert("x-extra".into(), "1".into());
    let (status, bytes) = send_link(&rig, id_a, a.user, Method::GET, &link.url, &extra, b"").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, Bytes::from_static(b"snapshot bytes"));
    let seen = rig.storage.seen();
    let last = seen.last().unwrap();
    assert_eq!(last.key, key);
    for name in &last.header_names {
        assert!(
            ["host", "accept", "user-agent"].contains(&name.as_str()),
            "{name} reached the bucket"
        );
    }

    // B's container with a link to A's object: refused before the bucket.
    let (status, _) = send_link(
        &rig,
        id_b,
        b.user,
        Method::GET,
        &link.url,
        &link.headers,
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (path, query) = link.url.split_once('?').unwrap();
    let prefix = format!("{}{}", rig.router_url, paths::BLOB_PREFIX);
    let at = |p: &str, q: &str| format!("{prefix}{p}?{q}");
    let raw = path.strip_prefix(&prefix).unwrap();
    for (bad, why) in [
        (
            at(&raw.replace("/snap/", "/snap/../snap/"), query),
            "dot segments",
        ),
        (
            at(&raw.replace("/snap/", "%2Fsnap/"), query),
            "encoded slash",
        ),
        (
            at(&raw.replace("/snap/", "%252Fsnap/"), query),
            "encoded twice",
        ),
        (at(&format!("{raw}%3F"), query), "encoded ?"),
        (at(&format!("{raw}%23"), query), "encoded #"),
        (at(&raw.replace("/snap/", "%5Csnap/"), query), "backslash"),
        (at(raw, &format!("{query}&extra=1")), "extra parameter"),
        (
            at(raw, &format!("{query}&X-Amz-Date=20260101T000000Z")),
            "repeated parameter",
        ),
        (
            at(raw, &query.replace("GetObject", "PutObject")),
            "x-id of another method",
        ),
    ] {
        let (status, _) =
            send_link(&rig, id_a, a.user, Method::GET, &bad, &link.headers, b"").await;
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
            "{why}: {status}"
        );
    }
    assert_eq!(
        rig.storage.seen().len(),
        seen.len(),
        "nothing more reached the bucket"
    );

    // Uploads: the signed length or nothing.
    let (_, body) = upload_link(&rig, id_a, a, 2, 5).await;
    let up: UploadLink = serde_json::from_slice(&body).unwrap();
    let mut longer = up.headers.clone();
    longer.insert("content-length".into(), "6".into());
    let (status, body) =
        send_link(&rig, id_a, a.user, Method::PUT, &up.url, &longer, b"sixsix").await;
    assert_eq!(
        rig.storage.seen().last().unwrap().status,
        403,
        "the bucket refuses another length"
    );
    // A bucket's refusal reaches the container as the broker's own `unavailable`.
    assert_eq!(
        (status, code(&body).0.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    );
    let mut chunked = http::Request::put(&up.url)
        .header("if-none-match", "*")
        .body(Body::from_stream(futures_util::stream::once(async {
            Ok::<_, std::io::Error>(Bytes::from_static(b"12345"))
        })))
        .unwrap();
    write_container_call(
        chunked.headers_mut(),
        &rig.secret.launch_secret(id_a),
        a.user,
        id_a,
    );
    let answer = rig.broker_client().request(chunked).await.unwrap();
    assert_eq!(answer.status(), StatusCode::BAD_REQUEST, "no length");
    let mut over = up.headers.clone();
    over.insert("content-length".into(), (u64::MAX / 4).to_string());
    let pending = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
    let mut req = http::Request::put(&up.url)
        .body(Body::from_stream(pending))
        .unwrap();
    for (n, v) in &over {
        req.headers_mut().insert(
            http::HeaderName::from_bytes(n.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    write_container_call(
        req.headers_mut(),
        &rig.secret.launch_secret(id_a),
        a.user,
        id_a,
    );
    let answer = rig.broker_client().request(req).await.unwrap();
    assert_eq!(answer.status(), StatusCode::BAD_REQUEST, "over the bound");

    // Downloads beyond the hourly budget (1,000 bytes; 14 used).
    rig.storage.insert(
        &names::snapshot_key(a.store, SnapshotName::new(g, 3).unwrap()),
        vec![0u8; 1_000],
        Timestamp::from_millis(rig.clock.now()),
    );
    let (_, body) = open(&rig, id_a, a, "").await;
    let big = serde_json::from_slice::<OpenAnswer>(&body)
        .unwrap()
        .snapshot
        .unwrap();
    let (status, _) = send_link(&rig, id_a, a.user, Method::GET, &big.url, &big.headers, b"").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send_link(&rig, id_a, a.user, Method::GET, &big.url, &big.headers, b"").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // A replaced container with a link that is still valid.
    let _next = holder(&rig, a.user, g + 1).await;
    let (status, _) = send_link(
        &rig,
        id_a,
        a.user,
        Method::GET,
        &link.url,
        &link.headers,
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn the_blob_route_answers_a_bucket_refusal_as_unavailable() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let g = rig.generation();
    let id = holder(&rig, a.user, g).await;
    put(&rig, a.store, g, 1, b"x");
    let (_, body) = open(&rig, id, a, "").await;
    let get = serde_json::from_slice::<OpenAnswer>(&body)
        .unwrap()
        .snapshot
        .unwrap();
    let (_, body) = upload_link(&rig, id, a, 2, 1).await;
    let up: UploadLink = serde_json::from_slice(&body).unwrap();
    for bucket in [404, 410, 409, 500] {
        rig.storage
            .set_fault(knoggin_server::storage::Fault::Answer(bucket))
            .await
            .unwrap();
        for (method, url, headers, bytes) in [
            (Method::GET, &get.url, &get.headers, &b""[..]),
            (Method::PUT, &up.url, &up.headers, &b"y"[..]),
        ] {
            let (status, body) = send_link(&rig, id, a.user, method, url, headers, bytes).await;
            assert_eq!(
                (status, code(&body).0.as_str()),
                (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
                "{bucket}"
            );
            assert_eq!(rig.storage.seen().last().unwrap().status, bucket);
        }
    }
    // A 412 (the name exists) is passed on: the container takes the next sequence.
    rig.storage
        .set_fault(knoggin_server::storage::Fault::None)
        .await
        .unwrap();
    let (_, body) = upload_link(&rig, id, a, 1, 1).await;
    let taken: UploadLink = serde_json::from_slice(&body).unwrap();
    let (status, _) = send_link(
        &rig,
        id,
        a.user,
        Method::PUT,
        &taken.url,
        &taken.headers,
        b"z",
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn without_one_host_there_is_no_blob_route() {
    let rig = Rig::start(Options {
        one_host: false,
        ..Options::default()
    })
    .await;
    let a = rig.account().await;
    let id = holder(&rig, a.user, rig.generation()).await;
    let key = put(&rig, a.store, rig.generation(), 1, b"x");
    let (_, body) = open(&rig, id, a, "").await;
    let link = serde_json::from_slice::<OpenAnswer>(&body)
        .unwrap()
        .snapshot
        .unwrap();
    assert!(link.url.starts_with(&rig.storage.endpoint()));
    let (status, _, _) = rig
        .broker_call(
            id,
            a.user,
            Method::GET,
            &format!("{}{key}", paths::BLOB_PREFIX),
            Body::empty(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn parked_runs_mark_gone_and_ends_the_lease() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let id = holder(&rig, a.user, rig.generation()).await;
    let (status, _, _) = rig
        .broker_call(id, a.user, Method::POST, paths::PARKED, Body::empty())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(*rig.parked.lock().unwrap(), [id]);
    assert_eq!(rig.lease_of(a.user).await.state, LeaseState::Parked);
    // From then on nothing is signed for it.
    assert_eq!(upload_link(&rig, id, a, 1, 1).await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_peer_with_bad_tokens_is_cut_off() {
    let rig = Rig::start(Options::default()).await;
    let a = rig.account().await;
    let id = holder(&rig, a.user, rig.generation()).await;
    let call = |as_id: Uuid| {
        // Each on a new connection, as a flood of handshakes would be.
        let client = knoggin_server::link::router_client::<Body>(rig.router_pin, None).unwrap();
        let mut req = http::Request::post(format!("{}{}", rig.router_url, paths::open(a.store)))
            .body(Body::empty())
            .unwrap();
        write_container_call(
            req.headers_mut(),
            &rig.secret.launch_secret(as_id),
            a.user,
            id,
        );
        async move { client.request(req).await.map(|r| r.status()) }
    };
    for _ in 0..5 {
        assert_eq!(call(Uuid::now_v7()).await.unwrap(), StatusCode::CONFLICT);
    }
    assert!(
        call(id).await.is_err(),
        "the address is refused for a minute, even with a valid token"
    );
}
