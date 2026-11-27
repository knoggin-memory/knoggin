// SPDX-License-Identifier: AGPL-3.0-only
// Collision-free map labels: place greedily in priority order,
// trying below, right, left, above and two diagonals; skip any spot that
// overlaps a placed label (with a margin) or leaves the viewport; prefer
// the spot covering the fewest nodes; drop the label if nothing fits.

export interface LabelCandidate {
  id: string;
  /** Node center and radius in viewport pixels. */
  x: number;
  y: number;
  r: number;
  width: number;
  height: number;
}

export interface Placement {
  id: string;
  /** Top-left of the label box, viewport pixels. */
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

const GAP = 3;

export function overlaps(a: Rect, b: Rect, margin = 0): boolean {
  return (
    a.x < b.x + b.width + margin &&
    b.x < a.x + a.width + margin &&
    a.y < b.y + b.height + margin &&
    b.y < a.y + a.height + margin
  );
}

function spots(c: LabelCandidate): Rect[] {
  const { x, y, r, width: w, height: h } = c;
  const d = r + GAP;
  return [
    { x: x - w / 2, y: y + d, width: w, height: h }, // below
    { x: x + d, y: y - h / 2, width: w, height: h }, // right
    { x: x - d - w, y: y - h / 2, width: w, height: h }, // left
    { x: x - w / 2, y: y - d - h, width: w, height: h }, // above
    { x: x + d * 0.7, y: y + d * 0.7, width: w, height: h }, // below-right
    { x: x - d * 0.7 - w, y: y + d * 0.7, width: w, height: h }, // below-left
  ];
}

/** Uniform grid over node centers, for counting how many a box covers. */
export class PointGrid {
  private cells = new Map<number, number[]>();
  constructor(
    private xs: ArrayLike<number>,
    private ys: ArrayLike<number>,
    private size = 32,
  ) {
    for (let i = 0; i < xs.length; i++) {
      const k = this.key(Math.floor(xs[i] / size), Math.floor(ys[i] / size));
      let cell = this.cells.get(k);
      if (!cell) this.cells.set(k, (cell = []));
      cell.push(i);
    }
  }
  private key(cx: number, cy: number) {
    return (cx + 32768) * 65536 + (cy + 32768);
  }
  count(r: Rect): number {
    let n = 0;
    const s = this.size;
    for (let cx = Math.floor(r.x / s); cx <= Math.floor((r.x + r.width) / s); cx++) {
      for (let cy = Math.floor(r.y / s); cy <= Math.floor((r.y + r.height) / s); cy++) {
        for (const i of this.cells.get(this.key(cx, cy)) ?? []) {
          const px = this.xs[i], py = this.ys[i];
          if (px >= r.x && px <= r.x + r.width && py >= r.y && py <= r.y + r.height) n++;
        }
      }
    }
    return n;
  }
}

export function placeLabels(
  candidates: LabelCandidate[],
  viewport: { width: number; height: number },
  grid?: PointGrid,
  margin = 4,
): Placement[] {
  const placed: Placement[] = [];
  for (const c of candidates) {
    let best: Rect | null = null;
    let bestCover = Infinity;
    for (const spot of spots(c)) {
      if (spot.x < 0 || spot.y < 0 || spot.x + spot.width > viewport.width || spot.y + spot.height > viewport.height) continue;
      if (placed.some((p) => overlaps(p, spot, margin))) continue;
      const cover = grid ? grid.count(spot) : 0;
      if (cover < bestCover) {
        best = spot;
        bestCover = cover;
        if (cover === 0) break;
      }
    }
    if (best) placed.push({ id: c.id, ...best });
  }
  return placed;
}
