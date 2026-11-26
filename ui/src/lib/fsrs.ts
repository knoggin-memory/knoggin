// recalld's forgetting curve, mirrored from src/decay/fsrs.rs. Unit-tested
// against values computed by the Rust implementation (fsrs.test.ts).
export const FACTOR = 19 / 81;
export const DECAY = -0.5;
export const DAY_MS = 86_400_000;

/** Default phase thresholds (Full→Summary, Summary→Ghost, Ghost→deletion). */
export const THRESHOLDS: [number, number, number] = [0.7, 0.3, 0.05];
export const PERMASTORE_DAYS = 1500;

/** R(t, S) = (1 + 19/81 · t/S)^-0.5, with recalld's multiplier on elapsed time.
 *  Pass the namespace's effective `decayMultiplier`, as the sweep applies it. */
export function retrievability(elapsedDays: number, stability: number, multiplier: number): number {
  if (elapsedDays <= 0 || multiplier === 0) return 1;
  if (stability <= 0) return 0;
  return Math.pow(1 + (FACTOR * (elapsedDays / multiplier)) / stability, DECAY);
}

/** Days until R falls to `target` (inverse of the curve). */
export function daysUntil(stability: number, target: number): number {
  return (stability / FACTOR) * (Math.pow(target, 1 / DECAY) - 1);
}

/** Phase code (1 Full, 2 Summary, 3 Ghost, 4 deleted) for a retrievability. */
export function phaseFor(r: number, thresholds = THRESHOLDS): number {
  if (r > thresholds[0]) return 1;
  if (r > thresholds[1]) return 2;
  if (r > thresholds[2]) return 3;
  return 4;
}

/** R now (or at `at`) for a memory last accessed at `lastAccess`. */
export function strengthAt(lastAccess: number, stability: number, multiplier: number, at = Date.now()) {
  return retrievability(Math.max(0, at - lastAccess) / DAY_MS, stability, multiplier);
}
