// Map positions cached per memory in IndexedDB (the only thing the UI
// stores: no memory content). One entry per scope: ids → x, y.
import { get, set } from 'idb-keyval';

// Bump the version when the layout forces change, so old layouts are redone.
const key = (scope: string | null) => `positions:v8:${scope ?? '*'}`;

export async function loadPositions(scope: string | null): Promise<Map<string, [number, number]>> {
  try {
    const stored = (await get(key(scope))) as { ids: string[]; xy: Float32Array } | undefined;
    const out = new Map<string, [number, number]>();
    if (stored) stored.ids.forEach((id, i) => out.set(id, [stored.xy[2 * i], stored.xy[2 * i + 1]]));
    return out;
  } catch {
    return new Map();
  }
}

export async function savePositions(scope: string | null, ids: string[], x: Float32Array, y: Float32Array) {
  const xy = new Float32Array(ids.length * 2);
  for (let i = 0; i < ids.length; i++) {
    xy[2 * i] = x[i];
    xy[2 * i + 1] = y[i];
  }
  try {
    await set(key(scope), { ids, xy });
  } catch {
    /* storage unavailable: the next visit lays out again */
  }
}
