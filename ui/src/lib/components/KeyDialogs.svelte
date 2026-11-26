<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- New key, the one-time reveal, rotate and revoke. The secret is shown once,
     with a copy button, and is never stored in the browser. -->
<script lang="ts">
  import Dialog from './Dialog.svelte';
  import CopyField from './CopyField.svelte';
  import { ApiError, del, post } from '$lib/api/client';
  import type { ApiKey } from '$lib/api/shapes';
  import { queryClient } from '$lib/query';

  let {
    storeId,
    storeName,
    creating = $bindable(false),
    rotating = $bindable(null),
    revoking = $bindable(null),
  }: { storeId: string; storeName: string; creating?: boolean; rotating?: ApiKey | null; revoking?: ApiKey | null } = $props();

  let name = $state('');
  let write = $state(true);
  let expires = $state('90');
  let busy = $state(false);
  let error = $state<string | null>(null);
  /** The secret lives only in this component, and only until the dialog closes. */
  let revealed = $state<{ title: string; secret: string } | null>(null);
  let revealOpen = $state(false);
  $effect(() => {
    if (!revealOpen) revealed = null;
  });
  $effect(() => {
    if (creating) {
      name = '';
      write = true;
      expires = '90';
      error = null;
    }
  });
  let rotateOpen = $state(false);
  let revokeOpen = $state(false);
  $effect(() => {
    rotateOpen = rotating !== null;
    error = null;
  });
  $effect(() => {
    revokeOpen = revoking !== null;
    error = null;
  });
  $effect(() => {
    if (!rotateOpen) rotating = null;
  });
  $effect(() => {
    if (!revokeOpen) revoking = null;
  });

  const refresh = () => queryClient.invalidateQueries({ queryKey: ['keys'] });

  async function run(fn: () => Promise<void>) {
    busy = true;
    error = null;
    try {
      await fn();
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Could not do that.';
    } finally {
      busy = false;
    }
  }

  const create = (e: SubmitEvent) => {
    e.preventDefault();
    if (!name.trim()) return;
    return run(async () => {
      const r = await post<{ key: ApiKey; secret: string }>(`/v1/stores/${storeId}/keys`, {
        name: name.trim(),
        permissions: write ? ['memory:read', 'memory:write'] : ['memory:read'],
        expiresInDays: Number(expires),
      });
      creating = false;
      revealed = { title: `Key created: ${r.key.name}`, secret: r.secret };
      revealOpen = true;
      await refresh();
    });
  };
  const rotate = () =>
    run(async () => {
      const k = rotating!;
      const r = await post<{ key: ApiKey; secret: string }>(`/v1/stores/${storeId}/keys/${k.id}/rotate`, { expiresInDays: 90 });
      rotateOpen = false;
      revealed = { title: `New key for ${r.key.name}`, secret: r.secret };
      revealOpen = true;
      await refresh();
    });
  const revoke = () =>
    run(async () => {
      await del(`/v1/stores/${storeId}/keys/${revoking!.id}`);
      revokeOpen = false;
      await refresh();
    });
</script>

<Dialog bind:open={creating} title="New key for {storeName}">
  <form id="new-key" class="stack" onsubmit={create}>
    <div class="field">
      <label for="key-name">Name</label>
      <input class="input" id="key-name" type="text" autocomplete="off" placeholder="home-assistant" maxlength="64" bind:value={name} />
      <span class="hint">Name it after the agent or machine that will hold it.</span>
    </div>
    <div class="field" role="group" aria-labelledby="key-perms">
      <span class="label" id="key-perms">Permissions</span>
      <label class="check" for="key-read"><input id="key-read" type="checkbox" checked disabled /><span class="t"><span>Read</span><span class="hint">Recall and search memories. Every key can read.</span></span></label>
      <label class="check" for="key-write"><input id="key-write" type="checkbox" bind:checked={write} /><span class="t"><span>Write</span><span class="hint">Store, reinforce and forget. Leave off for bots that should only recall.</span></span></label>
    </div>
    <div class="field">
      <label for="key-exp">Expires</label>
      <select class="select tall" id="key-exp" bind:value={expires}>
        <option value="30">In 30 days</option><option value="90">In 90 days</option><option value="180">In 180 days</option><option value="365">In 1 year</option>
      </select>
    </div>
    {#if error}<span class="field-error" role="alert">{error}</span>{/if}
  </form>
  {#snippet actions()}
    <button type="button" class="btn-secondary" onclick={() => (creating = false)}>Cancel</button>
    <button type="submit" form="new-key" class="btn-primary" disabled={!name.trim() || busy}>Create key</button>
  {/snippet}
</Dialog>

<Dialog bind:open={revealOpen} title={revealed?.title ?? ''}>
  <p class="dialog-text">Copy it now. You will not see this key again: Knoggin keeps only a hash of it.</p>
  <CopyField value={revealed?.secret ?? ''} primary label="The new key" />
  <span class="hint">Reaches {storeName} only.</span>
  {#snippet actions()}<button type="button" class="btn-secondary" onclick={() => (revealOpen = false)}>Done</button>{/snippet}
</Dialog>

<Dialog bind:open={rotateOpen} title="Rotate {rotating?.name ?? ''}?" alert>
  <p class="dialog-text">A new key replaces it and gets 90 days. The old key stops working as soon as you confirm, so update the agent straight after. Name, permissions and store stay the same.</p>
  {#if error}<span class="field-error" role="alert">{error}</span>{/if}
  {#snippet actions()}
    <button type="button" class="btn-secondary" onclick={() => (rotateOpen = false)}>Cancel</button>
    <button type="button" class="btn-primary" disabled={busy} onclick={rotate}>Rotate key</button>
  {/snippet}
</Dialog>

<Dialog bind:open={revokeOpen} title="Revoke {revoking?.name ?? ''}?" alert>
  <p class="dialog-text">Agents using it lose access at once. This cannot be undone. Stored memories are not touched.</p>
  {#if error}<span class="field-error" role="alert">{error}</span>{/if}
  {#snippet actions()}
    <button type="button" class="btn-secondary" onclick={() => (revokeOpen = false)}>Keep key</button>
    <button type="button" class="btn-danger" disabled={busy} onclick={revoke}>Revoke key</button>
  {/snippet}
</Dialog>
