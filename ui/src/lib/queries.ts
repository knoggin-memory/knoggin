// SPDX-License-Identifier: AGPL-3.0-only
// Shared TanStack Query definitions. Store data is keyed under
// ['store', <store id>, ...], so a store switch drops it.
import { createQuery } from '@tanstack/svelte-query';
import { get, sget } from './api/client';
import type { ApiKey, Items, Namespace, Store } from './api/shapes';
import { store } from './store.svelte';
import { session } from './session.svelte';

export const storeKey = (...rest: unknown[]) => ['store', store.id, ...rest];

/** GET /v1/namespaces, a store route the shell calls on every screen. No timer. */
export const namespacesQuery = () =>
  createQuery(() => ({
    queryKey: storeKey('namespaces'),
    queryFn: () => sget<Items<Namespace>>('/v1/namespaces').then((r) => r.items),
    enabled: !!store.id && !!session.value?.signedIn,
    staleTime: 10_000,
  }));

/** The count for a scope: the sum of the live counts for all namespaces (`null`), else that namespace's. */
export const memoryTotal = (namespaces: Namespace[], scope: string | null) =>
  scope === null ? namespaces.reduce((a, n) => a + n.memoryCount, 0) : (namespaces.find((n) => n.name === scope)?.memoryCount ?? 0);

/** GET /v1/stores, a control route: every figure on the Apps pages comes from it. */
export const storesQuery = () =>
  createQuery(() => ({
    queryKey: ['stores'],
    queryFn: () => get<Items<Store>>('/v1/stores').then((r) => r.items),
    enabled: !!session.value?.signedIn,
    staleTime: 10_000,
  }));

export const keysQuery = (id: () => string | null) =>
  createQuery(() => ({
    queryKey: ['keys', id()],
    queryFn: () => get<Items<ApiKey>>(`/v1/stores/${id()}/keys`).then((r) => r.items),
    enabled: !!id(),
  }));

/** A key that still works: neither revoked nor expired. */
export const liveKey = (k: ApiKey, now = Date.now()) => !k.revokedAt && Date.parse(k.expiresAt) > now;

/** The phase thresholds of a namespace, 0.70 and 0.30 unless a policy changed them. */
export function thresholdsOf(namespaces: Namespace[] | undefined, name: string): [number, number, number] {
  const t = namespaces?.find((n) => n.name === name)?.policy.phaseThresholds;
  return t ? [t.fullToSummary, t.summaryToGhost, t.ghostToDelete] : [0.7, 0.3, 0.05];
}
