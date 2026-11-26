<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Set up an agent, for the current store. The per-agent snippets are the
     prototype's examples until integrations/connectors exists (milestone 8);
     the plain MCP entry and the address come from the session. -->
<script lang="ts">
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import CopyField from '$lib/components/CopyField.svelte';
  import { storesQuery } from '$lib/queries';
  import { store } from '$lib/store.svelte';
  import { session } from '$lib/session.svelte';
  import { KEYS_ELSEWHERE } from '$lib/sentences';
  import { setupSteps, TABS, type Tab } from '$lib/setup';

  const stores = storesQuery();
  const current = $derived(stores.data?.find((s) => s.id === store.id));
  const name = $derived(current?.name ?? 'Personal');
  const isApp = $derived(current?.kind === 'app');
  let tab = $state<Tab>('claude-code');
  const s = $derived(session.value);
  // At a local install's public address the browser may be on another machine: every tab shows the address form.
  const addressForm = $derived(!!session.server || session.remoteLocal);
  const steps = $derived(
    s
      ? setupSteps({
          tab,
          app: isApp ? name : null,
          addressForm,
          signInOn: !!session.signInOn,
          publicUrl: s.publicUrl,
          mcpUrl: s.mcpUrl,
          version: s.version,
          keysElsewhere: session.remoteLocal,
        })
      : [],
  );
  const lede = $derived(
    isApp
      ? 'Agents reach an app with an API key. Sign-in connections always reach your personal store.'
      : !addressForm
        ? 'Agents on this machine start Knoggin themselves and reach your personal store.'
        : session.signInOn
          ? 'Agents connect to the server address and sign in as you. They reach your personal store.'
          : 'Agents connect to the server address. Sign-in is off, so a connection reaches the personal store without signing in.',
  );
  const addr = $derived(
    isApp ? (addressForm ? { label: 'API address', value: s?.publicUrl ?? '' } : null) : { label: addressForm ? 'Server address' : 'Local command', value: addressForm ? (s?.mcpUrl ?? '') : 'knoggin mcp' },
  );
  const link = (l: string) => session.docs(l === 'Sign-in guide' || l === 'sign-in guide' ? 'sign-in.md' : 'connect-agents.md');
</script>

<Screen title="Set up an agent">
  <PageHeader title="Set up an agent" meta={name} />
  <p class="lede">{lede}</p>
  {#if addr}
    <div class="field narrow-field">
      <span class="label" id="setup-addr">{addr.label}</span>
      <CopyField value={addr.value} label={addr.label} />
    </div>
  {/if}
  {#if !isApp}
    <div class="toolbar">
      <div class="segmented start" role="group" aria-label="Agent">
        {#each TABS as t (t.id)}<button type="button" aria-pressed={tab === t.id} onclick={() => (tab = t.id)}>{t.name}</button>{/each}
      </div>
      <a class="small" href={session.docs('connect-agents.md')}>More agents</a>
    </div>
  {/if}
  <section class="card" aria-label="Setup steps">
    <ol class="steps">
      {#each steps as st, i (i)}
        <li>
          <span>
            {#if st.keysElsewhere}{KEYS_ELSEWHERE}{:else}{st.text}{#if st.link}<a href={link(st.link)}>{st.link}</a>{st.linkAfter ?? ''}{/if}{/if}
          </span>
          {#if st.code}<CopyField value={st.code} />{/if}
          {#if st.note}<span class="hint">{st.note}</span>{/if}
          {#if st.keysLink}<a class="btn-secondary btn-small start" href="/keys">Open API keys</a>{/if}
        </li>
      {/each}
    </ol>
  </section>
  {#if !isApp}<p class="caption narrow-field">Connecting a bot or a script instead? Create an app, then use one of its API keys.</p>{/if}
</Screen>

<style>
  .narrow-field {
    max-width: var(--form-w);
  }
  .start {
    align-self: flex-start;
    justify-self: start;
  }
</style>
