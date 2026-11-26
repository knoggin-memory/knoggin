// SPDX-License-Identifier: AGPL-3.0-only
// "Values come from tokens": component styles may not use raw colors, font
// names or pixel sizes; those live in src/tokens.css. app.css is the
// prototype's component sheet, which keeps a few layout-only numbers literal
// (the handoff, "Open questions"), so it is checked for colours and fonts only.
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import { describe, expect, it } from 'vitest';

const root = join(import.meta.dirname, '../../src');
function walk(d: string): string[] {
  return readdirSync(d).flatMap((f) => {
    const p = join(d, f);
    return statSync(p).isDirectory() ? walk(p) : [p];
  });
}
const styled = walk(root).filter((f) => f.endsWith('.svelte') || (f.endsWith('.css') && !f.endsWith('tokens.css')));

function styles(file: string): string {
  const text = readFileSync(file, 'utf8');
  if (file.endsWith('.css')) return text.replace(/@import[^;]+;/g, '');
  return [...text.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/g)].map((m) => m[1]).join('\n');
}

describe('styles use tokens only', () => {
  it.each(styled.map((f) => relative(root, f)))('%s', (rel) => {
    // Media-query conditions cannot use custom properties; breakpoints are exempt.
    const css = styles(join(root, rel))
      .replace(/\/\*[\s\S]*?\*\//g, '')
      .replace(/@media[^{]*\{/g, '@media {');
    expect(css.match(/#[0-9a-fA-F]{3,8}\b/g) ?? [], 'raw colors').toEqual([]);
    expect(css.match(/rgba?\(/g) ?? [], 'raw colors').toEqual([]);
    if (rel !== 'app.css') expect(css.match(/\b\d+(\.\d+)?px\b/g) ?? [], 'raw pixel sizes').toEqual([]);
    expect(css.match(/Newsreader|Instrument|Plex|Georgia|system-ui/g) ?? [], 'font names').toEqual([]);
  });
});

describe('no inline styles in markup', () => {
  // The content security policy allows no inline styles: `style:` directives
  // (set from script) are fine, a static `style="..."` attribute is not.
  it.each(walk(root).filter((f) => f.endsWith('.svelte') || f.endsWith('.html')).map((f) => relative(root, f)))('%s', (rel) => {
    const markup = readFileSync(join(root, rel), 'utf8').replace(/<style[^>]*>[\s\S]*?<\/style>/g, '');
    expect(markup.match(/\sstyle=/g) ?? []).toEqual([]);
  });
});
