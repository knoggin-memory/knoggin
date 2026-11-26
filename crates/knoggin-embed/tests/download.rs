// SPDX-License-Identifier: AGPL-3.0-only
//! `model::ensure_from` against a local HTTP server.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use knoggin_core::ops::ModelUnavailableReason;
use knoggin_embed::model::{ModelSource, SourceFile, ensure_from, model_dir};
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SMALL: &[u8] = b"the tokenizer file";
const LARGE: &[u8] = b"the weights of the model, a little longer";

fn sha256(bytes: &[u8]) -> String {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn file(name: &str, content: &[u8], urls: Vec<String>) -> SourceFile {
    SourceFile {
        name: name.to_owned(),
        size: content.len() as u64,
        sha256: sha256(content),
        urls,
    }
}

fn source(files: Vec<SourceFile>) -> ModelSource {
    ModelSource {
        files,
        free_space: |_| Ok(u64::MAX),
        backoff: [Duration::from_millis(1); 3],
    }
}

async fn serve(server: &MockServer, at: &str, body: &[u8]) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
        .mount(server)
        .await;
}

fn requests_to(requests: &[wiremock::Request], at: &str) -> usize {
    requests.iter().filter(|r| r.url.path() == at).count()
}

/// Calls of the progress callback.
type Log = Arc<Mutex<Vec<(u64, u64)>>>;

fn progress(log: &Log) -> impl FnMut(u64, u64) + Send + use<> {
    let log = Arc::clone(log);
    move |done, total| log.lock().unwrap().push((done, total))
}

fn ignore(_: u64, _: u64) {}

#[tokio::test]
async fn clean_download_writes_files_and_markers() {
    let server = MockServer::start().await;
    serve(&server, "/small", SMALL).await;
    serve(&server, "/large", LARGE).await;
    let root = tempfile::tempdir().unwrap();
    let src = source(vec![
        file("large.bin", LARGE, vec![format!("{}/large", server.uri())]),
        file("small.bin", SMALL, vec![format!("{}/small", server.uri())]),
    ]);
    let log = Log::default();

    let dir = ensure_from(root.path(), true, progress(&log), &src)
        .await
        .unwrap();

    assert_eq!(dir, model_dir(root.path()));
    assert_eq!(fs::read(dir.join("small.bin")).unwrap(), SMALL);
    assert_eq!(fs::read(dir.join("large.bin")).unwrap(), LARGE);
    assert!(dir.join("small.bin.ok").is_file());
    assert!(dir.join("large.bin.ok").is_file());
    assert!(!dir.join("large.bin.part").exists());
    let log = log.lock().unwrap().clone();
    let total = (SMALL.len() + LARGE.len()) as u64;
    assert!(log.iter().all(|&(_, t)| t == total));
    assert!(log.windows(2).all(|w| w[0].0 <= w[1].0));
    assert_eq!(log.last(), Some(&(total, total)));
    // Smallest first.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].url.path(), "/small");

    // A second run accepts the files by their markers and downloads nothing.
    ensure_from(root.path(), true, ignore, &src).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_wrong_file_on_disk_is_replaced() {
    let server = MockServer::start().await;
    serve(&server, "/small", SMALL).await;
    let root = tempfile::tempdir().unwrap();
    let dir = model_dir(root.path());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("small.bin"), vec![b'x'; SMALL.len()]).unwrap();
    let src = source(vec![file(
        "small.bin",
        SMALL,
        vec![format!("{}/small", server.uri())],
    )]);

    ensure_from(root.path(), true, ignore, &src).await.unwrap();

    assert_eq!(fs::read(dir.join("small.bin")).unwrap(), SMALL);
}

#[tokio::test]
async fn resumes_from_a_partial_file() {
    let server = MockServer::start().await;
    let len = LARGE.len();
    Mock::given(method("GET"))
        .and(path("/large"))
        .and(header("range", "bytes=10-"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", format!("bytes 10-{}/{len}", len - 1))
                .set_body_bytes(LARGE[10..].to_vec()),
        )
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let dir = model_dir(root.path());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("large.bin.part"), &LARGE[..10]).unwrap();
    let src = source(vec![file(
        "large.bin",
        LARGE,
        vec![format!("{}/large", server.uri())],
    )]);
    let log = Log::default();

    ensure_from(root.path(), true, progress(&log), &src)
        .await
        .unwrap();

    assert_eq!(fs::read(dir.join("large.bin")).unwrap(), LARGE);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    // Progress starts from the bytes already there.
    assert!(log.lock().unwrap().contains(&(10, len as u64)));
}

#[tokio::test]
async fn a_partial_answer_with_the_wrong_total_moves_to_the_next_source() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/first"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 10-99/100")
                .set_body_bytes(vec![0; 90]),
        )
        .mount(&server)
        .await;
    serve(&server, "/second", LARGE).await;
    let root = tempfile::tempdir().unwrap();
    let dir = model_dir(root.path());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("large.bin.part"), &LARGE[..10]).unwrap();
    let src = source(vec![file(
        "large.bin",
        LARGE,
        vec![
            format!("{}/first", server.uri()),
            format!("{}/second", server.uri()),
        ],
    )]);

    ensure_from(root.path(), true, ignore, &src).await.unwrap();

    assert_eq!(fs::read(dir.join("large.bin")).unwrap(), LARGE);
}

#[tokio::test]
async fn a_wrong_hash_moves_to_the_second_source() {
    let server = MockServer::start().await;
    serve(&server, "/bad", &vec![b'x'; SMALL.len()]).await;
    serve(&server, "/good", SMALL).await;
    let root = tempfile::tempdir().unwrap();
    let src = source(vec![file(
        "small.bin",
        SMALL,
        vec![
            format!("{}/bad", server.uri()),
            format!("{}/good", server.uri()),
        ],
    )]);

    let dir = ensure_from(root.path(), true, ignore, &src).await.unwrap();

    assert_eq!(fs::read(dir.join("small.bin")).unwrap(), SMALL);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests_to(&requests, "/bad"), 1);
    assert_eq!(requests_to(&requests, "/good"), 1);
    // The bad copy was deleted before the second source started over.
    assert!(requests.iter().all(|r| !r.headers.contains_key("range")));
}

#[tokio::test]
async fn a_wrong_hash_at_every_source_is_a_hash_mismatch() {
    let server = MockServer::start().await;
    let wrong = vec![b'x'; SMALL.len()];
    serve(&server, "/bad", &wrong).await;
    let root = tempfile::tempdir().unwrap();
    let src = source(vec![file(
        "small.bin",
        SMALL,
        vec![
            format!("{}/bad", server.uri()),
            format!("{}/bad", server.uri()),
        ],
    )]);

    let err = ensure_from(root.path(), true, ignore, &src)
        .await
        .unwrap_err();

    assert_eq!(err.reason(), ModelUnavailableReason::HashMismatch);
    let text = err.to_string();
    assert!(text.contains(&sha256(SMALL)) && text.contains(&sha256(&wrong)));
    let dir = model_dir(root.path());
    assert!(!dir.join("small.bin").exists());
    assert!(!dir.join("small.bin.part").exists());
}

#[tokio::test]
async fn too_many_requests_backs_off_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/busy"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    serve(&server, "/busy", SMALL).await;
    let root = tempfile::tempdir().unwrap();
    let src = source(vec![file(
        "small.bin",
        SMALL,
        vec![format!("{}/busy", server.uri())],
    )]);

    let dir = ensure_from(root.path(), true, ignore, &src).await.unwrap();

    assert_eq!(fs::read(dir.join("small.bin")).unwrap(), SMALL);
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn too_many_requests_three_times_moves_to_the_next_source() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/busy"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    serve(&server, "/next", SMALL).await;
    let root = tempfile::tempdir().unwrap();
    let busy = format!("{}/busy", server.uri());
    let src = source(vec![file(
        "small.bin",
        SMALL,
        vec![busy.clone(), format!("{}/next", server.uri())],
    )]);

    ensure_from(root.path(), true, ignore, &src).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    // The first try and three retries.
    assert_eq!(requests_to(&requests, "/busy"), 4);
    assert_eq!(requests_to(&requests, "/next"), 1);

    // With no source left the error says offline.
    let root = tempfile::tempdir().unwrap();
    let src = source(vec![file("small.bin", SMALL, vec![busy])]);
    let err = ensure_from(root.path(), true, ignore, &src)
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ModelUnavailableReason::Offline);
    assert!(err.to_string().contains("small.bin"));
}

#[tokio::test]
async fn no_free_space_fails_before_any_request() {
    let server = MockServer::start().await;
    serve(&server, "/small", SMALL).await;
    let root = tempfile::tempdir().unwrap();
    let mut src = source(vec![file(
        "small.bin",
        SMALL,
        vec![format!("{}/small", server.uri())],
    )]);
    src.free_space = |_| Ok(1_000);
    let dir = model_dir(root.path());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("small.bin.part"), &SMALL[..4]).unwrap();

    let err = ensure_from(root.path(), true, ignore, &src)
        .await
        .unwrap_err();

    assert_eq!(err.reason(), ModelUnavailableReason::DiskFull);
    assert!(err.to_string().contains("250000000 bytes needed"));
    assert!(server.received_requests().await.unwrap().is_empty());
    // The partial file stays for a later resume.
    assert_eq!(fs::read(dir.join("small.bin.part")).unwrap(), &SMALL[..4]);
}

#[tokio::test]
async fn downloads_off_with_a_missing_file_writes_nothing() {
    let server = MockServer::start().await;
    serve(&server, "/small", SMALL).await;
    let root = tempfile::tempdir().unwrap();
    let src = source(vec![file(
        "small.bin",
        SMALL,
        vec![format!("{}/small", server.uri())],
    )]);

    let err = ensure_from(root.path(), false, ignore, &src)
        .await
        .unwrap_err();

    assert_eq!(err.reason(), ModelUnavailableReason::FilesMissing);
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn downloads_off_with_a_wrong_file_keeps_it_and_reports_the_hashes() {
    let root = tempfile::tempdir().unwrap();
    let dir = model_dir(root.path());
    fs::create_dir_all(&dir).unwrap();
    let wrong = vec![b'x'; SMALL.len()];
    fs::write(dir.join("small.bin"), &wrong).unwrap();
    let src = source(vec![file(
        "small.bin",
        SMALL,
        vec!["http://127.0.0.1:9/never".to_owned()],
    )]);

    let err = ensure_from(root.path(), false, ignore, &src)
        .await
        .unwrap_err();

    assert_eq!(err.reason(), ModelUnavailableReason::HashMismatch);
    assert!(err.to_string().contains(&sha256(&wrong)));
    assert_eq!(fs::read(dir.join("small.bin")).unwrap(), wrong);
    assert!(!dir.join("small.bin.ok").exists());
}

#[tokio::test]
async fn downloads_off_with_right_files_hashes_them_without_writing_markers() {
    let root = tempfile::tempdir().unwrap();
    let dir = model_dir(root.path());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("small.bin"), SMALL).unwrap();
    let src = source(vec![file("small.bin", SMALL, vec![])]);

    let found = ensure_from(root.path(), false, ignore, &src).await.unwrap();

    assert_eq!(found, dir);
    assert!(!dir.join("small.bin.ok").exists());
}

/// A file of the right size but the wrong bytes, with a marker beside it.
fn marked_wrong_file(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("small.bin"), vec![b'x'; SMALL.len()]).unwrap();
    fs::write(dir.join("small.bin.ok"), b"").unwrap();
}

#[tokio::test]
async fn a_marker_is_believed_only_in_a_folder_nobody_else_can_write() {
    let src = source(vec![file("small.bin", SMALL, vec![])]);

    // Our own folder, writable by us only: the marker stands for the hash.
    let root = tempfile::tempdir().unwrap();
    let dir = model_dir(root.path());
    marked_wrong_file(&dir);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    ensure_from(root.path(), false, ignore, &src).await.unwrap();

    // A folder that others can write to: the file is hashed and fails.
    let root = tempfile::tempdir().unwrap();
    let dir = model_dir(root.path());
    marked_wrong_file(&dir);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    let err = ensure_from(root.path(), false, ignore, &src)
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ModelUnavailableReason::HashMismatch);

    // The same when only the models root is writable by others.
    let root = tempfile::tempdir().unwrap();
    let dir = model_dir(root.path());
    marked_wrong_file(&dir);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let err = ensure_from(root.path(), false, ignore, &src)
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ModelUnavailableReason::HashMismatch);
}

#[tokio::test]
async fn a_symlink_is_not_accepted() {
    let root = tempfile::tempdir().unwrap();
    let dir = model_dir(root.path());
    fs::create_dir_all(&dir).unwrap();
    fs::write(root.path().join("elsewhere"), SMALL).unwrap();
    std::os::unix::fs::symlink(root.path().join("elsewhere"), dir.join("small.bin")).unwrap();
    let src = source(vec![file("small.bin", SMALL, vec![])]);

    let err = ensure_from(root.path(), false, ignore, &src)
        .await
        .unwrap_err();

    assert_eq!(err.reason(), ModelUnavailableReason::HashMismatch);
}

#[tokio::test]
async fn a_fresh_download_sends_no_range_header() {
    let server = MockServer::start().await;
    Mock::given(header_exists("range"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&server)
        .await;
    serve(&server, "/small", SMALL).await;
    let root = tempfile::tempdir().unwrap();
    let src = source(vec![file(
        "small.bin",
        SMALL,
        vec![format!("{}/small", server.uri())],
    )]);
    ensure_from(root.path(), true, ignore, &src).await.unwrap();
}
