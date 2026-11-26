// SPDX-License-Identifier: AGPL-3.0-only
//! The tasks around open stores: each store's snapshot timers, sweep and idle close, the
//! snapshot itself, and the one embedding worker of the process.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

use knoggin_core::snapshot::{SnapshotName, WRITE_CONTENT_FORMAT};
use knoggin_crypto::container::{self, Header};
use tokio::time::Instant;

use super::embedder::QueueOutcome;
use super::live;
use super::{
    BackendError, OpenStore, SWEEP_EVERY, SealedSnapshot, SnapshotTiming, StoreHost, WriteKind,
    lock, now,
};

/// The longest wait between two tries of a failed snapshot.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How often a store that is idle but not yet closable is looked at again.
const IDLE_RECHECK: Duration = Duration::from_secs(60);

/// When the timers of `open` next want a snapshot.
fn timer_deadline(open: &OpenStore, timing: SnapshotTiming) -> Option<Instant> {
    let t = lock(&open.timers);
    let content = match (t.content_first, t.content_last) {
        (Some(first), Some(last)) => Some((last + timing.quiet).min(first + timing.longest)),
        _ => None,
    };
    let bookkeeping = t.bookkeeping_first.map(|first| first + timing.bookkeeping);
    match (content, bookkeeping) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The task of one open store: snapshots on its timers with back-off on failure, the
/// sweep every 24 hours, and the idle close. Ends when the store is dropped.
pub(super) async fn store_task(host: Weak<StoreHost>, open: Arc<OpenStore>) {
    let Some((timing, idle, allowance)) = host.upgrade().map(|h| {
        (
            h.config.timing,
            h.config.idle_close,
            h.config.embed_allowance,
        )
    }) else {
        return;
    };
    let mut next_sweep = Instant::now() + SWEEP_EVERY;
    let mut retry: Option<(Instant, Duration)> = None;
    let mut idle_not_before = Instant::now();
    loop {
        let now = Instant::now();
        let timer = timer_deadline(&open, timing);
        let mut wake = next_sweep;
        // While a failed snapshot waits for its retry, new changes do not cut the wait.
        if let Some(at) = retry.map(|r| r.0).or(timer) {
            wake = wake.min(at);
        }
        if let Some(idle) = idle {
            let at = (*lock(&open.last_request) + idle).max(idle_not_before);
            wake = wake.min(if at > now { at } else { now + IDLE_RECHECK });
        }
        tokio::select! {
            () = tokio::time::sleep_until(wake) => {}
            () = open.wake.notified() => {}
        }
        if open.dropped.load(Ordering::SeqCst) {
            return;
        }
        let Some(host) = host.upgrade() else {
            return;
        };
        let now = Instant::now();
        if now >= next_sweep {
            sweep(&host, &open).await;
            next_sweep = now + SWEEP_EVERY;
        }
        let due = match retry {
            Some((at, _)) => now >= at,
            None => timer_deadline(&open, timing).is_some_and(|t| now >= t),
        };
        if due {
            *lock(&open.timers) = super::Timers::default();
            match snapshot(&host, &open).await {
                Ok(()) => retry = None,
                Err(BackendError::LeaseLost | BackendError::Deleted) => return,
                Err(_) => {
                    let backoff =
                        retry.map_or(Duration::from_secs(1), |r| (r.1 * 2).min(MAX_BACKOFF));
                    retry = Some((Instant::now() + backoff, backoff));
                }
            }
        }
        if let Some(idle) = idle {
            let last = *lock(&open.last_request);
            let quiet = Instant::now() >= last + idle && Instant::now() >= idle_not_before;
            let busy = open.requests.available_permits() < open.concurrency as usize;
            let queue_done = open.pending.load(Ordering::Relaxed) == 0
                || allowance.is_some_and(|a| Instant::now() >= last + a);
            if quiet && !busy && queue_done {
                match host.close_open(&open).await {
                    Ok(()) => return,
                    Err(e) => {
                        tracing::warn!(store_id = %open.id, kind = e.kind(), "closing an idle store failed");
                        idle_not_before = Instant::now() + IDLE_RECHECK;
                    }
                }
            }
        }
    }
}

async fn sweep(host: &StoreHost, open: &Arc<OpenStore>) {
    let result = host
        .write_on(open, WriteKind::Bookkeeping, |s| {
            let at = now();
            let report = s.sweep(at);
            let events = live::swept(s, &report, at);
            Ok((report, events, at))
        })
        .await;
    match result {
        Ok((report, events, at)) => {
            *lock(&open.last_sweep) = at;
            open.events.publish(events);
            if report.deleted > 0 || report.summary_to_ghost > 0 {
                host.recheck_budget();
            }
        }
        Err(e) => tracing::warn!(store_id = %open.id, code = e.code.as_str(), "the sweep failed"),
    }
}

/// Takes one snapshot of `open` if it has unsaved changes (rules 2 to 7 of "When
/// snapshots are taken"). One runs at a time per store. A store that was dropped is never
/// saved.
pub(super) async fn snapshot(host: &StoreHost, open: &Arc<OpenStore>) -> Result<(), BackendError> {
    let mut state = open.save.lock().await;
    loop {
        if open.dropped.load(Ordering::SeqCst) {
            return Ok(());
        }
        let name = SnapshotName::new(open.generation, state.next_sequence)
            .map_err(|_| BackendError::Unavailable)?;
        let store = open.store.clone();
        let key = open.key.clone();
        let wrapped = open.wrapped.clone();
        let (id, saved) = (open.id, state.saved);
        let started = std::time::Instant::now();
        let prepared = tokio::task::spawn_blocking(move || {
            let guard = store.read().map_err(|_| ())?;
            let counters = guard.changes();
            if counters == saved {
                return Ok(None);
            }
            // The one plaintext copy; the sealer owns it and wipes it after the last chunk.
            let content = guard.encode();
            let summary = guard.summary();
            drop(guard);
            let created_ms = u64::try_from(now().millis()).unwrap_or(0);
            let header = Header::new(
                id,
                name.generation(),
                name.sequence(),
                created_ms,
                WRITE_CONTENT_FORMAT,
                wrapped,
            );
            let len = container::sealed_len(&header, content.len() as u64);
            Ok(Some((
                counters,
                summary,
                container::seal(header, key, content),
                len,
            )))
        })
        .await;
        let (counters, summary, chunks, len) = match prepared {
            Ok(Ok(Some(p))) => p,
            Ok(Ok(None)) => return Ok(()),
            // A poisoned lock or a panic: the store may be half changed. Never save it.
            Ok(Err(())) | Err(_) => {
                tracing::error!(store_id = %id, kind = "poisoned", "a store could not be encoded");
                host.drop_open(open);
                return Ok(());
            }
        };
        let snap = SealedSnapshot {
            name,
            chunks,
            len,
            memory_count: summary.memory_count,
            namespace_count: summary.namespace_count,
            last_write_at: summary.last_write_at,
            last_write_client: summary.last_write_client,
            restored: state.restored,
        };
        match host.backend.save(id, snap).await {
            Ok(()) => {
                state.saved = counters;
                *lock(&open.saved) = counters;
                open.snapshot_failed.store(false, Ordering::Relaxed);
                *lock(&open.snapshot_bytes) = Some(len);
                state.next_sequence += 1;
                state.restored = None;
                open.last_sequence.store(name.sequence(), Ordering::Relaxed);
                open.upload_blocked.store(false, Ordering::Relaxed);
                let elapsed = started.elapsed();
                let c = &host.counters;
                c.snapshot_count.fetch_add(1, Ordering::Relaxed);
                c.snapshot_millis
                    .fetch_add(elapsed.as_millis() as u64, Ordering::Relaxed);
                c.snapshot_bytes.fetch_add(len, Ordering::Relaxed);
                metrics::histogram!("knoggin_snapshot_seconds").record(elapsed.as_secs_f64());
                metrics::histogram!("knoggin_snapshot_bytes").record(len as f64);
                return Ok(());
            }
            // Rule 6: take the next sequence at once.
            Err(BackendError::NameTaken) => state.next_sequence += 1,
            Err(e) => {
                open.snapshot_failed.store(true, Ordering::Relaxed);
                host.counters
                    .snapshot_failures
                    .fetch_add(1, Ordering::Relaxed);
                metrics::counter!("knoggin_snapshot_failures_total").increment(1);
                tracing::warn!(store_id = %id, kind = e.kind(), "a snapshot failed");
                match e {
                    BackendError::LeaseLost => host.on_lease_lost(),
                    BackendError::Deleted => host.drop_open(open),
                    BackendError::OverBudget => {
                        open.upload_blocked.store(true, Ordering::Relaxed);
                        metrics::counter!("knoggin_upload_budget_refused_total").increment(1);
                    }
                    _ => {}
                }
                return Err(e);
            }
        }
    }
}

/// The embedding worker: one per process. Takes the next job of the open stores in
/// turn, embeds it on the document thread and applies the vector under the write lock.
pub(super) async fn embed_worker(host: Weak<StoreHost>) {
    let mut cursor = 0_usize;
    loop {
        let Some(h) = host.upgrade() else {
            return;
        };
        let ready = h
            .embedder
            .ready_identity()
            .filter(|identity| *identity == h.config.model);
        let Some(model) = ready else {
            let mut changes = h.embedder.subscribe();
            drop(h);
            tokio::select! {
                r = changes.changed() => if r.is_err() {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                },
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            continue;
        };
        let stores = h.open_list();
        let mut found = None;
        for i in 0..stores.len() {
            let open = &stores[(cursor + i) % stores.len()];
            if open.pending.load(Ordering::Relaxed) == 0 {
                continue;
            }
            let job = h
                .read_on(open, |s| Ok(s.pending_embeddings(1).into_iter().next()))
                .await;
            if let Ok(Some(job)) = job {
                cursor = (cursor + i + 1) % stores.len();
                found = Some((open.clone(), job));
                break;
            }
        }
        let Some((open, job)) = found else {
            // An idle wait holds no open store: a closed store must go at once (its event
            // streams end when it does).
            drop(stores);
            h.idle_waits.fetch_add(1, Ordering::Relaxed);
            let mut changes = h.embedder.subscribe();
            // A closed channel means the model state can no longer change: wait for work.
            let model_changed = async move {
                if changes.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                () = h.work.notified() => {}
                () = model_changed => {}
                () = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
            continue;
        };
        let started = std::time::Instant::now();
        let id = job.id;
        let outcome = h.embedder.queue(job.text).await;
        let result = match outcome {
            QueueOutcome::Vector(vector) => {
                h.counters.embed_count.fetch_add(1, Ordering::Relaxed);
                h.counters
                    .embed_millis
                    .fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                h.embed_ms.note(started.elapsed());
                let applied = h
                    .write_on(&open, WriteKind::Bookkeeping, move |s| {
                        let at = now();
                        let outcome = s.apply_embedding(id, &model, vector, at);
                        Ok(live::linked(s, id, &outcome, at))
                    })
                    .await;
                applied.map(|events| {
                    let some = events.is_some();
                    open.events.publish(events.unwrap_or_default());
                    some
                })
            }
            QueueOutcome::Failed => {
                h.write_on(&open, WriteKind::Bookkeeping, move |s| {
                    s.fail_embedding(id);
                    Ok(false)
                })
                .await
            }
            QueueOutcome::NotReady => {
                // The document thread is gone or the model went away: do not spin.
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(false)
            }
        };
        if let Err(e) = result {
            tracing::warn!(store_id = %open.id, code = e.code.as_str(), "an embedding could not be applied");
        }
    }
}
