// SPDX-License-Identifier: AGPL-3.0-only
//! The snapshot container reader: the header from a slice and from a stream, then the
//! chunks under the test key set. Seeds: tests/fixtures/snapshots/v1/*.kgs, which open
//! with that key set, so a mutated seed reaches the chunk and content checks.
#![no_main]

use knoggin_core::Store;
use knoggin_core::snapshot::{MAX_CONTENT_FORMAT, MIN_CONTENT_FORMAT};
use knoggin_crypto::container::read_header_from;
use knoggin_crypto::testing::open_snapshot;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = read_header_from(&mut &data[..]);
    let Ok((header, content)) = open_snapshot(data) else {
        return;
    };
    if header
        .check_content_format(MIN_CONTENT_FORMAT, MAX_CONTENT_FORMAT)
        .is_ok()
    {
        let _ = Store::decode(header.content_format, &content);
    }
});
