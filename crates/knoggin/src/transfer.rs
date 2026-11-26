// SPDX-License-Identifier: AGPL-3.0-only
//! Moving stores in and out: `knoggin export`, `knoggin import`, `knoggin import recalld`
//! (tab "Config, CLI, local install", Export file format and Import from recalld) and the
//! recovery tools `knoggin admin snapshot inspect | export` (tab "Snapshots and keys").

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, anyhow};
use axum::body::Body;
use bytes::Bytes;
use http::{Method, Request, header};
use http_body_util::BodyExt;
use knoggin_core::transfer::{OnConflict, TransferRecord};
use knoggin_core::{Store, Timestamp};
use knoggin_crypto::container;
use knoggin_crypto::{KeyProvider, LocalKeyProvider};
use knoggin_import_recalld::Summary;
use serde::Serialize;
use serde_json::Value;

use crate::client::{Client, Failure, api_failure, store_header};
use crate::config::{Config, Home};
use crate::keys;

/// A new file with mode 0600; a path that exists, or is a link, is refused.
pub(crate) fn create_private(path: &Path) -> anyhow::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => anyhow!("{} already exists", path.display()),
            _ => anyhow!("creating {}: {e}", path.display()),
        })
}

fn now() -> Timestamp {
    Timestamp::from_millis((knoggin_state::system_clock())())
}

fn store_headers(
    store: Option<&str>,
) -> Result<Vec<(http::HeaderName, http::HeaderValue)>, Failure> {
    store
        .map(store_header)
        .transpose()
        .map(|h| h.into_iter().collect())
}

/// `knoggin export --out <file>`: streams `GET /v1/export` into a new private file.
/// Returns the bytes written.
pub(crate) async fn export(
    client: &Client,
    store: Option<&str>,
    out: &Path,
) -> anyhow::Result<u64> {
    let mut request = Request::builder().method(Method::GET).uri("/v1/export");
    for (k, v) in store_headers(store)? {
        request = request.header(k, v);
    }
    let response = client
        .send(request.body(Body::empty())?)
        .await
        .map_err(Failure::from)?;
    let status = response.status();
    if !status.is_success() {
        let body = response.into_body().collect().await?.to_bytes();
        return Err(api_failure(
            &serde_json::from_slice(&body).unwrap_or(Value::Null),
            status,
        )
        .into());
    }
    let mut file = create_private(out)?;
    let mut body = response.into_body();
    let mut written = 0u64;
    let result: anyhow::Result<()> = async {
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame?.into_data() {
                file.write_all(&data)?;
                written += data.len() as u64;
            }
        }
        file.sync_all()?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        // A cut-off file must not look like an export.
        let _ = std::fs::remove_file(out);
        return Err(e.context("the export broke off"));
    }
    Ok(written)
}

fn on_conflict_word(c: OnConflict) -> &'static str {
    match c {
        OnConflict::Skip => "skip",
        OnConflict::Error => "error",
    }
}

async fn post_import(
    client: &Client,
    store: Option<&str>,
    on_conflict: OnConflict,
    body: Body,
) -> anyhow::Result<Value> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/v1/import?onConflict={}",
            on_conflict_word(on_conflict)
        ))
        .header(header::CONTENT_TYPE, "application/x-ndjson");
    for (k, v) in store_headers(store)? {
        request = request.header(k, v);
    }
    let response = client
        .send(request.body(body)?)
        .await
        .map_err(Failure::from)?;
    let status = response.status();
    let bytes = response.into_body().collect().await?.to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status.is_success() {
        Ok(value)
    } else {
        Err(api_failure(&value, status).into())
    }
}

/// `knoggin import <file>`: streams the file to `POST /v1/import` and returns its answer.
pub(crate) async fn import_file(
    client: &Client,
    store: Option<&str>,
    on_conflict: OnConflict,
    file: &Path,
) -> anyhow::Result<Value> {
    let f = tokio::fs::File::open(file)
        .await
        .with_context(|| format!("opening {}", file.display()))?;
    let stream = tokio_util::io::ReaderStream::new(f);
    post_import(client, store, on_conflict, Body::from_stream(stream)).await
}

/// `knoggin import recalld <dir>`: reads the stopped recalld directory (copies in
/// `run/import-<random>/`, gone when the reader returns) and feeds the lines to the import
/// route. Returns the counts in the shape of `--json`.
pub(crate) async fn import_recalld(
    client: &Client,
    home: &Home,
    store: Option<&str>,
    dir: &Path,
) -> anyhow::Result<Summary> {
    let (d, run) = (dir.to_owned(), home.run());
    let read = tokio::task::spawn_blocking(move || knoggin_import_recalld::read(&d, &run, now()))
        .await
        .map_err(|_| anyhow!("internal error"))??;
    let mut text = String::new();
    let mut edge_lines = Vec::new();
    for (i, record) in read.records.iter().enumerate() {
        if matches!(record, TransferRecord::Edge(_)) {
            edge_lines.push(i as u64 + 1);
        }
        text.push_str(&serde_json::to_string(record)?);
        text.push('\n');
    }
    let answer = post_import(
        client,
        store,
        OnConflict::Skip,
        Body::from(Bytes::from(text)),
    )
    .await?;
    let error_lines: Vec<u64> = answer
        .get("errors")
        .and_then(Value::as_array)
        .map(|errors| {
            errors
                .iter()
                .filter_map(|e| e.get("line")?.as_u64())
                .collect()
        })
        .unwrap_or_default();
    let mut summary = read.summary;
    // `edges` counts the edge lines the route accepted; its error lines add to `errors`.
    summary.edges = edge_lines
        .iter()
        .filter(|l| !error_lines.contains(l))
        .count() as u64;
    summary.errors += error_lines.len() as u64;
    Ok(summary)
}

/// What `knoggin admin snapshot inspect` prints.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Inspected {
    pub(crate) container_format: u16,
    pub(crate) content_format: u32,
    pub(crate) store_id: String,
    pub(crate) generation: u64,
    pub(crate) sequence: u64,
    pub(crate) created_at: String,
    pub(crate) content_len: u64,
    pub(crate) kek_id: String,
}

/// `knoggin admin snapshot inspect <file>`: the header fields. Needs no key.
pub(crate) fn inspect(file: &Path) -> anyhow::Result<Inspected> {
    let mut f = File::open(file).with_context(|| format!("opening {}", file.display()))?;
    let h = container::read_header_from(&mut f)?;
    Ok(Inspected {
        container_format: h.container_format,
        content_format: h.content_format,
        store_id: h.store_id.to_string(),
        generation: h.generation,
        sequence: h.sequence,
        created_at: Timestamp::from_millis(i64::try_from(h.created_ms).unwrap_or(0)).to_string(),
        content_len: h.content_len,
        kek_id: h.kek_id,
    })
}

/// `knoggin admin snapshot export <file> --out <file>` on a local install: unwraps the
/// header's key with the local key source, decrypts, and writes the store in the export
/// format. Returns the number of lines.
pub(crate) async fn snapshot_export(
    config: &Config,
    home: &Home,
    file: &Path,
    out: &Path,
) -> anyhow::Result<u64> {
    let record = keys::read_record(&config.local.data_dir)?.ok_or_else(|| {
        anyhow!("no master key is recorded on this install; run `knoggin key import` first")
    })?;
    let (c, h) = (config.clone(), home.clone());
    let master = tokio::task::spawn_blocking(move || keys::load(&c, &h, &record))
        .await
        .map_err(|_| anyhow!("internal error"))??;
    let provider = LocalKeyProvider::new(master);
    let bytes = tokio::fs::read(file)
        .await
        .with_context(|| format!("reading {}", file.display()))?;
    let (header, n) = container::read_header(&bytes)?;
    let key = provider
        .unwrap(header.store_id, &header.wrapped_key())
        .await?;
    let rest = bytes
        .get(n..)
        .ok_or_else(|| anyhow!("the file is cut off"))?
        .to_vec();
    let out = out.to_owned();
    tokio::task::spawn_blocking(move || {
        let content = container::open(&header, &key, &rest[..], u64::MAX)?;
        let store = Store::decode(header.content_format, &content)?;
        let mut file = create_private(&out)?;
        let mut lines = 0u64;
        let mut after = None;
        let at = now();
        loop {
            let page = store.export_page(after, 1000, at);
            for record in &page.records {
                file.write_all(serde_json::to_string(record)?.as_bytes())?;
                file.write_all(b"\n")?;
                lines += 1;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        file.sync_all()?;
        Ok::<_, anyhow::Error>(lines)
    })
    .await
    .map_err(|_| anyhow!("internal error"))?
}
