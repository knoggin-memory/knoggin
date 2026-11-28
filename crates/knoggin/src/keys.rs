// SPDX-License-Identifier: AGPL-3.0-only
//! The local master key: where it lives (a
//! key file or the system keychain), the record of that choice in `data/key-source.json`,
//! the first run's choice, and `knoggin key export | import | rotate`.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use knoggin_crypto::source::{
    FileRule, Keychain, SourceError, account_name, create_key_file, read_key_file, replace_key_file,
};
use knoggin_crypto::{LocalKeyProvider, MasterKeySet};
use knoggin_state::{FieldKeys, LocalFileDb, Meta, StateDb, rewrap_master_key, system_clock};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Home, MasterKeyChoice};

/// Which kind of source holds the master key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceKind {
    Keychain,
    KeyFile,
}

impl SourceKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Keychain => "keychain",
            Self::KeyFile => "key_file",
        }
    }
}

/// `data/key-source.json`. `path` is a note for people: the key file's path comes only
/// from `local.master_key_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct KeySourceRecord {
    pub(crate) source: SourceKind,
    pub(crate) path: Option<String>,
    pub(crate) kek_id: String,
}

/// A place that holds a master key set.
#[derive(Debug, Clone)]
pub(crate) enum Source {
    File(PathBuf),
    Keychain(Keychain),
}

/// This platform's keychain for this home, if it has one.
fn keychain(home: &Home) -> Option<Keychain> {
    let account = account_name(home.root());
    match std::env::consts::OS {
        "macos" => Some(Keychain::macos(account, "/usr/bin/security".into())),
        // Found on `PATH`; a missing tool is `ToolMissing`.
        "linux" => Some(Keychain::secret_service(account, "secret-tool".into())),
        _ => None,
    }
}

impl Source {
    /// The source of `kind` for this install.
    pub(crate) fn of(kind: SourceKind, config: &Config, home: &Home) -> Option<Self> {
        match kind {
            SourceKind::KeyFile => Some(Self::File(config.local.master_key_file.clone())),
            SourceKind::Keychain => keychain(home).map(Self::Keychain),
        }
    }

    pub(crate) fn kind(&self) -> SourceKind {
        match self {
            Self::File(_) => SourceKind::KeyFile,
            Self::Keychain(_) => SourceKind::Keychain,
        }
    }

    /// Blocking: the keychain tool runs as a child process.
    pub(crate) fn read(&self) -> Result<MasterKeySet, SourceError> {
        match self {
            Self::File(path) => read_key_file(path, FileRule::Private),
            Self::Keychain(k) => k.read(),
        }
    }

    /// Saves a set where none is: a new key file, or the keychain item.
    fn store_new(&self, keys: &MasterKeySet) -> Result<(), SourceError> {
        match self {
            Self::File(path) => {
                make_parent(path).map_err(SourceError::Io)?;
                create_key_file(path, keys)
            }
            Self::Keychain(k) => k.write(keys),
        }
    }

    /// Replaces the set that is there (or makes it).
    fn replace(&self, keys: &MasterKeySet) -> Result<(), SourceError> {
        match self {
            Self::File(path) if path.exists() => replace_key_file(path, keys),
            other => other.store_new(keys),
        }
    }

    fn record(&self, keys: &MasterKeySet) -> KeySourceRecord {
        KeySourceRecord {
            source: self.kind(),
            path: match self {
                Self::File(p) => Some(p.display().to_string()),
                Self::Keychain(_) => None,
            },
            kek_id: keys.current_kek_id(),
        }
    }
}

fn make_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            DirBuilder::new().recursive(true).mode(0o700).create(dir)
        }
        _ => Ok(()),
    }
}

pub(crate) fn record_path(data_dir: &Path) -> PathBuf {
    data_dir.join("key-source.json")
}

/// Reads `key-source.json`; `None` when it is missing.
pub(crate) fn read_record(data_dir: &Path) -> anyhow::Result<Option<KeySourceRecord>> {
    let path = record_path(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("{} is not a key source record", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes `key-source.json` (mode 0600) with a temporary file and a rename.
pub(crate) fn write_record(data_dir: &Path, record: &KeySourceRecord) -> anyhow::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let path = record_path(data_dir);
    let tmp = data_dir.join(format!(".key-source.json.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let write = || -> io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec(record).map_err(io::Error::other)?)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        File::open(data_dir)?.sync_all()
    };
    write().with_context(|| format!("writing {}", path.display()))
}

/// What the first run chose.
#[derive(Debug)]
pub(crate) struct Chosen {
    pub(crate) source: Source,
    pub(crate) keys: MasterKeySet,
    /// Why `auto` used a key file although the system has a keychain.
    pub(crate) fell_back: Option<String>,
}

/// Uses the set the source already holds, or makes one there. Blocking.
fn use_or_make(source: &Source) -> Result<MasterKeySet, SourceError> {
    match source.read() {
        Ok(keys) => Ok(keys),
        Err(SourceError::NotFound) => {
            let keys = MasterKeySet::generate()?;
            source.store_new(&keys)?;
            Ok(keys)
        }
        Err(e) => Err(e),
    }
}

/// The first run's choice (`local.master_key`). A key set already in the chosen place is
/// used as it is, never replaced. Blocking: run it on the blocking pool.
pub(crate) fn first_run(config: &Config, home: &Home) -> anyhow::Result<Chosen> {
    let file = Source::File(config.local.master_key_file.clone());
    let chosen = |source: Source, keys| Chosen {
        source,
        keys,
        fell_back: None,
    };
    match config.local.master_key {
        MasterKeyChoice::KeyFile => {
            let keys = use_or_make(&file).context("the master key file")?;
            Ok(chosen(file, keys))
        }
        MasterKeyChoice::Keychain => {
            let kc = keychain(home)
                .map(Source::Keychain)
                .ok_or_else(|| anyhow!("this system has no keychain that Knoggin can use"))?;
            let keys = use_or_make(&kc).context("the system keychain")?;
            Ok(chosen(kc, keys))
        }
        MasterKeyChoice::Auto => {
            let mut fell_back = None;
            if let Some(kc) = keychain(home).map(Source::Keychain) {
                match use_or_make(&kc) {
                    Ok(keys) => return Ok(chosen(kc, keys)),
                    Err(e) => fell_back = Some(e.to_string()),
                }
            }
            let keys = use_or_make(&file).context("the master key file")?;
            Ok(Chosen {
                source: file,
                keys,
                fell_back,
            })
        }
    }
}

/// Writes the record of a choice.
pub(crate) fn record_choice(data_dir: &Path, chosen: &Chosen) -> anyhow::Result<()> {
    write_record(data_dir, &chosen.source.record(&chosen.keys))
}

/// The recorded source's key set. Blocking.
pub(crate) fn load(
    config: &Config,
    home: &Home,
    record: &KeySourceRecord,
) -> anyhow::Result<MasterKeySet> {
    let source = Source::of(record.source, config, home)
        .ok_or_else(|| anyhow!("this system has no keychain that Knoggin can use"))?;
    source
        .read()
        .with_context(|| format!("reading the master key from the {}", record.source.as_str()))
}

/// Holds the daemon lock for as long as the result lives, so that a daemon cannot start
/// meanwhile. Refuses while a daemon runs. `None` when no run folder exists yet.
pub(crate) fn hold_daemon_lock(home: &Home) -> anyhow::Result<Option<File>> {
    let path = home.lock();
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("the daemon is running; stop it first with `knoggin stop`")
        }
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("locking {}", path.display()))
        }
    }
}

fn require_record(config: &Config) -> anyhow::Result<KeySourceRecord> {
    read_record(&config.local.data_dir)?.ok_or_else(|| {
        anyhow!(
            "no master key is recorded in {}; start the daemon once, or run `knoggin key import`",
            record_path(&config.local.data_dir).display()
        )
    })
}

/// `knoggin key export --out <file>`: a new file, mode 0600; an existing path or a link is
/// refused.
pub(crate) fn export(config: &Config, home: &Home, out: &Path) -> anyhow::Result<()> {
    let _lock = hold_daemon_lock(home)?;
    let record = require_record(config)?;
    let keys = load(config, home, &record)?;
    create_key_file(out, &keys).with_context(|| format!("writing {}", out.display()))
}

/// `knoggin key import <file>`: loads the set into the recorded source, or, without a
/// record or on a machine without that source, into the source `auto` picks.
pub(crate) fn import(config: &Config, home: &Home, file: &Path) -> anyhow::Result<SourceKind> {
    let _lock = hold_daemon_lock(home)?;
    let keys = read_key_file(file, FileRule::NotWritableByOthers)
        .with_context(|| format!("reading {}", file.display()))?;
    let recorded =
        read_record(&config.local.data_dir)?.and_then(|r| Source::of(r.source, config, home));
    let written = match recorded {
        Some(source) => match source.replace(&keys) {
            Ok(()) => Some(source),
            Err(SourceError::ToolMissing(_)) => None,
            Err(e) => return Err(e).context("writing the master key"),
        },
        None => None,
    };
    let source = match written {
        Some(s) => s,
        None => {
            let kc = keychain(home).map(Source::Keychain);
            match kc {
                Some(kc)
                    if config.local.master_key != MasterKeyChoice::KeyFile
                        && kc.replace(&keys).is_ok() =>
                {
                    kc
                }
                _ => {
                    let file = Source::File(config.local.master_key_file.clone());
                    file.replace(&keys).context("writing the master key file")?;
                    file
                }
            }
        }
    };
    write_record(&config.local.data_dir, &source.record(&keys))?;
    Ok(source.kind())
}

/// What `knoggin key rotate` did.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Rotated {
    pub(crate) kek_id: String,
    pub(crate) stores: usize,
}

/// `knoggin key rotate`, with the daemon stopped: a new key becomes `current` (the old ones
/// stay in the set), then every store's data key, the field key and the state file's own
/// key are wrapped again, and the record names the new key.
pub(crate) async fn rotate(config: &Config, home: &Home) -> anyhow::Result<Rotated> {
    let lock_home = home.clone();
    let _lock = tokio::task::spawn_blocking(move || hold_daemon_lock(&lock_home))
        .await
        .map_err(|_| anyhow!("internal error"))??;
    let record = require_record(config)?;
    let (c, h, r) = (config.clone(), home.clone(), record.clone());
    let (old, mut keys) = tokio::task::spawn_blocking(move || {
        Ok::<_, anyhow::Error>((load(&c, &h, &r)?, load(&c, &h, &r)?))
    })
    .await
    .map_err(|_| anyhow!("internal error"))??;
    let state_dir = config.local.data_dir.join("state");
    let old = LocalKeyProvider::new(old);
    // Read the state first: a rotation never starts on an install it cannot open.
    LocalFileDb::open(&state_dir, &old, system_clock())
        .await
        .context("opening the state file")?
        .ok_or_else(|| anyhow!("there is no state file in {}", state_dir.display()))?;
    keys.rotate()?;
    let source = Source::of(record.source, config, home)
        .ok_or_else(|| anyhow!("this system has no keychain that Knoggin can use"))?;
    let kek_id = keys.current_kek_id();
    let new_record = source.record(&keys);
    let save = source.clone();
    let keys = tokio::task::spawn_blocking(move || save.replace(&keys).map(|()| keys))
        .await
        .map_err(|_| anyhow!("internal error"))?
        .context("saving the new master key")?;
    let provider = LocalKeyProvider::new(keys);
    let db = LocalFileDb::open(&state_dir, &provider, system_clock())
        .await?
        .ok_or_else(|| anyhow!("the state file went away"))?;
    let db: Arc<LocalFileDb> = Arc::new(db);
    let fields: Arc<FieldKeys> = Arc::new(Meta::new(db.clone()).field_keys(&provider).await?);
    let stores = rewrap_master_key(db.clone() as Arc<dyn StateDb>, fields, &provider).await?;
    db.rewrap(&provider).await?;
    write_record(&config.local.data_dir, &new_record)?;
    Ok(Rotated { kek_id, stores })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{Env, Flags, Role};

    fn config(dir: &Path) -> (Config, Home) {
        let vars = BTreeMap::from([
            ("HOME".to_owned(), dir.display().to_string()),
            (
                "KNOGGIN_HOME".to_owned(),
                dir.join("k").display().to_string(),
            ),
            (
                "KNOGGIN_LOCAL__MASTER_KEY".to_owned(),
                "key_file".to_owned(),
            ),
        ]);
        let env = Env::from_vars(vars, dir.into()).unwrap();
        (
            Config::load(&env, Role::Local, None, &Flags::default()).unwrap(),
            env.paths(),
        )
    }

    #[test]
    fn a_first_run_keeps_a_key_file_that_is_there() {
        let d = tempfile::tempdir().unwrap();
        let (c, h) = config(d.path());
        let first = first_run(&c, &h).unwrap();
        assert_eq!(first.source.kind(), SourceKind::KeyFile);
        record_choice(&c.local.data_dir, &first).unwrap();
        let again = first_run(&c, &h).unwrap();
        assert_eq!(first.keys.current_kek_id(), again.keys.current_kek_id());
        let rec = read_record(&c.local.data_dir).unwrap().unwrap();
        assert_eq!(rec.kek_id, first.keys.current_kek_id());
        assert_eq!(load(&c, &h, &rec).unwrap().current_kek_id(), rec.kek_id);
    }

    #[test]
    fn export_refuses_an_existing_path_and_import_restores_the_set() {
        let d = tempfile::tempdir().unwrap();
        let (c, h) = config(d.path());
        let first = first_run(&c, &h).unwrap();
        record_choice(&c.local.data_dir, &first).unwrap();
        let out = d.path().join("backup.json");
        export(&c, &h, &out).unwrap();
        assert!(export(&c, &h, &out).is_err());
        std::fs::remove_file(&c.local.master_key_file).unwrap();
        std::fs::remove_file(record_path(&c.local.data_dir)).unwrap();
        assert_eq!(import(&c, &h, &out).unwrap(), SourceKind::KeyFile);
        let rec = read_record(&c.local.data_dir).unwrap().unwrap();
        assert_eq!(rec.kek_id, first.keys.current_kek_id());
    }
}
