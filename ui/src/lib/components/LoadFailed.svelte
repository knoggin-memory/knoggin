<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- A screen whose own data could not be loaded: one sentence and Retry, in the empty-state layout. -->
<script lang="ts">
  import { ApiError } from '$lib/api/client';
  import { session } from '$lib/session.svelte';
  let { error, onretry }: { error: unknown; onretry: () => void } = $props();
  // The waiting states speak for these codes (and for a local Knoggin that does not answer).
  const WAITING = new Set(['store_locked', 'upgrading', 'unavailable', 'rate_limited', 'unauthenticated', 'store_deleting']);
  const shown = $derived(!(error instanceof ApiError && (WAITING.has(error.code) || (error.code === 'unreachable' && session.local))));
</script>

{#if shown}
  <div class="empty" role="alert"><p>Could not load this.</p><button type="button" class="btn-secondary" onclick={onretry}>Retry</button></div>
{/if}
