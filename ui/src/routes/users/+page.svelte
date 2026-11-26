<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Users (server, sign-in on, admins). The list loads its next page when the reader reaches the end. -->
<script lang="ts">
  import { createInfiniteQuery, createQuery } from '@tanstack/svelte-query';
  import { DropdownMenu } from 'bits-ui';
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import Dialog from '$lib/components/Dialog.svelte';
  import Icon from '$lib/components/Icon.svelte';
  import NotForYou from '$lib/components/NotForYou.svelte';
  import LoadFailed from '$lib/components/LoadFailed.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import { session, dayLabel, who } from '$lib/session.svelte';
  import { NOT_FOR_YOU } from '$lib/sentences';
  import { ApiError, del, get, post } from '$lib/api/client';
  import type { AdminStatus, Page, User } from '$lib/api/shapes';
  import { queryClient } from '$lib/query';
  import { ago, count, days, ms } from '$lib/format';

  const denied = $derived(
    session.local ? NOT_FOR_YOU.usersLocal : !session.signInOn ? NOT_FOR_YOU.usersSignInOff : !session.isAdmin ? NOT_FOR_YOU.member : null,
  );
  const admin = createQuery(() => ({ queryKey: ['admin-status'], queryFn: () => get<AdminStatus>('/v1/admin/status'), enabled: !denied }));
  const users = createInfiniteQuery(() => ({
    queryKey: ['users'],
    queryFn: ({ pageParam }) => get<Page<User>>('/v1/users', { query: { cursor: pageParam } }),
    initialPageParam: null as string | null,
    getNextPageParam: (last: Page<User>) => last.nextCursor,
    enabled: !denied,
  }));
  const rows = $derived((users.data?.pages ?? []).flatMap((p) => p.items));
  const me = $derived(session.value?.user?.id);
  const idleMinutes = $derived(session.value?.idleSeconds ? Math.round(session.value.idleSeconds / 60) : 10);

  // Load the next page when the end of the list comes into view.
  function sentinel(node: HTMLElement) {
    const io = new IntersectionObserver((e) => {
      if (e[0].isIntersecting && users.hasNextPage && !users.isFetchingNextPage) void users.fetchNextPage();
    });
    io.observe(node);
    return { destroy: () => io.disconnect() };
  }

  let removing = $state<User | null>(null);
  let removeOpen = $state(false);
  $effect(() => {
    if (!removeOpen) removing = null;
  });
  let busy = $state(false);
  let error = $state<string | null>(null);
  async function act(fn: () => Promise<unknown>) {
    busy = true;
    error = null;
    try {
      await fn();
      await queryClient.invalidateQueries({ queryKey: ['users'] });
      await queryClient.invalidateQueries({ queryKey: ['admin-status'] });
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Could not do that.';
    } finally {
      busy = false;
    }
  }
  const remove = () =>
    act(async () => {
      await del(`/v1/users/${removing!.id}`);
      removeOpen = false;
    });
  const restore = (u: User) => act(() => post(`/v1/users/${u.id}/undelete`));
  const since = (u: User) => (u.container.since ? (u.container.state === 'active' ? `for ${ago(ms(u.container.since)).replace(' ago', '')}` : ago(ms(u.container.since))) : '');
</script>

<Screen title="Users">
  {#if denied}
    <NotForYou title="Users" sentence={denied} />
  {:else}
    <PageHeader title="Users" meta={admin.data ? `${count(admin.data.users.total)} users` : undefined} />
    <p class="lede">
      Your identity provider and the allow-list in Knoggin’s settings decide who can sign in. People appear here after they first sign in or connect an agent. Admins are named in the server config.
    </p>
    {#if error}<p class="field-error" role="alert">{error}</p>{/if}
    {#if users.isPending}
      <Loading />
    {:else if users.isError}
      <LoadFailed onretry={() => users.refetch()} />
    {:else}
      <div class="card table-card">
        <div class="table-scroll">
          <div class="table users-table" role="table" aria-label="Users">
            <div class="thead" role="row">
              <span role="columnheader">User</span><span role="columnheader">Role</span><span role="columnheader">Store</span><span role="columnheader" class="num">Apps</span><span role="columnheader">Last active</span><span role="columnheader"><span class="visually-hidden">Actions</span></span>
            </div>
            {#each rows as u (u.id)}
              {@const w = who(u)}
              {@const removed = !!u.deletedAt}
              <div class="trow" class:dim={removed} role="row">
                <span role="cell" class="cell-user">
                  <span class="avatar" aria-hidden="true">{w.initials}</span>
                  <span class="cell-main"><strong class="user-name">{w.text}</strong>{#if u.name && u.email}<span class="sub">{u.email}</span>{/if}</span>
                </span>
                <span role="cell"><span class="tag">{u.isAdmin ? 'Admin' : 'Member'}</span></span>
                {#if removed}
                  <span role="cell"><span class="tag warn">Scheduled for deletion · {u.deletedBy === 'self' ? 'by the user' : 'by an admin'}</span></span>
                {:else}
                  <span role="cell" class="state">
                    <span class="sdot" class:idle={u.container.state !== 'active'}></span>{u.container.state === 'active' ? 'Active' : 'Parked'}
                    <span class="mono small muted">{since(u)}</span>
                  </span>
                {/if}
                <span role="cell" class="val num">{u.appCount}</span>
                <span role="cell" class="val">{dayLabel(u.lastActiveOn)}</span>
                <span role="cell" class="end">
                  {#if removed}
                    <button type="button" class="btn-secondary btn-small" disabled={busy} onclick={() => restore(u)}>Restore</button>
                  {:else if u.id !== me}
                    <DropdownMenu.Root>
                      <DropdownMenu.Trigger class="icon-btn" aria-label="Actions for {u.name || u.email || w.text}"><Icon name="more" /></DropdownMenu.Trigger>
                      <DropdownMenu.Portal>
                        <DropdownMenu.Content class="menu" sideOffset={4} align="end">
                          <DropdownMenu.Item class="menu-item danger" onSelect={() => ((removing = u), (removeOpen = true))}>Remove user…</DropdownMenu.Item>
                        </DropdownMenu.Content>
                      </DropdownMenu.Portal>
                    </DropdownMenu.Root>
                  {/if}
                </span>
              </div>
            {/each}
          </div>
        </div>
        <div use:sentinel class="sentinel" aria-hidden="true"></div>
      </div>
      <p class="caption">A store is active while its container runs, and parks after {idleMinutes} idle minutes. A parked store wakes on the next request.</p>
    {/if}
  {/if}
</Screen>

<Dialog bind:open={removeOpen} title="Remove {removing ? removing.name || removing.email || who(removing).text : ''}?" alert>
  <p class="dialog-text">
    Their personal store and {removing?.appCount === 1 ? '1 app' : `${removing?.appCount ?? 0} apps`} are scheduled for deletion and can be restored for {days(session.value?.softDeleteDays ?? 7)}. To stop them signing in again, remove them in your identity provider too, or take them off the allow-list. After the undo period they could sign in again and start with an empty account.
  </p>
  {#if error}<span class="field-error" role="alert">{error}</span>{/if}
  {#snippet actions()}
    <button type="button" class="btn-secondary" onclick={() => (removeOpen = false)}>Keep user</button>
    <button type="button" class="btn-danger" disabled={busy} onclick={remove}>Remove user</button>
  {/snippet}
</Dialog>

<style>
  .user-name {
    font-weight: var(--weight-medium);
  }
  .end {
    justify-self: end;
  }
  .sentinel {
    height: var(--hairline);
  }
</style>
