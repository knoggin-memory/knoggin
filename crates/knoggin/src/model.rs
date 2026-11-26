// SPDX-License-Identifier: AGPL-3.0-only
//! The embedding model on the binary's side (tab "Embeddings"): the loop that checks and
//! downloads the files and loads the two providers, the ONNX Runtime library path, and
//! `knoggin model fetch | verify`.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use knoggin_core::embed::{EmbeddingProvider, ModelIdentity};
use knoggin_core::ops::{ModelState, ModelStatus, ModelUnavailableReason};
use knoggin_core::testing::FakeProvider;
use knoggin_embed::model::{self as files, ModelError};
use knoggin_embed::{OnnxProvider, Role, runtime};
use knoggin_http::host::Providers;
use serde::Serialize;
use tokio::sync::{OnceCell, watch};
use tokio::task::JoinHandle;

/// While the files are missing, `ensure` runs again this often.
const RETRY: Duration = Duration::from_secs(60);
/// After `hash_mismatch`: each try downloads the files again.
const RETRY_AFTER_MISMATCH: Duration = Duration::from_secs(3600);

/// What the host gets: the Model state channel and the cell that holds the providers once
/// they have loaded, with the loop's task.
#[derive(Debug)]
pub(crate) struct Model {
    pub(crate) identity: ModelIdentity,
    pub(crate) state: watch::Receiver<ModelState>,
    pub(crate) providers: Arc<OnceCell<Providers>>,
    pub(crate) task: Option<JoinHandle<()>>,
}

fn state(status: ModelStatus, reason: Option<ModelUnavailableReason>) -> ModelState {
    ModelState {
        state: status,
        identity: files::identity_string().to_owned(),
        name: files::NAME.to_owned(),
        bytes_done: None,
        bytes_total: None,
        reason,
    }
}

/// Test mode: the fake provider, `ready` at once, no library and no files.
pub(crate) fn fake() -> Model {
    let fake = FakeProvider::new();
    let identity = fake.identity().clone();
    let (_tx, rx) = watch::channel(ModelState {
        state: ModelStatus::Ready,
        identity: identity.id.clone(),
        name: "Fake model".into(),
        bytes_done: None,
        bytes_total: None,
        reason: None,
    });
    let cell = OnceCell::new();
    let _ = cell.set(Providers {
        query: Arc::new(FakeProvider::new()),
        document: Arc::new(fake),
    });
    Model {
        identity,
        state: rx,
        providers: Arc::new(cell),
        task: None,
    }
}

/// The ONNX Runtime library: `ORT_DYLIB_PATH`, else `../lib/libonnxruntime.<ext>` beside
/// the binary's own canonical path (the installed `knoggin` is a link).
pub(crate) fn library_path(ort_dylib: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = ort_dylib {
        return Ok(p.to_owned());
    }
    let exe = std::env::current_exe().and_then(std::fs::canonicalize)?;
    let ext = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let bin = exe
        .parent()
        .ok_or_else(|| anyhow!("the binary has no folder"))?;
    Ok(bin.join(format!("../lib/libonnxruntime.{ext}")))
}

/// The real model: starts the loop in the background and returns at once.
pub(crate) fn start(root: PathBuf, download: bool, library: PathBuf) -> Model {
    let (tx, rx) = watch::channel(state(ModelStatus::Loading, None));
    let providers = Arc::new(OnceCell::new());
    let task = tokio::spawn(run(root, download, library, tx, providers.clone()));
    Model {
        identity: files::identity(),
        state: rx,
        providers,
        task: Some(task),
    }
}

async fn run(
    root: PathBuf,
    download: bool,
    library: PathBuf,
    tx: watch::Sender<ModelState>,
    providers: Arc<OnceCell<Providers>>,
) {
    let mut runtime_ready = false;
    loop {
        let progress = |done: u64, total: u64| {
            if done < total {
                tx.send_modify(|s| {
                    *s = ModelState {
                        bytes_done: Some(done),
                        bytes_total: Some(total),
                        ..state(ModelStatus::Downloading, None)
                    }
                });
            }
        };
        let wait = match files::ensure(&root, download, progress).await {
            Ok(dir) => {
                tx.send_replace(state(ModelStatus::Loading, None));
                let lib = library.clone();
                let init = !runtime_ready;
                let loaded = tokio::task::spawn_blocking(move || {
                    // `init` succeeds once per process; it is not called again after that.
                    if init {
                        runtime::init(&lib).map_err(|e| (false, e))?;
                    }
                    let query = OnnxProvider::load(&dir, Role::Query).map_err(|e| (true, e))?;
                    let document =
                        OnnxProvider::load(&dir, Role::Document).map_err(|e| (true, e))?;
                    Ok((query, document))
                })
                .await;
                match loaded {
                    Ok(Ok((query, document))) => {
                        // The cell is filled before `ready` is sent.
                        let _ = providers.set(Providers {
                            query: Arc::new(query),
                            document: Arc::new(document),
                        });
                        tx.send_replace(state(ModelStatus::Ready, None));
                        tracing::info!(kind = "model_ready", "the embedding model is loaded");
                        return;
                    }
                    Ok(Err((library_loaded, e))) => {
                        runtime_ready |= library_loaded;
                        tracing::warn!(kind = "model_load_failed", error = %e, "the embedding model did not load");
                        unavailable(&tx, ModelUnavailableReason::LoadFailed)
                    }
                    Err(_) => unavailable(&tx, ModelUnavailableReason::LoadFailed),
                }
            }
            Err(e) => {
                tracing::warn!(kind = "model_files", error = %e, "the model files are not ready");
                unavailable(&tx, e.reason())
            }
        };
        tokio::time::sleep(wait).await;
    }
}

fn unavailable(tx: &watch::Sender<ModelState>, reason: ModelUnavailableReason) -> Duration {
    tx.send_replace(state(ModelStatus::Unavailable, Some(reason)));
    if reason == ModelUnavailableReason::HashMismatch {
        RETRY_AFTER_MISMATCH
    } else {
        RETRY
    }
}

/// `knoggin model fetch`: the same check and download as the daemon. Returns the folder.
pub(crate) async fn fetch(
    root: &Path,
    progress: impl FnMut(u64, u64) + Send,
) -> Result<PathBuf, ModelError> {
    files::ensure(root, true, progress).await
}

/// What `knoggin model verify` found.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Verified {
    pub(crate) dir: String,
    pub(crate) graph: String,
    pub(crate) weights: String,
    pub(crate) tokenizer: String,
    pub(crate) library: String,
    #[serde(skip)]
    pub(crate) ok: bool,
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut ctx = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(ctx
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// `knoggin model verify`: hashes every file again, markers or not, then loads the library
/// and the model once. Blocking.
pub(crate) fn verify(root: &Path, library: &Path) -> anyhow::Result<Verified> {
    let dir = files::model_dir(root);
    let check = |f: files::PinnedFile| -> String {
        match sha256_file(&dir.join(f.name)) {
            Ok(h) if h == f.sha256 => "ok".into(),
            Ok(h) => format!("hash mismatch: expected {}, got {h}", f.sha256),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "missing".into(),
            Err(e) => format!("unreadable: {e}"),
        }
    };
    let (graph, weights, tokenizer) = (
        check(files::GRAPH),
        check(files::WEIGHTS),
        check(files::TOKENIZER),
    );
    let files_ok = [&graph, &weights, &tokenizer].iter().all(|s| *s == "ok");
    let library_state = if files_ok {
        match runtime::init(library)
            .and_then(|()| OnnxProvider::load(&dir, Role::Query).map(|_| ()))
        {
            Ok(()) => "loaded".to_owned(),
            Err(e) => format!("did not load from {}: {e}", library.display()),
        }
    } else {
        "not tried".to_owned()
    };
    let ok = files_ok && library_state == "loaded";
    Ok(Verified {
        dir: dir.display().to_string(),
        graph,
        weights,
        tokenizer,
        library: library_state,
        ok,
    })
}

/// The library path, for commands.
pub(crate) fn library_or_hint(ort_dylib: Option<&Path>) -> anyhow::Result<PathBuf> {
    library_path(ort_dylib).context("finding the ONNX Runtime library")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_files_with_downloads_off_are_files_missing() {
        let d = tempfile::tempdir().unwrap();
        let mut m = start(d.path().to_owned(), false, d.path().join("none.dylib"));
        let seen = tokio::time::timeout(
            Duration::from_secs(10),
            m.state.wait_for(|s| s.state == ModelStatus::Unavailable),
        )
        .await
        .unwrap()
        .unwrap()
        .clone();
        assert_eq!(seen.reason, Some(ModelUnavailableReason::FilesMissing));
        assert!(m.providers.get().is_none());
        m.task.take().unwrap().abort();
        // Nothing was written with downloads off.
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn test_mode_is_ready_at_once_with_the_fake_model() {
        let m = fake();
        assert_eq!(m.state.borrow().state, ModelStatus::Ready);
        assert_eq!(m.identity.id, "fake-64");
        assert!(m.providers.get().is_some());
    }

    #[test]
    fn verify_reports_missing_files_without_loading_the_library() {
        let d = tempfile::tempdir().unwrap();
        let v = verify(d.path(), &d.path().join("none")).unwrap();
        assert!(!v.ok);
        assert_eq!(v.graph, "missing");
        assert_eq!(v.library, "not tried");
    }
}
