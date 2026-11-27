// SPDX-License-Identifier: AGPL-3.0-only
//! The recalld record decoder: a value of recalld's `memories` table (record versions 1
//! and 2), and the same bytes as an edge key and value.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    knoggin_import_recalld::fuzzing::decode(data);
});
