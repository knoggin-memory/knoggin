// SPDX-License-Identifier: AGPL-3.0-only
//! Files of a recalld directory: finding the store, testing its locks, the private
//! copies, and the text file. Nothing here writes to the recalld directory.

use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use uuid::Uuid;

use crate::ImportError;

/// The journal of a recalld 2.2 container volume (`src/storage/container_layout.rs`).
const LAYOUT_JOURNAL: &str = ".recalld-container-layout.json";
/// The store folder inside a container volume.
const LAYOUT_STORE: &str = ".recalld-store";

/// `path` as given, absolute, with its parent resolved but not its last part.
fn resolve(path: &Path) -> Result<(PathBuf, PathBuf), ImportError> {
    let absolute = std::path::absolute(path)?;
    let name = absolute
        .file_name()
        .ok_or_else(|| ImportError::NotADirectory(absolute.clone()))?;
    let parent = absolute
        .parent()
        .ok_or_else(|| ImportError::NotADirectory(absolute.clone()))?;
    let parent = parent.canonicalize().map_err(|e| missing(e, parent))?;
    Ok((parent.join(name), parent))
}

fn missing(e: io::Error, path: &Path) -> ImportError {
    if e.kind() == io::ErrorKind::NotFound {
        ImportError::Missing(path.to_path_buf())
    } else {
        ImportError::Io(e)
    }
}

/// `Ok(None)` when nothing is there; a symbolic link is refused.
fn lstat(path: &Path) -> Result<Option<fs::Metadata>, ImportError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(ImportError::SymbolicLink(path.to_path_buf())),
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn require_dir(path: &Path) -> Result<(), ImportError> {
    match lstat(path)? {
        Some(m) if m.is_dir() => Ok(()),
        Some(_) => Err(ImportError::NotADirectory(path.to_path_buf())),
        None => Err(ImportError::Missing(path.to_path_buf())),
    }
}

/// Opens a regular file for reading. A symbolic link is refused, also one swapped in
/// between the check and the open.
pub(crate) fn open_regular(path: &Path) -> Result<File, ImportError> {
    let meta = match lstat(path)? {
        Some(m) if m.is_file() => m,
        Some(_) => return Err(ImportError::NotAFile(path.to_path_buf())),
        None => return Err(ImportError::Missing(path.to_path_buf())),
    };
    let file = File::open(path).map_err(|e| missing(e, path))?;
    let opened = file.metadata()?;
    if opened.dev() != meta.dev() || opened.ino() != meta.ino() {
        return Err(ImportError::SymbolicLink(path.to_path_buf()));
    }
    Ok(file)
}

/// Where a recalld store is, and the folder the user named.
#[derive(Debug)]
pub(crate) struct Located {
    /// The folder that holds `meta.db`.
    pub store: PathBuf,
    /// The folder the user gave (the volume root of a container layout).
    pub given: PathBuf,
    /// The given folder's parent.
    pub parent: PathBuf,
    /// The lock files to test, in the order step 2 before reading tests them.
    pub locks: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct LayoutJournal {
    ready: bool,
}

fn owner_lock(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(".recalld-owner.lock");
    dir.with_file_name(name)
}

/// Step 1 before reading: find the store, and with it the lock files.
pub(crate) fn locate(path: &Path) -> Result<Located, ImportError> {
    let (given, parent) = resolve(path)?;
    require_dir(&given)?;
    let mut locks = vec![owner_lock(&given), given.join("recalld.lock")];
    let journal = given.join(LAYOUT_JOURNAL);
    let store = if lstat(&journal)?.is_some() {
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut open_regular(&journal)?, &mut bytes)?;
        let ready = serde_json::from_slice::<LayoutJournal>(&bytes).is_ok_and(|j| j.ready);
        let child = given.join(LAYOUT_STORE);
        if !ready || !lstat(&child)?.is_some_and(|m| m.is_dir()) {
            return Err(ImportError::LayoutChanging);
        }
        locks.push(owner_lock(&child));
        locks.push(child.join("recalld.lock"));
        child
    } else {
        given.clone()
    };
    Ok(Located {
        store,
        given,
        parent,
        locks,
    })
}

/// Step 2 before reading: a shared lock on each lock file that exists, without
/// waiting and without creating one. The locks are held while the copies are made, so
/// that recalld cannot start in the middle.
pub(crate) fn hold_locks(paths: &[PathBuf]) -> Result<Vec<File>, ImportError> {
    let mut held = Vec::new();
    for path in paths {
        if lstat(path)?.is_none() {
            continue;
        }
        let file = open_regular(path)?;
        match file.try_lock_shared() {
            Ok(()) => held.push(file),
            Err(TryLockError::WouldBlock) => return Err(ImportError::Running),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    Ok(held)
}

/// The private folder that holds the copies of `meta.db` and `edges.db`. Dropping it
/// deletes the folder, after a success, a failure or a panic alike.
#[derive(Debug)]
pub(crate) struct Copies {
    dir: PathBuf,
}

impl Copies {
    /// Makes `<run_dir>/import-<random>/` with mode 0700. `run_dir` must not be in or
    /// beside the recalld directory.
    pub(crate) fn create(run_dir: &Path, located: &Located) -> Result<Copies, ImportError> {
        // Checked before anything is created: the longest part that exists, resolved,
        // plus the rest.
        let absolute = std::path::absolute(run_dir)?;
        let mut existing = absolute.as_path();
        while fs::symlink_metadata(existing).is_err() {
            existing = existing.parent().unwrap_or(Path::new("/"));
        }
        let resolved = existing
            .canonicalize()?
            .join(absolute.strip_prefix(existing).unwrap_or(Path::new("")));
        if resolved.starts_with(&located.given) || resolved == located.parent {
            return Err(ImportError::RunDirInside);
        }
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&resolved)?;
        let run_dir = resolved;
        let dir = run_dir.join(format!("import-{}", Uuid::now_v7().simple()));
        DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Copies { dir })
    }

    /// Copies one file of the store into the folder and returns the copy's path.
    pub(crate) fn copy(&self, source: &Path) -> Result<PathBuf, ImportError> {
        let mut from = open_regular(source)?;
        let name = source.file_name().unwrap_or_default();
        let target = self.dir.join(name);
        let mut to = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)?;
        io::copy(&mut from, &mut to)?;
        Ok(target)
    }
}

impl Drop for Copies {
    fn drop(&mut self) {
        // Nothing can be reported from a drop; the daemon removes a left-over folder
        // at its next start.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The header of a text file in format 2 (`src/storage/text/generation.rs`).
const TEXT_HEADER: usize = 48;
/// An entry's header: `u32` length, `u32` CRC32 (`src/storage/text.rs`).
const ENTRY_HEADER: u64 = 8;
/// recalld's `MAX_ENTRY_SIZE`.
const MAX_ENTRY: u32 = 1_048_576;

/// The active text file, read where it is.
#[derive(Debug)]
pub(crate) struct TextFile {
    file: File,
    len: u64,
}

impl TextFile {
    /// Opens `fulltext.<generation>.dat` and checks its header against the store and the
    /// generation. A file in format 1 is refused as too old.
    pub(crate) fn open(
        path: &Path,
        store_id: Uuid,
        generation: Uuid,
    ) -> Result<TextFile, ImportError> {
        let file = open_regular(path)?;
        let len = file.metadata()?.len();
        let mut header = [0_u8; TEXT_HEADER];
        if len < TEXT_HEADER as u64 {
            return Err(ImportError::Corrupt(
                "the text file is shorter than its header",
            ));
        }
        file.read_exact_at(&mut header, 0)?;
        if header[..4] != *b"MEMT" {
            return Err(ImportError::Corrupt("the text file has no MEMT header"));
        }
        if header[4..6] != 2_u16.to_le_bytes() {
            return Err(ImportError::TooOld);
        }
        let mut want = [0_u8; TEXT_HEADER];
        want[..4].copy_from_slice(b"MEMT");
        want[4..6].copy_from_slice(&2_u16.to_le_bytes());
        want[16..32].copy_from_slice(store_id.as_bytes());
        want[32..48].copy_from_slice(generation.as_bytes());
        if header != want {
            return Err(ImportError::Corrupt(
                "the text file belongs to another store or generation",
            ));
        }
        Ok(TextFile { file, len })
    }

    /// The text at `offset`, which points at an entry header, of `length` bytes. The
    /// length must match the entry's, and the CRC32 the text.
    pub(crate) fn read(&self, offset: u64, length: u32) -> Result<String, &'static str> {
        let end = offset
            .checked_add(ENTRY_HEADER)
            .and_then(|o| o.checked_add(u64::from(length)));
        if length > MAX_ENTRY || offset < TEXT_HEADER as u64 || end.is_none_or(|e| e > self.len) {
            return Err("the text reference points outside the text file");
        }
        let mut head = [0_u8; 8];
        self.file
            .read_exact_at(&mut head, offset)
            .map_err(|_| "the text file could not be read")?;
        let (stored_len, stored_crc) = head.split_at(4);
        if stored_len != length.to_le_bytes() {
            return Err("the text entry's length does not match the record");
        }
        let mut text = vec![0_u8; usize::try_from(length).map_err(|_| "text too long")?];
        self.file
            .read_exact_at(&mut text, offset.saturating_add(ENTRY_HEADER))
            .map_err(|_| "the text file could not be read")?;
        if crc32fast::hash(&text).to_le_bytes() != stored_crc {
            return Err("the text entry's CRC32 does not match");
        }
        String::from_utf8(text).map_err(|_| "the text entry is not UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn copies_are_private_and_go_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("meta.db"), b"bytes").unwrap();
        let located = locate(&data).unwrap();
        let copies = Copies::create(&tmp.path().join("home/run"), &located).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&copies.dir), 0o700);
        assert!(
            copies
                .dir
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("import-")
        );
        let copy = copies.copy(&data.join("meta.db")).unwrap();
        assert_eq!(mode(&copy), 0o600);
        assert_eq!(fs::read(&copy).unwrap(), b"bytes");
        let dir = copies.dir.clone();
        drop(copies);
        assert!(!dir.exists());
    }
}
