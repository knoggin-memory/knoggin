// Memory reads (all read-only on the daemon) and the UI's only two writes.
import { get, post } from './client';
import type {
  AccessHistory,
  ListTagsResponse,
  MemoryPage,
  MemoryRecord,
  MemoryRow,
  Neighborhood,
  UiSearch,
} from './types';

export type SortKey = 'lastAccess' | 'strength' | 'created' | 'nextChange' | 'summary';

export interface PageQuery {
  namespace: string | null;
  phases: number[];
  tags: string[];
  entities: string[];
  sort: SortKey;
  desc: boolean;
  /** Restrict to these memories (the Agent filter). */
  ids?: string[];
}

export const PAGE = 200;

export const memoryPage = (q: PageQuery, offset: number, limit = PAGE) =>
  get<MemoryPage>('/memories', {
    namespace: q.namespace,
    phase: q.phases.map(String),
    tag: q.tags,
    entity: q.entities,
    id: q.ids ?? [],
    sort: q.sort,
    desc: q.desc,
    offset,
    limit,
  });

export async function memoryRows(ids: string[]): Promise<MemoryRow[]> {
  if (!ids.length) return [];
  const page = await get<MemoryPage>('/memories', { id: ids, limit: ids.length });
  return page.rows;
}

export const memoryRecord = (id: string) => get<MemoryRecord>(`/memories/${id}`);
export const memoryHistory = (id: string, limit = 3) =>
  get<AccessHistory>(`/memories/${id}/history`, { limit });
export const memoryNeighbors = (id: string, depth = 2) =>
  get<Neighborhood>(`/memories/${id}/neighbors`, { depth });
export const tags = (namespace: string | null) => get<ListTagsResponse>('/tags', { namespace });
export const search = (q: string, namespace: string | null, limit = 10, signal?: AbortSignal) =>
  get<UiSearch>('/search', { q, namespace, limit }).then((r) => {
    if (signal?.aborted) throw new DOMException('aborted', 'AbortError');
    return r;
  });

/** Reinforce with an FSRS rating: 1 again, 2 hard, 3 good, 4 easy. */
export const reinforce = (id: string, namespace: string, rating: number) =>
  post<unknown>(`/memories/${id}/reinforce`, { namespace, rating });
export const forget = (id: string, namespace: string) =>
  post<unknown>(`/memories/${id}/forget`, { namespace });

export const activity = (namespace: string | null, since: number | null, limit = 120) =>
  get<import('./types').Activity>('/activity', { namespace, since, limit });
