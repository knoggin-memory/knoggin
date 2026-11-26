<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- App overview: figures from the Store shape, the key list from GET /v1/stores/{id}/keys.
     Both are control routes, so the app's container is not woken. -->
<script lang="ts">
  import { DropdownMenu } from 'bits-ui';
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import Banner from '$lib/components/Banner.svelte';
  import CopyField from '$lib/components/CopyField.svelte';
  import Dialog from '$lib/components/Dialog.svelte';
  import Icon from '$lib/components/Icon.svelte';
  import AppNameDialog from '$lib/components/AppNameDialog.svelte';
  import LoadFailed from '$lib/components/LoadFailed.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import { keysQuery, liveKey, storesQuery } from '$lib/queries';
  import { store } from '$lib/store.svelte';
  import { session } from '$lib/session.svelte';
  import { ApiError, del, downloadExport, post } from '$lib/api/client';
  import { queryClient } from '$lib/query';
  import { ago, count, days, daysUntil, ms } from '$lib/format';

  const id = $derived(page.params.id ?? '');
  const stores = storesQuery();
  const app = $derived(stores.data?.find((s) => s.id === id));
  const deleted = $derived(app?.status === 'deleted');
  const keys = keysQuery(() => (app && !deleted ? id : null));
  const live = $derived((keys.data ?? []).filter((k) => liveKey(k)));
  const own = $derived(!session.remoteLocal);
  const soonCount = $derived(live.filter((k) => daysUntil(k.expiresAt) <= 14).length);
  let renaming = $state(false);
  let deleting = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);

  const env = $derived(
    session.server || session.remoteLocal ? `KNOGGIN_BASE_URL=${session.value?.publicUrl}\nKNOGGIN_API_KEY=<key>` : 'KNOGGIN_API_KEY=<key>',
  );
  const connectText = $derived(
    'Agents reach this app with one of its keys. A key reaches this store and nothing else.' +
      (session.server ? '' : session.remoteLocal ? ` MCP address: ${session.value?.mcpUrl}` : ' On this machine no address is needed.'),
  );

  function openIn(path: string) {
    store.set(id);
    goto(path);
  }
  async function act(fn: () => Promise<void>) {
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
  const undo = () =>
    act(async () => {
      await post(`/v1/stores/${id}/undelete`);
      await queryClient.invalidateQueries({ queryKey: ['stores'] });
    });
  const remove = () =>
    act(async () => {
      await del(`/v1/stores/${id}`);
      deleting = false;
      if (store.id === id) store.gone();
      await queryClient.invalidateQueries({ queryKey: ['stores'] });
    });
  const exportFirst = () => act(() => downloadExport(id, `knoggin-${app?.name ?? 'app'}.jsonl`));
</script>

<Screen title="App overview">
  <PageHeader title={app?.name ?? 'App'} pageTitle="App overview" meta={app?.id} crumb={{ href: '/apps', label: 'Apps' }} />
  {#if stores.isPending}
    <Loading />
  {:else if stores.isError}
    <LoadFailed error={stores.error} onretry={() => stores.refetch()} />
  {:else if !app || app.kind !== 'app'}
    <div class="empty"><p>No app with this ID.</p><a class="btn-secondary" href="/apps">Back to Apps</a></div>
  {:else}
    {#if deleted}
      <Banner warn title="Scheduled for deletion in {days(daysUntil(app.deleteAfter))}." text="Its keys are turned off and agents cannot reach it. After that the store and its encryption key are removed for good.">
        {#snippet action()}<button type="button" class="btn-secondary btn-small" disabled={busy} onclick={undo}>Undo delete</button>{/snippet}
      </Banner>
    {:else}
      <div class="toolbar">
        <button type="button" class="btn-primary" onclick={() => openIn('/map')}>Open memories</button>
        <button type="button" class="btn-secondary" onclick={() => (renaming = true)}>Rename</button>
        {#if own}
          <DropdownMenu.Root>
            <DropdownMenu.Trigger class="btn-secondary square" aria-label="More actions"><Icon name="more" /></DropdownMenu.Trigger>
            <DropdownMenu.Portal>
              <DropdownMenu.Content class="menu" sideOffset={4} align="start">
                <DropdownMenu.Item class="menu-item danger" onSelect={() => (deleting = true)}>Delete app…</DropdownMenu.Item>
              </DropdownMenu.Content>
            </DropdownMenu.Portal>
          </DropdownMenu.Root>
        {/if}
      </div>
    {/if}
    {#if error && !deleting}<p class="field-error" role="alert">{error}</p>{/if}

    <section class="card" aria-label="Store">
      <div class="tiles">
        <div class="tile"><span class="tl">Memories</span><span class="tv">{count(app.memoryCount)}</span><span class="tc">{app.namespaceCount} {app.namespaceCount === 1 ? 'namespace' : 'namespaces'}</span></div>
        <div class="tile">
          <span class="tl">API keys</span><span class="tv">{deleted ? 'Off' : keys.data ? live.length : '…'}</span>
          <span class="tc">{deleted ? 'until restored' : soonCount ? `${soonCount} ${soonCount === 1 ? 'expires' : 'expire'} soon` : 'none expiring soon'}</span>
        </div>
        <div class="tile">
          <span class="tl">Last write</span><span class="tv">{app.lastWriteAt ? ago(ms(app.lastWriteAt)) : 'Never'}</span>
          <span class="tc">{app.lastWriteClient ?? (app.lastWriteAt ? 'no agent name' : 'no agent yet')}</span>
        </div>
        <div class="tile">
          <span class="tl">Created</span><span class="tv">{new Date(app.createdAt).toLocaleDateString('en-US', { month: 'short', day: 'numeric' })}</span>
          <span class="tc">{new Date(app.createdAt).getFullYear()}</span>
        </div>
      </div>
    </section>

    <div class="row">
      <section class="card wide" aria-label="API keys">
        <div class="card-head">
          <h2 class="card-title">API keys</h2>
          {#if !deleted}<button type="button" class="link small" onclick={() => openIn('/keys')}>Manage keys</button>{/if}
        </div>
        {#if deleted}
          <p class="muted">Keys are turned off while the app is scheduled for deletion.</p>
        {:else if keys.data && !live.length}
          <p class="muted">No keys yet. Create one to connect an agent.</p>
        {/if}
        {#if !own}<p class="caption">Keys are managed on the machine that runs Knoggin. Run <span class="mono">knoggin open</span> there.</p>{/if}
        {#if !deleted}
          {#each live.slice(0, 4) as k (k.id)}
            {@const left = daysUntil(k.expiresAt)}
            <div class="fade-row">
              <span class="fade-text">{k.name}</span><span class="mono small muted">knoggin_live_{k.prefix}…</span>
              <span class="tags">{#each k.permissions.includes('memory:write') ? ['Read', 'Write'] : ['Read only'] as p (p)}<span class="tag">{p}</span>{/each}</span>
              <span class={left <= 14 ? 'tag warn' : 'val mono small'}>in {left} d</span>
            </div>
          {/each}
        {/if}
      </section>
      {#if !deleted}
        <section class="card narrow" aria-label="Connect an agent">
          <div class="card-head"><h2 class="card-title">Connect an agent</h2></div>
          <div class="stack">
            <p class="caption">{connectText}</p>
            <CopyField value={env} stacked />
            <button type="button" class="link small start" onclick={() => openIn('/setup')}>Setup steps for each agent</button>
          </div>
        </section>
      {/if}
    </div>

    <AppNameDialog bind:open={renaming} {app} />
    <Dialog bind:open={deleting} title="Delete {app.name}?" alert>
      <p class="dialog-text">
        Its {count(app.memoryCount)} memories and {live.length} {live.length === 1 ? 'key' : 'keys'} stop working now. You can undo this for {days(session.value?.softDeleteDays ?? 7)}. After that the store and its encryption key are removed for good.
      </p>
      <button type="button" class="link small start" disabled={busy} onclick={exportFirst}>Export first</button>
      {#if error}<span class="field-error" role="alert">{error}</span>{/if}
      {#snippet actions()}
        <button type="button" class="btn-secondary" onclick={() => (deleting = false)}>Keep app</button>
        <button type="button" class="btn-danger" disabled={busy} onclick={remove}>Delete app</button>
      {/snippet}
    </Dialog>
  {/if}
</Screen>

<style>
  .start {
    align-self: flex-start;
  }
</style>
