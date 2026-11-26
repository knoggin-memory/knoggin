<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Server status (admins on a server). Read-only. Restore points: pick a user,
     then one of that user's stores; each snapshot shows the store ID and its
     `<generation>-<sequence>` name for `knoggin admin restore`. -->
<script lang="ts">
  import { createQuery } from '@tanstack/svelte-query';
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import NotForYou from '$lib/components/NotForYou.svelte';
  import LoadFailed from '$lib/components/LoadFailed.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import { session, who } from '$lib/session.svelte';
  import { NOT_FOR_YOU } from '$lib/sentences';
  import { get } from '$lib/api/client';
  import type { AdminStatus, Items, Page, Snapshot, Store, User } from '$lib/api/shapes';
  import { ago, bytes, count, days, ms } from '$lib/format';

  const denied = $derived(session.local ? NOT_FOR_YOU.statusLocal : !session.isAdmin ? NOT_FOR_YOU.member : null);
  const status = createQuery(() => ({ queryKey: ['admin-status'], queryFn: () => get<AdminStatus>('/v1/admin/status'), enabled: !denied }));
  // The user picker follows the cursor of GET /v1/users to the end, so it lists every user.
  const users = createQuery(() => ({
    queryKey: ['all-users'],
    queryFn: async () => {
      const all: User[] = [];
      let cursor: string | null = null;
      do {
        const page: Page<User> = await get<Page<User>>('/v1/users', { query: { cursor, limit: 200 } });
        all.push(...page.items);
        cursor = page.nextCursor;
      } while (cursor);
      return all;
    },
    enabled: !denied,
  }));
  let userId = $state<string | null>(null);
  const chosenUser = $derived(userId ?? users.data?.[0]?.id ?? null);
  const stores = createQuery(() => ({
    queryKey: ['user-stores', chosenUser],
    queryFn: () => get<Items<Store>>(`/v1/users/${chosenUser}/stores`).then((r) => r.items),
    enabled: !!chosenUser,
  }));
  let storeId = $state<string | null>(null);
  const chosenStore = $derived(stores.data?.find((s) => s.id === storeId)?.id ?? stores.data?.[0]?.id ?? null);
  const snaps = createQuery(() => ({
    queryKey: ['snapshots', chosenStore],
    queryFn: () => get<Items<Snapshot>>(`/v1/admin/stores/${chosenStore}/snapshots`).then((r) => r.items),
    enabled: !!chosenStore,
  }));

  const KEPT: Record<string, string> = { newest: 'Latest', last_hour: 'Last hour', hourly: 'Hourly', daily: 'Daily', restore: 'Restore point' };
  const s = $derived(status.data);
  const BACKENDS: Record<string, string> = {
    dynamodb: 'DynamoDB',
    postgres: 'Postgres',
    local_file: 'Local file',
    kms: 'AWS KMS · one data key per store',
    keychain: 'System keychain',
    key_file: 'Key file',
  };
  const storage = $derived(
    s ? (s.backends.objectStorage === 's3' && s.storage ? `S3 · ${s.storage.bucket} · ${s.storage.region}` : s.backends.objectStorage === 'local_dir' ? 'Local folder' : s.backends.objectStorage) : '',
  );
  const digest = (d: string | null) => (d ? `${d.slice(0, 15)}…` : 'no image digest');
</script>

<Screen title="Server status">
  {#if denied}
    <NotForYou title="Server status" sentence={denied} />
  {:else}
    <PageHeader title="Server status" meta="read-only" />
    {#if status.isPending}
      <Loading />
    {:else if status.isError || !s}
      <LoadFailed onretry={() => status.refetch()} />
    {:else}
      <section class="card" aria-label="Now">
        <div class="tiles">
          <div class="tile"><span class="tl">Build</span><span class="tv">{s.version}</span><span class="tc mono">{digest(s.imageDigest)}</span></div>
          <div class="tile"><span class="tl">Active containers</span><span class="tv">{count(s.containers?.claimed ?? 0)}</span><span class="tc">of {count(s.users.total)} users</span></div>
          <div class="tile"><span class="tl">Warm pool</span><span class="tv">{count(s.containers?.ready ?? 0)}</span><span class="tc">blank, ready to claim</span></div>
          <div class="tile">
            <span class="tl">Sign-in</span><span class="tv">{session.signInOn ? 'On' : 'Off'}</span>
            <span class="tc">{session.signInOn ? `OAuth · ${session.value?.provider?.name ?? ''}` : 'anyone who can reach it'}</span>
          </div>
        </div>
      </section>
      <div class="row top">
        <section class="card narrow" aria-label="Backends">
          <div class="card-head"><h2 class="card-title">Backends</h2></div>
          <dl class="kv">
            <dt>Storage</dt><dd>{storage}</dd>
            <dt>State database</dt><dd>{BACKENDS[s.backends.stateDb] ?? s.backends.stateDb}</dd>
            <dt>Key service</dt><dd>{BACKENDS[s.backends.keyProvider] ?? s.backends.keyProvider}</dd>
          </dl>
        </section>
        <section class="card wide" aria-label="Restore points">
          <div class="card-head">
            <h2 class="card-title">Restore points</h2>
            <div class="toolbar">
              <label class="window"><span>User</span>
                <select class="select" aria-label="User" value={chosenUser ?? ''} onchange={(e) => ((userId = e.currentTarget.value), (storeId = null))}>
                  {#each users.data ?? [] as u (u.id)}<option value={u.id}>{who(u).text}</option>{/each}
                </select>
              </label>
              <label class="window"><span>Store</span>
                <select class="select" aria-label="Store of this user" value={chosenStore ?? ''} onchange={(e) => (storeId = e.currentTarget.value)}>
                  {#each stores.data ?? [] as st (st.id)}<option value={st.id}>{st.name}</option>{/each}
                </select>
              </label>
            </div>
          </div>
          {#if chosenStore}<p class="mono small muted store-id">Store ID {chosenStore}</p>{/if}
          {#if snaps.isError}
            <LoadFailed onretry={() => snaps.refetch()} />
          {:else if snaps.data && !snaps.data.length}
            <p class="muted">No snapshots yet.</p>
          {:else if snaps.data}
            <div class="table-scroll">
              <div class="table snaps-table flush" role="table" aria-label="Snapshots">
                <div class="thead" role="row">
                  <span role="columnheader">Snapshot</span><span role="columnheader">Restore ID</span><span role="columnheader">Taken</span><span role="columnheader">Kept as</span><span role="columnheader" class="num">Size</span>
                </div>
                {#each snaps.data as sn (sn.name)}
                  <div class="trow" role="row">
                    <span role="cell" class="val">{sn.sequence}</span>
                    <span role="cell" class="val">{sn.name}</span>
                    <span role="cell" class="val">{ago(ms(sn.lastModified))}</span>
                    <span role="cell">{sn.keptBecause ? KEPT[sn.keptBecause] : ''}</span>
                    <span role="cell" class="val num">{bytes(sn.sizeBytes)}</span>
                  </div>
                {/each}
              </div>
            </div>
          {/if}
          <p class="caption snaps-caption">
            Snapshots are the backups. They are kept for {days(session.value?.retentionDays ?? 7)} and thinned as they age; the newest is always kept. Restoring one is a command-line action in this version.
          </p>
        </section>
      </div>
    {/if}
  {/if}
</Screen>

<style>
  .top {
    align-items: flex-start;
  }
  .store-id {
    margin: 0 0 var(--space-3);
  }
  .flush :global(.thead),
  .flush :global(.trow) {
    padding: 0;
  }
  .snaps-caption {
    margin-top: var(--space-4);
  }
</style>
