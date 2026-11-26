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
}

fn committed() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/lib/api/types")
}

/// The `.ts` files of `dir`, by name.
fn files(dir: &Path) -> BTreeMap<String, String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.ends_with(".ts")
                        .then(|| Some((name, std::fs::read_to_string(e.path()).ok()?)))
                        .flatten()
                })
                .collect()
        })
        .unwrap_or_default()
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
    assert!(
        files(fresh.path()) == files(&committed()),
        "ui/src/lib/api/types is stale: run `cargo nextest run -p knoggin-http --run-ignored only -E 'test(export_ui_types)'`"
    );
}

#[test]
fn large_integers_are_numbers_and_times_are_strings() {
    let fresh = tempfile::tempdir().unwrap();
    export(fresh.path());
    let all = files(fresh.path());
    assert!(all.values().all(|text| !text.contains("bigint")));
    assert!(all["MemoryPage.ts"].contains("total: number"));
    assert!(all["MemoryRow.ts"].contains("createdAt: string"));
    assert!(all["UiEvent.ts"].contains("\"kind\": \"recall\""));
}
