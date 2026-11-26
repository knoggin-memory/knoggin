// Messages between the map and its layout worker.
export type ToWorker =
  | {
      type: 'init';
      n: number;
      src: Uint32Array;
      dst: Uint32Array;
      weight: Float32Array;
      degree: Uint32Array;
      x: Float32Array;
      y: Float32Array;
      /** 1 where the position came from the cache. */
      known: Uint8Array;
      maxDegree: number;
      /** Positions came from another view: settle slowly, as if in thick fluid. */
      reflow: boolean;
    }
  | { type: 'drag'; phase: 'start' | 'move' | 'end'; index: number; x: number; y: number }
  /** A memory stored while the map is open; it takes the next index. */
  | { type: 'add'; x: number; y: number; degree: number; links: { to: number; w: number }[] }
  /** A memory forgotten while the map is open: its neighbors close the gap. */
  | { type: 'remove'; index: number }
  /** The tab is hidden / visible again: hold the simulation meanwhile. */
  | { type: 'pause' }
  | { type: 'resume' }
  | { type: 'stop' };

export type FromWorker =
  | { type: 'tick'; x: Float32Array; y: Float32Array; alpha: number }
  | { type: 'settled'; x: Float32Array; y: Float32Array };

let maxDegree = 1;
/** Size nodes relative to this graph's most-connected memory. */
export function setDegreeScale(max: number) {
  maxDegree = Math.max(1, max);
}
/** 0 for an unlinked memory, 1 for the most-connected one (log-scaled). */
export const hubness = (degree: number) => Math.log(1 + degree) / Math.log(1 + maxDegree);
/** Radius for the physics at zoom 1: links only, so a memory fading doesn't
 *  reshuffle the layout (DESIGN §6 used 2.4 + 1.9·ln(1+degree); widened on review). */
export const nodeSize = (degree: number) => 2.5 + 9.5 * Math.pow(Math.min(1, hubness(degree)), 1.6);
/** Drawn radius: 60% how connected a memory is, 40% how strong it is now
 *  (retrievability, 0..1), so a hub that is fading shrinks and a strong
 *  memory with few links still reads. */
export const displaySize = (degree: number, strength: number) =>
  DRAW * (2.5 + 9.5 * Math.pow(0.6 * Math.min(1, hubness(degree)) + 0.4 * Math.min(1, Math.max(0, strength)), 1.6));
/** Drawn radius per size unit, in layout units. Kept below the layout's
 *  collision radius (r·2.5 + 10), so drawn nodes can never overlap. */
export const DRAW = 2.5;
