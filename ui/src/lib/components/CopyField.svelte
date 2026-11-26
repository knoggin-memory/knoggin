<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- A value in mono on a sunken ground with a Copy button (server address, setup commands, the one-time key). -->
<script lang="ts">
  let { value, primary = false, stacked = false, label = undefined }: { value: string; primary?: boolean; stacked?: boolean; label?: string } = $props();
  let copied = $state(false);
  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
    } catch {
      /* the text stays selectable */
    }
    copied = true;
  }
</script>

<div class="copy-field" class:stacked aria-label={label}>
  <code class="v">{value}</code>
  <button type="button" class={primary ? 'btn-primary btn-small' : 'btn-secondary btn-small'} onclick={copy}>{copied ? 'Copied' : 'Copy'}</button>
</div>
