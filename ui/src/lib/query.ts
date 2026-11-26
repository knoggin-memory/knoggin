// SPDX-License-Identifier: AGPL-3.0-only
// The one TanStack Query client. Store data lives under keys that start with
// ['store', <store id>], so a store switch drops it all at once.
import { QueryClient } from '@tanstack/svelte-query';
import { ApiError } from './api/client';

// The waiting states retry these themselves (status.svelte.ts), and a refused
// request does not get better by asking again at once.
const NO_RETRY = new Set([
  'unreachable', 'unauthenticated', 'permission_denied', 'not_found', 'store_locked', 'store_deleting',
  'upgrading', 'unavailable', 'rate_limited', 'invalid_argument', 'resource_exhausted', 'conflict',
]);

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: (n, e) => n < 1 && !(e instanceof ApiError && NO_RETRY.has(e.code)),
      // Store data is refreshed after the user acts, when the tab regains focus and when an event arrives.
      refetchOnWindowFocus: true,
      staleTime: 5_000,
    },
  },
});
