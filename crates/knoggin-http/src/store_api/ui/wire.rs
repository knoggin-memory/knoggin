// SPDX-License-Identifier: AGPL-3.0-only
//! The answers of the `/v1/ui/*` read models (tab "Web UI", Read-model routes), ported
//! from recalld's `src/ui/wire.rs` with Knoggin's names: times are RFC 3339 strings,
//! namespaces, phases and edge types are names (except the graph's typed arrays), and
//! there is no query text. They are also the source of the UI's TypeScript types.

use knoggin_core::model::{AccessEvent, EdgeType, Phase};
use knoggin_core::ops::ModelState;
use knoggin_core::{MemoryId, Timestamp};
use serde::Serialize;
use ts_rs::TS;
use utoipa::ToSchema;

/// `GET /v1/ui/graph`. Nodes are columns of base64 little-endian typed arrays: `u32` for
/// `ns`, `degree`, `src` and `dst`; `u8` for `phase` (1 to 3), `permastore` and `type`
/// (1, 2, 5, 6, 7); `f32` for `strength`, `decayStrength`, `stability` and `weight`; `f64`
/// milliseconds for `createdAt` and `lastAccess` (0 when never accessed).
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GraphSnapshot {
    pub(crate) generated_at: Timestamp,
    pub(crate) namespaces: Vec<NamespaceRef>,
    pub(crate) node_count: u64,
    /// On the first page (`edgeOffset` 0) only.
    pub(crate) nodes: Option<SnapshotNodes>,
    pub(crate) edges: SnapshotEdges,
    pub(crate) edge_total: u64,
    pub(crate) edge_offset: u64,
    pub(crate) next_edge_offset: Option<u64>,
}

/// A namespace of the graph answer, by the number its node column uses.
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NamespaceRef {
    pub(crate) id: u32,
    pub(crate) name: String,
    /// Strength thresholds for full to summary, summary to ghost, ghost to deletion.
    pub(crate) thresholds: [f32; 3],
    pub(crate) permastore_days: f32,
    pub(crate) decay_multiplier: f32,
}

/// The node columns.
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotNodes {
    pub(crate) ids: Vec<MemoryId>,
    pub(crate) ns: String,
    pub(crate) phase: String,
    pub(crate) strength: String,
    pub(crate) decay_strength: String,
    pub(crate) stability: String,
    pub(crate) created_at: String,
    pub(crate) last_access: String,
    pub(crate) degree: String,
    pub(crate) permastore: String,
}

/// The edge columns of one page.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct SnapshotEdges {
    pub(crate) src: String,
    pub(crate) dst: String,
    #[serde(rename = "type")]
    #[ts(rename = "type")]
    #[schema(rename = "type")]
    pub(crate) kind: String,
    pub(crate) weight: String,
}

/// `GET /v1/ui/memories`: a page by offset.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct MemoryPage {
    pub(crate) total: u64,
    pub(crate) offset: u64,
    pub(crate) rows: Vec<MemoryRow>,
}

/// One Explore row.
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemoryRow {
    pub(crate) id: MemoryId,
    pub(crate) namespace: String,
    /// `null` for a ghost.
    pub(crate) summary: Option<String>,
    pub(crate) phase: Phase,
    /// Raw strength now.
    pub(crate) strength: f32,
    /// The namespace's decay-rate multiplier, for drawing the curve.
    pub(crate) decay_multiplier: f32,
    pub(crate) stability: f32,
    pub(crate) created_at: Timestamp,
    /// `null` when never accessed.
    pub(crate) retrieved_at: Option<Timestamp>,
    pub(crate) permastore: bool,
    pub(crate) degree: u32,
    pub(crate) next_change: Option<NextChange>,
    pub(crate) tags: Vec<String>,
    pub(crate) entities: Vec<String>,
}

/// The next phase change; `at` in the past means the next sweep.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct NextChange {
    pub(crate) at: Timestamp,
    /// `summary`, `ghost` or `deleted`.
    pub(crate) to: &'static str,
}

/// `GET /v1/ui/facets`.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct Facets {
    pub(crate) clients: Vec<Facet>,
    pub(crate) entities: Vec<Facet>,
    pub(crate) tags: Vec<Facet>,
}

/// A filter value and the memories that have it.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct Facet {
    pub(crate) name: String,
    pub(crate) count: u64,
}

/// `GET /v1/ui/memories/{id}/history`.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct AccessHistory {
    pub(crate) id: MemoryId,
    /// Newest first.
    pub(crate) events: Vec<AccessEvent>,
    /// Events the memory keeps.
    pub(crate) total: u64,
}

/// `GET /v1/ui/memories/{id}/neighbors`.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct Neighborhood {
    pub(crate) center: MemoryId,
    pub(crate) nodes: Vec<NeighborNode>,
    pub(crate) edges: Vec<NeighborEdge>,
    /// The neighborhood was larger than 150 nodes.
    pub(crate) truncated: bool,
}

/// A memory near the center.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct NeighborNode {
    pub(crate) id: MemoryId,
    pub(crate) namespace: String,
    pub(crate) summary: Option<String>,
    pub(crate) phase: Phase,
    pub(crate) hop: u8,
}

/// An edge between two memories of the neighborhood.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct NeighborEdge {
    pub(crate) src: MemoryId,
    pub(crate) dst: MemoryId,
    #[serde(rename = "type")]
    #[ts(rename = "type")]
    #[schema(rename = "type")]
    pub(crate) kind: EdgeType,
    pub(crate) weight: f32,
}

/// `GET /v1/ui/search`. The query is not echoed.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct UiSearch {
    /// The hits come from more than one namespace.
    pub(crate) merged: bool,
    pub(crate) results: Vec<UiSearchHit>,
}

/// One hit with the engine's explanation.
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiSearchHit {
    pub(crate) id: MemoryId,
    pub(crate) namespace: String,
    pub(crate) summary: Option<String>,
    pub(crate) phase: Phase,
    pub(crate) score: f32,
    pub(crate) vector: Option<f32>,
    pub(crate) text: Option<f32>,
    pub(crate) activation: Option<f32>,
    pub(crate) hops: Option<u8>,
    pub(crate) via: Option<MemoryId>,
    pub(crate) via_summary: Option<String>,
    pub(crate) via_edge_type: Option<EdgeType>,
    /// The superseded memory this hit stands in for.
    pub(crate) supersedes: Option<MemoryId>,
}

/// `GET /v1/ui/activity`.
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Activity {
    /// 7 days, or 30 when the range is longer than 730 days.
    pub(crate) bucket_ms: i64,
    pub(crate) buckets: Vec<ActivityBucket>,
    pub(crate) rows: Vec<ActivityRow>,
}

/// Memories created in the bucket by their phase today, and recall entries in it.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct ActivityBucket {
    pub(crate) start: Timestamp,
    pub(crate) full: u32,
    pub(crate) summary: u32,
    pub(crate) ghost: u32,
    pub(crate) recalls: u32,
}

/// One Timeline row.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct ActivityRow {
    pub(crate) at: Timestamp,
    /// `store`, `recall`, `reinforce` or `sweep`.
    pub(crate) kind: &'static str,
    /// `null` for a sweep.
    pub(crate) id: Option<MemoryId>,
    pub(crate) summary: Option<String>,
    pub(crate) namespace: Option<String>,
    /// Links at save, memories reached, 1 for a reinforcement, memories moved by a sweep;
    /// `null` for a save without its event.
    pub(crate) count: Option<u32>,
    pub(crate) client: Option<String>,
}

/// `GET /v1/ui/health`.
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HealthReport {
    /// The namespace, or `all`.
    pub(crate) scope: String,
    pub(crate) overview: HealthOverview,
    pub(crate) decay_forecast: DecayForecast,
    pub(crate) at_risk: Vec<AtRiskMemory>,
    pub(crate) storage: StorageBreakdown,
}

/// Counts for the scope.
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HealthOverview {
    pub(crate) total_memories: u64,
    pub(crate) phase_counts: ReportPhaseCounts,
    pub(crate) permastore_count: u64,
}

/// Memories by phase.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct ReportPhaseCounts {
    pub(crate) full: u64,
    pub(crate) summary: u64,
    pub(crate) ghost: u64,
}

/// Phase changes expected within 7, 30 and 90 days.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct DecayForecast {
    #[serde(rename = "transitions7d")]
    #[ts(rename = "transitions7d")]
    #[schema(rename = "transitions7d")]
    pub(crate) transitions_7d: TransitionCounts,
    #[serde(rename = "transitions30d")]
    #[ts(rename = "transitions30d")]
    #[schema(rename = "transitions30d")]
    pub(crate) transitions_30d: TransitionCounts,
    #[serde(rename = "transitions90d")]
    #[ts(rename = "transitions90d")]
    #[schema(rename = "transitions90d")]
    pub(crate) transitions_90d: TransitionCounts,
}

/// Phase changes by kind.
#[derive(Serialize, TS, ToSchema, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TransitionCounts {
    pub(crate) full_to_summary: u64,
    pub(crate) summary_to_ghost: u64,
    pub(crate) ghost_to_deleted: u64,
}

/// A memory that loses content soonest ("Fading soon").
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AtRiskMemory {
    pub(crate) id: MemoryId,
    pub(crate) summary: Option<String>,
    pub(crate) strength: f32,
    /// Days until the next phase change; 0 means the next sweep.
    pub(crate) days_until_change: f32,
    pub(crate) phase: Phase,
    /// `summary` or `ghost`.
    pub(crate) to: &'static str,
}

/// Sizes for the whole store, whatever the scope.
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageBreakdown {
    pub(crate) text_bytes: u64,
    pub(crate) vector_bytes: u64,
    pub(crate) last_snapshot_bytes: Option<u64>,
}

/// `GET /v1/ui/system`: the process serving this store.
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SystemStatus {
    pub(crate) version: String,
    pub(crate) started_at: Timestamp,
    pub(crate) uptime_secs: u64,
    pub(crate) process: ProcessStatus,
    pub(crate) health: HealthStatus,
    pub(crate) model: ModelState,
    pub(crate) recall_latency_ms: Option<f64>,
    pub(crate) embed_latency_ms: Option<f64>,
    pub(crate) sweep: SweepStatus,
    pub(crate) clients: Vec<ClientInfo>,
}

/// The process's resident memory, when known.
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProcessStatus {
    pub(crate) rss_bytes: Option<u64>,
}

/// `ok` or `degraded`, and why.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct HealthStatus {
    pub(crate) status: &'static str,
    pub(crate) subsystems: Vec<SubsystemHealth>,
}

/// `store`, `embedding` or `sweeper`.
#[derive(Serialize, TS, ToSchema)]
pub(crate) struct SubsystemHealth {
    pub(crate) name: &'static str,
    pub(crate) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub(crate) message: Option<&'static str>,
}

/// The store's last and next sweep.
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SweepStatus {
    pub(crate) last_at: Option<Timestamp>,
    pub(crate) next_at: Option<Timestamp>,
}

/// An MCP client the store has seen.
#[derive(Serialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClientInfo {
    pub(crate) name: String,
    pub(crate) last_seen_at: Timestamp,
}
