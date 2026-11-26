// SPDX-License-Identifier: AGPL-3.0-only
//! Every refusal of the recalld reader, each on a copy of the fixture in a temporary
//! folder: a held lock (each of them), a layout change in progress, an old layout, a
//! text file in format 1, no active text file, symbolic links and missing files. Each
//! also checks that nothing was created in or beside the recalld directory and that the
//! private copies are gone.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use knoggin_core::Timestamp;
use knoggin_import_recalld::{Import, ImportError, read};
use redb::TableDefinition;

const NOW: Timestamp = Timestamp::from_millis(1_796_083_200_000);
const CONTROL: TableDefinition<&str, &[u8]> = TableDefinition::new("canonical_control");
const TEXT: TableDefinition<&str, &[u8]> = TableDefinition::new("text_generations_v1");

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A temporary folder with `volume/data`, a copy of the fixture, and `home/run`.
struct Setup {
    tmp: tempfile::TempDir,
}

impl Setup {
    fn new() -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/recalld-data");
        copy_dir(&fixture, &tmp.path().join("volume/data"));
        Setup { tmp }
    }

    fn volume(&self) -> PathBuf {
        self.tmp.path().join("volume")
    }

    fn data(&self) -> PathBuf {
        self.volume().join("data")
    }

    fn run(&self) -> PathBuf {
        self.tmp.path().join("home/run")
    }

    fn listing(&self) -> BTreeSet<(PathBuf, u64)> {
        fn walk(dir: &Path, out: &mut BTreeSet<(PathBuf, u64)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let meta = fs::symlink_metadata(entry.path()).unwrap();
                out.insert((entry.path(), meta.len()));
                if meta.is_dir() {
                    walk(&entry.path(), out);
                }
            }
        }
        let mut out = BTreeSet::new();
        walk(&self.volume(), &mut out);
        out
    }

    /// Reads `dir`, and checks that the recalld side is unchanged and that no copy is
    /// left.
    fn read(&self, dir: &Path) -> Result<Import, ImportError> {
        let before = self.listing();
        let result = read(dir, &self.run(), NOW);
        assert_eq!(
            self.listing(),
            before,
            "nothing in or beside the recalld directory"
        );
        if self.run().exists() {
            assert_eq!(
                fs::read_dir(self.run()).unwrap().count(),
                0,
                "the copies are gone"
            );
            let mode = fs::metadata(self.run()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        result
    }

    fn rewrite(&self, table: TableDefinition<&str, &[u8]>, key: &str, value: Option<Vec<u8>>) {
        let db = redb::Database::open(self.data().join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(table).unwrap();
            match value {
                Some(v) => {
                    t.insert(key, v.as_slice()).unwrap();
                }
                None => {
                    t.remove(key).unwrap();
                }
            }
        }
        tx.commit().unwrap();
    }

    fn get(&self, table: TableDefinition<&str, &[u8]>, key: &str) -> serde_json::Value {
        let db = redb::Database::open(self.data().join("meta.db")).unwrap();
        let tx = db.begin_read().unwrap();
        let t = tx.open_table(table).unwrap();
        serde_json::from_slice(t.get(key).unwrap().unwrap().value()).unwrap()
    }

    fn text_file(&self) -> PathBuf {
        fs::read_dir(self.data())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                let name = p.file_name().unwrap().to_str().unwrap();
                name.starts_with("fulltext.") && name != "fulltext.dat"
            })
            .unwrap()
    }
}

#[test]
fn a_copy_of_the_fixture_reads() {
    let s = Setup::new();
    let import = s.read(&s.data()).unwrap();
    assert!(import.skipped.is_empty());
    assert_eq!(import.summary.namespaces, 3);
}

#[test]
fn a_held_lock_stops_the_import() {
    let s = Setup::new();
    for lock in [
        s.volume().join("data.recalld-owner.lock"),
        s.data().join("recalld.lock"),
    ] {
        let file = File::create(&lock).unwrap();
        file.lock().unwrap();
        let err = s.read(&s.data()).unwrap_err();
        assert!(matches!(err, ImportError::Running), "{err:?}");
        assert_eq!(err.to_string(), "Stop recalld first: recalld daemon stop");
        file.unlock().unwrap();
        s.read(&s.data()).unwrap();
    }
}

#[test]
fn a_lock_file_is_never_created() {
    let s = Setup::new();
    fs::remove_file(s.data().join("recalld.lock")).unwrap();
    s.read(&s.data()).unwrap();
    assert!(!s.data().join("recalld.lock").exists());
    assert!(!s.volume().join("data.recalld-owner.lock").exists());
}

#[test]
fn a_container_volume_is_read_through_its_store_folder() {
    let s = Setup::new();
    let root = s.volume().join("root");
    fs::create_dir(&root).unwrap();
    fs::rename(s.data(), root.join(".recalld-store")).unwrap();
    let journal = root.join(".recalld-container-layout.json");
    fs::write(
        &journal,
        br#"{"version":1,"root":"/x","id":"x","ready":true,"files":[]}"#,
    )
    .unwrap();
    let import = s.read(&root).unwrap();
    assert_eq!(import.summary.namespaces, 3);

    // All four lock files are tested.
    for lock in [
        s.volume().join("root.recalld-owner.lock"),
        root.join("recalld.lock"),
        root.join(".recalld-store.recalld-owner.lock"),
        root.join(".recalld-store/recalld.lock"),
    ] {
        let file = File::create(&lock).unwrap();
        file.lock().unwrap();
        assert!(
            matches!(s.read(&root), Err(ImportError::Running)),
            "{lock:?}"
        );
        file.unlock().unwrap();
    }

    fs::write(&journal, br#"{"version":1,"ready":false}"#).unwrap();
    let err = s.read(&root).unwrap_err();
    assert!(matches!(err, ImportError::LayoutChanging));
    assert_eq!(
        err.to_string(),
        "This recalld volume is in the middle of a layout change. Start recalld once, stop it, and try again"
    );
    fs::write(&journal, br#"{"ready":true}"#).unwrap();
    fs::rename(root.join(".recalld-store"), root.join("moved")).unwrap();
    assert!(matches!(s.read(&root), Err(ImportError::LayoutChanging)));
}

#[test]
fn an_old_layout_is_refused() {
    let s = Setup::new();
    let mut state = s.get(CONTROL, "state");
    assert_eq!(state["controlSchemaVersion"], 2);
    state["controlSchemaVersion"] = 1.into();
    s.rewrite(CONTROL, "state", Some(serde_json::to_vec(&state).unwrap()));
    let err = s.read(&s.data()).unwrap_err();
    assert!(matches!(err, ImportError::TooOld));
    assert_eq!(
        err.to_string(),
        "Upgrade recalld to 2.2 and start it once, then stop it and try again"
    );
    s.rewrite(CONTROL, "state", None);
    assert!(
        matches!(s.read(&s.data()), Err(ImportError::TooOld)),
        "no state row"
    );
}

#[test]
fn a_text_file_in_format_1_is_refused() {
    let s = Setup::new();
    let active: String = serde_json::from_value(s.get(TEXT, "active")).unwrap();
    let mut generation = s.get(TEXT, &active);
    generation["formatVersion"] = 1.into();
    s.rewrite(
        TEXT,
        &active,
        Some(serde_json::to_vec(&generation).unwrap()),
    );
    assert!(matches!(s.read(&s.data()), Err(ImportError::TooOld)));

    let s = Setup::new();
    let path = s.text_file();
    let mut bytes = fs::read(&path).unwrap();
    bytes[4] = 1;
    fs::write(&path, bytes).unwrap();
    assert!(matches!(s.read(&s.data()), Err(ImportError::TooOld)));
}

#[test]
fn no_active_text_file_is_refused() {
    let s = Setup::new();
    s.rewrite(TEXT, "active", None);
    let err = s.read(&s.data()).unwrap_err();
    assert!(matches!(err, ImportError::NoActiveText), "{err:?}");
}

#[test]
fn missing_files_are_refused() {
    for name in ["meta.db", "edges.db", "text"] {
        let s = Setup::new();
        let path = if name == "text" {
            s.text_file()
        } else {
            s.data().join(name)
        };
        fs::remove_file(&path).unwrap();
        let err = s.read(&s.data()).unwrap_err();
        assert!(
            matches!(&err, ImportError::Missing(p) if p.file_name() == path.file_name()),
            "{name}: {err:?}"
        );
    }
    let s = Setup::new();
    let err = s.read(&s.volume().join("nothing")).unwrap_err();
    assert!(matches!(err, ImportError::Missing(_)), "{err:?}");
}

#[test]
fn symbolic_links_are_refused() {
    for name in ["meta.db", "edges.db", "text", "recalld.lock"] {
        let s = Setup::new();
        let path = if name == "text" {
            s.text_file()
        } else {
            s.data().join(name)
        };
        let real = s.tmp.path().join("elsewhere");
        fs::rename(&path, &real).unwrap();
        symlink(&real, &path).unwrap();
        let err = s.read(&s.data()).unwrap_err();
        assert!(
            matches!(&err, ImportError::SymbolicLink(p) if p.file_name() == path.file_name()),
            "{name}: {err:?}"
        );
    }
    let s = Setup::new();
    let link = s.volume().join("link");
    symlink(s.data(), &link).unwrap();
    assert!(matches!(s.read(&link), Err(ImportError::SymbolicLink(_))));
}

#[test]
fn the_run_folder_may_not_be_in_or_beside_the_recalld_directory() {
    let s = Setup::new();
    for run in [s.data().join("run"), s.volume()] {
        let before = s.listing();
        let err = read(&s.data(), &run, NOW).unwrap_err();
        assert!(matches!(err, ImportError::RunDirInside), "{err:?}");
        let _ = fs::remove_dir(s.data().join("run"));
        assert_eq!(s.listing(), before);
    }
}
