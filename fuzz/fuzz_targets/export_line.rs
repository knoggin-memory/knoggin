// SPDX-License-Identifier: AGPL-3.0-only
//! The export line parser: each line of the input parsed as `POST /v1/import` does and
//! applied to one store. The store must hold its invariants afterwards and survive a
//! snapshot round trip. Seeds: tests/fixtures/recalld-data/expected-export.jsonl and
//! tests/fixtures/export/*.jsonl.
#![no_main]

use knoggin_core::embed::ModelIdentity;
use knoggin_core::snapshot::WRITE_CONTENT_FORMAT;
use knoggin_core::transfer::{OnConflict, TransferRecord};
use knoggin_core::{Store, StoreId, Timestamp};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let now = Timestamp::from_millis(1_800_000_000_000);
    let model = ModelIdentity {
        id: "fake-64".to_owned(),
        dimensions: 64,
    };
    let mut store = Store::create(StoreId::from_bytes([1; 16]), model, now);
    for line in data.split(|b| *b == b'\n') {
        match TransferRecord::from_line(line) {
            Ok(TransferRecord::Header(h)) => {
                let _ = h.check();
            }
            Ok(record) => {
                let _ = store.import_record(record, OnConflict::Skip, now);
            }
            Err(_) => {}
        }
    }
    assert_eq!(store.invariant_violations_for_test(), Vec::<String>::new());
    Store::decode(WRITE_CONTENT_FORMAT, &store.encode()).expect("an imported store round-trips");
});
