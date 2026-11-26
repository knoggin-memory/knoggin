// SPDX-License-Identifier: AGPL-3.0-only
// Memory reads (none records access) and the UI's two memory writes. Every one
// is a store route and carries the current store.
import { del, post, sget } from './client';
import type { AccessHistory, Activity, Facets, Memory, MemoryPage, MemoryRow, Neighborhood, Phase, UiSearch } from './shapes';

export type SortKey = 'lastAccess' | 'strength' | 'created' | 'nextChange' | 'summary';

export interface PageQuery {
  namespace: string | null;
  phases: Phase[];
  tags: string[];
  entities: string[];
  clients: string[];
  sort: SortKey;
  desc: boolean;
}

export const PAGE = 200;

export const memoryPage = (q: PageQuery, offset: number, limit = PAGE) =>
  sget<MemoryPage>(
    '/v1/ui/memories',
    { namespace: q.namespace, phase: q.phases, tag: q.tags, entity: q.entities, client: q.clients, sort: q.sort, desc: q.desc, offset, limit },
    true,
  );

/** Rows by ID, whatever the filters (the map's labels, a search hit opened in Explore). */
export async function memoryRows(ids: string[]): Promise<MemoryRow[]> {
  if (!ids.length) return [];
  const page = await sget<MemoryPage>('/v1/ui/memories', { id: ids, limit: Math.min(PAGE, ids.length) });
  return page.rows;
}

export const memoryRecord = (id: string) => sget<Memory>(`/v1/memories/${id}`, { recordAccess: false });
export const memoryHistory = (id: string, limit = 3) => sget<AccessHistory>(`/v1/ui/memories/${id}/history`, { limit });
export const memoryNeighbors = (id: string, depth = 2) => sget<Neighborhood>(`/v1/ui/memories/${id}/neighbors`, { depth });
export const facets = (namespace: string | null) => sget<Facets>('/v1/ui/facets', { namespace }, true);
export const search = (q: string, namespace: string | null, limit = 10) => sget<UiSearch>('/v1/ui/search', { q, namespace, limit }, true);
export const activity = (namespace: string | null, since: string | null, limit = 120) =>
  sget<Activity>('/v1/ui/activity', { namespace, since, limit }, true);

/** Reinforce: quality 1 Again, 2 Hard, 3 Good, 4 Easy. */
export const reinforce = (id: string, quality: number) =>
  post<{ id: string; strength: number; stability: number; phase: Phase; isPermastore: boolean }>(
    `/v1/memories/${id}/reinforce`,
    { quality },
    { store: true },
  );
export const forget = (id: string) => del<{ id: string; deleted: boolean }>(`/v1/memories/${id}`, { store: true });
