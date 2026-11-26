<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Account (a cookie session, with its profile) or Settings (every other session). -->
<script lang="ts">
  import { createQuery } from '@tanstack/svelte-query';
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import Banner from '$lib/components/Banner.svelte';
  import Dialog from '$lib/components/Dialog.svelte';
  import { storesQuery } from '$lib/queries';
  import { session, who } from '$lib/session.svelte';
  import { status } from '$lib/status.svelte';
  import { prefs, type Theme } from '$lib/prefs.svelte';
  import { ApiError, del, downloadExport, get, post, token } from '$lib/api/client';
  import type { AdminStatus } from '$lib/api/shapes';
  import { count, days } from '$lib/format';

  const s = $derived(session.value);
  const profile = $derived(session.cookieSession && s?.user ? who(s.user) : null);
  const title = $derived(profile ? 'Account' : 'Settings');
  const stores = storesQuery();
  const admin = createQuery(() => ({ queryKey: ['admin-status'], queryFn: () => get<AdminStatus>('/v1/admin/status'), enabled: !!session.local }));
  const undoDays = $derived(s?.softDeleteDays ?? 7);
  const THEMES: [Theme, string][] = [
    ['system', 'System'],
    ['light', 'Light'],
    ['dark', 'Dark'],
  ];
  let deleting = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let exportErrors = $state<Record<string, string>>({});
  let downloaded = $state<Record<string, boolean>>({});

  const signInLabel = $derived(s?.auth === 'oauth' ? `On · OAuth through ${s.provider?.name ?? 'the identity provider'}` : 'Off');
  const keySource = $derived(
    admin.data?.backends.keyProvider === 'keychain' ? 'On · key in the system keychain' : admin.data?.backends.keyProvider === 'key_file' ? 'On · key in a key file' : 'On',
  );
  const about = $derived(
    session.server
      ? [
          { k: 'Address', v: s ? new URL(s.publicUrl).host : '' },
          { k: 'Sign-in', v: signInLabel },
          { k: 'Your stores', v: 'encrypted at rest' },
        ]
      : [
          { k: 'Data directory', v: admin.data?.dataDir ?? '' },
          { k: 'Sign-in', v: signInLabel },
          { k: 'Encryption', v: keySource },
          { k: 'Embedding model', v: status.status?.model?.name ?? '' },
        ],
  );

  async function signOut() {
    await post('/auth/logout').catch(() => undefined);
    token.drop();
    location.href = '/signin?reason=signed_out';
  }
  async function exportStore(id: string, name: string) {
    exportErrors = { ...exportErrors, [id]: '' };
    try {
      await downloadExport(id, `knoggin-${name}.jsonl`);
      downloaded = { ...downloaded, [id]: true };
    } catch (e) {
      exportErrors = { ...exportErrors, [id]: e instanceof ApiError ? e.message : 'Could not download.' };
    }
  }
  /** With a cookie session: delete, then sign out. Otherwise the scheduled-for-deletion page shows at once. */
  async function deleteData() {
    busy = true;
    error = null;
    try {
      const cookie = session.cookieSession;
      await del('/v1/account', { body: { confirm: 'delete my data' } });
      if (cookie) {
        await post('/auth/logout').catch(() => undefined);
        location.href = '/signin?reason=signed_out';
      } else {
        deleting = false;
        await session.load();
      }
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Could not delete.';
    } finally {
      busy = false;
    }
  }
</script>

<Screen {title}>
  <PageHeader {title} />
  <div class="stack page">
    {#if profile}
      <section class="card" aria-label="Profile">
        <div class="split">
          <div class="profile">
            <span class="avatar lg" aria-hidden="true">{profile.initials}</span>
            <div class="who">
              <span class="name">{profile.text}</span>
              {#if s?.user?.name && s.user.email}<span class="muted">{s.user.email}</span>{/if}
              <span class="caption">Profile from {s?.provider?.name ?? 'the identity provider'}{session.server ? ` · ${session.isAdmin ? 'Admin' : 'Member'}` : ''}</span>
            </div>
          </div>
          <button type="button" class="btn-secondary" onclick={signOut}>Sign out</button>
        </div>
      </section>
    {/if}
    {#if session.server && !session.signInOn}
      <Banner warn role="note" title="Sign-in is off." text="Anyone who can reach this address can read and write these memories. That suits your own machine or a private network.">
        <code>To turn it on, set auth.mode = "oauth" in the config file.</code>
        <a class="small start" href={session.docs('sign-in.md')}>Sign-in guide</a>
      </Banner>
    {/if}

    <section class="card" aria-label="Appearance">
      <div class="split">
        <div class="t"><h2 class="card-title">Appearance</h2><span class="caption">System follows your device, so Knoggin turns dark when it does.</span></div>
        <div class="segmented" role="group" aria-label="Theme">
          {#each THEMES as [id, label] (id)}<button type="button" aria-pressed={prefs.theme === id} onclick={() => prefs.setTheme(id)}>{label}</button>{/each}
        </div>
      </div>
    </section>

    <section class="card" aria-label="Your data">
      <div class="card-head"><h2 class="card-title">Your data</h2></div>
      <div class="split">
        <div class="t"><strong>Export my memories</strong><span class="caption">One file for each store. Two downloads can run at a time.</span></div>
      </div>
      {#each stores.data ?? [] as x (x.id)}
        {@const gone = x.status === 'deleted'}
        <div class="fade-row">
          <span class="fade-text">{x.name}</span>
          {#if gone}<span class="tag warn">Scheduled for deletion</span>{/if}
          {#if exportErrors[x.id]}<span class="field-error" role="alert">{exportErrors[x.id]}</span>{/if}
          <span class="mono small muted">{count(x.memoryCount)} {x.memoryCount === 1 ? "memory" : "memories"}</span>
          <button type="button" class="btn-secondary btn-small" disabled={gone} onclick={() => exportStore(x.id, x.name)}>{downloaded[x.id] ? 'Downloaded' : 'Download'}</button>
        </div>
      {/each}
      {#if !session.remoteLocal}
        <div class="rule"></div>
        <div class="split">
          <div class="t"><strong>Delete my data</strong><span class="caption">Removes your personal store, your apps and their keys. You can undo it for {days(undoDays)}.</span></div>
          <button type="button" class="btn-secondary" onclick={() => (deleting = true)}>Delete my data…</button>
        </div>
      {/if}
    </section>

    <section class="card" aria-label={session.server ? 'This server' : 'This install'}>
      <div class="card-head"><h2 class="card-title">{session.server ? 'This server' : 'This install'}</h2><span class="caption">Set in the config file. Read-only here.</span></div>
      {#if session.local && !session.signInOn}
        <p class="only-you">Only you can open this. <span class="mono">knoggin open</span> works only from your own account on this machine.</p>
      {/if}
      <dl class="kv">{#each about as kv (kv.k)}<dt>{kv.k}</dt><dd>{kv.v}</dd>{/each}</dl>
      {#if session.local && !session.signInOn}
        <p class="caption turn-on">
          To turn it on, follow the sign-in guide. A local install also needs a public https address and a list of allowed email addresses. <a href={session.docs('sign-in.md')}>Sign-in guide</a>
        </p>
      {/if}
    </section>
    {#if s}<a class="small start" href={s.sourceUrl}>Source code</a>{/if}
  </div>
</Screen>

<Dialog bind:open={deleting} title="Delete your data?" alert>
  <p class="dialog-text">
    Your personal store, your apps and their keys are scheduled for deletion{session.cookieSession
      ? `, and you are signed out. Sign in again within ${days(undoDays)} to undo it.`
      : `. You can undo it for ${days(undoDays)}.`}
  </p>
  <button type="button" class="link small start" onclick={() => (deleting = false)}>Export first</button>
  {#if error}<span class="field-error" role="alert">{error}</span>{/if}
  {#snippet actions()}
    <button type="button" class="btn-secondary" onclick={() => (deleting = false)}>Keep my data</button>
    <button type="button" class="btn-danger" disabled={busy} onclick={deleteData}>Delete my data</button>
  {/snippet}
</Dialog>

<style>
  .page {
    max-width: calc(var(--form-w) + var(--sidebar-width));
  }
  .start {
    align-self: flex-start;
  }
  .only-you {
    margin: 0 0 var(--space-4);
  }
  .turn-on {
    margin: var(--space-4) 0 0;
  }
</style>
