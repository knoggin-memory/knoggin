// SPDX-License-Identifier: AGPL-3.0-only
//! The one-host upgrade:
//! stopping the router parks every claimed container, which saves first; the router
//! starts again, attaches to the per-user networks that still exist, and serves
//! everyone again. The user is served by a pool container that was started before the
//! restart, which proves that `attach` found its network.
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

use std::time::{Duration, Instant};

use stack::{Stack, eventually};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker"]
async fn stopping_and_starting_the_router_parks_and_serves_everyone_again() {
    let s = Stack::up().await;
    let ana = s.sign_in("ana").await;
    let ben = s.sign_in("ben").await;
    for (u, text) in [(&ana, "ana before"), (&ben, "ben before")] {
        let a = s.save(&u.token, None, text).await;
        assert_eq!(a.status, 201, "{}", a.body);
    }
    // Busy, so that they are still claimed when the router stops (idle time is 5 s).
    let alive = s.keep_alive(vec![ana.token.clone(), ben.token.clone()]);
    let claimed: Vec<_> = [&ana, &ben]
        .iter()
        .map(|u| s.container_of(u).expect("a claimed container"))
        .collect();
    // The pool refills to its target of 2 ready containers.
    assert!(
        eventually(Duration::from_secs(60), || async {
            s.user_containers().iter().filter(|c| c.running).count() >= 4
        })
        .await,
        "the pool did not refill"
    );
    let pool: Vec<String> = s
        .user_containers()
        .into_iter()
        .filter(|c| c.running && claimed.iter().all(|k| k.docker_id != c.docker_id))
        .map(|c| c.knoggin_id)
        .collect();
    assert!(!pool.is_empty());

    // Stop: the router parks the claimed containers and waits while they save.
    drop(alive);
    let started = Instant::now();
    s.compose_ok(&["stop", "--timeout", "150", "router"]);
    println!("the router stopped in {:?}", started.elapsed());
    let left = s.user_containers();
    for c in &claimed {
        assert!(
            !left.iter().any(|x| x.docker_id == c.docker_id && x.running),
            "a claimed container still runs after the router stopped"
        );
    }

    // Start: the router attaches to the existing networks and serves from the pool.
    s.compose_ok(&["start", "router"]);
    s.wait_ready(Duration::from_secs(180)).await;
    for (u, text) in [(&ana, "ana before"), (&ben, "ben before")] {
        assert_eq!(
            s.summaries(&u.token, None).await,
            (200, vec![text.to_owned()]),
            "{} is served again with what was saved",
            u.sub
        );
    }
    let now: Vec<String> = [&ana, &ben]
        .iter()
        .map(|u| s.container_of(u).unwrap().knoggin_id)
        .collect();
    assert!(
        now.iter().any(|id| pool.contains(id)),
        "no user was served by a container from before the restart: {now:?} vs {pool:?}"
    );
    assert!(
        now.iter()
            .all(|id| claimed.iter().all(|c| &c.knoggin_id != id)),
        "a parked container was reused"
    );
}
