<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- The pages shown without the shell: loading, the locked install, a browser
     without a session, the sign-in pages and the account's scheduled deletion. -->
<script module lang="ts">
  export type Page =
    | { kind: 'loading' }
    | { kind: 'failed' }
    | { kind: 'locked' }
    | { kind: 'open-local' }
    | { kind: 'signin'; returnTo: string }
    | { kind: 'signin-failed'; error: string }
    | { kind: 'expired'; returnTo: string }
    | { kind: 'signed-out' }
    | { kind: 'deleting' };
</script>

<script lang="ts">
  import AuthPage from './AuthPage.svelte';
  import Banner from './Banner.svelte';
  import CopyField from './CopyField.svelte';
  import Logo from './Logo.svelte';
  import { session } from '$lib/session.svelte';
  import { lockSentence, signInError } from '$lib/sentences';
  import { post, token } from '$lib/api/client';
  import type { Session } from '$lib/api/shapes';
  import { longDate } from '$lib/format';


  let { page }: { page: Page } = $props();
  const s = $derived(session.value);
  const source = $derived(s?.sourceUrl);
  const host = $derived(s ? new URL(s.publicUrl).host : '');
  let signingIn = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);

  const login = (returnTo: string) => `/auth/login?return_to=${encodeURIComponent(returnTo)}`;
  function startSignIn(returnTo: string) {
    signingIn = true;
    location.href = login(returnTo);
  }

  async function undo() {
    busy = true;
    error = null;
    try {
      session.replace(await post<Session>('/v1/account/undelete'));
    } catch (e) {
      error = (e as Error).message;
    } finally {
      busy = false;
    }
  }
  async function signOut() {
    await post('/auth/logout').catch(() => undefined);
    token.drop();
    location.href = '/signin?reason=signed_out';
  }
</script>

<svelte:head>{#if page.kind === 'deleting'}<title>Knoggin · Scheduled for deletion</title>{/if}</svelte:head>

{#if page.kind === 'loading'}
  <AuthPage title="Loading." breathing busy />
{:else if page.kind === 'failed'}
  <AuthPage title="Could not load this." pageTitle="Loading">
    {#snippet actions()}<button type="button" class="btn-secondary btn-wide" onclick={() => session.load()}>Retry</button>{/snippet}
  </AuthPage>
{:else if page.kind === 'locked'}
  {@const l = lockSentence(session.lockReason ?? 'master_key_missing', false)}
  <AuthPage title="Knoggin is locked" hideTitle>
    <p>{l.text}{#if l.guide}<a href={session.docs('local-install.md')}>recovery guide</a>.{/if}</p>
  </AuthPage>
{:else if page.kind === 'open-local'}
  <AuthPage title="Open Knoggin from your terminal" stacked sourceUrl={source}>
    <div class="auth-cmd"><CopyField value="knoggin open" /></div>
  </AuthPage>
{:else if page.kind === 'signin'}
  {#if signingIn}
    <AuthPage title="Signing you in" stacked busy detail={host} sourceUrl={source} />
  {:else}
    <AuthPage
      title="Sign in to Knoggin"
      stacked
      detail={host}
      sourceUrl={source}
      foot={s?.provider ? `Sign-in is handled by ${s.provider.name}. Knoggin never sees your password.` : undefined}
    >
      <p>{session.server ? 'This server uses your organization’s sign-in.' : 'This Knoggin install uses a sign-in service.'}</p>
      {#snippet actions()}<button type="button" class="btn-primary btn-wide" onclick={() => startSignIn(page.returnTo)}>Continue to sign in</button>{/snippet}
    </AuthPage>
  {/if}
{:else if page.kind === 'signin-failed'}
  <AuthPage title="Sign-in did not finish" sourceUrl={source}>
    <p>{signInError(page.error, !!session.server)}</p>
    {#snippet actions()}
      {#if page.error !== 'account_removed'}<a class="btn-primary btn-wide" href={login('/map')}>Sign in</a>{/if}
    {/snippet}
  </AuthPage>
{:else if page.kind === 'expired'}
  <AuthPage title="Your session ended" sourceUrl={source}>
    <p>Sign in again to pick up where you left off.</p>
    {#snippet actions()}<a class="btn-primary btn-wide" href={login(page.returnTo)}>Sign in</a>{/snippet}
  </AuthPage>
{:else if page.kind === 'signed-out'}
  <AuthPage title="You are signed out" sourceUrl={source}>
    <p>Sign in again to open your memories.</p>
    {#snippet actions()}<a class="btn-primary btn-wide" href={login('/map')}>Sign in</a>{/snippet}
  </AuthPage>
{:else if page.kind === 'deleting'}
  <div class="standalone">
    <main class="auth">
      <div class="auth-col wide">
        <Logo variant="mark" alt="Knoggin" />
        <div class="auth-notice">
          <Banner warn title="Your data is scheduled for deletion on {longDate(s?.user?.deleteAfter ?? null)}.">
            {#if error}<span class="field-error" role="alert">{error}</span>{/if}
            {#snippet action()}
              <button type="button" class="btn-secondary btn-small" disabled={busy} onclick={undo}>Undo</button>
              {#if session.cookieSession}<button type="button" class="btn-secondary btn-small" onclick={signOut}>Sign out</button>{/if}
            {/snippet}
          </Banner>
        </div>
      </div>
    </main>
  </div>
{/if}
