<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- A screen whose own data could not be loaded: one sentence and Retry, in the empty-state layout. -->
<script lang="ts">
  let { onretry }: { onretry: () => void } = $props();
</script>

<div class="empty" role="alert"><p>Could not load this.</p><button type="button" class="btn-secondary" onclick={onretry}>Retry</button></div>
