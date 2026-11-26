<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Apps: every figure but the key count comes from the Store shape (GET /v1/stores),
     a control route, so no app's container is woken. -->
<script lang="ts">
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import AppNameDialog from '$lib/components/AppNameDialog.svelte';
  import KeyCount from '$lib/components/KeyCount.svelte';
  import LoadFailed from '$lib/components/LoadFailed.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import { storesQuery } from '$lib/queries';
  import { ApiError, post } from '$lib/api/client';
  import { queryClient } from '$lib/query';
  import { ago, count, days, daysUntil, longDate, ms } from '$lib/format';

  const stores = storesQuery();
  const apps = $derived((stores.data ?? []).filter((s) => s.kind === 'app'));
  const liveCount = $derived(apps.filter((a) => a.status === 'active').length);
  let creating = $state(false);
  let errors = $state<Record<string, string>>({});

  async function undo(id: string) {
    try {
      await post(`/v1/stores/${id}/undelete`);
      await queryClient.invalidateQueries({ queryKey: ['stores'] });
    } catch (e) {
      errors = { ...errors, [id]: e instanceof ApiError ? e.message : 'Could not undo.' };
    }
  }
</script>

<Screen title="Apps">
  <PageHeader title="Apps" meta={stores.data ? `${liveCount} ${liveCount === 1 ? 'app' : 'apps'}` : undefined} />
  <div class="toolbar">
    <p class="lede">An app is a separate memory store with its own API keys, for one specialized agent. It never sees your personal memories.</p>
    <div class="end"><button type="button" class="btn-primary" onclick={() => (creating = true)}>New app</button></div>
  </div>
  {#if stores.isPending}
    <Loading />
  {:else if stores.isError}
    <LoadFailed error={stores.error} onretry={() => stores.refetch()} />
  {:else if !apps.length}
    <div class="empty">
      <p>No apps yet. Create one for a support bot, a coding agent for one repo, or a research pipeline.</p>
      <button type="button" class="btn-primary" onclick={() => (creating = true)}>New app</button>
    </div>
  {:else}
    <div class="card table-card">
      <div class="table-scroll">
        <div class="table apps-table" role="table" aria-label="Apps">
          <div class="thead" role="row">
            <span role="columnheader">App</span><span role="columnheader" class="num">Memories</span><span role="columnheader" class="num">Keys</span><span role="columnheader">Last write</span><span role="columnheader">Created</span><span role="columnheader"><span class="visually-hidden">State</span></span>
          </div>
          {#each apps as a (a.id)}
            {@const deleted = a.status === 'deleted'}
            <div class="trow" class:dim={deleted} role="row">
              <span role="cell" class="cell-main"><a class="cell-btn" href="/apps/{a.id}">{a.name}</a><span class="sub">{a.id}</span></span>
              <span role="cell" class="val num">{count(a.memoryCount)}</span>
              <span role="cell" class="val num"><KeyCount app={a} /></span>
              <span role="cell" class="val">{a.lastWriteAt ? ago(ms(a.lastWriteAt)) : 'Never'}</span>
              <span role="cell" class="val">{longDate(a.createdAt)}</span>
              <span role="cell" class="row-actions">
                {#if deleted}
                  <span class="tag warn">Deletes in {days(daysUntil(a.deleteAfter))}</span>
                  <button type="button" class="btn-secondary btn-small" onclick={() => undo(a.id)}>Undo</button>
                {/if}
                {#if errors[a.id]}<span class="field-error" role="alert">{errors[a.id]}</span>{/if}
              </span>
            </div>
          {/each}
        </div>
      </div>
    </div>
  {/if}
</Screen>

<AppNameDialog bind:open={creating} />
