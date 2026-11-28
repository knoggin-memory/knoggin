// SPDX-License-Identifier: AGPL-3.0-only
//! The pinned model files, the model identity, and checking and downloading the files.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use aws_lc_rs::digest;
use knoggin_core::embed::ModelIdentity;
use knoggin_core::ops::ModelUnavailableReason;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, RANGE};
use tokio::io::AsyncWriteExt;

/// The Hugging Face repository of the export.
pub const REPOSITORY: &str = "onnx-community/embeddinggemma-2-ONNX";
/// The pinned commit of that repository.
pub const COMMIT: &str = "daa72c51243991dfcaf9f9137d2c573d8f7790c0";
/// The name to show people.
pub const NAME: &str = "EmbeddingGemma 2 \u{b7} q4";
/// Values per vector.
pub const DIMENSIONS: u32 = 768;
/// Tokens per text; the tokenizer cuts longer text.
pub const MAX_TOKENS: usize = 1024;
/// Free space a download needs before it starts (250 MB).
pub const MIN_FREE_BYTES: u64 = 250_000_000;

/// One pinned file of the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedFile {
    /// Path in the Hugging Face repository.
    pub repo_path: &'static str,
    /// Name it is saved as.
    pub name: &'static str,
    /// Size in bytes.
    pub size: u64,
    /// SHA-256, lower-case hex.
    pub sha256: &'static str,
}

/// The graph: the published `model_q4.onnx` with `accuracy_level = 4` on every
/// `MatMulNBits` node (the faster compute path, `scripts/patch_q4_graph.py`). Only the project's
/// release serves it; `repo_path` names the file it was made from.
pub const GRAPH: PinnedFile = PinnedFile {
    repo_path: "onnx/model_q4.onnx",
    name: "model_q4.onnx",
    size: 494_100,
    sha256: "1afd951bf447f631786cc1302921f94d4bc30f602ee6a61e8e517d0a3203c8a8",
};
/// The weights; the graph refers to them by this name.
pub const WEIGHTS: PinnedFile = PinnedFile {
    repo_path: "onnx/model_q4.onnx_data",
    name: "model_q4.onnx_data",
    size: 174_028_800,
    sha256: "c3975f2d1ab7a1878ae31a7d7a9b7804a827aff3800b60dfceafce21cac3df49",
};
/// The tokenizer.
pub const TOKENIZER: PinnedFile = PinnedFile {
    repo_path: "tokenizer.json",
    name: "tokenizer.json",
    size: 32_170_510,
    sha256: "4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4",
};
/// The three files the model needs.
pub const FILES: [PinnedFile; 3] = [GRAPH, WEIGHTS, TOKENIZER];

/// The identity JSON: keys sorted, no spaces.
pub fn identity_json() -> String {
    format!(
        concat!(
            r#"{{"dimensions":{},"document_recipe":"doc-v1","graph_sha256":"{}","max_tokens":{},"#,
            r#""model":"google/embeddinggemma-2","normalization":"l2","precision":"q4","#,
            r#""query_recipe":"query-v1","source":"{}@{}","tokenizer_sha256":"{}","weights_sha256":"{}"}}"#
        ),
        DIMENSIONS, GRAPH.sha256, MAX_TOKENS, REPOSITORY, COMMIT, TOKENIZER.sha256, WEIGHTS.sha256,
    )
}

static IDENTITY: LazyLock<String> = LazyLock::new(|| {
    let hash = hex(digest::digest(&digest::SHA256, identity_json().as_bytes()).as_ref());
    format!("embeddinggemma-2-q4-{DIMENSIONS}-{}", &hash[..16])
});

/// The identity string, for example `embeddinggemma-2-q4-768-1d894cac0c7bb07b`.
pub fn identity_string() -> &'static str {
    &IDENTITY
}

/// The pinned model identity.
pub fn identity() -> ModelIdentity {
    ModelIdentity {
        id: identity_string().to_owned(),
        dimensions: DIMENSIONS,
    }
}

/// The folder under the models root that holds the files of the pinned model.
pub fn model_dir(root: &Path) -> PathBuf {
    root.join(identity_string())
}

/// One file to check and fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// Name it is saved as.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// SHA-256, lower-case hex.
    pub sha256: String,
    /// Download addresses, tried in order.
    pub urls: Vec<String>,
}

/// Reports the bytes free for an unprivileged user on the file system of a path.
pub type FreeSpace = fn(&Path) -> io::Result<u64>;

/// Where the files come from: the pinned constants in production, a local server in tests.
#[derive(Debug, Clone)]
pub struct ModelSource {
    /// The files.
    pub files: Vec<SourceFile>,
    /// The free-space check.
    pub free_space: FreeSpace,
    /// Waits before the retries after a `429` or `503`.
    pub backoff: [Duration; 3],
}

impl ModelSource {
    /// The pinned model: Hugging Face first, then the project's own release. The patched
    /// graph comes from the release only, because Hugging Face serves the published one.
    pub fn pinned() -> Self {
        let files = FILES
            .iter()
            .map(|f| {
                let release = format!(
                    "{}/releases/download/model-{}/{}",
                    env!("CARGO_PKG_REPOSITORY"),
                    identity_string(),
                    f.name
                );
                let urls = if *f == GRAPH {
                    vec![release]
                } else {
                    vec![
                        format!(
                            "https://huggingface.co/{REPOSITORY}/resolve/{COMMIT}/{}",
                            f.repo_path
                        ),
                        release,
                    ]
                };
                SourceFile {
                    name: f.name.to_owned(),
                    size: f.size,
                    sha256: f.sha256.to_owned(),
                    urls,
                }
            })
            .collect();
        Self {
            files,
            free_space: statvfs_free,
            backoff: [1, 4, 16].map(Duration::from_secs),
        }
    }
}

fn statvfs_free(path: &Path) -> io::Result<u64> {
    let stat = rustix::fs::statvfs(path)?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

const MANUAL: &str = "to supply the files by hand, put model_q4.onnx from the project's \
                      model release and model_q4.onnx_data and tokenizer.json from the \
                      pinned commit in that folder";

/// Why the model files are not ready.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// A file is missing and downloads are off.
    #[error("model file {file} is missing in {dir} and downloads are off; {MANUAL}")]
    FilesMissing {
        /// The file.
        file: String,
        /// The model folder.
        dir: PathBuf,
    },
    /// A file failed its hash: on disk with downloads off, or at every source.
    #[error("model file {file} failed its check: expected SHA-256 {expected}, got {actual}")]
    HashMismatch {
        /// The file.
        file: String,
        /// The pinned hash.
        expected: String,
        /// The hash found.
        actual: String,
    },
    /// Not enough free space. A partial file is kept for a later resume.
    #[error("not enough free space in {dir}: {needed} bytes needed, {free} free")]
    DiskFull {
        /// The folder.
        dir: PathBuf,
        /// Bytes needed.
        needed: u64,
        /// Bytes free.
        free: u64,
    },
    /// No source completed a file.
    #[error(
        "could not download model file {file}: {step} failed at the last source: {detail}; {MANUAL}"
    )]
    Offline {
        /// The file.
        file: String,
        /// The step that failed: `connect`, `response`, `body` or `hash`.
        step: &'static str,
        /// What went wrong there.
        detail: String,
    },
    /// A local file operation failed.
    #[error("model files: could not {action} {path}: {source}")]
    Io {
        /// What was being done.
        action: &'static str,
        /// The file or folder.
        path: PathBuf,
        /// The cause.
        source: io::Error,
    },
}

impl ModelError {
    /// The reason that the Model state reports.
    pub fn reason(&self) -> ModelUnavailableReason {
        match self {
            // A folder that cannot be written leaves the files missing as surely as a switch.
            Self::FilesMissing { .. } | Self::Io { .. } => ModelUnavailableReason::FilesMissing,
            Self::HashMismatch { .. } => ModelUnavailableReason::HashMismatch,
            Self::DiskFull { .. } => ModelUnavailableReason::DiskFull,
            Self::Offline { .. } => ModelUnavailableReason::Offline,
        }
    }
}

fn io_err(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> ModelError {
    let path = path.to_owned();
    move |source| ModelError::Io {
        action,
        path,
        source,
    }
}

/// Checks the pinned model files under the models root `root` and, when `download` is true,
/// downloads what is missing. Returns the folder that holds the checked files. `progress`
/// gets bytes done and bytes total over all files.
pub async fn ensure(
    root: &Path,
    download: bool,
    progress: impl FnMut(u64, u64) + Send,
) -> Result<PathBuf, ModelError> {
    ensure_from(root, download, progress, &ModelSource::pinned()).await
}

/// [`ensure`] with the files and the free-space check of `source`.
pub async fn ensure_from(
    root: &Path,
    download: bool,
    mut progress: impl FnMut(u64, u64) + Send,
    source: &ModelSource,
) -> Result<PathBuf, ModelError> {
    let dir = model_dir(root);
    let total: u64 = source.files.iter().map(|f| f.size).sum();
    if !download {
        // Check only; write nothing.
        let trusted = trusted_dir(root).await && trusted_dir(&dir).await;
        for file in &source.files {
            match check(&dir, file, trusted).await? {
                Check::Accepted { .. } => {}
                Check::Missing => {
                    return Err(ModelError::FilesMissing {
                        file: file.name.clone(),
                        dir,
                    });
                }
                Check::Wrong { actual } => {
                    return Err(ModelError::HashMismatch {
                        file: file.name.clone(),
                        expected: file.sha256.clone(),
                        actual,
                    });
                }
            }
        }
        progress(total, total);
        return Ok(dir);
    }

    // One downloader at a time. The lock goes when `_lock` drops.
    make_dir(root).await?;
    let _lock = lock(root.join(".lock")).await?;
    make_dir(&dir).await?;
    let trusted = trusted_dir(root).await && trusted_dir(&dir).await;

    // The files already there.
    let mut missing = Vec::new();
    let mut done = 0;
    for file in &source.files {
        match check(&dir, file, trusted).await? {
            Check::Accepted { hashed } => {
                if hashed {
                    write_marker(&dir, &file.name).await?;
                }
                done += file.size;
            }
            Check::Missing => missing.push(file),
            Check::Wrong { .. } => {
                remove(&dir.join(&file.name)).await?;
                remove(&marker(&dir, &file.name)).await?;
                missing.push(file);
            }
        }
    }
    progress(done, total);
    if missing.is_empty() {
        return Ok(dir);
    }

    // Enough free space before any download.
    let free = (source.free_space)(&dir).map_err(io_err("read the free space of", &dir))?;
    if free < MIN_FREE_BYTES {
        return Err(ModelError::DiskFull {
            dir,
            needed: MIN_FREE_BYTES,
            free,
        });
    }

    // Each missing file, smallest first.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| ModelError::Offline {
            file: missing[0].name.clone(),
            step: "connect",
            detail: e.to_string(),
        })?;
    missing.sort_by_key(|f| f.size);
    for file in missing {
        fetch_file(&client, &dir, file, source, &mut |n| {
            progress(done + n, total)
        })
        .await?;
        done += file.size;
    }
    Ok(dir)
}

enum Check {
    Accepted { hashed: bool },
    Missing,
    Wrong { actual: String },
}

/// Checks one file already on disk. Changes nothing on disk.
async fn check(dir: &Path, file: &SourceFile, trusted: bool) -> Result<Check, ModelError> {
    let path = dir.join(&file.name);
    let meta = match tokio::fs::symlink_metadata(&path).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Check::Missing),
        Err(e) => return Err(io_err("read", &path)(e)),
    };
    // The graph finds its weights by name in the same folder, so links are not accepted.
    if !meta.is_file() || meta.len() != file.size {
        return Ok(Check::Wrong {
            actual: format!("a {}-byte {}", meta.len(), kind(&meta)),
        });
    }
    let marked = tokio::fs::metadata(marker(dir, &file.name))
        .await
        .is_ok_and(|m| m.is_file());
    if trusted && marked && trusted_entry(&meta) {
        return Ok(Check::Accepted { hashed: false });
    }
    let actual = sha256_file(path).await?;
    if actual == file.sha256 {
        Ok(Check::Accepted { hashed: true })
    } else {
        Ok(Check::Wrong { actual })
    }
}

fn kind(meta: &fs::Metadata) -> &'static str {
    if meta.is_symlink() {
        "link"
    } else if meta.is_dir() {
        "folder"
    } else {
        "file"
    }
}

/// Downloads one file: every source in order until one gives the right bytes.
async fn fetch_file(
    client: &reqwest::Client,
    dir: &Path,
    file: &SourceFile,
    source: &ModelSource,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<(), ModelError> {
    let part = dir.join(format!("{}.part", file.name));
    // `hash_mismatch` only when the file failed its hash at every source.
    let mut offline = None;
    let mut mismatch = None;
    for url in &file.urls {
        match fetch_part(client, url, &part, file, source, progress).await {
            Ok(()) => {}
            Err(Fetch::Fatal(e)) => return Err(e),
            Err(Fetch::Failed { step, detail }) => {
                offline = Some(ModelError::Offline {
                    file: file.name.clone(),
                    step,
                    detail,
                });
                continue;
            }
        }
        // The hash.
        let actual = sha256_file(part.clone()).await?;
        if actual != file.sha256 {
            remove(&part).await?;
            mismatch = Some(ModelError::HashMismatch {
                file: file.name.clone(),
                expected: file.sha256.clone(),
                actual,
            });
            continue;
        }
        let path = dir.join(&file.name);
        tokio::fs::rename(&part, &path)
            .await
            .map_err(io_err("rename", &part))?;
        write_marker(dir, &file.name).await?;
        return Ok(());
    }
    Err(offline.or(mismatch).unwrap_or_else(|| ModelError::Offline {
        file: file.name.clone(),
        step: "connect",
        detail: "no download address".to_owned(),
    }))
}

enum Fetch {
    /// This source did not complete; try the next.
    Failed { step: &'static str, detail: String },
    /// Stop: disk full or a local file error.
    Fatal(ModelError),
}

fn failed(step: &'static str, detail: impl ToString) -> Fetch {
    Fetch::Failed {
        step,
        detail: detail.to_string(),
    }
}

/// Brings `part` to the full size from one address, resuming it and backing off between
/// tries, synced. It does not check the hash.
async fn fetch_part(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    file: &SourceFile,
    source: &ModelSource,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<(), Fetch> {
    let mut have = match tokio::fs::metadata(part).await {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(e) => return Err(Fetch::Fatal(io_err("read", part)(e))),
    };
    if have > file.size {
        remove(part).await.map_err(Fetch::Fatal)?;
        have = 0;
    }
    if have == file.size {
        return Ok(());
    }
    let mut retries = source.backoff.iter();
    let mut response = loop {
        let mut request = client.get(url);
        if have > 0 {
            request = request.header(RANGE, format!("bytes={have}-"));
        }
        let response = request.send().await.map_err(|e| failed("connect", e))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            match retries.next() {
                Some(wait) => {
                    tokio::time::sleep(*wait).await;
                    continue;
                }
                None => return Err(failed("response", format!("status {status}"))),
            }
        }
        break response;
    };
    let start = match response.status() {
        StatusCode::OK => 0,
        StatusCode::PARTIAL_CONTENT => {
            let range = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range);
            match range {
                Some((first, total)) if total == file.size && first == have => have,
                _ => return Err(failed("response", "a partial answer with the wrong range")),
            }
        }
        status => return Err(failed("response", format!("status {status}"))),
    };
    let mut out = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(start == 0)
        .append(start > 0)
        .mode(0o644)
        .open(part)
        .await
        .map_err(|e| Fetch::Fatal(io_err("open", part)(e)))?;
    let mut written = start;
    progress(written);
    let streamed = loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) if written == file.size => break Ok(()),
            Ok(None) => {
                break Err(failed(
                    "body",
                    format!("the answer ended at {written} bytes"),
                ));
            }
            Err(e) => break Err(failed("body", e)),
        };
        let len = chunk.len() as u64;
        if written + len > file.size {
            drop(out);
            remove(part).await.map_err(Fetch::Fatal)?;
            return Err(failed("body", "the answer is longer than the file"));
        }
        if let Err(e) = out.write_all(&chunk).await {
            break Err(write_error(e, part, file.size - written));
        }
        written += len;
        progress(written);
    };
    // A tokio file finishes its last write in the background unless flushed, and the next
    // source reads the length of this file.
    out.flush()
        .await
        .map_err(|e| write_error(e, part, file.size - written))?;
    streamed?;
    out.sync_all().await.map_err(|e| write_error(e, part, 0))?;
    Ok(())
}

fn write_error(e: io::Error, part: &Path, needed: u64) -> Fetch {
    if e.raw_os_error() == Some(rustix::io::Errno::NOSPC.raw_os_error()) {
        let dir = part.parent().map(Path::to_owned).unwrap_or_default();
        Fetch::Fatal(ModelError::DiskFull {
            dir,
            needed,
            free: 0,
        })
    } else {
        Fetch::Fatal(io_err("write", part)(e))
    }
}

/// `bytes <first>-<last>/<total>` to `(first, total)`.
fn parse_content_range(value: &str) -> Option<(u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (first, _) = range.split_once('-')?;
    Some((first.parse().ok()?, total.parse().ok()?))
}

fn marker(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.ok"))
}

async fn write_marker(dir: &Path, name: &str) -> Result<(), ModelError> {
    let path = marker(dir, name);
    tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o644)
        .open(&path)
        .await
        .map(drop)
        .map_err(io_err("write", &path))
}

async fn remove(path: &Path) -> Result<(), ModelError> {
    match tokio::fs::remove_file(path).await {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_err("delete", path)(e)),
        _ => Ok(()),
    }
}

async fn make_dir(dir: &Path) -> Result<(), ModelError> {
    // 0o755 so that a group-writable umask does not make the markers untrusted.
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .await
        .map_err(io_err("create", dir))
}

/// Owned by this user or root, and writable by nobody else.
fn trusted_entry(meta: &fs::Metadata) -> bool {
    let uid = meta.uid();
    (uid == rustix::process::geteuid().as_raw() || uid == 0) && meta.mode() & 0o022 == 0
}

async fn trusted_dir(dir: &Path) -> bool {
    tokio::fs::metadata(dir)
        .await
        .is_ok_and(|m| m.is_dir() && trusted_entry(&m))
}

async fn lock(path: PathBuf) -> Result<fs::File, ModelError> {
    use std::os::unix::fs::OpenOptionsExt;
    blocking(move || {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o644)
            .open(&path)
            .map_err(io_err("open", &path))?;
        file.lock().map_err(io_err("lock", &path))?;
        Ok(file)
    })
    .await
}

async fn sha256_file(path: PathBuf) -> Result<String, ModelError> {
    blocking(move || {
        let mut file = fs::File::open(&path).map_err(io_err("open", &path))?;
        let mut context = digest::Context::new(&digest::SHA256);
        let mut buf = vec![0; 1 << 20];
        loop {
            let n = file.read(&mut buf).map_err(io_err("read", &path))?;
            if n == 0 {
                break;
            }
            context.update(&buf[..n]);
        }
        Ok(hex(context.finish().as_ref()))
    })
    .await
}

/// Runs file work off the runtime threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ModelError> + Send + 'static,
) -> Result<T, ModelError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        // Never format a JoinError: it prints the panic message.
        Err(e) => Err(ModelError::Io {
            action: if e.is_panic() {
                "finish (panic)"
            } else {
                "finish (cancelled)"
            },
            path: PathBuf::new(),
            source: io::Error::other("blocking task ended"),
        }),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_string_is_the_pinned_one() {
        assert_eq!(
            identity_json(),
            r#"{"dimensions":768,"document_recipe":"doc-v1","graph_sha256":"1afd951bf447f631786cc1302921f94d4bc30f602ee6a61e8e517d0a3203c8a8","max_tokens":1024,"model":"google/embeddinggemma-2","normalization":"l2","precision":"q4","query_recipe":"query-v1","source":"onnx-community/embeddinggemma-2-ONNX@daa72c51243991dfcaf9f9137d2c573d8f7790c0","tokenizer_sha256":"4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4","weights_sha256":"c3975f2d1ab7a1878ae31a7d7a9b7804a827aff3800b60dfceafce21cac3df49"}"#
        );
        assert_eq!(
            identity_string(),
            "embeddinggemma-2-q4-768-1d894cac0c7bb07b"
        );
        assert_eq!(identity().dimensions, 768);
    }

    #[test]
    fn pinned_sources_are_in_order() {
        let source = ModelSource::pinned();
        assert_eq!(
            source.files[0].urls,
            [
                "https://github.com/knoggin-memory/knoggin/releases/download/model-embeddinggemma-2-q4-768-1d894cac0c7bb07b/model_q4.onnx"
            ]
        );
        assert_eq!(source.files[2].urls.len(), 2);
        let urls = &source.files[1].urls;
        assert_eq!(
            urls[0],
            "https://huggingface.co/onnx-community/embeddinggemma-2-ONNX/resolve/daa72c51243991dfcaf9f9137d2c573d8f7790c0/onnx/model_q4.onnx_data"
        );
        assert_eq!(
            urls[1],
            "https://github.com/knoggin-memory/knoggin/releases/download/model-embeddinggemma-2-q4-768-1d894cac0c7bb07b/model_q4.onnx_data"
        );
        assert_eq!(FILES.iter().map(|f| f.size).sum::<u64>(), 206_693_410);
    }

    #[test]
    fn content_range() {
        assert_eq!(parse_content_range("bytes 3-9/10"), Some((3, 10)));
        assert_eq!(parse_content_range("bytes */10"), None);
        assert_eq!(parse_content_range("items 3-9/10"), None);
    }
}
