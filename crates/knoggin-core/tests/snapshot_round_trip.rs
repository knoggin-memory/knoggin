// SPDX-License-Identifier: AGPL-3.0-only
//! Snapshot round trip: over random sequences of every operation, `decode(encode(store))`
//! encoded again gives the same bytes after each step, and the decoded store holds the
//! same observable state with every invariant intact.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod support;

use knoggin_core::Store;
use knoggin_core::snapshot::WRITE_CONTENT_FORMAT;
use proptest::prelude::*;
use support::{Harness, op};

proptest! {
    // 256 cases, pinned so that PROPTEST_CASES cannot lower it.
    #![proptest_config(ProptestConfig { cases: 256, max_shrink_iters: 2_000, ..ProptestConfig::default() })]

    #[test]
    fn decode_then_encode_gives_the_same_bytes(ops in prop::collection::vec((op(), 0_i64..=120_000), 1..=200)) {
        let mut h = Harness::new();
        for (step, (op, advance)) in ops.iter().enumerate() {
            h.now += advance;
            let _ = h.apply(op);
            let bytes = h.store.encode();
            let decoded = Store::decode(WRITE_CONTENT_FORMAT, &bytes);
            prop_assert!(decoded.is_ok(), "step {} ({:?}): {:?}", step, op, decoded.err());
            let opened = Harness { store: decoded.unwrap(), now: h.now, provider: h.provider.clone() };
            prop_assert!(opened.store.encode() == bytes, "step {} ({:?}): bytes differ", step, op);
            // Vectors are written in ID order, so only the slot order may differ; no answer
            // depends on it, because every scan breaks ties by ID.
            let by_id = |mut s: support::State| { s.vectors.sort_by_key(|(id, _)| *id); s };
            prop_assert_eq!(by_id(opened.state()), by_id(h.state()), "step {}: {:?}", step, op);
            let broken = opened.store.invariant_violations_for_test();
            prop_assert!(broken.is_empty(), "step {} ({:?}) broke invariants: {:?}", step, op, broken);
        }
    }
}
