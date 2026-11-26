// SPDX-License-Identifier: AGPL-3.0-only
import { describe, expect, it } from 'vitest';
import { overlaps, placeLabels, PointGrid, type LabelCandidate } from './labels';

function rng(seed: number) {
  return () => ((seed = (seed * 1664525 + 1013904223) >>> 0) / 2 ** 32);
}

describe('placeLabels', () => {
  it('never overlaps, never leaves the viewport', () => {
    const rand = rng(7);
    const vp = { width: 800, height: 600 };
    for (let trial = 0; trial < 50; trial++) {
      const cands: LabelCandidate[] = Array.from({ length: 80 }, (_, i) => ({
        id: String(i),
        x: rand() * vp.width,
        y: rand() * vp.height,
        r: 3 + rand() * 6,
        width: 40 + rand() * 120,
        height: 15,
      }));
      const placed = placeLabels(cands, vp, undefined, 4);
      for (const p of placed) {
        expect(p.x).toBeGreaterThanOrEqual(0);
        expect(p.y).toBeGreaterThanOrEqual(0);
        expect(p.x + p.width).toBeLessThanOrEqual(vp.width);
        expect(p.y + p.height).toBeLessThanOrEqual(vp.height);
      }
      for (let i = 0; i < placed.length; i++)
        for (let j = i + 1; j < placed.length; j++) expect(overlaps(placed[i], placed[j], 4)).toBe(false);
    }
  });

  it('places in priority order and drops what does not fit', () => {
    const vp = { width: 200, height: 100 };
    const c = (id: string): LabelCandidate => ({ id, x: 100, y: 50, r: 4, width: 90, height: 14 });
    const placed = placeLabels([c('seed'), c('result'), c('hover'), c('x'), c('y'), c('z'), c('w')], vp);
    expect(placed[0].id).toBe('seed');
    expect(placed[0].y).toBeGreaterThan(50); // below first
    expect(placed.length).toBeLessThan(7);
  });

  it('prefers the spot covering the fewest nodes', () => {
    const vp = { width: 400, height: 400 };
    // Crowd the area below the node so "right" wins.
    const xs = [200, 195, 205, 200], ys = [200, 215, 215, 222];
    const grid = new PointGrid(xs, ys, 16);
    const [p] = placeLabels([{ id: 'a', x: 200, y: 200, r: 4, width: 50, height: 12 }], vp, grid);
    expect(p.x).toBeGreaterThan(200);
  });
});
