// SPDX-License-Identifier: AGPL-3.0-only
//! What the web UI reads live from an open store: the events of each open store, kept in
//! memory and sent to the event streams, and the latencies that `GET /v1/ui/system`
//! reports. Events hold memory IDs and client names, never text or a query, and names in
//! place of numbers.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use knoggin_core::model::{EdgeType, Phase};
use knoggin_core::ops::{ApplyOutcome, RecallTrace, SweepReport};
use knoggin_core::{MemoryId, NamespaceId, Store, Timestamp};
use serde::Serialize;
use tokio::sync::broadcast;

use super::lock;

/// Events an open store keeps, whether or not anyone is subscribed.
pub(crate) const KEPT: usize = 1000;
/// Events a slow subscriber may fall behind before it gets `lagged`.
const CHANNEL: usize = 1024;

/// One live event; `kind` tags it on the wire and names the server-sent event.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum UiEvent {
    Recall(RecallEvent),
    Store(StoreEvent),
    Link(LinkEvent),
    Reinforce(ReinforceEvent),
    Forget(ForgetEvent),
    PhaseChange(PhaseChangeEvent),
    Sweep(SweepEvent),
    /// A slow reader missed this many events.
    Lagged(LaggedEvent),
}

/// A recall, built from the engine's trace. It holds no query text.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecallEvent {
    pub(crate) seq: u64,
    /// Milliseconds since 1970.
    pub(crate) at: i64,
    pub(crate) client: Option<String>,
    // The namespace's name as stored, whatever the request's spelling, and the namespace
    // itself for `GET /v1/ui/activity`'s scope, which is not sent.
    pub(crate) namespace: String,
    #[serde(skip)]
    pub(crate) namespace_id: NamespaceId,
    pub(crate) seeds: Vec<Seed>,
    pub(crate) spreads: Vec<Spread>,
    pub(crate) returned: Vec<MemoryId>,
    pub(crate) suppressed: Vec<MemoryId>,
}

/// A seed of a recall's graph expansion.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
pub(crate) struct Seed {
    pub(crate) id: MemoryId,
    pub(crate) score: f32,
}

/// One spread of activation, in firing order.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Spread {
    pub(crate) from: MemoryId,
    pub(crate) to: MemoryId,
    pub(crate) edge_type: EdgeType,
    pub(crate) amount: f32,
    pub(crate) order: u32,
}

/// A memory was saved, with the links made at save time.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoreEvent {
    pub(crate) seq: u64,
    pub(crate) at: i64,
    pub(crate) client: Option<String>,
    pub(crate) id: MemoryId,
    pub(crate) namespace: String,
    pub(crate) phase: Phase,
    pub(crate) edges: Vec<EdgeRef>,
}

/// One link of a saved memory.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EdgeRef {
    pub(crate) to: MemoryId,
    pub(crate) edge_type: EdgeType,
    pub(crate) weight: f32,
}

/// A link made later, when a vector landed.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LinkEvent {
    pub(crate) seq: u64,
    pub(crate) at: i64,
    pub(crate) client: Option<String>,
    pub(crate) src: MemoryId,
    pub(crate) dst: MemoryId,
    pub(crate) edge_type: EdgeType,
    pub(crate) weight: f32,
}

/// A reinforcement.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReinforceEvent {
    pub(crate) seq: u64,
    pub(crate) at: i64,
    pub(crate) client: Option<String>,
    pub(crate) id: MemoryId,
    pub(crate) rating: u8,
    pub(crate) stability_before: Option<f32>,
    pub(crate) stability_after: f32,
}

/// A memory was removed.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForgetEvent {
    pub(crate) seq: u64,
    pub(crate) at: i64,
    pub(crate) client: Option<String>,
    pub(crate) id: MemoryId,
}

/// A sweep moved a memory one phase down.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhaseChangeEvent {
    pub(crate) seq: u64,
    pub(crate) at: i64,
    pub(crate) id: MemoryId,
    pub(crate) from: Phase,
    pub(crate) to: Phase,
}

/// A sweep ran.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SweepEvent {
    pub(crate) seq: u64,
    pub(crate) at: i64,
    pub(crate) moved: SweepMoved,
}

/// What a sweep moved.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SweepMoved {
    pub(crate) full_to_summary: u64,
    pub(crate) summary_to_ghost: u64,
    pub(crate) ghost_to_deleted: u64,
}

/// `{ kind: "lagged", missed }`.
#[derive(Clone, Serialize, ts_rs::TS, utoipa::ToSchema)]
pub(crate) struct LaggedEvent {
    pub(crate) missed: u64,
}

impl UiEvent {
    /// The server-sent event's name.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Recall(_) => "recall",
            Self::Store(_) => "store",
            Self::Link(_) => "link",
            Self::Reinforce(_) => "reinforce",
            Self::Forget(_) => "forget",
            Self::PhaseChange(_) => "phase_change",
            Self::Sweep(_) => "sweep",
            Self::Lagged(_) => "lagged",
        }
    }

    /// Milliseconds since 1970; 0 for `lagged`.
    pub(crate) fn at(&self) -> i64 {
        match self {
            Self::Recall(e) => e.at,
            Self::Store(e) => e.at,
            Self::Link(e) => e.at,
            Self::Reinforce(e) => e.at,
            Self::Forget(e) => e.at,
            Self::PhaseChange(e) => e.at,
            Self::Sweep(e) => e.at,
            Self::Lagged(_) => 0,
        }
    }

    fn set_seq(&mut self, seq: u64) {
        match self {
            Self::Recall(e) => e.seq = seq,
            Self::Store(e) => e.seq = seq,
            Self::Link(e) => e.seq = seq,
            Self::Reinforce(e) => e.seq = seq,
            Self::Forget(e) => e.seq = seq,
            Self::PhaseChange(e) => e.seq = seq,
            Self::Sweep(e) => e.seq = seq,
            Self::Lagged(_) => {}
        }
    }
}

/// The events of one open store: the last [`KEPT`], and a channel to the streams.
pub(crate) struct EventLog {
    recent: Mutex<VecDeque<Arc<UiEvent>>>,
    tx: broadcast::Sender<Arc<UiEvent>>,
    seq: AtomicU64,
}

impl EventLog {
    pub(crate) fn new() -> Self {
        Self {
            recent: Mutex::new(VecDeque::with_capacity(KEPT)),
            tx: broadcast::channel(CHANNEL).0,
            seq: AtomicU64::new(1),
        }
    }

    /// Numbers, keeps and sends `events`.
    pub(crate) fn publish(&self, events: Vec<UiEvent>) {
        if events.is_empty() {
            return;
        }
        let mut recent = lock(&self.recent);
        for mut event in events {
            event.set_seq(self.seq.fetch_add(1, Ordering::Relaxed));
            let event = Arc::new(event);
            if recent.len() == KEPT {
                recent.pop_front();
            }
            recent.push_back(event.clone());
            // No subscriber is not an error.
            let _ = self.tx.send(event);
        }
    }

    /// The kept events, oldest first.
    pub(crate) fn recent(&self) -> Vec<Arc<UiEvent>> {
        lock(&self.recent).iter().cloned().collect()
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Arc<UiEvent>> {
        self.tx.subscribe()
    }
}

/// The `store` event of a memory just saved, with its edges as they are now.
pub(crate) fn stored(s: &Store, id: MemoryId, client: Option<&str>, at: Timestamp) -> UiEvent {
    let memory = s.memory(id);
    let edges = s
        .graph()
        .edges_of(id)
        .map(|e| EdgeRef {
            to: if e.source == id { e.target } else { e.source },
            edge_type: e.edge_type,
            weight: e.weight,
        })
        .collect();
    UiEvent::Store(StoreEvent {
        seq: 0,
        at: at.millis(),
        client: client.map(str::to_owned),
        id,
        namespace: memory
            .and_then(|m| s.namespace(m.namespace))
            .map(|n| n.name.clone())
            .unwrap_or_default(),
        phase: memory.map_or(Phase::Full, |m| m.phase),
        edges,
    })
}

/// A `forget` event.
pub(crate) fn forgotten(id: MemoryId, client: Option<&str>, at: Timestamp) -> UiEvent {
    UiEvent::Forget(ForgetEvent {
        seq: 0,
        at: at.millis(),
        client: client.map(str::to_owned),
        id,
    })
}

/// The `recall` event of a trace, cut down as recalld's light shows it: the strongest
/// seeds, at most 500 spreads and 24 suppressed memories that were not returned.
pub(crate) fn recalled(
    trace: &RecallTrace,
    (namespace_id, namespace): (NamespaceId, String),
    client: Option<&str>,
    at: Timestamp,
) -> UiEvent {
    let mut seeds: Vec<Seed> = Vec::new();
    for seed in &trace.seeds {
        match seeds.iter_mut().find(|s| s.id == seed.id) {
            Some(s) => s.score = s.score.max(seed.score),
            None => seeds.push(Seed {
                id: seed.id,
                score: seed.score,
            }),
        }
    }
    seeds.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    let top = seeds.first().map_or(0.0, |s| s.score);
    seeds.retain(|s| s.score >= top * 0.6);
    seeds.truncate(trace.returned.len().max(8));
    let returned: HashSet<MemoryId> = trace.returned.iter().copied().collect();
    let mut seen = HashSet::new();
    let suppressed = trace
        .suppressed
        .iter()
        .copied()
        .filter(|id| !returned.contains(id) && seen.insert(*id))
        .take(24)
        .collect();
    UiEvent::Recall(RecallEvent {
        seq: 0,
        at: at.millis(),
        client: client.map(str::to_owned),
        namespace,
        namespace_id,
        seeds,
        spreads: trace
            .spreads
            .iter()
            .take(500)
            .map(|s| Spread {
                from: s.from,
                to: s.to,
                edge_type: s.edge_type,
                amount: s.amount,
                order: s.order,
            })
            .collect(),
        returned: trace.returned.clone(),
        suppressed,
    })
}

/// The `link` events of the semantic links made when a vector landed, named after the
/// client that saved the memory; `None` when the vector was not stored.
pub(crate) fn linked(
    s: &Store,
    id: MemoryId,
    outcome: &ApplyOutcome,
    at: Timestamp,
) -> Option<Vec<UiEvent>> {
    let ApplyOutcome::Applied { links } = outcome else {
        return None;
    };
    let client = s.memory(id).and_then(|m| m.client.clone());
    Some(
        links
            .iter()
            .map(|e| {
                UiEvent::Link(LinkEvent {
                    seq: 0,
                    at: at.millis(),
                    client: client.clone(),
                    src: e.source,
                    dst: e.target,
                    edge_type: e.edge_type,
                    weight: e.weight,
                })
            })
            .collect(),
    )
}

/// The events of a sweep: each phase change, each removal, then the sweep itself.
pub(crate) fn swept(s: &Store, report: &SweepReport, at: Timestamp) -> Vec<UiEvent> {
    let mut out = Vec::new();
    for &id in &report.phase_changed {
        let Some(to) = s.memory(id).map(|m| m.phase) else {
            continue;
        };
        let from = match to {
            Phase::Ghost => Phase::Summary,
            Phase::Summary | Phase::Full => Phase::Full,
        };
        out.push(UiEvent::PhaseChange(PhaseChangeEvent {
            seq: 0,
            at: at.millis(),
            id,
            from,
            to,
        }));
    }
    out.extend(report.removed.iter().map(|&id| forgotten(id, None, at)));
    out.push(UiEvent::Sweep(SweepEvent {
        seq: 0,
        at: at.millis(),
        moved: SweepMoved {
            full_to_summary: report.full_to_summary,
            summary_to_ghost: report.summary_to_ghost,
            ghost_to_deleted: report.deleted,
        },
    }));
    out
}

/// The durations of the last 100 of one kind of work, for their median.
#[derive(Debug, Default)]
pub(crate) struct Latencies(Mutex<VecDeque<f64>>);

impl Latencies {
    pub(crate) fn note(&self, took: std::time::Duration) {
        let mut q = lock(&self.0);
        if q.len() == 100 {
            q.pop_front();
        }
        q.push_back(took.as_secs_f64() * 1000.0);
    }

    /// The median in milliseconds, or `None` before the first.
    pub(crate) fn median(&self) -> Option<f64> {
        let mut v: Vec<f64> = lock(&self.0).iter().copied().collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        let mid = v.len() / 2;
        Some(if v.len().is_multiple_of(2) {
            (v[mid - 1] + v[mid]) / 2.0
        } else {
            v[mid]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forget(n: u8) -> UiEvent {
        forgotten(
            MemoryId::from_bytes([n; 16]),
            None,
            Timestamp::from_millis(5),
        )
    }

    #[test]
    fn the_log_keeps_the_last_thousand_and_numbers_them() {
        let log = EventLog::new();
        let mut rx = log.subscribe();
        log.publish((0..1005).map(|i| forget((i % 200) as u8)).collect());
        let recent = log.recent();
        assert_eq!(recent.len(), KEPT);
        let seq = |e: &UiEvent| match e {
            UiEvent::Forget(f) => f.seq,
            _ => 0,
        };
        assert_eq!((seq(&recent[0]), seq(&recent[KEPT - 1])), (6, 1005));
        assert_eq!(seq(&rx.try_recv().unwrap()), 1);
        let json = serde_json::to_value(&*recent[0]).unwrap();
        assert_eq!(json["kind"], "forget");
        assert_eq!(json["at"], 5);
        assert!(json["client"].is_null());
    }

    #[test]
    fn a_recall_event_keeps_the_strong_seeds_and_drops_returned_suppressions() {
        use knoggin_core::ops::{TraceSeed, TraceSpread};
        let id = |n| MemoryId::from_bytes([n; 16]);
        let trace = RecallTrace {
            seeds: vec![
                TraceSeed {
                    id: id(1),
                    score: 1.0,
                },
                TraceSeed {
                    id: id(1),
                    score: 0.5,
                },
                TraceSeed {
                    id: id(2),
                    score: 0.7,
                },
                TraceSeed {
                    id: id(3),
                    score: 0.1,
                },
            ],
            spreads: vec![TraceSpread {
                from: id(1),
                to: id(4),
                amount: 0.2,
                edge_type: EdgeType::Entity,
                order: 0,
            }],
            returned: vec![id(1), id(4)],
            suppressed: vec![id(4), id(5), id(5)],
        };
        let json = serde_json::to_value(recalled(
            &trace,
            (NamespaceId::new(2), "work".to_owned()),
            Some("codex"),
            Timestamp::from_millis(9),
        ))
        .unwrap();
        assert_eq!(json["kind"], "recall");
        assert_eq!(json["seeds"].as_array().unwrap().len(), 2);
        assert_eq!(json["seeds"][0]["score"], 1.0);
        assert_eq!(json["spreads"][0]["edgeType"], "entity");
        assert_eq!(json["suppressed"], serde_json::json!([id(5)]));
        assert_eq!(json["namespace"], "work");
        assert!(json.get("query").is_none());
    }

    #[test]
    fn the_median_of_the_last_hundred() {
        let l = Latencies::default();
        assert_eq!(l.median(), None);
        for ms in [5, 1, 3] {
            l.note(std::time::Duration::from_millis(ms));
        }
        assert_eq!(l.median(), Some(3.0));
        for _ in 0..100 {
            l.note(std::time::Duration::from_millis(10));
        }
        assert_eq!(l.median(), Some(10.0));
    }
}
