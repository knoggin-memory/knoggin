<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- The Keys column of the Apps table: keys that are neither revoked nor expired,
     loaded for the rows on screen. "off" for an app scheduled for deletion, whose keys are not asked for. -->
<script lang="ts">
  import type { Store } from '$lib/api/shapes';
  import { keysQuery, liveKey } from '$lib/queries';
  let { app }: { app: Store } = $props();
  const keys = keysQuery(() => (app.status === 'deleted' ? null : app.id));
</script>

{#if app.status === 'deleted'}off{:else if keys.data}{keys.data.filter((k) => liveKey(k)).length}{:else}…{/if}
