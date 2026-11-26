// SPDX-License-Identifier: AGPL-3.0-only
import { afterAll, expect, it, vi } from 'vitest';
import { sget, token } from './api/client';
import { connectEvents } from './events';

afterAll(() => vi.unstubAllGlobals());

it('the event stream sends the in-memory token when storage is blocked', async () => {
  const blocked = () => {
    throw new DOMException('blocked', 'SecurityError');
  };
  vi.stubGlobal('localStorage', { getItem: blocked, setItem: blocked, removeItem: blocked });
  const fetch = vi.fn(async (url: string) => (url === '/v1/ui/events' ? new Response(null, { status: 204 }) : Response.json({ items: [] })));
  vi.stubGlobal('fetch', fetch);

  token.set('knoggin_session_abc');
  const stop = connectEvents(() => 'store-1', () => {});
  // The stream opens after the first store request that succeeds.
  await sget('/v1/namespaces');
  await vi.waitFor(() => expect(fetch.mock.calls.some(([u]) => u === '/v1/ui/events')).toBe(true));
  const [, init] = fetch.mock.calls.find(([u]) => u === '/v1/ui/events') as unknown as [string, RequestInit];
  expect(new Headers(init.headers).get('Authorization')).toBe('Bearer knoggin_session_abc');
  stop();
});
