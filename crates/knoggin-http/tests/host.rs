// SPDX-License-Identifier: AGPL-3.0-only
//! The store host: snapshot timing on paused time (no test here crosses the embedding
//! threads: the model is never ready), saves on close and shutdown, panics, the memory
//! budget, loading with fall-back, the model change and the request hash.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use knoggin_core::embed::EmbeddingProvider;
use knoggin_core::ops::{GetMemory, StoreMemory};
use knoggin_core::testing::FakeProvider;
use knoggin_http::error::{Code, LockReason};
use knoggin_http::host::{HostConfig, StoreState, request_hash};
use knoggin_http::testing::{TestHost, fake_config};
use knoggin_http::{Permissions, RequestContext};
use serde_json::json;
use tokio::time::{Instant, sleep};

use support::{Env, WAITS};

fn mem(summary: &str) -> StoreMemory {
    serde_json::from_value(json!({ "summary": summary })).unwrap()
}

async fn save(env: &Env, ctx: &RequestContext, summary: &str) -> knoggin_core::MemoryId {
    match env
        .t
        .host
        .store_memory(ctx, mem(summary), None)
        .await
        .unwrap()
    {
        knoggin_http::host::Reply::Fresh(m) => m.id,
        knoggin_http::host::Reply::Replayed(_) => unreachable!(),
    }
}

#[test]
fn the_request_hash_of_the_fixed_request_is_pinned() {
    let req: StoreMemory = serde_json::from_value(json!({
        "summary": "Maya prefers window seats",
        "namespace": "default",
        "tags": ["travel"],
    }))
    .unwrap();
    let hash = request_hash("store_memory", &req).unwrap();
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "d2d25a61f99d2215c890a333bdc6a8afcce9a0e714bda2d4af5f6991123457f9"
    );
}

#[tokio::test(start_paused = true)]
async fn a_content_change_is_saved_after_the_quiet_time() {
    let env = Env::new(false).await;
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    let t0 = Instant::now();
    save(&env, &ctx, "one").await;
    sleep(Duration::from_millis(450)).await;
    assert_eq!(env.counting.count(), 0);
    sleep(Duration::from_millis(100)).await;
    assert_eq!(env.counting.count(), 1);
    let at = env.counting.times()[0] - t0;
    assert!(
        at >= Duration::from_millis(500) && at < Duration::from_millis(520),
        "{at:?}"
    );
    // Nothing more to save: no second snapshot.
    sleep(Duration::from_secs(600)).await;
    assert_eq!(env.counting.count(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_burst_is_saved_by_the_longest_delay() {
    let env = Env::new(false).await;
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    let t0 = Instant::now();
    for i in 0..10 {
        save(&env, &ctx, &format!("m{i}")).await;
        sleep(Duration::from_millis(400)).await;
    }
    let times = env.counting.times();
    assert!(!times.is_empty());
    let first = times[0] - t0;
    assert!(
        first >= Duration::from_secs(2) && first < Duration::from_millis(2100),
        "{first:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn bookkeeping_waits_five_minutes() {
    let env = Env::new(false).await;
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    let id = save(&env, &ctx, "one").await;
    sleep(Duration::from_secs(1)).await;
    assert_eq!(env.counting.count(), 1);
    let t1 = Instant::now();
    let get = GetMemory {
        id,
        namespace: None,
        record_access: true,
        include_history: false,
    };
    env.t.host.get_memory(&ctx, get).await.unwrap();
    sleep(Duration::from_secs(299)).await;
    assert_eq!(env.counting.count(), 1, "access recording waits");
    sleep(Duration::from_secs(2)).await;
    assert_eq!(env.counting.count(), 2);
    let at = env.counting.times()[1] - t1;
    assert!(
        at >= Duration::from_secs(300) && at < Duration::from_secs(301),
        "{at:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_snapshot_is_retried_with_back_off_and_takes_the_next_sequence() {
    let env = Env::new(false).await;
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    env.counting.fail_next.store(3, Ordering::SeqCst);
    let t0 = Instant::now();
    save(&env, &ctx, "one").await;
    // Tries at 0.5 s, then 1, 2 and 4 seconds later.
    sleep(Duration::from_millis(7400)).await;
    assert_eq!(
        (
            env.counting.failures.load(Ordering::SeqCst),
            env.counting.count()
        ),
        (3, 0)
    );
    sleep(Duration::from_millis(200)).await;
    assert_eq!(env.counting.count(), 1);
    let at = env.counting.times()[0] - t0;
    assert!(
        at >= Duration::from_millis(7500) && at < Duration::from_millis(7600),
        "{at:?}"
    );
    // A taken name moves to the next sequence at once.
    let dir = env.t.backend.store_dir(env.t.store);
    let next = knoggin_core::snapshot::SnapshotName::new(1, 2).unwrap();
    std::fs::write(dir.join(next.file_name()), b"taken").unwrap();
    save(&env, &ctx, "two").await;
    sleep(Duration::from_secs(1)).await;
    let saves = env.counting.saves.lock().unwrap().clone();
    assert_eq!(saves.last().unwrap().2.sequence(), 3);
}

#[tokio::test(start_paused = true)]
async fn close_and_shutdown_save_what_is_unsaved() {
    let env = Env::new(false).await;
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    save(&env, &ctx, "one").await;
    env.t.host.close(env.t.store).await.unwrap();
    assert_eq!(env.counting.count(), 1);
    assert!(!env.t.host.is_open(env.t.store));
    save(&env, &ctx, "two").await;
    assert!(env.t.host.save_all().await.is_empty());
    assert_eq!(env.counting.count(), 2);
    // Nothing unsaved: a second final save writes nothing.
    assert!(env.t.host.save_all().await.is_empty());
    assert_eq!(env.counting.count(), 2);
    env.t.host.shutdown();
    let e = env.t.host.list_namespaces(&ctx).await.unwrap_err();
    assert_eq!(e.code, Code::Unavailable);
}

#[tokio::test(start_paused = true)]
async fn an_idle_store_is_saved_and_closed_once_its_queue_allows() {
    // The model never loads, so the memory stays in the queue.
    let env = Env::new(false).await;
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    save(&env, &ctx, "one").await;
    sleep(Duration::from_secs(11 * 60)).await;
    assert!(
        env.t.host.is_open(env.t.store),
        "the daemon waits for the queue"
    );
    let mut config = fake_config();
    config.embed_allowance = Some(Duration::from_secs(30 * 60));
    let env = Env::with_config(config, false).await;
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    save(&env, &ctx, "one").await;
    sleep(Duration::from_secs(29 * 60)).await;
    assert!(env.t.host.is_open(env.t.store));
    sleep(Duration::from_secs(2 * 60)).await;
    assert!(
        !env.t.host.is_open(env.t.store),
        "closed after the 30-minute allowance"
    );
    assert_eq!(env.counting.count(), 1);
}

#[tokio::test]
async fn a_panic_inside_a_store_operation_drops_the_store_without_saving() {
    let env = Env::new(false).await;
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    save(&env, &ctx, "saved").await;
    env.t.host.close(env.t.store).await.unwrap();
    save(&env, &ctx, "lost with the panic").await;
    let before = env.counting.count();
    let e = env
        .t
        .host
        .write_for_test(&ctx, |_s| -> () { panic!("marker-in-a-store-operation") })
        .await
        .unwrap_err();
    assert_eq!(e.code, Code::Internal);
    assert_eq!(e.message, "Internal error");
    assert!(!env.t.host.is_open(env.t.store));
    assert!(env.t.host.save_all().await.is_empty());
    assert_eq!(
        env.counting.count(),
        before,
        "the half-changed store was not saved"
    );
    // The next request loads the last snapshot.
    let list = env
        .t
        .host
        .list_memories(&ctx, serde_json::from_value(json!({})).unwrap())
        .await
        .unwrap();
    assert_eq!(list.items.len(), 1);
}

#[tokio::test]
async fn the_budget_refuses_writes_and_reports_full_until_a_forget() {
    let mut config = fake_config();
    config.memory_budget = Some(4_000);
    let env = Env::with_config(config, false).await;
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    let mut ids = Vec::new();
    let refused = loop {
        match env
            .t
            .host
            .store_memory(&ctx, mem("a memory that takes room"), None)
            .await
        {
            Ok(knoggin_http::host::Reply::Fresh(m)) => ids.push(m.id),
            Ok(_) => unreachable!(),
            Err(e) => break e,
        }
        assert!(ids.len() < 100);
    };
    assert_eq!(refused.code, Code::ResourceExhausted);
    assert!(env.t.host.store_status(env.t.store).full);
    // Reads keep working; forgetting is let through and makes room.
    env.t.host.list_namespaces(&ctx).await.unwrap();
    let forget = serde_json::from_value(json!({ "ids": ids })).unwrap();
    env.t
        .host
        .forget_memories(&ctx, forget, None)
        .await
        .unwrap();
    assert!(!env.t.host.store_status(env.t.store).full);
    env.t
        .host
        .store_memory(&ctx, mem("fits again"), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_unreadable_newest_snapshot_falls_back_and_three_lock_the_store() {
    let env = Env::new(false).await;
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    for summary in ["one", "two", "three", "four"] {
        save(&env, &ctx, summary).await;
        env.t.host.close(env.t.store).await.unwrap();
    }
    let dir = env.t.backend.store_dir(env.t.store);
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    assert_eq!(files.len(), 3, "retention keeps the 3 newest of today");
    let flip = |path: &std::path::Path| {
        let mut bytes = std::fs::read(path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(path, bytes).unwrap();
    };
    flip(&files[2]);
    let all = |env: &Env| {
        let ctx = ctx.clone();
        let host = env.t.host.clone();
        async move {
            host.list_memories(&ctx, serde_json::from_value(json!({})).unwrap())
                .await
                .map(|p| p.items.len())
        }
    };
    assert_eq!(
        all(&env).await.unwrap(),
        3,
        "the snapshot before the damaged one"
    );
    env.t.host.drop_store(env.t.store);
    flip(&files[1]);
    flip(&files[0]);
    let e = all(&env).await.unwrap_err();
    assert_eq!(
        (e.code, e.reason),
        (Code::StoreLocked, Some("snapshot_unreadable"))
    );
    assert_eq!(
        env.t.host.store_status(env.t.store).state,
        StoreState::Locked(LockReason::SnapshotUnreadable)
    );
}

#[tokio::test]
async fn a_store_saved_under_another_model_queues_every_memory_again() {
    let dir = tempfile::tempdir().unwrap();
    let t = TestHost::new(dir.path(), fake_config(), true, WAITS).await;
    let ctx = t.ctx(Permissions::READ_WRITE);
    for s in ["one", "two"] {
        t.host.store_memory(&ctx, mem(s), None).await.unwrap();
    }
    for _ in 0..200 {
        if t.host.store_status(t.store).embedding.unwrap().pending == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(t.host.store_status(t.store).embedding.unwrap().pending, 0);
    assert!(t.host.save_all().await.is_empty());
    t.host.shutdown();
    // The same files under a host that runs another model.
    let other = HostConfig::local(FakeProvider::with_dim(32).identity().clone());
    let backend = t.backend.clone();
    let (_tx, rx) = tokio::sync::watch::channel(knoggin_http::testing::fake_model_state(
        knoggin_core::ops::ModelStatus::Loading,
    ));
    let embedder = knoggin_http::host::Embedder::new(rx, Default::default(), WAITS);
    let host = knoggin_http::StoreHost::new(other, backend, embedder);
    host.list_namespaces(&ctx).await.unwrap();
    let status = host.store_status(t.store);
    assert_eq!(status.embedding.unwrap().pending, 2);
}

#[tokio::test(start_paused = true)]
async fn a_write_racing_a_close_is_never_lost() {
    let env = Env::new(false).await;
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    save(&env, &ctx, "before the close").await;
    let (release, stalled) = tokio::sync::oneshot::channel();
    *env.counting.stall_save.lock().unwrap() = Some(stalled);
    let host = env.t.host.clone();
    let store = env.t.store;
    let close = tokio::spawn(async move { host.close(store).await });
    // The final snapshot is being written.
    env.counting.save_entered.notified().await;
    let host = env.t.host.clone();
    let c = ctx.clone();
    let write =
        tokio::spawn(async move { host.store_memory(&c, mem("during the close"), None).await });
    // Paused time moves on only once every task waits: the write has done what it can.
    sleep(Duration::from_millis(100)).await;
    release.send(()).unwrap();
    close.await.unwrap().unwrap();
    write.await.unwrap().unwrap();
    // Whatever was acknowledged is in a snapshot.
    assert!(env.t.host.save_all().await.is_empty());
    env.t.host.drop_store(env.t.store);
    let list = env
        .t
        .host
        .list_memories(&ctx, serde_json::from_value(json!({})).unwrap())
        .await
        .unwrap();
    assert_eq!(list.items.len(), 2, "the write during the close was kept");
}

#[tokio::test(start_paused = true)]
async fn a_caller_that_gives_up_does_not_cancel_the_load() {
    let env = Env::new(false).await;
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    save(&env, &ctx, "one").await;
    env.t.host.close(env.t.store).await.unwrap();
    *env.counting.open_delay.lock().unwrap() = Duration::from_secs(5);
    let before = env.counting.opens.load(Ordering::SeqCst);
    let gave_up =
        tokio::time::timeout(Duration::from_secs(1), env.t.host.list_namespaces(&ctx)).await;
    assert!(gave_up.is_err());
    assert_eq!(
        env.t.host.store_status(env.t.store).state,
        StoreState::Waking
    );
    // A second caller waits for the same load.
    let list = env
        .t
        .host
        .list_memories(&ctx, serde_json::from_value(json!({})).unwrap())
        .await
        .unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!(
        env.counting.opens.load(Ordering::SeqCst) - before,
        1,
        "one load"
    );
}
