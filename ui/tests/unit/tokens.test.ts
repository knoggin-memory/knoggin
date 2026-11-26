// "Values come from tokens" (ui/CLAUDE.md): component styles may not use raw
// colors, font names or pixel sizes; those live in docs/ui/tokens.css.
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
const styled = walk(root).filter((f) => f.endsWith('.svelte') || f.endsWith('.css'));

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
    expect(css.match(/\b\d+(\.\d+)?px\b/g) ?? [], 'raw pixel sizes').toEqual([]);
    expect(css.match(/Newsreader|Instrument|Plex|Georgia|system-ui/g) ?? [], 'font names').toEqual([]);
  });
});
