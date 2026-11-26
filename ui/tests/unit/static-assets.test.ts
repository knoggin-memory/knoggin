// SPDX-License-Identifier: AGPL-3.0-only
// The logo files are used as supplied; they must carry no embedded metadata,
// script or outside reference, because the page loads nothing from another origin.
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

const dir = join(import.meta.dirname, '../../static');
const files = readdirSync(dir);

describe('ui/static', () => {
  it('holds every logo file, light and dark', () => {
    for (const v of ['horizontal', 'mark', 'stacked']) {
      expect(files).toContain(`logo-${v}.svg`);
      expect(files).toContain(`logo-${v}-white.svg`);
    }
    expect(files).toContain('favicon.svg');
  });
  it.each(files.filter((f) => f.endsWith('.svg')))('%s is drawing only', (f) => {
    const text = readFileSync(join(dir, f), 'utf8');
    expect(text.toLowerCase()).not.toContain('c2pa');
    expect(text).not.toMatch(/<metadata|<script|<foreignObject|href=|url\(/i);
  });
});
