// SPDX-License-Identifier: AGPL-3.0-only
// Polling stops while the tab is hidden and starts again when it is back.
import { afterAll, beforeAll, expect, it, vi } from 'vitest';

const doc = Object.assign(new EventTarget(), { hidden: false });
const fetch = vi.fn(async () => Response.json({ error: { code: 'store_locked', message: 'locked', reason: 'state_missing' } }, { status: 503 }));
const show = (hidden: boolean) => {
  doc.hidden = hidden;
  doc.dispatchEvent(new Event('visibilitychange'));
};

beforeAll(() => {
  vi.stubGlobal('document', doc);
  vi.stubGlobal('fetch', fetch);
  vi.useFakeTimers();
});
afterAll(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

it('the whole-install lock poll and the not-running poll wait while the tab is hidden', async () => {
  const { session } = await import('./session.svelte');
  const { status } = await import('./status.svelte');
  status.start();

  await session.load();
  expect(session.phase).toBe('locked');
  expect(fetch).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(60_000);
  expect(fetch).toHaveBeenCalledTimes(2);

  show(true);
  await vi.advanceTimersByTimeAsync(300_000);
  expect(fetch).toHaveBeenCalledTimes(2);
  show(false);
  await vi.advanceTimersByTimeAsync(0);
  expect(fetch).toHaveBeenCalledTimes(3);

  // "Knoggin is not running" asks the session route every minute, but not while hidden.
  (status as unknown as { markNotRunning(): void }).markNotRunning();
  show(true);
  const before = fetch.mock.calls.length;
  await vi.advanceTimersByTimeAsync(300_000);
  expect(fetch.mock.calls.length).toBe(before);
  show(false);
  await vi.advanceTimersByTimeAsync(60_000);
  expect(fetch.mock.calls.length).toBeGreaterThan(before);
});
