// SPDX-License-Identifier: AGPL-3.0-only
//! Index equivalence: random sequences of every write operation, and after each step the
//! live indexes must equal a rebuild from the memories and edges alone. A step that
//! fails must leave the store unchanged.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod support;

use proptest::prelude::*;
use support::{Harness, op};

/// The fixed full-text queries: 10 single words and 10 pairs from `WORDS`.
const QUERIES: [&str; 20] = [
    "maya",
    "lisbon",
    "coffee",
    "project",
    "alpha",
    "river",
    "dinner",
    "blue",
    "memory",
    "ticket",
    "maya lisbon",
    "window seat",
    "coffee morning",
    "project deadline",
    "alpha beta",
    "pasta tomato",
    "train ticket",
    "red green",
    "graph vector",
    "doctor tooth",
];

proptest! {
    // 256 cases, pinned so that PROPTEST_CASES cannot lower it.
    #![proptest_config(ProptestConfig { cases: 256, max_shrink_iters: 2_000, ..ProptestConfig::default() })]

    #[test]
    fn live_indexes_equal_a_rebuild(ops in prop::collection::vec((op(), 0_i64..=120_000), 1..=200)) {
        let mut h = Harness::new();
        for (step, (op, advance)) in ops.iter().enumerate() {
            h.now += advance;
            let before = h.state();
            if h.apply(op).is_err() {
                prop_assert_eq!(&h.state(), &before, "step {} ({:?}) failed but changed the store", step, op);
            }
            let live = h.store.index_snapshot_for_test(&QUERIES);
            h.store.rebuild_indexes_for_test();
            let rebuilt = h.store.index_snapshot_for_test(&QUERIES);
            prop_assert_eq!(live, rebuilt, "step {}: {:?}", step, op);
            let broken = h.store.invariant_violations_for_test();
            prop_assert!(broken.is_empty(), "step {} ({:?}) broke invariants: {:?}", step, op, broken);
        }
    }
}
