<!-- "Daemon unavailable" state shared by every screen (DESIGN §8). -->
<script lang="ts">
  import { session } from '$lib/session.svelte';
</script>

{#if session.status === 'signedOut'}
  <div class="banner" role="alert">
    <span class="dot" aria-hidden="true"></span>
    <div class="text">
      <strong>This page isn’t signed in.</strong>
      <span>Open the link <code class="mono">recalld ui</code> printed. Each launch prints a new one.</span>
    </div>
  </div>
{:else if session.status === 'unavailable' || session.status === 'unsupported'}
  <div class="banner" role="alert">
    <span class="dot" aria-hidden="true"></span>
    <div class="text">
      {#if session.status === 'unsupported'}
        <strong>This daemon was started without UI support.</strong>
        <span>Run <code class="mono">recalld daemon stop</code>, then <code class="mono">recalld ui</code> again.</span>
      {:else}
        <strong>The recalld daemon is not running.</strong>
        <span class="mono path">{session.value?.socket ?? ''}</span>
      {/if}
    </div>
    {#if session.status === 'unavailable'}
      <button type="button" class="btn-primary" disabled={session.starting} onclick={() => session.startDaemon()}>
        {session.starting ? 'Starting…' : 'Start daemon'}
      </button>
    {/if}
  </div>
{/if}

<style>
  .banner {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4);
    background: var(--color-surface);
    border: var(--hairline) solid var(--color-border-strong);
    border-radius: var(--radius-md);
  }
  .dot {
    width: var(--dot);
    height: var(--dot);
    border-radius: var(--radius-pill);
    background: var(--color-text-muted);
    flex: none;
  }
  .text {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
  }
  strong {
    font-weight: var(--weight-medium);
  }
  .path,
  code {
    font-size: var(--text-status);
    color: var(--color-text-muted);
  }
</style>
