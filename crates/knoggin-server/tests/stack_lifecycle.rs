// SPDX-License-Identifier: AGPL-3.0-only
//! The one-host stack's lifecycle: a request
//! claims a container, which serves and parks when idle; a container that crashes after
//! its snapshot loses nothing, one that crashes before it loses that write and the store
//! still loads; an admin restore brings back an older snapshot; the bucket self-test
//! passes against SeaweedFS; and the lifecycle task runs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

#[path = "support/stack.rs"]
mod stack;

use std::time::Duration;

use serde_json::Value;
use stack::{Stack, docker_ok, eventually, text};

/// The store's snapshot names, newest first, from `knoggin admin snapshots`.
fn snapshots(s: &Stack, store: &str) -> Vec<String> {
    let out = s.knoggin(&["admin", "snapshots", "--store", store, "--json"]);
    assert!(out.status.success(), "admin snapshots: {}", text(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker"]
async fn claim_serve_park_crash_and_restore() {
    let s = Stack::up().await;
    let u = s.sign_in("lena").await;
    let limit = Duration::from_secs(90);

    // The bucket self-test passes on SeaweedFS (the router ran it at start too).
    let selftest = s.knoggin(&["admin", "selftest"]);
    println!("admin selftest:\n{}", text(&selftest));
    assert!(selftest.status.success(), "{}", text(&selftest));

    // Claim and serve.
    let saved = s.save(&u.token, None, "first memory").await;
    assert_eq!(saved.status, 201, "{}", saved.body);
    let first = s
        .container_of(&u)
        .expect("a container claimed for the user");

    // Idle: the container parks, exits 0, and the launcher removes it and its network.
    assert!(
        eventually(limit, || async {
            !s.user_containers()
                .iter()
                .any(|c| c.docker_id == first.docker_id)
                && !s.user_networks().contains(&first.network)
        })
        .await,
        "the idle container was not parked and removed"
    );
    assert_eq!(
        snapshots(&s, &u.personal).len(),
        1,
        "the park saved one snapshot"
    );

    // The next request claims a new container, which loads the snapshot.
    assert_eq!(
        s.summaries(&u.token, None).await,
        (200, vec!["first memory".to_owned()])
    );
    let second = s.container_of(&u).unwrap();
    assert_ne!(second.knoggin_id, first.knoggin_id, "never reused");

    // A crash after the snapshot loses nothing.
    let before = snapshots(&s, &u.personal).len();
    let alive = s.keep_alive(vec![u.token.clone()]);
    assert_eq!(s.save(&u.token, None, "second memory").await.status, 201);
    assert!(
        eventually(limit, || async {
            snapshots(&s, &u.personal).len() > before
        })
        .await,
        "the write was never saved"
    );
    docker_ok(&["kill", &second.docker_id]);
    drop(alive);
    assert_eq!(
        s.summaries(&u.token, None).await,
        (
            200,
            vec!["second memory".to_owned(), "first memory".to_owned()]
        )
    );

    // A crash before the snapshot loses that write, and the store still loads.
    let third = s.container_of(&u).unwrap();
    assert_eq!(s.save(&u.token, None, "lost memory").await.status, 201);
    docker_ok(&["kill", &third.docker_id]);
    assert_eq!(
        s.summaries(&u.token, None).await,
        (
            200,
            vec!["second memory".to_owned(), "first memory".to_owned()]
        )
    );

    // Restore the first snapshot: the command parks the container and waits for it.
    let oldest = snapshots(&s, &u.personal).last().unwrap().clone();
    let restore = s.knoggin(&[
        "admin",
        "restore",
        "--store",
        &u.personal,
        "--snapshot",
        &oldest,
    ]);
    assert!(
        restore.status.success(),
        "admin restore: {}",
        text(&restore)
    );
    assert_eq!(
        s.summaries(&u.token, None).await,
        (200, vec!["first memory".to_owned()])
    );
    let names = snapshots(&s, &u.personal);
    assert!(names.contains(&oldest), "a restore deletes nothing");

    // The router's Compose health check (`knoggin healthcheck --url .../readyz`) passes.
    let router = s.service_id("router");
    assert!(
        eventually(limit, || async {
            docker_ok(&["inspect", "--format", "{{.State.Health.Status}}", &router]).trim()
                == "healthy"
        })
        .await,
        "the router's health check never passed"
    );

    // The lifecycle task runs once and exits 0.
    let run = s.compose(&["run", "--rm", "--no-deps", "lifecycle", "lifecycle"]);
    assert!(run.status.success(), "lifecycle: {}", text(&run));
}
