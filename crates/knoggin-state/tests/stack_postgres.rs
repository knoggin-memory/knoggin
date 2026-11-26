// SPDX-License-Identifier: AGPL-3.0-only
//! The state conformance suite against `PostgresDb` on a real Postgres container, plus what
//! only Postgres has: sealed rows, a row moved to another key, expired-row housekeeping and
//! writes that name the same items in opposite orders.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use knoggin_crypto::testing::test_key_set;
use knoggin_state::testing::{TestClock, conformance};
use knoggin_state::{Cond, Item, PostgresDb, StateDb, StateError, Write};
use serde_json::{Value, json};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

/// `<repository>@sha256:<digest>` from `tests/images.toml`.
fn pinned_image(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/images.toml");
    let file: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
    file["images"][name].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs docker"]
async fn conformance_on_postgres() {
    let image = pinned_image("postgres");
    // testcontainers joins name and tag with `:`, which turns this back into `<repo>@sha256:<hex>`.
    let (name, digest) = image.rsplit_once(':').unwrap();
    let container = GenericImage::new(name, digest)
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stdout(
            "PostgreSQL init process complete",
        ))
        .with_env_var("POSTGRES_PASSWORD", "knoggin-test")
        .with_cmd(["postgres", "-c", "fsync=off"])
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:knoggin-test@127.0.0.1:{port}/postgres?sslmode=disable");

    let clock = TestClock::new();
    // The server restarts once after its init scripts; wait until it takes connections.
    let deadline = Instant::now() + Duration::from_secs(60);
    let db = loop {
        match PostgresDb::connect(&url, &test_key_set(), clock.clock()).await {
            Ok(db) => break Arc::new(db),
            Err(e) if Instant::now() < deadline => {
                eprintln!("waiting for postgres: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("postgres did not start: {e}"),
        }
    };
    db.migrate().await.unwrap();
    db.migrate().await.unwrap(); // a second router start finds it applied

    conformance(db.clone(), &clock).await;

    let raw = sqlx::PgPool::connect(&url).await.unwrap();

    // Every document is sealed: none parses as JSON and none shows a field name.
    let docs: Vec<(String, String, Vec<u8>)> =
        sqlx::query_as("SELECT pk, sk, doc FROM knoggin_state")
            .fetch_all(&raw)
            .await
            .unwrap();
    assert!(docs.len() > 50, "{} rows", docs.len());
    for (pk, sk, doc) in &docs {
        assert!(serde_json::from_slice::<Value>(doc).is_err(), "{pk} {sk}");
        assert!(!doc.windows(9).any(|w| w == b"\"user_id\""), "{pk} {sk}");
    }

    // A sealed document copied to another row does not open there, and the read fails.
    sqlx::query(
        "INSERT INTO knoggin_state (pk, sk, version, doc) \
         SELECT 'moved', '-', 1, doc FROM knoggin_state WHERE pk = 'meta' AND sk = 'schema'",
    )
    .execute(&raw)
    .await
    .unwrap();
    assert!(db.get("meta", "schema").await.unwrap().is_some());
    assert!(matches!(
        db.get("moved", "-").await,
        Err(StateError::Corrupt(_))
    ));
    assert!(matches!(
        db.query("moved", "", 10, None).await,
        Err(StateError::Corrupt(_))
    ));

    // Housekeeping deletes exactly the expired rows.
    let now = clock.now();
    db.write(vec![
        Write::put(
            Item::new("hk", "old", json!({})).expiring(Some(now + 5)),
            Cond::Absent,
        ),
        Write::put(
            Item::new("hk", "new", json!({})).expiring(Some(now + 50)),
            Cond::Absent,
        ),
    ])
    .await
    .unwrap();
    clock.advance(5);
    assert!(db.delete_expired().await.unwrap() >= 1);
    let (expired,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM knoggin_state WHERE expires_ms <= $1")
            .bind(clock.now())
            .fetch_one(&raw)
            .await
            .unwrap();
    assert_eq!(expired, 0);
    assert!(db.get("hk", "new").await.unwrap().is_some());

    // Writes that name the same items in opposite orders lock them in one order: no deadlock.
    let tasks: Vec<_> = (0..20)
        .map(|n| {
            let db = db.clone();
            tokio::spawn(async move {
                let mut writes = vec![
                    Write::put(Item::new("dl", "a", json!({ "n": n })), Cond::Any),
                    Write::put(Item::new("dl", "b", json!({ "n": n })), Cond::Any),
                    Write::put(Item::new("dl", "c", json!({ "n": n })), Cond::Any),
                ];
                if n % 2 == 1 {
                    writes.reverse();
                }
                db.write(writes).await
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(db.get("dl", "a").await.unwrap().unwrap().version, 20);

    raw.close().await;
    container.rm().await.unwrap();
}
