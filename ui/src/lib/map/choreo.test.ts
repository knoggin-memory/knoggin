// SPDX-License-Identifier: AGPL-3.0-only
import { describe, expect, it } from 'vitest';
import { choreograph, HOP_GAP_MS } from './choreo';
import type { UiEvent } from '$lib/live.svelte';

const pos = (id: string) => ({ a: { x: 0, y: 0 }, b: { x: 120, y: 0 }, c: { x: 240, y: 0 }, d: { x: 120, y: 120 } })[id] ?? null;
const colors = { agent: [0, 1, 0] as [number, number, number], lit: [0, 1, 0] as [number, number, number], moss: [1, 1, 0] as [number, number, number], cool: [0, 0, 1] as [number, number, number] };

const recall: UiEvent = {
  kind: 'recall', seq: 1, at: 0, client: 'claude-code', namespace: 'default',
  seeds: [{ id: 'a', score: 0.9 }],
  spreads: [
    { from: 'b', to: 'c', edgeType: 2, amount: 0.2, order: 1 },
    { from: 'a', to: 'b', edgeType: 5, amount: 0.4, order: 0 },
    { from: 'a', to: 'd', edgeType: 7, amount: 0.3, order: 0 },
  ],
  returned: ['a', 'c'],
  suppressed: ['d'],
};

describe('recall light', () => {
  it('fires pulses in PQSA order, one hop at a time', () => {
    const c = choreograph(recall, pos, 1000, colors);
    expect(c.pulses.map((p) => [p.x0, p.x1])).toEqual([[0, 120], [120, 240]]);
    expect(c.pulses[1].start - c.pulses[0].start).toBe(HOP_GAP_MS);
    expect(c.pulses[0].brightness).toBeGreaterThan(c.pulses[1].brightness);
  });
  it('never sends light along supersedes edges', () => {
    const c = choreograph(recall, pos, 0, colors);
    expect(c.pulses.some((p) => p.x1 === 120 && p.y1 === 120)).toBe(false);
  });
  it('blooms seeds, rings results (moss for linked) and cools suppressed', () => {
    const c = choreograph(recall, pos, 0, colors);
    expect(c.glows[0].start).toBe(0);
    expect(c.rings).toEqual([
      expect.objectContaining({ id: 'a', linked: false }),
      expect.objectContaining({ id: 'c', linked: true }),
    ]);
    expect(c.suppressed.map((s) => s.id)).toEqual(['d']);
    expect(c.labels).toEqual(['a', 'c']);
  });
  it('reduced motion flashes without travel', () => {
    const c = choreograph(recall, pos, 0, colors, { reducedMotion: true });
    expect(c.pulses).toEqual([]);
    expect(c.glows.length).toBeGreaterThan(1);
  });
  it('replay slows everything down', () => {
    const slow = choreograph(recall, pos, 0, colors, { speed: 3 });
    expect(slow.pulses[1].start).toBe(3 * HOP_GAP_MS);
  });
  it('draws only the spreads that carry most of the activation', () => {
    const spreads = [{ from: 'a', to: 'b', edgeType: 2, amount: 0.9, order: 0 }, ...Array.from({ length: 20 }, (_, i) => ({ from: 'a', to: 'c', edgeType: 2, amount: 0.01, order: i + 1 }))];
    const c = choreograph({ ...recall, spreads }, pos, 0, colors);
    expect(c.pulses.length).toBe(1);
  });
  it('marks what the light touches so the rest of the map can dim', () => {
    const c = choreograph(recall, pos, 0, colors);
    expect(new Set(c.lit.nodes)).toEqual(new Set(['a', 'b', 'c']));
    expect(c.lit.edges).toEqual([['a', 'b'], ['b', 'c']]);
    expect(c.lit.until).toBeGreaterThan(Math.max(...c.pulses.map((p) => p.start + p.duration)));
  });
});
