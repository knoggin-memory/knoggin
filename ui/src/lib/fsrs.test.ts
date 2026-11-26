import { describe, expect, it } from 'vitest';
import fixture from './fsrs.fixture.json';
import { daysUntil, PERMASTORE_DAYS, phaseFor, retrievability, THRESHOLDS } from './fsrs';

// fsrs.fixture.json is generated from src/decay/fsrs.rs by
// `cargo test --features ui ui::wire` (and fails CI when stale).
describe('fsrs.ts matches recalld', () => {
  it('uses recalld’s thresholds', () => {
    THRESHOLDS.forEach((t, i) => expect(t).toBeCloseTo(fixture.thresholds[i], 6));
    expect(PERMASTORE_DAYS).toBeCloseTo(fixture.permastoreDays, 3);
  });
  it('retrievability R(t, S, multiplier)', () => {
    for (const [t, s, m, r] of fixture.retrievability) expect(retrievability(t, s, m)).toBeCloseTo(r, 5);
  });
  it('days until a threshold', () => {
    for (const [s, target, days] of fixture.daysUntil) {
      expect(daysUntil(s, target) / days).toBeCloseTo(1, 4);
    }
  });
  it('maps R to phases at the thresholds', () => {
    expect(phaseFor(0.71)).toBe(1);
    expect(phaseFor(0.7)).toBe(2);
    expect(phaseFor(0.3)).toBe(3);
    expect(phaseFor(0.05)).toBe(4);
  });
});
