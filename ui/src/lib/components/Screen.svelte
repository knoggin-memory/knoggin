<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- The main column every view shares. It holds the one waiting indicator that
     applies: locked, updating and waking take the screen; the
     others are banners above a screen that stays usable. The short notice for
     `rate_limited` can show beside a banner. -->
<script lang="ts">
  import type { Snippet } from 'svelte';
  import Banner from './Banner.svelte';
  import CopyField from './CopyField.svelte';
  import WaitState from './WaitState.svelte';
  import { status, WHOLE_SCREEN } from '$lib/status.svelte';
  import { session } from '$lib/session.svelte';
  import { containersPerUser, lockSentence, modelCause, plural } from '$lib/sentences';
  import { count } from '$lib/format';

  let { title, loose = false, children }: { title: string; loose?: boolean; children: Snippet } = $props();

  const ind = $derived(status.indicator);
  const whole = $derived(ind !== null && WHOLE_SCREEN.has(ind.kind));
  let now = $state(Date.now());
  $effect(() => {
    if (!status.rateLimitedUntil) return;
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });
  const limitedFor = $derived(status.rateLimitedUntil ? Math.ceil((status.rateLimitedUntil - now) / 1000) : 0);
  const idleMinutes = $derived(session.value?.idleSeconds ? Math.round(session.value.idleSeconds / 60) : null);
  const mb = (b: number) => Math.round(b / 1_000_000);
</script>

<main class="screen" class:loose>
  {#if ind?.kind === 'not-running'}
    {#if ind.remote}
      <Banner role="alert" title="Knoggin cannot be reached." text="It may not be running on the machine that hosts it." />
    {:else}
      <Banner role="alert" title="Knoggin is not running." text="Run this to start it and open a new page:">
        {#snippet action()}<CopyField value="knoggin open" />{/snippet}
      </Banner>
    {/if}
  {:else if ind?.kind === 'unavailable'}
    {#if ind.reason === 'out_of_memory'}
      <Banner warn title="Temporarily unavailable." text="This store ran out of memory several times in a row. Knoggin tries again every minute." />
    {:else if ind.reason === 'crashed'}
      <Banner warn title="Temporarily unavailable." text="This store stopped unexpectedly several times in a row. Knoggin tries again every minute." />
    {:else if ind.exhausted}
      <Banner warn title="Temporarily unavailable.">
        {#snippet action()}<button type="button" class="btn-secondary btn-small" onclick={() => status.retryNow()}>Retry</button>{/snippet}
      </Banner>
    {:else}
      <Banner warn title="Temporarily unavailable. Trying again." />
    {/if}
  {:else if ind?.kind === 'full'}
    <Banner warn title="This store is full." text="Agents cannot save new memories. Forgetting memories makes room, and whoever runs this server can raise the limit." />
  {:else if ind?.kind === 'model-unavailable'}
    <Banner warn title="The model is not available." text="{modelCause(ind.reason)}Saving and keyword search already work. Knoggin tries again by itself." />
  {:else if ind?.kind === 'downloading'}
    {@const pct = ind.total ? Math.round((ind.done / ind.total) * 100) : 0}
    <Banner title="Downloading the model." text="Saving and keyword search already work. Search by meaning starts when the download is done.">
      <div class="progress" role="progressbar" aria-label="Model download" aria-valuemin="0" aria-valuemax="100" aria-valuenow={pct}><span style:width="{pct}%"></span></div>
      <span class="path">{mb(ind.done)} of {mb(ind.total)} MB · {ind.name}</span>
    </Banner>
  {:else if ind?.kind === 'catching-up'}
    <Banner title="Catching up: {count(ind.pending)} {plural(ind.pending, 'memory', 'memories')} left to index." />
  {/if}
  {#if limitedFor > 0}
    {#if status.rateLimitedReason === 'containers_per_user'}
      <!-- Every store the UI lists is the reader's own, so the owner reads this. -->
      <Banner warn title={containersPerUser(true)} />
    {:else}
      <Banner warn title="Too many requests. Try again in {limitedFor} {plural(limitedFor, 'second', 'seconds')}." />
    {/if}
  {/if}

  {#if whole}
    <header class="header"><div class="title"><h1>{title}</h1></div></header>
    {#if ind?.kind === 'waking'}
      <WaitState title="Waking your store" breathing bar="indeterminate">
        {idleMinutes ? `Stores are parked after ${idleMinutes} idle minutes. This takes a few seconds.` : 'This takes a few seconds.'}
      </WaitState>
    {:else if ind?.kind === 'updating'}
      <WaitState title="Knoggin is updating" bar="indeterminate" detail={ind.retryIn ? `trying again in ${ind.retryIn} s` : undefined}>
        Your store is moving to the new version. This page carries on by itself.
      </WaitState>
    {:else if ind?.kind === 'locked'}
      {@const s = lockSentence(ind.reason, !!session.server)}
      <WaitState title="This store is locked">
        {s.text}{#if s.guide}<a href={session.docs('local-install.md')}>recovery guide</a>.{/if}
      </WaitState>
    {/if}
  {:else}
    {@render children()}
  {/if}
</main>
