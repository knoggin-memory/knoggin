// SPDX-License-Identifier: AGPL-3.0-only
//! `knoggin lifecycle` (tab "Server", The lifecycle task): thins snapshots, erases deleted
//! stores and users, and keeps house. It is the only code that deletes objects, and it
//! holds no master key, no data key and no field key: it reads only unsealed fields.
//! Each run logs counts only, and a run that fails part-way is safe to repeat.

use std::sync::Arc;
use std::time::Duration;

use knoggin_core::Timestamp;
use knoggin_core::snapshot::{SnapshotName, SnapshotObject, thin};
use knoggin_core::time::MILLIS_PER_DAY;
use knoggin_state::{PostgresDb, StateDb, StateError, erase, thin_targets};
use tokio::sync::watch;

use crate::settings::RetentionSettings;
use crate::storage::{ObjectStorage, StorageError, names};

/// What one run did, in counts.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleReport {
    /// Stores whose snapshots were looked at.
    pub stores_thinned: usize,
    /// Snapshot objects that thinning deleted.
    pub snapshots_deleted: usize,
    /// Stores whose objects and records were erased.
    pub stores_erased: usize,
    /// Users whose records were erased.
    pub users_erased: usize,
    /// Due stores whose objects could not be deleted; the next run tries again.
    pub stores_failed: usize,
    /// Self-test objects older than a day that were deleted.
    pub selftest_deleted: usize,
    /// Expired Postgres rows that were deleted.
    pub expired_rows: u64,
}

/// Why a run stopped.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    /// The state database failed.
    #[error("the state database: {0}")]
    State(#[from] StateError),
    /// The bucket failed.
    #[error("the bucket: {0}")]
    Storage(#[from] StorageError),
}

/// The lifecycle task's pieces.
pub struct Lifecycle {
    /// The state database, read and delete only.
    pub db: Arc<dyn StateDb>,
    /// On one host the Postgres backend, for its expired rows.
    pub postgres: Option<Arc<PostgresDb>>,
    /// The bucket, with delete.
    pub storage: Arc<dyn ObjectStorage>,
    /// `retention.*`.
    pub retention: RetentionSettings,
}

impl std::fmt::Debug for Lifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lifecycle")
            .field("retention", &self.retention)
            .finish_non_exhaustive()
    }
}

impl Lifecycle {
    /// One pass: thinning, erasing deleted stores and users, housekeeping. `now` is a
    /// parameter so that tests can set it.
    pub async fn run_once(&self, now: Timestamp) -> Result<LifecycleReport, LifecycleError> {
        let mut report = LifecycleReport::default();
        self.thin_all(now, &mut report).await?;
        let storage = self.storage.clone();
        let erased = erase(
            self.db.clone(),
            now.millis(),
            self.retention.soft_delete_days,
            move |store| {
                let storage = storage.clone();
                async move { delete_prefix(&*storage, &names::store_prefix(store)).await }
            },
        )
        .await?;
        report.stores_erased = erased.stores_erased;
        report.users_erased = erased.users_erased;
        report.stores_failed = erased.stores_failed;
        let day_ago = now.millis().saturating_sub(MILLIS_PER_DAY);
        let old: Vec<String> = self
            .storage
            .list(names::SELFTEST)
            .await?
            .into_iter()
            .filter(|o| o.last_modified.millis() < day_ago)
            .map(|o| o.key)
            .collect();
        delete(&*self.storage, &old).await?;
        report.selftest_deleted = old.len();
        if let Some(pg) = &self.postgres {
            report.expired_rows = pg.delete_expired().await?;
        }
        Ok(report)
    }

    /// Step 1: for every store that is not deleted, deletes the snapshots `thin` gives
    /// no label. Objects under the folder that are not snapshot names are left alone.
    async fn thin_all(
        &self,
        now: Timestamp,
        report: &mut LifecycleReport,
    ) -> Result<(), LifecycleError> {
        for target in thin_targets(&*self.db).await? {
            let prefix = names::snapshot_prefix(target.store);
            let (keys, objects): (Vec<String>, Vec<SnapshotObject>) = self
                .storage
                .list(&prefix)
                .await?
                .into_iter()
                .filter_map(|o| {
                    let name = names::name_in(&prefix, &o.key)?;
                    Some((
                        o.key,
                        SnapshotObject {
                            name,
                            last_modified: o.last_modified,
                        },
                    ))
                })
                .unzip();
            let newest = target
                .newest
                .and_then(|n| SnapshotName::new(n.generation, n.sequence).ok());
            let restore_from = target
                .restore_from
                .as_deref()
                .and_then(|r| r.parse::<SnapshotName>().ok());
            let labels = thin(&objects, restore_from, newest, self.retention.days, now);
            let doomed: Vec<String> = keys
                .into_iter()
                .zip(labels)
                .filter(|(_, label)| label.is_none())
                .map(|(key, _)| key)
                .collect();
            delete(&*self.storage, &doomed).await?;
            report.stores_thinned += 1;
            report.snapshots_deleted += doomed.len();
        }
        Ok(())
    }
}

/// Deletes every object under `prefix`.
async fn delete_prefix(storage: &dyn ObjectStorage, prefix: &str) -> Result<(), StorageError> {
    let keys: Vec<String> = storage
        .list(prefix)
        .await?
        .into_iter()
        .map(|o| o.key)
        .collect();
    delete(storage, &keys).await
}

/// The one place that deletes objects (tab "Security", I11; a test checks that no other
/// code calls `ObjectStorage::delete`).
async fn delete(storage: &dyn ObjectStorage, keys: &[String]) -> Result<(), StorageError> {
    if keys.is_empty() {
        return Ok(());
    }
    ObjectStorage::delete(storage, keys).await
}

/// `knoggin lifecycle`: one pass, or with `every` one pass per interval until `stop`.
/// Returns the exit code: 1 when the single pass failed; a repeating run logs a failed
/// pass and goes on.
pub async fn run(
    lifecycle: Lifecycle,
    every: Option<Duration>,
    clock: knoggin_state::Clock,
    mut stop: watch::Receiver<bool>,
) -> i32 {
    loop {
        let result = lifecycle.run_once(Timestamp::from_millis(clock())).await;
        match &result {
            Ok(r) => tracing::info!(
                kind = "lifecycle",
                stores_thinned = r.stores_thinned,
                snapshots_deleted = r.snapshots_deleted,
                stores_erased = r.stores_erased,
                users_erased = r.users_erased,
                stores_failed = r.stores_failed,
                selftest_deleted = r.selftest_deleted,
                expired_rows = r.expired_rows,
                "a lifecycle pass ended"
            ),
            // Only which part failed: a state error's text can quote a database message.
            Err(e) => {
                let failed = match e {
                    LifecycleError::State(_) => "state",
                    LifecycleError::Storage(_) => "bucket",
                };
                tracing::error!(kind = "lifecycle", failed, "a lifecycle pass failed");
            }
        }
        let Some(every) = every else {
            return i32::from(result.is_err());
        };
        tokio::select! {
            () = tokio::time::sleep(every) => {}
            _ = stop.wait_for(|s| *s) => return 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// I11: only this module calls `ObjectStorage::delete`. Every other `.delete(` in the
    /// crate's code must be on a receiver that is not the bucket.
    #[test]
    fn nothing_else_deletes_objects() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path
                    .strip_prefix(&src)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                // The trait and its implementations, and this module.
                if rel == "lifecycle.rs" || rel.starts_with("storage") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                for (n, line) in text.lines().enumerate() {
                    let allowed = line.contains("containers.delete(");
                    if (line.contains(".delete(") && !allowed)
                        || line.contains("ObjectStorage::delete")
                        || line.contains("delete_objects")
                    {
                        offenders.push(format!("{rel}:{}: {}", n + 1, line.trim()));
                    }
                }
            }
        }
        assert!(offenders.is_empty(), "{offenders:#?}");
    }
}
