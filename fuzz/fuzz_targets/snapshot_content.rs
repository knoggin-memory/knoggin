// SPDX-License-Identifier: AGPL-3.0-only
//! The snapshot content decoder. A store it accepts must hold its invariants, and must
//! encode to bytes that decode again to the same encoding.
#![no_main]

use knoggin_core::Store;
use knoggin_core::snapshot::WRITE_CONTENT_FORMAT;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(store) = Store::decode(WRITE_CONTENT_FORMAT, data) else {
        return;
    };
    assert_eq!(store.invariant_violations_for_test(), Vec::<String>::new());
    let encoded = store.encode();
    let again = Store::decode(WRITE_CONTENT_FORMAT, &encoded).expect("an encoded store decodes");
    assert!(again.encode() == encoded, "encoding is not stable");
});
