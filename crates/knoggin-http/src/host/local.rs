// SPDX-License-Identifier: AGPL-3.0-only
//! `LocalBackend`: snapshot files under `<data dir>/stores/<store id>/`, data keys wrapped
//! by the local key provider and kept in the store records of the local state file.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use knoggin_core::snapshot::SnapshotName;
use knoggin_core::time::MILLIS_PER_DAY;
use knoggin_core::{Sensitive, StoreId, UserId};
use knoggin_crypto::{KeyError, KeyProvider};
use knoggin_state::record::Newest;
use knoggin_state::{StateError, Stores};

use super::backend::{BackendError, OpenedStore, SealedSnapshot, SnapshotSource};
use crate::error::LockReason;

/// How many of a store's newest files are always kept.
const KEEP_NEWEST: usize = 3;

/// The local install's backend.
pub struct LocalBackend {
    stores_dir: PathBuf,
    keys: Arc<dyn KeyProvider>,
    records: Stores,
    retention_days: u32,
    saves: Mutex<HashMap<StoreId, Tracked>>,
}

impl std::fmt::Debug for LocalBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalBackend")
            .field("retention_days", &self.retention_days)
            .finish_non_exhaustive()
    }
}

/// Per store: its owner, the name its record holds as newest, and the last save, which a
/// flush writes into the record.
#[derive(Clone)]
struct Tracked {
    user: UserId,
    recorded: Option<SnapshotName>,
    last: Option<LastSave>,
}

/// The figures of one save.
#[derive(Clone)]
struct LastSave {
    name: SnapshotName,
    memory_count: u64,
    namespace_count: u64,
    last_write_ms: Option<i64>,
    last_write_client: Option<String>,
}

impl LocalBackend {
    /// Snapshots under `data_dir/stores`, keys from `keys`, records through `records`.
    /// `retention_days` is the setting `retention.days`.
    pub fn new(
        data_dir: &Path,
        keys: Arc<dyn KeyProvider>,
        records: Stores,
        retention_days: u32,
    ) -> Self {
        Self {
            stores_dir: data_dir.join("stores"),
            keys,
            records,
            retention_days,
            saves: Mutex::new(HashMap::new()),
        }
    }

    /// The folder of one store's snapshot files.
    pub fn store_dir(&self, store: StoreId) -> PathBuf {
        self.stores_dir.join(store.to_string())
    }

    /// Writes the counts and the name of each store's last save into its record, for the
    /// stores where they changed. The daemon calls it once a minute and at shutdown.
    pub async fn flush_records(&self) -> Result<(), StateError> {
        let dirty: Vec<(StoreId, UserId, LastSave)> = self
            .saves
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|(id, t)| {
                let last = t.last.clone()?;
                (t.recorded != Some(last.name)).then_some((*id, t.user, last))
            })
            .collect();
        for (id, user, save) in dirty {
            self.records
                .update(user, id, |r| {
                    r.newest = Some(Newest {
                        generation: save.name.generation(),
                        sequence: save.name.sequence(),
                    });
                    r.memory_count = save.memory_count;
                    r.namespace_count = save.namespace_count;
                    r.last_write_ms = save.last_write_ms;
                    r.last_write_client = save.last_write_client.clone().map(Sensitive::new);
                    Ok::<(), StateError>(())
                })
                .await?;
            if let Some(t) = self
                .saves
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_mut(&id)
            {
                t.recorded = Some(save.name);
            }
        }
        Ok(())
    }

    fn remember_open(&self, store: StoreId, user: UserId, recorded: Option<SnapshotName>) {
        let mut saves = self.saves.lock().unwrap_or_else(PoisonError::into_inner);
        let t = saves.entry(store).or_insert(Tracked {
            user,
            recorded,
            last: None,
        });
        t.user = user;
        t.recorded = t.recorded.max(recorded);
    }

    fn recorded(&self, store: StoreId) -> Option<SnapshotName> {
        self.saves
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&store)
            .and_then(|t| t.recorded)
    }
}

fn state_error(e: &StateError) -> BackendError {
    tracing::warn!(kind = %state_kind(e), "the state database failed");
    BackendError::Unavailable
}

fn state_kind(e: &StateError) -> &'static str {
    match e {
        StateError::Conflict => "conflict",
        StateError::Unavailable(_) => "unavailable",
        _ => "other",
    }
}

#[async_trait]
impl super::backend::StoreBackend for LocalBackend {
    async fn open(
        &self,
        store: StoreId,
        after: Option<SnapshotName>,
    ) -> Result<OpenedStore, BackendError> {
        let record = self
            .records
            .get(store)
            .await
            .map_err(|e| state_error(&e))?
            .ok_or(BackendError::Deleted)?
            .value;
        if record.deleted_at_ms.is_some() {
            return Err(BackendError::Deleted);
        }
        let wrapped = record.wrapped().map_err(|e| state_error(&e))?;
        let key = self
            .keys
            .unwrap(store, &wrapped)
            .await
            .map_err(|e| match e {
                KeyError::UnknownMasterKey { .. } | KeyError::Unwrap => {
                    BackendError::Locked(LockReason::MasterKeyMissing)
                }
                _ => BackendError::Unavailable,
            })?;
        let floor = record
            .newest
            .and_then(|n| SnapshotName::new(n.generation, n.sequence).ok());
        let dir = self.store_dir(store);
        let (generation, snapshot) = tokio::task::spawn_blocking(move || pick(&dir, floor, after))
            .await
            .map_err(|e| joined(&e))??;
        self.remember_open(store, record.user_id, floor);
        Ok(OpenedStore {
            key,
            wrapped,
            generation,
            next_sequence: 1,
            restore: false,
            snapshot,
        })
    }

    async fn save(&self, store: StoreId, snapshot: SealedSnapshot) -> Result<(), BackendError> {
        let dir = self.store_dir(store);
        let retention_days = self.retention_days;
        let floor = self.recorded(store);
        let SealedSnapshot {
            name,
            chunks,
            memory_count,
            namespace_count,
            last_write_at,
            last_write_client,
            ..
        } = snapshot;
        tokio::task::spawn_blocking(move || {
            write_file(&dir, name, chunks)?;
            // Retention is tidying: a failure there does not undo the save.
            if let Err(e) = thin(&dir, floor, retention_days, super::now().millis()) {
                tracing::warn!(kind = ?e.kind(), "removing old snapshot files failed");
            }
            Ok::<(), BackendError>(())
        })
        .await
        .map_err(|e| joined(&e))??;
        if let Some(t) = self
            .saves
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&store)
        {
            t.last = Some(LastSave {
                name,
                memory_count,
                namespace_count,
                last_write_ms: last_write_at.map(|t| t.millis()),
                last_write_client,
            });
        }
        Ok(())
    }
}

/// A blocking task that did not finish: a panic is a bug (`internal`), anything else is
/// a cancel at shutdown (`unavailable`). The error itself is never printed.
fn joined(e: &tokio::task::JoinError) -> BackendError {
    if e.is_panic() {
        tracing::error!(kind = "panic", "a snapshot file task panicked");
        BackendError::Internal
    } else {
        BackendError::Unavailable
    }
}

fn make_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// The store's snapshot names, highest first, after removing stray `.tmp` files.
fn names(dir: &Path) -> io::Result<Vec<SnapshotName>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(text) = file_name.to_str() else {
            continue;
        };
        if text.ends_with(".kgs.tmp") {
            fs::remove_file(entry.path())?;
        } else if let Ok(name) = SnapshotName::from_file_name(text) {
            out.push(name);
        }
    }
    out.sort_unstable_by(|a, b| b.cmp(a));
    Ok(out)
}

/// The generation of this open and the snapshot to load, including when a snapshot is
/// missing on a local install.
fn pick(
    dir: &Path,
    floor: Option<SnapshotName>,
    after: Option<SnapshotName>,
) -> Result<(u64, Option<SnapshotSource>), BackendError> {
    let io_error = |e: io::Error| {
        tracing::warn!(kind = ?e.kind(), "reading the store folder failed");
        BackendError::Unavailable
    };
    let all = names(dir).map_err(io_error)?;
    let generation = all.first().map_or(1, |n| n.generation().saturating_add(1));
    if SnapshotName::new(generation, 1).is_err() {
        return Err(BackendError::Unavailable);
    }
    let chosen = all
        .iter()
        .copied()
        .filter(|n| floor.is_none_or(|f| *n >= f))
        .find(|n| after.is_none_or(|a| *n < a));
    let Some(name) = chosen else {
        // Never an empty store over data that should exist or that could not be read.
        if after.is_some() || floor.is_some() {
            return Err(BackendError::Locked(LockReason::SnapshotUnreadable));
        }
        return Ok((generation, None));
    };
    let file = File::open(dir.join(name.file_name())).map_err(io_error)?;
    Ok((
        generation,
        Some(SnapshotSource {
            name,
            reader: Box::new(BufReader::new(file)),
        }),
    ))
}

/// The write steps: a temporary file, `sync_all`, a hard link that fails if the name
/// exists, removing the temporary file, syncing the folder.
fn write_file(
    dir: &Path,
    name: SnapshotName,
    chunks: impl Iterator<Item = Result<Vec<u8>, knoggin_crypto::container::ContainerError>>,
) -> Result<(), BackendError> {
    let failed = |e: io::Error| {
        tracing::warn!(kind = ?e.kind(), "writing a snapshot file failed");
        BackendError::Unavailable
    };
    make_dir(dir).map_err(failed)?;
    let tmp = dir.join(name.temp_file_name());
    let path = dir.join(name.file_name());
    if path.exists() {
        return Err(BackendError::NameTaken);
    }
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(failed)?;
        for chunk in chunks {
            let chunk = chunk.map_err(|_| {
                tracing::error!(kind = "seal", "sealing a snapshot failed");
                BackendError::Unavailable
            })?;
            file.write_all(&chunk).map_err(failed)?;
        }
        file.sync_all().map_err(failed)?;
        match fs::hard_link(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(BackendError::NameTaken),
            Err(e) => Err(failed(e)),
        }
    })();
    let _ = fs::remove_file(&tmp);
    result?;
    File::open(dir).and_then(|d| d.sync_all()).map_err(failed)
}

/// Local retention: the 3 newest files, the newest file of each of the last
/// `retention_days` UTC days by modification time, and the recorded newest.
fn thin(
    dir: &Path,
    recorded: Option<SnapshotName>,
    retention_days: u32,
    now_ms: i64,
) -> io::Result<()> {
    let all = names(dir)?;
    let today = now_ms.div_euclid(MILLIS_PER_DAY);
    let mut kept_days = Vec::new();
    for (index, name) in all.iter().enumerate() {
        let path = dir.join(name.file_name());
        let day = fs::metadata(&path)?
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .div_euclid(MILLIS_PER_DAY);
        // `all` is highest first, so the first file seen of a day is its newest.
        let daily = today - day < i64::from(retention_days) && !kept_days.contains(&day);
        if daily {
            kept_days.push(day);
        }
        if index < KEEP_NEWEST || daily || Some(*name) == recorded {
            continue;
        }
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, g: u64, s: u64) -> SnapshotName {
        let name = SnapshotName::new(g, s).unwrap();
        fs::write(dir.join(name.file_name()), b"x").unwrap();
        name
    }

    #[test]
    fn pick_takes_the_newest_at_or_above_the_floor_and_below_after() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert!(matches!(pick(dir, None, None), Ok((1, None))));
        let a = touch(dir, 1, 1);
        let b = touch(dir, 1, 2);
        let c = touch(dir, 2, 1);
        fs::write(dir.join("0000000002-0000000002.kgs.tmp"), b"x").unwrap();
        let (generation, snap) = pick(dir, None, None).unwrap();
        assert_eq!((generation, snap.unwrap().name), (3, c));
        assert!(!dir.join("0000000002-0000000002.kgs.tmp").exists());
        assert_eq!(pick(dir, None, Some(c)).unwrap().1.unwrap().name, b);
        assert_eq!(pick(dir, Some(a), Some(b)).unwrap().1.unwrap().name, a);
        assert!(matches!(
            pick(dir, Some(b), Some(b)),
            Err(BackendError::Locked(LockReason::SnapshotUnreadable))
        ));
        let empty = tempfile::tempdir().unwrap();
        assert!(matches!(
            pick(empty.path(), Some(a), None),
            Err(BackendError::Locked(LockReason::SnapshotUnreadable))
        ));
    }

    #[tokio::test]
    async fn a_panic_in_a_file_task_is_internal_and_a_cancel_unavailable() {
        let panicked = tokio::task::spawn_blocking(|| -> () { panic!("marker") })
            .await
            .unwrap_err();
        assert_eq!(joined(&panicked), BackendError::Internal);
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        let cancelled = task.await.unwrap_err();
        assert_eq!(joined(&cancelled), BackendError::Unavailable);
    }

    #[test]
    fn a_taken_name_is_refused_and_nothing_is_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let name = SnapshotName::new(1, 1).unwrap();
        write_file(tmp.path(), name, [Ok(b"first".to_vec())].into_iter()).unwrap();
        let again = write_file(tmp.path(), name, [Ok(b"second".to_vec())].into_iter());
        assert_eq!(again, Err(BackendError::NameTaken));
        assert_eq!(
            fs::read(tmp.path().join(name.file_name())).unwrap(),
            b"first"
        );
        assert!(!tmp.path().join(name.temp_file_name()).exists());
    }

    #[test]
    fn thinning_keeps_three_newest_one_per_day_and_the_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let names: Vec<_> = (1..=6).map(|s| touch(dir, 1, s)).collect();
        // All are from today: the 3 newest stay, today's newest is among them, and the
        // recorded one stays.
        thin(dir, Some(names[0]), 7, crate::host::now().millis()).unwrap();
        let mut left = super::names(dir).unwrap();
        left.sort();
        assert_eq!(left, vec![names[0], names[3], names[4], names[5]]);
        // Seen from 30 days on, no file is in a daily window.
        thin(
            dir,
            None,
            7,
            crate::host::now().millis() + 30 * MILLIS_PER_DAY,
        )
        .unwrap();
        assert_eq!(super::names(dir).unwrap().len(), 3);
    }
}
