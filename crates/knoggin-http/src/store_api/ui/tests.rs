// SPDX-License-Identifier: AGPL-3.0-only
//! The web UI's TypeScript types (tab "Engineering rules", ts-rs): every API shape the UI
//! reads or sends, written into `ui/src/lib/api/types/` by the explicit step
//! `export_ui_types`, and a test that fails when the committed files are out of date.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use knoggin_core::ops::{
    ForgetManyResult, ForgetMemories, Forgotten, Items, MemoryView, ModelState,
    NamespaceStatsResult, NamespaceView, Page, ReinforceMemory, Reinforced, TagCount,
};
use ts_rs::{Config, TS};

use super::wire::*;
use crate::edge::openapi::ErrorEnvelope;
use crate::edge::shapes::*;
use crate::host::live::UiEvent;
use crate::local::{AdminStatus, BrowserCode, CodeBody, SessionToken};

/// 64-bit integers are JSON numbers, and the UI needs none of them exactly above 2^53.
fn config(dir: &Path) -> Config {
    Config::new().with_large_int("number").with_out_dir(dir)
}

/// Writes every type the UI needs, with the types they use, into `dir`.
fn export(dir: &Path) {
    let cfg = config(dir);
    macro_rules! all {
        ($($t:ty),* $(,)?) => {
            $( <$t as TS>::export_all(&cfg).unwrap(); )*
        };
    }
    all!(
        // The read models and the event stream.
        GraphSnapshot,
        MemoryPage,
        Facets,
        AccessHistory,
        Neighborhood,
        UiSearch,
        Activity,
        HealthReport,
        SystemStatus,
        UiEvent,
        // The store routes the UI calls.
        MemoryView,
        Items<NamespaceView>,
        NamespaceStatsResult,
        Page<TagCount>,
        ReinforceMemory,
        Reinforced,
        Forgotten,
        ForgetMemories,
        ForgetManyResult,
        ModelState,
        // The control routes.
        SessionView,
        Items<StoreView>,
        NameBody,
        StoreStatusView,
        Items<KeyView>,
        CreateKey,
        RotateKey,
        CreatedKey,
        DeleteAccount,
        AccountDeleted,
        Page<UserView>,
        AdminStatus,
        Items<SnapshotView>,
        BrowserCode,
        CodeBody,
        SessionToken,
        ErrorEnvelope,
    );
    // Every source file starts with the licence line (tab "Engineering rules").
    for (name, text) in files(dir) {
        let path = dir.join(name);
        std::fs::write(path, format!("{SPDX}\n{text}")).unwrap();
    }
}

const SPDX: &str = "// SPDX-License-Identifier: AGPL-3.0-only";

fn committed() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/lib/api/types")
}

/// The `.ts` files under `dir`, nested folders included, by path relative to `dir`.
fn files(dir: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.extension().is_some_and(|x| x == "ts")
                && let (Ok(rel), Ok(text)) =
                    (path.strip_prefix(root), std::fs::read_to_string(&path))
            {
                out.insert(rel.to_string_lossy().into_owned(), text);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// The explicit step that refreshes the committed types; the `openapi` CI job runs it
/// and fails on a difference.
#[test]
#[ignore = "writes ui/src/lib/api/types"]
fn export_ui_types() {
    let dir = committed();
    for name in files(&dir).keys() {
        std::fs::remove_file(dir.join(name)).unwrap();
    }
    export(&dir);
}

#[test]
fn the_committed_ui_types_are_current() {
    let fresh = tempfile::tempdir().unwrap();
    export(fresh.path());
    let fresh = files(fresh.path());
    assert!(
        fresh.contains_key("serde_json/JsonValue.ts"),
        "nested folders are compared"
    );
    assert!(
        fresh == files(&committed()),
        "ui/src/lib/api/types is stale: run `cargo nextest run -p knoggin-http --run-ignored only -E 'test(export_ui_types)'`"
    );
}

#[test]
fn large_integers_are_numbers_and_times_are_strings() {
    let fresh = tempfile::tempdir().unwrap();
    export(fresh.path());
    let all = files(fresh.path());
    assert!(all.values().all(|text| !text.contains("bigint")));
    assert!(all.values().all(|text| text.starts_with(SPDX)));
    assert!(all["MemoryPage.ts"].contains("total: number"));
    assert!(all["MemoryRow.ts"].contains("createdAt: string"));
    assert!(all["UiEvent.ts"].contains("\"kind\": \"recall\""));
}
