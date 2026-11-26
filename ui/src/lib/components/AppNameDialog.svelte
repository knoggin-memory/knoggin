<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- New app and Rename: one name field. A failed action shows the API's message under the field. -->
<script lang="ts">
  import { goto } from '$app/navigation';
  import Dialog from './Dialog.svelte';
  import { ApiError, patch, post } from '$lib/api/client';
  import type { Store } from '$lib/api/shapes';
  import { queryClient } from '$lib/query';

  let { open = $bindable(false), app = null }: { open?: boolean; app?: Store | null } = $props();
  let name = $state('');
  let error = $state<string | null>(null);
  let busy = $state(false);
  $effect(() => {
    if (open) {
      name = app?.name ?? '';
      error = null;
    }
  });
  const trimmed = $derived(name.trim());

  async function save(e: SubmitEvent) {
    e.preventDefault();
    if (!trimmed || busy) return;
    busy = true;
    try {
      const s = app ? await patch<Store>(`/v1/stores/${app.id}`, { name: trimmed }) : await post<Store>('/v1/stores', { name: trimmed });
      await queryClient.invalidateQueries({ queryKey: ['stores'] });
      open = false;
      if (!app) goto(`/apps/${s.id}`);
    } catch (err) {
      error = err instanceof ApiError ? err.message : 'Could not save.';
    } finally {
      busy = false;
    }
  }
</script>

<Dialog bind:open title={app ? `Rename ${app.name}` : 'New app'}>
  <form id="app-name-form" class="field" onsubmit={save}>
    <label for="app-name">Name</label>
    <input
      class="input"
      id="app-name"
      type="text"
      autocomplete="off"
      placeholder="Support bot"
      maxlength="64"
      aria-invalid={!!error}
      aria-describedby="app-name-note"
      bind:value={name}
      oninput={() => (error = null)}
    />
    {#if error}
      <span class="field-error" id="app-name-note" role="alert">{error}</span>
    {:else}
      <span class="hint" id="app-name-note">{app ? 'Renaming changes the label only. Keys and memories stay as they are.' : 'A new, empty store with its own keys. You can rename it later.'}</span>
    {/if}
  </form>
  {#snippet actions()}
    <button type="button" class="btn-secondary" onclick={() => (open = false)}>Cancel</button>
    <button type="submit" form="app-name-form" class="btn-primary" disabled={!trimmed || busy}>{app ? 'Save' : 'Create app'}</button>
  {/snippet}
</Dialog>
