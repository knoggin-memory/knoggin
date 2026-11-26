<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- The banner bar (DaemonBanner) and the Notice: on the surface for news, on Sand for warnings.
     The rest of the UI stays usable under it. -->
<script lang="ts">
  import type { Snippet } from 'svelte';
  let {
    title,
    text = undefined,
    warn = false,
    role = 'status',
    children,
    action,
  }: { title: string; text?: string; warn?: boolean; role?: 'status' | 'alert' | 'note'; children?: Snippet; action?: Snippet } = $props();
</script>

<div class="banner" class:warn {role}>
  <span class="dot" aria-hidden="true"></span>
  <div class="text">
    <strong>{title}</strong>
    {#if text}<span>{text}</span>{/if}
    {@render children?.()}
  </div>
  {@render action?.()}
</div>
