// SPDX-License-Identifier: AGPL-3.0-only
//! Export and import. The file format is that of `knoggin_core::transfer`.
//!
//! An export reads pages of 1,000 memories, each under its own short read lock, and is
//! ended when the client takes less than 256 KiB in any 60 seconds while more is waiting.
//! An import is streamed line by line (at most 4 MiB a line), applied in small batches
//! under the write lock, and ended when no body bytes arrive for 60 seconds.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use http_body_util::BodyExt;
use knoggin_core::transfer::{ExportCursor, ImportOutcome, OnConflict, TransferRecord};
use knoggin_core::{EngineError, Store};
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::{OpenStore, StoreHost, WriteKind, now};
use crate::ctx::RequestContext;
use crate::error::{ApiError, Code};
use crate::settings::IMPORT_LINE_BYTES;

/// Memories per export page, each page under its own read lock.
pub const EXPORT_PAGE: usize = 1000;
/// The slow-client rule: at least this many bytes taken in every window while more waits.
pub const EXPORT_MIN_BYTES: u64 = 256 * 1024;
/// The window of the slow-client rule, and the idle limit of an import.
pub const TRANSFER_WINDOW: Duration = Duration::from_secs(60);
/// The size of the frames an export is cut into, so the slow-client rule sees progress.
const FRAME: usize = 64 * 1024;
/// Lines applied under one hold of the write lock.
const IMPORT_BATCH_LINES: usize = 256;
/// Bytes of lines applied under one hold of the write lock.
const IMPORT_BATCH_BYTES: usize = 1024 * 1024;
/// Error lines kept in an import's answer; the rest are counted in no field.
pub const MAX_IMPORT_ERRORS: usize = 10_000;

/// The body of an export: JSON Lines, streamed. Dropping it stops the producer.
pub struct ExportStream {
    rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
    taken: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for ExportStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportStream")
            .field("taken", &self.taken.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Drop for ExportStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Stream for ExportStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let polled = self.rx.poll_recv(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled {
            self.taken.fetch_add(frame.len() as u64, Ordering::Relaxed);
        }
        polled
    }
}

/// Why sending an export frame stopped.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    /// The client took too little in a window while more was waiting.
    Stalled,
    /// The client is gone.
    Gone,
}

/// One window of the slow-client rule.
struct Window {
    start: Instant,
    taken_at_start: u64,
    waited: bool,
}

/// Sends one frame, ending the export when a window closes with less than 256 KiB taken
/// while the producer had to wait for the client.
async fn pace(
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    frame: Bytes,
    taken: &AtomicU64,
    w: &mut Window,
) -> Result<(), Stop> {
    let mut frame = Some(frame);
    loop {
        let deadline = w.start + TRANSFER_WINDOW;
        let permit = match tx.try_reserve() {
            Ok(permit) => Some(permit),
            Err(mpsc::error::TrySendError::Closed(())) => return Err(Stop::Gone),
            Err(mpsc::error::TrySendError::Full(())) => {
                w.waited = true;
                match tokio::time::timeout_at(deadline, tx.reserve()).await {
                    Ok(Ok(permit)) => Some(permit),
                    Ok(Err(_)) => return Err(Stop::Gone),
                    Err(_) => None,
                }
            }
        };
        if Instant::now() >= deadline {
            let now_taken = taken.load(Ordering::Relaxed);
            if w.waited && now_taken.saturating_sub(w.taken_at_start) < EXPORT_MIN_BYTES {
                return Err(Stop::Stalled);
            }
            *w = Window {
                start: Instant::now(),
                taken_at_start: now_taken,
                waited: false,
            };
        }
        if let Some(permit) = permit {
            if let Some(frame) = frame.take() {
                permit.send(Ok(frame));
            }
            return Ok(());
        }
    }
}

/// The lines of one export page, cut into frames.
fn frames(records: &[TransferRecord]) -> Result<Vec<Bytes>, ApiError> {
    let mut out = Vec::new();
    let mut buf = BytesMut::new();
    for record in records {
        let line = serde_json::to_vec(record).map_err(|_| ApiError::internal())?;
        buf.extend_from_slice(&line);
        buf.extend_from_slice(b"\n");
        while buf.len() >= FRAME {
            out.push(buf.split_to(FRAME).freeze());
        }
    }
    if !buf.is_empty() {
        out.push(buf.freeze());
    }
    Ok(out)
}

async fn produce(
    host: std::sync::Weak<StoreHost>,
    open: Arc<OpenStore>,
    permit: OwnedSemaphorePermit,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    taken: Arc<AtomicU64>,
) {
    let _permit = permit;
    let mut cursor: Option<ExportCursor> = None;
    let mut window = Window {
        start: Instant::now(),
        taken_at_start: 0,
        waited: false,
    };
    loop {
        let Some(host) = host.upgrade() else {
            return;
        };
        let page = host
            .read_on(&open, move |s| {
                Ok(s.export_page(cursor, EXPORT_PAGE, now()))
            })
            .await;
        drop(host);
        // Serializing a page of 1,000 memories is real work: off the runtime, and outside
        // the read lock.
        let page = match page {
            Ok(page) => tokio::task::spawn_blocking(move || {
                Ok::<_, ApiError>((frames(&page.records)?, page.next))
            })
            .await
            .map_err(|e| ApiError::from_join(&e))
            .and_then(|framed| framed),
            Err(e) => Err(e),
        };
        let (frames, next) = match page {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(code = e.code.as_str(), "an export page failed");
                let _ = tx.send(Err(std::io::Error::other("export failed"))).await;
                return;
            }
        };
        for frame in frames {
            match pace(&tx, frame, &taken, &mut window).await {
                Ok(()) => {}
                Err(Stop::Gone) => return,
                Err(Stop::Stalled) => {
                    tracing::info!(kind = "slow_client", "an export was ended");
                    let _ = tx.try_send(Err(std::io::Error::other("slow client")));
                    return;
                }
            }
        }
        match next {
            Some(next) => cursor = Some(next),
            None => return,
        }
    }
}

/// One error line of an import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportError {
    /// The line number, from 1.
    pub line: u64,
    /// Its error code.
    pub code: Code,
    /// For people.
    pub message: String,
}

/// The answer of an import.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct ImportResult {
    /// Lines of every kind that changed the store.
    pub imported: u64,
    /// Lines that changed nothing.
    pub skipped: u64,
    /// Lines that failed a check, in order.
    pub errors: Vec<ImportError>,
}

impl ImportResult {
    fn error(&mut self, line: u64, code: Code, message: String) {
        if self.errors.len() < MAX_IMPORT_ERRORS {
            self.errors.push(ImportError {
                line,
                code,
                message,
            });
        }
    }
}

/// One line of an import body: its number and its bytes, or that it was too long.
enum Line {
    Text(u64, Bytes),
    TooLong(u64),
}

/// Splits a streamed body into lines of at most 4 MiB, ending it after 60 seconds
/// without a byte.
struct Lines {
    body: Body,
    buf: BytesMut,
    /// How much of `buf` is known to hold no newline, so each byte is looked at once.
    scanned: usize,
    number: u64,
    skipping: bool,
    done: bool,
}

impl Lines {
    fn new(body: Body) -> Self {
        Self {
            body,
            buf: BytesMut::new(),
            scanned: 0,
            number: 0,
            skipping: false,
            done: false,
        }
    }

    async fn next(&mut self) -> Result<Option<Line>, ApiError> {
        loop {
            let from = self.scanned;
            self.scanned = self.buf.len();
            if let Some(found) = self.buf[from..].iter().position(|b| *b == b'\n') {
                let at = from + found;
                let mut line = self.buf.split_to(at + 1);
                self.scanned = 0;
                line.truncate(at);
                self.number += 1;
                if std::mem::take(&mut self.skipping) || line.len() > IMPORT_LINE_BYTES {
                    return Ok(Some(Line::TooLong(self.number)));
                }
                return Ok(Some(Line::Text(self.number, line.freeze())));
            }
            if self.buf.len() > IMPORT_LINE_BYTES {
                // Keep no more of an overlong line than it takes to see that it is one.
                self.buf.clear();
                self.scanned = 0;
                self.skipping = true;
            }
            if self.done {
                if self.buf.is_empty() && !self.skipping {
                    return Ok(None);
                }
                self.number += 1;
                let line = std::mem::take(&mut self.buf).freeze();
                if std::mem::take(&mut self.skipping) {
                    return Ok(Some(Line::TooLong(self.number)));
                }
                return Ok(Some(Line::Text(self.number, line)));
            }
            match tokio::time::timeout(TRANSFER_WINDOW, self.body.frame()).await {
                Err(_) => {
                    tracing::info!(kind = "idle", "an import was ended");
                    return Err(ApiError::timeout());
                }
                Ok(None) => self.done = true,
                Ok(Some(Err(_))) => {
                    return Err(ApiError::invalid_argument(
                        "body",
                        "the body could not be read",
                    ));
                }
                Ok(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        self.buf.extend_from_slice(&data);
                    }
                }
            }
        }
    }
}

fn is_blank(line: &[u8]) -> bool {
    line.iter().all(u8::is_ascii_whitespace)
}

/// One parsed line waiting for the write lock.
enum Parsed {
    Record(u64, TransferRecord),
    Error(u64, String),
}

/// Parses one batch of lines (`None`: longer than 4 MiB), on a blocking thread.
async fn parse(batch: Vec<(u64, Option<Bytes>)>) -> Result<Vec<Parsed>, ApiError> {
    tokio::task::spawn_blocking(move || {
        batch
            .into_iter()
            .map(|(number, text)| match text {
                None => Parsed::Error(number, format!("line {number} is longer than 4 MiB")),
                Some(text) => match TransferRecord::from_line(&text) {
                    Ok(record) => Parsed::Record(number, record),
                    Err(e) => Parsed::Error(number, format!("line {number}: {e}")),
                },
            })
            .collect()
    })
    .await
    .map_err(|e| ApiError::from_join(&e))
}

impl StoreHost {
    /// `GET /v1/export`: a stream of JSON Lines. The edge layer names the file.
    pub async fn export(&self, ctx: &RequestContext) -> Result<ExportStream, ApiError> {
        ctx.require_read()?;
        let (open, permit) = self.open_for(ctx).await?;
        let (tx, rx) = mpsc::channel(1);
        let taken = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn(produce(self.me.clone(), open, permit, tx, taken.clone()));
        Ok(ExportStream { rx, taken, task })
    }

    /// `POST /v1/import`: applies the lines of `body` in order, a bad line is reported and
    /// skipped. Not atomic. The first line must be a valid header.
    pub async fn import(
        &self,
        ctx: &RequestContext,
        on_conflict: OnConflict,
        body: Body,
    ) -> Result<ImportResult, ApiError> {
        ctx.require_write()?;
        let (open, _permit) = self.open_for(ctx).await?;
        let mut lines = Lines::new(body);
        let header = loop {
            match lines.next().await? {
                Some(Line::Text(_, text)) if is_blank(&text) => continue,
                Some(Line::Text(_, text)) => break Some(text),
                _ => break None,
            }
        };
        let header_ok = match header {
            Some(text) => matches!(
                parse(vec![(0, Some(text))]).await?.pop(),
                Some(Parsed::Record(_, TransferRecord::Header(h))) if h.check().is_ok()
            ),
            None => false,
        };
        if !header_ok {
            return Err(ApiError::invalid_argument(
                "header",
                "the first line must be a knoggin-export version 1 header",
            ));
        }
        let mut result = ImportResult::default();
        let mut batch = Vec::new();
        let mut batch_bytes = 0;
        loop {
            let line = lines.next().await?;
            let end = line.is_none();
            match line {
                Some(Line::Text(_, text)) if is_blank(&text) => {}
                Some(Line::Text(number, text)) => {
                    batch_bytes += text.len();
                    batch.push((number, Some(text)));
                }
                Some(Line::TooLong(number)) => batch.push((number, None)),
                None => {}
            }
            if !batch.is_empty()
                && (end || batch.len() >= IMPORT_BATCH_LINES || batch_bytes >= IMPORT_BATCH_BYTES)
            {
                let parsed = parse(std::mem::take(&mut batch)).await?;
                self.apply_batch(&open, on_conflict, parsed, &mut result)
                    .await?;
                batch_bytes = 0;
            }
            if end {
                break;
            }
        }
        self.notify_work();
        Ok(result)
    }

    async fn apply_batch(
        &self,
        open: &Arc<OpenStore>,
        on_conflict: OnConflict,
        batch: Vec<Parsed>,
        result: &mut ImportResult,
    ) -> Result<(), ApiError> {
        if open.restoring.load(Ordering::Relaxed) {
            for p in batch {
                let number = match p {
                    Parsed::Record(n, _) | Parsed::Error(n, _) => n,
                };
                result.error(
                    number,
                    Code::Unavailable,
                    "the restored store takes no writes until it has been saved".into(),
                );
            }
            return Ok(());
        }
        if open.upload_blocked.load(Ordering::Relaxed) {
            for p in batch {
                let number = match p {
                    Parsed::Record(n, _) | Parsed::Error(n, _) => n,
                };
                result.error(
                    number,
                    Code::RateLimited,
                    "the store takes no writes until its changes have been saved".into(),
                );
            }
            return Ok(());
        }
        // What this store may still grow to; `None` without a budget.
        let room = self.config.memory_budget.map(|budget| {
            let others = self
                .total_estimated()
                .saturating_sub(open.estimated.load(Ordering::Relaxed));
            if open.read_only.load(Ordering::Relaxed) {
                0
            } else {
                budget.saturating_sub(others)
            }
        });
        let outcomes = self
            .write_on(open, WriteKind::Bookkeeping, move |s: &mut Store| {
                let at = now();
                let mut out = Vec::with_capacity(batch.len());
                for p in batch {
                    out.push(match p {
                        Parsed::Error(n, message) => (n, Err((Code::InvalidArgument, message))),
                        Parsed::Record(n, record) => {
                            let memory = matches!(record, TransferRecord::Memory(_));
                            if memory && room.is_some_and(|r| s.estimated_bytes() >= r) {
                                let e = ApiError::resource_exhausted();
                                (n, Err((e.code, e.message)))
                            } else {
                                let r = s.import_record(record, on_conflict, at);
                                (
                                    n,
                                    r.map_err(|e: EngineError| (e.code().into(), e.to_string())),
                                )
                            }
                        }
                    });
                }
                Ok(out)
            })
            .await?;
        for (number, outcome) in outcomes {
            match outcome {
                Ok(ImportOutcome::Imported) => result.imported += 1,
                Ok(ImportOutcome::Skipped) => result.skipped += 1,
                Err((code, message)) => {
                    if code == Code::ResourceExhausted {
                        open.full.store(true, Ordering::Relaxed);
                    }
                    result.error(number, code, message);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_long_line_sent_in_small_chunks_is_split_in_linear_time() {
        // 1 MiB in 16-byte chunks: looking at the whole buffer again after every chunk
        // would read about 32 GiB.
        let long = vec![b'x'; 1024 * 1024];
        let mut body = long.clone();
        body.extend_from_slice(b"\nshort\n");
        let chunks: Vec<Result<Bytes, std::io::Error>> = body
            .chunks(16)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let mut lines = Lines::new(Body::from_stream(futures_util::stream::iter(chunks)));
        let started = std::time::Instant::now();
        match lines.next().await.unwrap() {
            Some(Line::Text(1, text)) => assert_eq!(text, long),
            _ => panic!("not the long line"),
        }
        match lines.next().await.unwrap() {
            Some(Line::Text(2, text)) => assert_eq!(&text[..], b"short"),
            _ => panic!("not the short line"),
        }
        assert!(lines.next().await.unwrap().is_none());
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "{took:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_takes_nothing_is_cut_off_after_a_window() {
        let (tx, mut rx) = mpsc::channel(1);
        let taken = AtomicU64::new(0);
        let mut w = Window {
            start: Instant::now(),
            taken_at_start: 0,
            waited: false,
        };
        pace(&tx, Bytes::from_static(b"a"), &taken, &mut w)
            .await
            .unwrap();
        let start = Instant::now();
        let stop = pace(&tx, Bytes::from_static(b"b"), &taken, &mut w).await;
        assert_eq!(stop, Err(Stop::Stalled));
        assert_eq!(start.elapsed(), TRANSFER_WINDOW);
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_keeps_up_is_not_cut_off() {
        let (tx, mut rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
        let taken = Arc::new(AtomicU64::new(0));
        let t = taken.clone();
        let reader = tokio::spawn(async move {
            while let Some(Ok(frame)) = rx.recv().await {
                t.fetch_add(frame.len() as u64, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let mut w = Window {
            start: Instant::now(),
            taken_at_start: 0,
            waited: false,
        };
        // 64 KiB a second is far more than 256 KiB a minute.
        for _ in 0..200 {
            let frame = Bytes::from(vec![b'x'; FRAME]);
            pace(&tx, frame, &taken, &mut w).await.unwrap();
        }
        drop(tx);
        reader.await.unwrap();
    }
}
