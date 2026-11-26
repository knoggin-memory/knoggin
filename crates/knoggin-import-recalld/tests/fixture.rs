// SPDX-License-Identifier: AGPL-3.0-only
//! The recalld data fixture (tab "Config, CLI, local install", "Test fixture"): the
//! lines the reader yields from `tests/fixtures/recalld-data` equal the committed
//! `expected-export.jsonl` with `exportedAt` masked, and the counts equal
//! `expected-counts.json`. The ignored test `write_expected_files` writes both; the
//! script `scripts/make-recalld-data-fixture.sh` runs it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use knoggin_core::embed::EmbeddingProvider;
use knoggin_core::testing::FakeProvider;
use knoggin_core::transfer::{ImportOutcome, OnConflict, TransferRecord};
use knoggin_core::{Store, StoreId, Timestamp};
use knoggin_import_recalld::{Import, read};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/recalld-data")
}

/// 2026-12-01T00:00:00Z: after every time in the fixture.
const NOW: Timestamp = Timestamp::from_millis(1_796_083_200_000);

/// The lines, one JSON object each, with the header's `exportedAt` set to `null`.
fn lines(import: &Import) -> String {
    let mut out = String::new();
    for record in &import.records {
        let mut record = record.clone();
        if let TransferRecord::Header(h) = &mut record {
            h.exported_at = None;
        }
        out.push_str(&serde_json::to_string(&record).unwrap());
        out.push('\n');
    }
    out
}

/// What `knoggin import recalld --json` prints after a fresh daemon took the lines: the
/// import route is the core's `import_record` on a new store, which decides the
/// accepted edges and the route's error lines.
fn counts(import: &Import) -> String {
    let mut store = Store::create(
        StoreId::default(),
        FakeProvider::new().identity().clone(),
        NOW,
    );
    let mut summary = import.summary.clone();
    summary.edges = 0;
    for record in import.records.iter().skip(1) {
        let edge = matches!(record, TransferRecord::Edge(_));
        match store.import_record(record.clone(), OnConflict::Skip, NOW) {
            Ok(ImportOutcome::Imported) if edge => summary.edges += 1,
            Ok(_) => {}
            Err(_) => summary.errors += 1,
        }
    }
    let mut out = serde_json::to_string_pretty(&summary).unwrap();
    out.push('\n');
    out
}

fn listing(dir: &Path) -> BTreeSet<(PathBuf, u64, std::time::SystemTime)> {
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let meta = entry.metadata().unwrap();
        out.insert((entry.path(), meta.len(), meta.modified().unwrap()));
        if meta.is_dir() {
            out.extend(listing(&entry.path()));
        }
    }
    out
}

fn read_fixture() -> Import {
    let run = tempfile::tempdir().unwrap();
    let import = read(&fixture(), &run.path().join("run"), NOW).unwrap();
    assert!(
        std::fs::read_dir(run.path().join("run"))
            .unwrap()
            .next()
            .is_none(),
        "the copies are removed"
    );
    import
}

#[test]
fn lines_equal_the_committed_file() {
    let parent = fixture().parent().unwrap().to_path_buf();
    let before = listing(&parent);
    let import = read_fixture();
    assert_eq!(
        listing(&parent),
        before,
        "nothing is created in or beside the directory"
    );
    let want = std::fs::read_to_string(fixture().join("expected-export.jsonl")).unwrap();
    assert_eq!(lines(&import), want);
    assert!(import.skipped.is_empty(), "{:?}", import.skipped);
}

#[test]
fn counts_equal_the_committed_file() {
    let import = read_fixture();
    let want = std::fs::read_to_string(fixture().join("expected-counts.json")).unwrap();
    assert_eq!(counts(&import), want);
}

#[test]
fn header_carries_the_time_and_no_model() {
    let import = read_fixture();
    let TransferRecord::Header(h) = &import.records[0] else {
        panic!("the first line is the header");
    };
    assert_eq!(h.exported_at, Some(NOW));
    assert_eq!(h.model, None);
}

#[test]
#[ignore = "writes the fixture's expected files; run by scripts/make-recalld-data-fixture.sh"]
fn write_expected_files() {
    let import = read_fixture();
    std::fs::write(fixture().join("expected-export.jsonl"), lines(&import)).unwrap();
    std::fs::write(fixture().join("expected-counts.json"), counts(&import)).unwrap();
    for s in &import.skipped {
        eprintln!("skipped: {s:?}");
    }
}
