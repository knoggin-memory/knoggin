import { describe, expect, it } from 'vitest';
import { activeSince, projectedPhase, strengthAt, type Graph } from './snapshot';

const DAY = 86_400_000;
const now = 1_000 * DAY;

function graph(): Graph {
  // a: old, accessed recently · b: new · c: old and idle · edges a-b, b-c, a-c
  const ids = ['a', 'b', 'c'];
  return {
    generatedAt: now,
    namespaces: [],
    ids,
    index: new Map(ids.map((id, i) => [id, i])),
    ns: Uint32Array.of(1, 1, 1),
    phase: Uint8Array.of(1, 1, 2),
    strength: Float32Array.of(0.9, 0.8, 0.2),
    decayStrength: Float32Array.of(0.9, 0.8, 0.2),
    stability: Float32Array.of(5, 6, 7),
    createdAt: Float64Array.of(now - 400 * DAY, now - 2 * DAY, now - 400 * DAY),
    lastAccess: Float64Array.of(now - 1 * DAY, now - 2 * DAY, now - 300 * DAY),
    degree: Uint32Array.of(2, 2, 2),
    permastore: Uint8Array.of(0, 0, 0),
    src: Uint32Array.of(0, 1, 0),
    dst: Uint32Array.of(1, 2, 2),
    type: Uint8Array.of(2, 2, 1),
    weight: Float32Array.of(0.5, 0.6, 0.7),
  };
}

describe('activeSince', () => {
  it('keeps memories created or accessed in the window and the edges between them', () => {
    const g = activeSince(graph(), now - 90 * DAY);
    expect(g.ids).toEqual(['a', 'b']);
    expect([...g.index]).toEqual([['a', 0], ['b', 1]]);
    expect([...g.stability]).toEqual([5, 6]);
    expect([...g.degree]).toEqual([2, 2]);
    expect([...g.src]).toEqual([0]);
    expect([...g.dst]).toEqual([1]);
    expect([...g.weight]).toEqual([0.5]);
    expect(g.lastAccess).toBeInstanceOf(Float64Array);
  });

  it('returns the same graph when everything is in the window', () => {
    const g = graph();
    expect(activeSince(g, 0)).toBe(g);
  });
});

describe('decay projection', () => {
  // Two memories identical but for their namespace: `slow` decays at half speed.
  const ns = (id: number, name: string, decayMultiplier: number) =>
    ({ id, name, thresholds: [0.7, 0.3, 0.05] as [number, number, number], permastoreDays: 1500, decayMultiplier });
  const g: Graph = {
    ...graph(),
    namespaces: [ns(1, 'normal', 1), ns(2, 'slow', 2)],
    ns: Uint32Array.of(1, 2, 1),
    phase: Uint8Array.of(1, 1, 1),
    stability: Float32Array.of(3.7, 3.7, 3.7),
    lastAccess: Float64Array.of(now, now, now),
  };

  it('uses each namespace’s decay multiplier, like the sweep', () => {
    expect(strengthAt(g, 1, now + 10 * DAY)).toBeGreaterThan(strengthAt(g, 0, now + 10 * DAY));
    // Full → Summary at R 0.7: ~16.4 days at multiplier 1, ~32.8 at 2.
    expect(projectedPhase(g, 0, now + 20 * DAY)).toBe(2);
    expect(projectedPhase(g, 1, now + 20 * DAY)).toBe(1);
    expect(projectedPhase(g, 1, now + 33 * DAY)).toBe(2);
  });
});
