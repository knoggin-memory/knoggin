// SPDX-License-Identifier: AGPL-3.0-only
import { describe, expect, it } from 'vitest';
import { parseError, query } from './client';
import { SseParser } from '../events';
import { normalize } from '../live.svelte';

describe('the error shape', () => {
  it('reads code, reason and Retry-After', () => {
    const e = parseError(503, { error: { code: 'unavailable', message: 'busy', reason: 'crashed' } }, '60');
    expect([e.code, e.reason, e.retryAfter, e.message]).toEqual(['unavailable', 'crashed', 60, 'busy']);
  });
  it('treats an answer that is not the API shape as unreachable (a tunnel page)', () => {
    expect(parseError(502, null, null).code).toBe('unreachable');
    expect(parseError(530, { message: 'origin down' }, null).code).toBe('unreachable');
  });
  it('repeats list parameters and drops empty ones', () => {
    expect(query({ phase: ['full', 'ghost'], namespace: null, q: '', limit: 5 })).toBe('?phase=full&phase=ghost&limit=5');
  });
});

describe('the event stream', () => {
  it('splits events across chunks and skips keep-alive comments', () => {
    const got: [string, string][] = [];
    const p = new SseParser((n, d) => got.push([n, d]));
    p.push(': keep-alive\n\nevent: store\nda');
    p.push('ta: {"a":1}\n\nevent: lagged\r\ndata: {"kind":"lagged","missed":3}\r\n\r\n');
    expect(got).toEqual([['store', '{"a":1}'], ['lagged', '{"kind":"lagged","missed":3}']]);
  });
  it('turns names into the map’s numbers once', () => {
    const e = normalize({ kind: 'store', seq: 1, at: 0, client: null, id: 'a', namespace: 'default', phase: 'summary', edges: [{ to: 'b', edgeType: 'supersedes', weight: 1 }] });
    expect(e).toMatchObject({ phase: 2, edges: [{ edgeType: 7 }] });
    expect(normalize({ kind: 'phase_change', seq: 2, at: 0, id: 'a', from: 'full', to: 'summary' })).toMatchObject({ from: 1, to: 2 });
  });
});
