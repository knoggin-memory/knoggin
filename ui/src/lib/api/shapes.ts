// SPDX-License-Identifier: AGPL-3.0-only
// The API shapes the UI reads: the TypeScript types that ts-rs generates from
// the Rust types (./types, written by the ignored test `export_ui_types`), with
// the few fields the generated files leave loose narrowed here.
import type { AdminStatus as GeneratedAdminStatus } from './types/AdminStatus';
import type { ErrorBody } from './types/ErrorBody';
import type { UiEvent } from './types/UiEvent';

export type { Phase } from './types/Phase';
export type { EdgeType as EdgeTypeName } from './types/EdgeType';
export type { Session } from './types/Session';
export type { SessionUser } from './types/SessionUser';
export type { Store } from './types/Store';
export type { ApiKey } from './types/ApiKey';
export type { ModelState } from './types/ModelState';
export type { StoreStatus } from './types/StoreStatus';
export type { PhaseThresholds } from './types/PhaseThresholds';
export type { Namespace } from './types/Namespace';
export type { Memory } from './types/Memory';
export type { User } from './types/User';
export type { Snapshot } from './types/Snapshot';
export type { Items } from './types/Items';
export type { Page } from './types/Page';
export type { NamespaceRef } from './types/NamespaceRef';
export type { GraphSnapshot } from './types/GraphSnapshot';
export type { NextChange } from './types/NextChange';
export type { MemoryRow } from './types/MemoryRow';
export type { MemoryPage } from './types/MemoryPage';
export type { Facet } from './types/Facet';
export type { Facets } from './types/Facets';
export type { AccessHistory } from './types/AccessHistory';
export type { NeighborNode } from './types/NeighborNode';
export type { NeighborEdge } from './types/NeighborEdge';
export type { Neighborhood } from './types/Neighborhood';
export type { UiSearchHit } from './types/UiSearchHit';
export type { UiSearch } from './types/UiSearch';
export type { ActivityBucket } from './types/ActivityBucket';
export type { ActivityRow } from './types/ActivityRow';
export type { Activity } from './types/Activity';
export type { TransitionCounts } from './types/TransitionCounts';
export type { AtRiskMemory } from './types/AtRiskMemory';
export type { HealthReport } from './types/HealthReport';
export type { SystemStatus } from './types/SystemStatus';

export type Permission = 'memory:read' | 'memory:write';
export type ApiErrorBody = { error: ErrorBody };
/** A live event as sent. */
export type WireEvent = UiEvent;

/** `storage` and `containers` are generated as plain JSON; their shape is the tab "REST API"'s. */
export type AdminStatus = Omit<GeneratedAdminStatus, 'storage' | 'containers'> & {
  storage: { bucket: string; region: string } | null;
  containers: { claimed: number; ready: number; starting: number } | null;
};
