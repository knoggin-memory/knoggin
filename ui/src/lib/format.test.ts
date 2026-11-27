// SPDX-License-Identifier: AGPL-3.0-only
import { describe, expect, it } from 'vitest';
import { ago, bytes, duration, truncateWords } from './format';
import { isPaletteShortcut } from './palette.svelte';

const NOW = Date.UTC(2026, 10, 19, 12);
const MIN = 60_000;

describe('words', () => {
  it('relative times', () => {
    expect(ago(NOW - 10_000, NOW)).toBe('just now');
    expect(ago(NOW - 14 * MIN, NOW)).toBe('14m ago');
    expect(ago(NOW - 3 * 1440 * MIN, NOW)).toBe('3d ago');
    expect(ago(NOW - 15 * 1440 * MIN, NOW)).toBe('2w ago');
    expect(ago(NOW - 125 * 1440 * MIN, NOW)).toBe('4mo ago');
  });
  it('durations', () => {
    expect(duration(16)).toBe('16 d');
    expect(duration(95)).toBe('3 mo');
    expect(duration(1500)).toBe('4 yr');
  });
  it('bytes', () => {
    expect(bytes(512)).toBe('512 B');
    expect(bytes(1536)).toBe('1.5 KB');
  });
  it('truncates map labels at a word boundary', () => {
    expect(truncateWords('Connection bonus keeps well-linked memories in Full')).toBe('Connection bonus keeps…');
    expect(truncateWords('Short label')).toBe('Short label');
  });
});

describe('palette shortcuts', () => {
  const key = (k: string, opts: Partial<KeyboardEvent> = {}, tag = 'BODY') =>
    ({ key: k, metaKey: false, ctrlKey: false, altKey: false, target: { tagName: tag, isContentEditable: false }, ...opts }) as unknown as KeyboardEvent;
  it('⌘K, Ctrl+K and / open it', () => {
    expect(isPaletteShortcut(key('k', { metaKey: true }))).toBe(true);
    expect(isPaletteShortcut(key('K', { ctrlKey: true }))).toBe(true);
    expect(isPaletteShortcut(key('/'))).toBe(true);
  });
  it('/ in a text field types a slash', () => {
    expect(isPaletteShortcut(key('/', {}, 'INPUT'))).toBe(false);
    expect(isPaletteShortcut(key('k'))).toBe(false);
  });
});
