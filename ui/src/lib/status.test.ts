// SPDX-License-Identifier: AGPL-3.0-only
import { describe, expect, it } from 'vitest';
import { pick, pollDelay } from './status.svelte';
import type { StoreStatus } from './api/shapes';

const ready: StoreStatus = { state: 'ready', lockedReason: null, full: false, model: { state: 'ready', identity: 'fake-64', name: 'Fake model' }, embedding: { pending: 0, total: 3 } };
const base = { notRunning: false, remote: false, locked: null, upgrading: undefined, unavailable: null, waking: false, full: false, status: ready };

describe('one waiting indicator at a time, in order', () => {
  it('shows nothing for a ready store', () => expect(pick(base)).toBeNull());
  it('puts Knoggin is not running first', () => {
    expect(pick({ ...base, notRunning: true, locked: 'format_too_new', full: true })).toEqual({ kind: 'not-running', remote: false });
  });
  it('then locked, updating, unavailable, waking, full', () => {
    expect(pick({ ...base, locked: 'snapshot_unreadable', upgrading: 5 })?.kind).toBe('locked');
    expect(pick({ ...base, upgrading: 5, unavailable: { reason: null, attempts: 1 } })?.kind).toBe('updating');
    expect(pick({ ...base, unavailable: { reason: null, attempts: 1 }, waking: true })?.kind).toBe('unavailable');
    expect(pick({ ...base, waking: true, full: true })?.kind).toBe('waking');
    expect(pick({ ...base, full: true, status: { ...ready, embedding: { pending: 4, total: 9 } } })?.kind).toBe('full');
  });
  it('then the model banners and catching up', () => {
    const m = (state: 'downloading' | 'unavailable') => ({ ...ready, model: { state, identity: 'x', name: 'EmbeddingGemma 2 · q4', bytesDone: 10, bytesTotal: 20, reason: 'offline' as const }, embedding: { pending: 4, total: 9 } });
    expect(pick({ ...base, status: m('unavailable') })).toEqual({ kind: 'model-unavailable', reason: 'offline' });
    expect(pick({ ...base, status: m('downloading') })).toEqual({ kind: 'downloading', done: 10, total: 20, name: 'EmbeddingGemma 2 · q4' });
    expect(pick({ ...base, status: { ...ready, embedding: { pending: 4, total: 9 } } })).toEqual({ kind: 'catching-up', pending: 4 });
  });
  it('the model state loading shows no banner', () => {
    expect(pick({ ...base, status: { ...ready, model: { state: 'loading', identity: 'x', name: 'x' } } })).toBeNull();
  });
  it('gives up retrying an unavailable store after five tries, but not one that keeps stopping', () => {
    expect(pick({ ...base, unavailable: { reason: null, attempts: 5 } })).toMatchObject({ exhausted: true });
    expect(pick({ ...base, unavailable: { reason: null, attempts: 4 } })).toMatchObject({ exhausted: false });
    expect(pick({ ...base, unavailable: { reason: 'crashed', attempts: 9 } })).toMatchObject({ exhausted: false, reason: 'crashed' });
  });
});

describe('store status polling', () => {
  it('every 2 s while waking or downloading, 5 s while catching up, 60 s while locked, else not at all', () => {
    expect(pollDelay(ready, false)).toBeNull();
    expect(pollDelay(ready, true)).toBe(2_000);
    expect(pollDelay({ ...ready, state: 'waking' }, false)).toBe(2_000);
    expect(pollDelay({ ...ready, model: { state: 'downloading', identity: 'x', name: 'x' } }, false)).toBe(2_000);
    expect(pollDelay({ ...ready, embedding: { pending: 1, total: 2 } }, false)).toBe(5_000);
    expect(pollDelay({ ...ready, state: 'locked', lockedReason: 'format_too_new' }, false)).toBe(60_000);
    expect(pollDelay(null, false)).toBeNull();
  });
});
