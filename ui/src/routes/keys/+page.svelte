<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- API keys of the current store. Revoked and expired keys are not listed. -->
<script lang="ts">
  import { DropdownMenu } from 'bits-ui';
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import Banner from '$lib/components/Banner.svelte';
  import Icon from '$lib/components/Icon.svelte';
  import KeyDialogs from '$lib/components/KeyDialogs.svelte';
  import LoadFailed from '$lib/components/LoadFailed.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import { keysQuery, liveKey, storesQuery } from '$lib/queries';
  import { store } from '$lib/store.svelte';
  import { session, dayLabel } from '$lib/session.svelte';
  import { days, daysUntil, longDate } from '$lib/format';
  import type { ApiKey } from '$lib/api/shapes';

  const stores = storesQuery();
  const current = $derived(stores.data?.find((s) => s.id === store.id));
  const name = $derived(current?.name ?? 'Personal');
  const keys = keysQuery(() => store.id);
  const live = $derived((keys.data ?? []).filter((k) => liveKey(k)));
  const soon = $derived(
    live.filter((k) => daysUntil(k.expiresAt) <= 14).sort((a, b) => Date.parse(a.expiresAt) - Date.parse(b.expiresAt))[0],
  );
  // A cookie session at a local install's public address cannot make, rotate or revoke keys.
  const own = $derived(!session.remoteLocal);
  let creating = $state(false);
  let rotating = $state<ApiKey | null>(null);
  let revoking = $state<ApiKey | null>(null);
  const perms = (k: ApiKey) => (k.permissions.includes('memory:write') ? ['Read', 'Write'] : ['Read only']);
</script>

<Screen title="API keys">
  <PageHeader title="API keys" meta={name} />
  <div class="toolbar">
    <p class="lede">A key reaches {current?.kind === 'app' ? name : 'your personal store'} and nothing else. Use one for an agent that cannot sign in, such as a bot or a script.</p>
    {#if own}<div class="end"><button type="button" class="btn-primary" onclick={() => (creating = true)}>New key</button></div>{/if}
  </div>
  {#if !own}<p class="caption">Keys are managed on the machine that runs Knoggin. Run <span class="mono">knoggin open</span> there.</p>{/if}

  {#if keys.isPending}
    <Loading />
  {:else if keys.isError}
    <LoadFailed onretry={() => keys.refetch()} />
  {:else}
    {#if soon}
      <Banner warn title="{soon.name} expires in {days(daysUntil(soon.expiresAt))}." text="Rotate it to keep its agent connected. The new key gets 90 days.">
        {#snippet action()}
          {#if own}<button type="button" class="btn-secondary btn-small" onclick={() => (rotating = soon)}>Rotate…</button>{/if}
        {/snippet}
      </Banner>
    {/if}
    {#if !live.length}
      <div class="empty">
        <p>No keys for this store yet. Create one for an agent that cannot sign in, such as a bot or a script.</p>
        {#if own}<button type="button" class="btn-primary" onclick={() => (creating = true)}>New key</button>{/if}
      </div>
    {:else}
      <div class="card table-card">
        <div class="table-scroll">
          <div class="table keys-table" role="table" aria-label="API keys">
            <div class="thead" role="row">
              <span role="columnheader">Name</span><span role="columnheader">Key</span><span role="columnheader">Permissions</span><span role="columnheader">Created</span><span role="columnheader">Last used</span><span role="columnheader">Expires</span><span role="columnheader"><span class="visually-hidden">Actions</span></span>
            </div>
            {#each live as k (k.id)}
              {@const left = daysUntil(k.expiresAt)}
              <div class="trow" role="row">
                <span role="cell" class="cell-main"><strong class="key-name">{k.name}</strong></span>
                <span role="cell" class="val">knoggin_live_{k.prefix}…</span>
                <span role="cell" class="tags">{#each perms(k) as p (p)}<span class="tag">{p}</span>{/each}</span>
                <span role="cell" class="val">{longDate(k.createdAt)}</span>
                <span role="cell" class="val">{dayLabel(k.lastUsedOn)}</span>
                <span role="cell"><span class={left <= 14 ? 'tag warn' : 'val'}>in {left} d</span></span>
                <span role="cell" class="end">
                  {#if own}
                    <DropdownMenu.Root>
                      <DropdownMenu.Trigger class="icon-btn" aria-label="Actions for {k.name}"><Icon name="more" /></DropdownMenu.Trigger>
                      <DropdownMenu.Portal>
                        <DropdownMenu.Content class="menu" sideOffset={4} align="end">
                          <DropdownMenu.Item class="menu-item" onSelect={() => (rotating = k)}>Rotate…</DropdownMenu.Item>
                          <DropdownMenu.Item class="menu-item danger" onSelect={() => (revoking = k)}>Revoke…</DropdownMenu.Item>
                        </DropdownMenu.Content>
                      </DropdownMenu.Portal>
                    </DropdownMenu.Root>
                  {/if}
                </span>
              </div>
            {/each}
          </div>
        </div>
      </div>
      <p class="caption">A key is shown once, when it is created. Knoggin keeps only a hash of it. Read-only keys suit always-on bots that should recall but never save.</p>
    {/if}
  {/if}
</Screen>

{#if store.id && own}
  <KeyDialogs storeId={store.id} storeName={name} bind:creating bind:rotating bind:revoking />
{/if}

<style>
  .key-name {
    font-weight: var(--weight-medium);
  }
  .end {
    justify-self: end;
  }
</style>
