// SPDX-License-Identifier: AGPL-3.0-only
// API shapes the UI reads, written from the tabs "REST API" and "Web UI".
// ponytail: hand-written until the ts-rs types in ./types land on main; then
// these become re-exports of the generated ones.

export type Phase = 'full' | 'summary' | 'ghost';
export type EdgeTypeName = 'parent_child' | 'associative' | 'entity' | 'temporal' | 'supersedes';
export type Permission = 'memory:read' | 'memory:write';

export interface ApiErrorBody {
  error: { code: string; message: string; field?: string; requestId?: string; reason?: string; docsUrl?: string };
}

export interface SessionUser {
  id: string;
  name: string | null;
  email: string | null;
  isAdmin: boolean;
  deletedAt: string | null;
  deleteAfter: string | null;
}

export interface Session {
  mode: 'local' | 'server';
  auth: 'none' | 'oauth';
  version: string;
  sourceUrl: string;
  docsUrl: string;
  publicUrl: string;
  mcpUrl: string;
  signedIn: boolean;
  signedOutReason?: 'expired' | 'account_removed' | 'not_allowed' | null;
  provider: { name: string } | null;
  user: SessionUser | null;
  credential: { kind: string; permissions: Permission[]; storeId: string | null } | null;
  personalStoreId: string | null;
  softDeleteDays: number;
  retentionDays: number;
  idleSeconds: number | null;
  webSignIn: boolean;
}

export interface Store {
  id: string;
  kind: 'personal' | 'app';
  name: string;
  createdAt: string;
  memoryCount: number;
  lastWriteAt: string | null;
  lastWriteClient: string | null;
  namespaceCount: number;
  status: 'active' | 'deleted';
  deletedAt: string | null;
  deleteAfter: string | null;
}

export interface ApiKey {
  id: string;
  name: string;
  prefix: string;
  permissions: Permission[];
  createdAt: string;
  expiresAt: string;
  lastUsedOn: string | null;
  revokedAt: string | null;
}

export interface ModelState {
  state: 'ready' | 'loading' | 'downloading' | 'unavailable';
  identity: string;
  name: string;
  bytesDone?: number | null;
  bytesTotal?: number | null;
  reason?: 'offline' | 'files_missing' | 'hash_mismatch' | 'disk_full' | 'load_failed' | null;
}

export interface StoreStatus {
  state: 'parked' | 'waking' | 'ready' | 'locked';
  lockedReason: string | null;
  full: boolean;
  model: ModelState | null;
  embedding: { pending: number; total: number } | null;
}

export interface PhaseThresholds {
  fullToSummary: number;
  summaryToGhost: number;
  ghostToDelete: number;
}

export interface Namespace {
  name: string;
  createdAt: string;
  memoryCount: number;
  policy: {
    initialStability: number;
    defaultDifficulty: number;
    phaseThresholds: PhaseThresholds;
    permastoreThreshold: number;
    decayRateMultiplier: number | null;
  };
}

export interface Memory {
  id: string;
  namespace: string;
  createdAt: string;
  summary?: string;
  fullText?: string;
  tags: string[];
  entities: string[];
  topics: string[];
  emotions: string[];
  phase: Phase;
  strength: number;
  effectiveStrength: number;
  stability: number;
  isPermastore: boolean;
  retrievedAt: string | null;
  reinforcedAt: string | null;
  accessCount: number;
  edgeCount: number;
  embedding: string;
  savedBy: string;
  client: string | null;
}

export interface AdminStatus {
  version: string;
  imageDigest: string | null;
  mode: 'local' | 'server';
  backends: { stateDb: string; objectStorage: string; keyProvider: string; launcher: string };
  storage: { bucket: string; region: string } | null;
  dataDir: string | null;
  containers: { claimed: number; ready: number; starting: number } | null;
  pool: { target: number } | null;
  rollout: { claimBuild: string; desiredBuild: string } | null;
  users: { total: number; deleting: number };
  retireBefore: string | null;
}

export interface User {
  id: string;
  name: string | null;
  email: string | null;
  isAdmin: boolean;
  createdAt: string;
  lastActiveOn: string | null;
  appCount: number;
  container: { state: 'active' | 'parked'; since: string | null };
  deletedAt: string | null;
  deletedBy: 'self' | 'admin' | null;
  deleteAfter: string | null;
}

export interface Snapshot {
  name: string;
  generation: number;
  sequence: number;
  sizeBytes: number;
  lastModified: string;
  keptBecause: 'newest' | 'restore' | 'last_hour' | 'hourly' | 'daily' | null;
}

export interface Items<T> {
  items: T[];
}
export interface Page<T> extends Items<T> {
  nextCursor: string | null;
}

// ── Read models (/v1/ui/*) ────────────────────────────────────────────────

export interface NamespaceRef {
  id: number;
  name: string;
  thresholds: [number, number, number];
  permastoreDays: number;
  decayMultiplier: number;
}

export interface GraphSnapshot {
  generatedAt: string;
  namespaces: NamespaceRef[];
  nodeCount: number;
  nodes: {
    ids: string[];
    ns: string;
    phase: string;
    strength: string;
    decayStrength: string;
    stability: string;
    createdAt: string;
    lastAccess: string;
    degree: string;
    permastore: string;
  } | null;
  edges: { src: string; dst: string; type: string; weight: string };
  edgeTotal: number;
  edgeOffset: number;
  nextEdgeOffset: number | null;
}

export interface NextChange {
  at: string;
  to: Phase | 'deleted';
}

export interface MemoryRow {
  id: string;
  namespace: string;
  summary: string | null;
  phase: Phase;
  strength: number;
  decayMultiplier: number;
  stability: number;
  createdAt: string;
  retrievedAt: string | null;
  permastore: boolean;
  degree: number;
  nextChange: NextChange | null;
  tags: string[];
  entities: string[];
  client: string | null;
}

export interface MemoryPage {
  total: number;
  offset: number;
  rows: MemoryRow[];
}

export interface Facet {
  name: string;
  count: number;
}
export interface Facets {
  clients: Facet[];
  entities: Facet[];
  tags: Facet[];
}

export interface AccessHistory {
  id: string;
  events: { at: string; kind: 'direct' | 'associative' | 'reinforcement' }[];
  total: number;
}

export interface NeighborNode {
  id: string;
  namespace: string;
  summary: string | null;
  phase: Phase;
  hop: number;
}
export interface NeighborEdge {
  src: string;
  dst: string;
  type: EdgeTypeName;
  weight: number;
}
export interface Neighborhood {
  center: string;
  nodes: NeighborNode[];
  edges: NeighborEdge[];
  truncated: boolean;
}

export interface UiSearchHit {
  id: string;
  namespace: string;
  summary: string | null;
  phase: Phase;
  score: number;
  vector: number | null;
  text: number | null;
  activation: number | null;
  hops: number | null;
  via: string | null;
  viaSummary: string | null;
  viaEdgeType: EdgeTypeName | null;
  supersedes: string | null;
}
export interface UiSearch {
  merged: boolean;
  results: UiSearchHit[];
}

export interface ActivityBucket {
  start: string;
  full: number;
  summary: number;
  ghost: number;
  recalls: number;
}
export interface ActivityRow {
  at: string;
  kind: 'store' | 'recall' | 'reinforce' | 'sweep';
  id: string | null;
  summary: string | null;
  namespace: string | null;
  count: number | null;
  client: string | null;
}
export interface Activity {
  bucketMs: number;
  buckets: ActivityBucket[];
  rows: ActivityRow[];
}

export interface TransitionCounts {
  fullToSummary: number;
  summaryToGhost: number;
  ghostToDeleted: number;
}
export interface AtRiskMemory {
  id: string;
  summary: string;
  strength: number;
  daysUntilDeletion: number;
  phase: string;
}
export interface HealthReport {
  scope: string;
  overview: {
    totalMemories: number;
    phaseCounts: { full: number; summary: number; ghost: number };
    permastoreCount: number;
  };
  decayForecast: { transitions7d: TransitionCounts; transitions30d: TransitionCounts; transitions90d: TransitionCounts };
  atRisk: AtRiskMemory[];
  storage: { textBytes: number; vectorBytes: number; lastSnapshotBytes: number | null };
}

export interface SystemStatus {
  version: string;
  startedAt: string;
  uptimeSecs: number;
  process: { rssBytes: number | null };
  health: { status: 'ok' | 'degraded'; subsystems: { name: string; status: 'ok' | 'degraded'; message?: string | null }[] };
  model: ModelState;
  recallLatencyMs: number | null;
  embedLatencyMs: number | null;
  sweep: { lastAt: string | null; nextAt: string | null };
  clients: { name: string; lastSeenAt: string }[];
}

// ── Live events (GET /v1/ui/events), as sent ─────────────────────────────

export type WireEvent =
  | {
      kind: 'recall';
      seq: number;
      at: number;
      client: string | null;
      namespace: string;
      seeds: { id: string; score: number }[];
      spreads: { from: string; to: string; edgeType: EdgeTypeName; amount: number; order: number }[];
      returned: string[];
      suppressed: string[];
    }
  | {
      kind: 'store';
      seq: number;
      at: number;
      client: string | null;
      id: string;
      namespace: string;
      phase: Phase;
      edges: { to: string; edgeType: EdgeTypeName; weight: number }[];
    }
  | { kind: 'link'; seq: number; at: number; client: string | null; src: string; dst: string; edgeType: EdgeTypeName; weight: number }
  | {
      kind: 'reinforce';
      seq: number;
      at: number;
      client: string | null;
      id: string;
      rating: number | null;
      stabilityBefore: number | null;
      stabilityAfter: number | null;
    }
  | { kind: 'forget'; seq: number; at: number; client: string | null; id: string }
  | { kind: 'phase_change'; seq: number; at: number; id: string; from: Phase; to: Phase | 'deleted' }
  | { kind: 'sweep'; seq: number; at: number; moved: TransitionCounts }
  | { kind: 'lagged'; missed: number };
