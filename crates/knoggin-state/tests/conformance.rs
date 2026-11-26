// SPDX-License-Identifier: AGPL-3.0-only
//! The state conformance suite against `LocalFileDb`, in memory and on a file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::sync::Arc;

use knoggin_crypto::testing::test_provider;
use knoggin_state::LocalFileDb;
use knoggin_state::testing::{TestClock, conformance};

#[tokio::test]
async fn in_memory() {
    let clock = TestClock::new();
    conformance(Arc::new(LocalFileDb::in_memory(clock.clock())), &clock).await;
}

#[tokio::test]
async fn on_a_state_file() {
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let db = LocalFileDb::create(dir.path(), &test_provider(), clock.clock())
        .await
        .unwrap();
    conformance(Arc::new(db), &clock).await;
    // Everything the suite wrote is in the newest file.
    let reopened = LocalFileDb::open(dir.path(), &test_provider(), clock.clock())
        .await
        .unwrap()
        .unwrap();
    let meta = knoggin_state::Meta::new(Arc::new(reopened));
    meta.check_schema().await.unwrap();
}
