// SPDX-License-Identifier: AGPL-3.0-only
//! `LocalFileDb`: the items in memory, saved whole as one encrypted state file in the
//! container format on every change, before the change becomes visible.
//!
//! Files are `state-<sequence:010>.kgs` in one directory. A write seals the new map under
//! the next sequence into `<name>.tmp`, syncs it, hard-links it to `<name>` (which fails if
//! that name exists), removes the temporary file and syncs the directory; then it keeps
//! the 3 newest files. The header has the nil store ID and generation 0, and the file's
//! own data key is wrapped in it.

use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use knoggin_core::StoreId;
use knoggin_core::snapshot::SnapshotName;
use knoggin_crypto::container::{self, ContainerError, Header};
use knoggin_crypto::{DataKey, KeyProvider, LocalKeyProvider, WrappedKey};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::db::{
    Clock, Item, StateDb, StateError, Table, Write, table_apply, table_get, table_query,
};

/// The content format of the state file, counted apart from the store format.
pub const STATE_CONTENT_FORMAT: u32 = 1;
/// The state file keeps this many of its newest copies.
pub const KEEP_FILES: usize = 3;
/// A state file larger than this is refused as corrupt.
const MAX_CONTENT: u64 = 1 << 30;
const PREFIX: &str = "state-";
const EXT: &str = ".kgs";
const TMP_EXT: &str = ".kgs.tmp";

/// One item in the state file. postcard is not self-describing: never change this struct.
#[derive(Serialize, Deserialize)]
struct StateItemV1 {
    pk: String,
    sk: String,
    version: u64,
    expires_ms: Option<i64>,
    doc: String,
}

struct StateFile {
    dir: PathBuf,
    key: DataKey,
    wrapped: WrappedKey,
    sequence: u64,
}

struct Inner {
    table: Table,
    file: Option<StateFile>,
}

/// The local state database. In production only the daemon opens it, under the daemon
/// lock; fast tests share one in-memory instance between several routers.
pub struct LocalFileDb {
    inner: Mutex<Inner>,
    clock: Clock,
}

impl fmt::Debug for LocalFileDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalFileDb").finish_non_exhaustive()
    }
}

/// What `knoggin state use-previous` shows about the file it would fall back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateFileSummary {
    /// The file's path.
    pub path: PathBuf,
    /// When it was written.
    pub created_ms: u64,
    /// The stores it holds a record of (`store#` items).
    pub store_ids: Vec<StoreId>,
    /// How many API keys it holds (`key#` items).
    pub key_count: usize,
}

impl LocalFileDb {
    /// A database that lives in memory only, for tests.
    pub fn in_memory(clock: Clock) -> Self {
        Self {
            inner: Mutex::new(Inner {
                table: Table::new(),
                file: None,
            }),
            clock,
        }
    }

    /// The first run: makes the file's data key with `KeyProvider::generate` under the nil
    /// store ID and writes an empty state file as sequence 1. Refuses a directory that
    /// already holds a state file.
    pub async fn create(
        dir: &Path,
        provider: &dyn KeyProvider,
        clock: Clock,
    ) -> Result<Self, StateError> {
        let d = dir.to_owned();
        let existing = blocking(move || {
            DirBuilder::new().recursive(true).mode(0o700).create(&d)?;
            Ok(list(&d)?)
        })
        .await?;
        if !existing.files.is_empty() {
            return Err(StateError::Invalid("a state file exists already"));
        }
        let (key, wrapped) = provider.generate(StoreId::default()).await?;
        let file = StateFile {
            dir: dir.to_owned(),
            key,
            wrapped,
            sequence: 0,
        };
        let db = Self {
            inner: Mutex::new(Inner {
                table: Table::new(),
                file: Some(file),
            }),
            clock,
        };
        db.write(Vec::new()).await?;
        Ok(db)
    }

    /// Opens the newest state file. `None` when there is none. Removes leftover temporary
    /// files. Never falls back to an older file: an unreadable newest file is an error
    /// (`Corrupt`, or `Key` when the master key cannot unwrap the file's key).
    pub async fn open(
        dir: &Path,
        provider: &dyn KeyProvider,
        clock: Clock,
    ) -> Result<Option<Self>, StateError> {
        let d = dir.to_owned();
        let files = blocking(move || {
            let found = list(&d)?;
            for t in &found.temps {
                fs::remove_file(t)?;
            }
            Ok(found.files)
        })
        .await?;
        let Some((sequence, path)) = files.last().cloned() else {
            return Ok(None);
        };
        let (header, key, table) = read_state_file(path, sequence, provider).await?;
        let file = StateFile {
            dir: dir.to_owned(),
            key,
            wrapped: header.wrapped_key(),
            sequence,
        };
        Ok(Some(Self {
            inner: Mutex::new(Inner {
                table,
                file: Some(file),
            }),
            clock,
        }))
    }

    /// Master-key rotation: wraps the file's own data key under the provider's current
    /// master key and writes the file again.
    pub async fn rewrap(&self, provider: &LocalKeyProvider) -> Result<(), StateError> {
        let mut inner = self.inner.lock().await;
        let wrapped = match &inner.file {
            Some(file) => provider.wrap(StoreId::default(), &file.key)?,
            None => return Ok(()),
        };
        let table = inner.table.clone();
        self.save(&mut inner, table, Some(wrapped)).await
    }

    /// Writes `table` as the next state file, then makes it the visible state.
    async fn save(
        &self,
        inner: &mut Inner,
        table: Table,
        wrapped: Option<WrappedKey>,
    ) -> Result<(), StateError> {
        let Some(file) = inner.file.as_mut() else {
            inner.table = table;
            return Ok(());
        };
        let wrapped = wrapped.unwrap_or_else(|| file.wrapped.clone());
        let (dir, key, first) = (file.dir.clone(), file.key.clone(), file.sequence + 1);
        let now = u64::try_from((self.clock)()).unwrap_or(0);
        let w = wrapped.clone();
        let (table, sequence) = blocking(move || {
            let content = encode(&table)?;
            let sequence = write_file(&dir, first, now, key, w, content)?;
            Ok((table, sequence))
        })
        .await?;
        file.sequence = sequence;
        file.wrapped = wrapped;
        inner.table = table;
        Ok(())
    }
}

#[async_trait]
impl StateDb for LocalFileDb {
    async fn get(&self, pk: &str, sk: &str) -> Result<Option<Item>, StateError> {
        let inner = self.inner.lock().await;
        Ok(table_get(&inner.table, pk, sk, (self.clock)()))
    }

    async fn query(
        &self,
        pk: &str,
        sk_prefix: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Vec<Item>, StateError> {
        let inner = self.inner.lock().await;
        Ok(table_query(
            &inner.table,
            pk,
            sk_prefix,
            limit,
            after,
            (self.clock)(),
        ))
    }

    async fn write(&self, writes: Vec<Write>) -> Result<(), StateError> {
        let mut inner = self.inner.lock().await;
        let next = table_apply(&inner.table, writes, (self.clock)())?;
        self.save(&mut inner, next, None).await
    }
}

/// The file `knoggin state use-previous` would fall back to: the newest but one.
pub async fn previous_state_file(
    dir: &Path,
    provider: &dyn KeyProvider,
) -> Result<Option<StateFileSummary>, StateError> {
    let d = dir.to_owned();
    let files = blocking(move || Ok(list(&d)?.files)).await?;
    let Some((sequence, path)) = files.iter().rev().nth(1).cloned() else {
        return Ok(None);
    };
    let (header, _, table) = read_state_file(path.clone(), sequence, provider).await?;
    let store_ids = table
        .keys()
        .filter_map(|(pk, _)| pk.strip_prefix("store#"))
        .filter_map(|id| id.parse().ok().map(StoreId::from_uuid))
        .collect();
    let key_count = table
        .keys()
        .filter(|(pk, _)| pk.starts_with("key#"))
        .count();
    Ok(Some(StateFileSummary {
        path,
        created_ms: header.created_ms,
        store_ids,
        key_count,
    }))
}

/// `knoggin state use-previous`, with the daemon stopped: renames the newest state file
/// to `<name>.bad`, so that the next start loads the one before it. Returns the new path.
pub async fn set_aside_newest(dir: &Path) -> Result<Option<PathBuf>, StateError> {
    let d = dir.to_owned();
    blocking(move || {
        let Some((_, path)) = list(&d)?.files.pop() else {
            return Ok(None);
        };
        let mut bad = path.clone().into_os_string();
        bad.push(".bad");
        let bad = PathBuf::from(bad);
        fs::rename(&path, &bad)?;
        File::open(&d)?.sync_all()?;
        Ok(Some(bad))
    })
    .await
}

/// Reads, checks and decrypts one state file.
async fn read_state_file(
    path: PathBuf,
    sequence: u64,
    provider: &dyn KeyProvider,
) -> Result<(Header, DataKey, Table), StateError> {
    let (header, bytes, body) = blocking(move || {
        let bytes = fs::read(&path)?;
        let (header, n) = container::read_header(&bytes).map_err(container_error)?;
        Ok((header, bytes, n))
    })
    .await?;
    header
        .check_content_format(STATE_CONTENT_FORMAT, STATE_CONTENT_FORMAT)
        .map_err(container_error)?;
    let name =
        SnapshotName::new(0, sequence).map_err(|_| StateError::Corrupt("a state file name"))?;
    header
        .expect(StoreId::default(), name)
        .map_err(container_error)?;
    let key = provider
        .unwrap(StoreId::default(), &header.wrapped_key())
        .await?;
    let (h, k) = (header.clone(), key.clone());
    let table = blocking(move || {
        let content =
            container::open(&h, &k, &bytes[body..], MAX_CONTENT).map_err(container_error)?;
        Ok(decode(&content)?)
    })
    .await?;
    Ok((header, key, table))
}

fn encode(table: &Table) -> Result<Vec<u8>, StateError> {
    let items: Vec<StateItemV1> = table
        .values()
        .map(|i| StateItemV1 {
            pk: i.pk.clone(),
            sk: i.sk.clone(),
            version: i.version,
            expires_ms: i.expires_ms,
            doc: i.doc.to_string(),
        })
        .collect();
    postcard::to_allocvec(&items).map_err(|_| StateError::Internal)
}

fn decode(content: &[u8]) -> Result<Table, StateError> {
    let bad = || StateError::Corrupt("the state file's content");
    let (items, rest) =
        postcard::take_from_bytes::<Vec<StateItemV1>>(content).map_err(|_| bad())?;
    if !rest.is_empty() {
        return Err(bad());
    }
    let mut table = Table::new();
    let mut last: Option<(String, String)> = None;
    for i in items {
        let key = (i.pk.clone(), i.sk.clone());
        if last.as_ref().is_some_and(|l| *l >= key) || i.version == 0 {
            return Err(bad());
        }
        last = Some(key.clone());
        let doc = serde_json::from_str(&i.doc).map_err(|_| bad())?;
        table.insert(
            key,
            Item {
                pk: i.pk,
                sk: i.sk,
                version: i.version,
                expires_ms: i.expires_ms,
                doc,
            },
        );
    }
    Ok(table)
}

fn container_error(e: ContainerError) -> StateError {
    match e {
        ContainerError::Io(e) => StateError::Unavailable(e.to_string()),
        ContainerError::Random | ContainerError::Crypto => StateError::Internal,
        _ => StateError::Corrupt("the state file does not open"),
    }
}

fn file_name(sequence: u64) -> String {
    format!("{PREFIX}{sequence:010}{EXT}")
}

struct Listing {
    /// `(sequence, path)`, oldest first.
    files: Vec<(u64, PathBuf)>,
    temps: Vec<PathBuf>,
}

fn list(dir: &Path) -> io::Result<Listing> {
    let mut files = Vec::new();
    let mut temps = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(PREFIX) else {
            continue;
        };
        if rest.ends_with(TMP_EXT) {
            temps.push(entry.path());
        } else if let Some(n) = rest.strip_suffix(EXT)
            && n.len() == 10
            && n.bytes().all(|b| b.is_ascii_digit())
            && let Ok(seq) = n.parse()
        {
            files.push((seq, entry.path()));
        }
    }
    files.sort();
    Ok(Listing { files, temps })
}

/// Writes the file under `sequence`, or the next free one if that name is taken (a write
/// that linked its file and then failed to sync the directory may have left it), and keeps
/// the 3 newest. Returns the sequence used.
fn write_file(
    dir: &Path,
    mut sequence: u64,
    now: u64,
    key: DataKey,
    wrapped: WrappedKey,
    content: Vec<u8>,
) -> Result<u64, StateError> {
    let mut header = Header::new(
        StoreId::default(),
        0,
        sequence,
        now,
        STATE_CONTENT_FORMAT,
        wrapped,
    );
    for _ in 0..3 {
        SnapshotName::new(0, sequence).map_err(|_| StateError::Invalid("state file sequence"))?;
        header.sequence = sequence;
        let name = dir.join(file_name(sequence));
        let tmp = dir.join(format!("{PREFIX}{sequence:010}{TMP_EXT}"));
        let result = write_once(
            &tmp,
            &name,
            dir,
            header.clone(),
            key.clone(),
            content.clone(),
        );
        // Tidying after the link: the new file is already durable, so neither step may
        // turn the write into a failure.
        if let Err(e) = fs::remove_file(&tmp)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(kind = ?e.kind(), "removing a temporary state file failed");
        }
        match result {
            Ok(()) => {
                if let Err(e) = prune(dir) {
                    tracing::warn!(kind = ?e.kind(), "removing old state files failed");
                }
                return Ok(sequence);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => sequence += 1,
            Err(e) => return Err(StateError::Unavailable(e.to_string())),
        }
    }
    Err(StateError::Unavailable("state file names are taken".into()))
}

fn write_once(
    tmp: &Path,
    name: &Path,
    dir: &Path,
    header: Header,
    key: DataKey,
    content: Vec<u8>,
) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    for part in container::seal(header, key, content) {
        let part = part.map_err(io::Error::other)?;
        f.write_all(&part)?;
    }
    f.sync_all()?;
    drop(f);
    fs::hard_link(tmp, name)?;
    if let Err(e) = File::open(dir).and_then(|d| d.sync_all()) {
        // Not known to be durable: take it back, so the answer `unavailable` is true.
        let _ = fs::remove_file(name);
        return Err(e);
    }
    Ok(())
}

/// Keeps the 3 newest state files.
fn prune(dir: &Path) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_PRUNE.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(io::Error::other("pruning fails in this test"));
    }
    let files = list(dir)?.files;
    let old = files.len().saturating_sub(KEEP_FILES);
    for (_, path) in &files[..old] {
        // A file that cannot be removed now is removed after a later write.
        let _ = fs::remove_file(path);
    }
    Ok(())
}

/// Makes `prune` fail, for the test that a failed prune does not fail the write.
#[cfg(test)]
static FAIL_PRUNE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Runs file work on the blocking pool. A panic there is `Internal`; a cancel at
/// shutdown is `Unavailable`. The `JoinError` itself is never formatted.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, BlockingError> + Send + 'static,
) -> Result<T, StateError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r.map_err(Into::into),
        Err(e) if e.is_panic() => Err(StateError::Internal),
        Err(_) => Err(StateError::Unavailable("shutting down".into())),
    }
}

/// An error inside a blocking task: plain I/O, or one already decided.
enum BlockingError {
    Io(io::Error),
    State(StateError),
}

impl From<io::Error> for BlockingError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<StateError> for BlockingError {
    fn from(e: StateError) -> Self {
        Self::State(e)
    }
}

impl From<BlockingError> for StateError {
    fn from(e: BlockingError) -> Self {
        match e {
            BlockingError::Io(e) => Self::Unavailable(e.to_string()),
            BlockingError::State(e) => e,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use knoggin_crypto::testing::test_provider;

    use super::*;
    use crate::db::{Cond, Item};

    #[tokio::test]
    async fn a_write_whose_tidying_fails_still_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let db = LocalFileDb::create(tmp.path(), &test_provider(), crate::system_clock())
            .await
            .unwrap();
        FAIL_PRUNE.store(true, Ordering::SeqCst);
        let put = Write::put(Item::new("p", "s", serde_json::json!({})), Cond::Any);
        let result = db.write(vec![put]).await;
        FAIL_PRUNE.store(false, Ordering::SeqCst);
        result.unwrap();
        assert!(tmp.path().join(file_name(2)).exists());
        assert!(db.get("p", "s").await.unwrap().is_some());
    }
}
