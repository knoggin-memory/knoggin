// Committed icon assets must carry no embedded metadata (C2PA manifests,
// ancillary PNG chunks) and SVGs must stay small and drawing-only.
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

const dir = join(import.meta.dirname, '../../static');
const files = readdirSync(dir);

function pngChunks(buf: Buffer): string[] {
  const out: string[] = [];
  for (let i = 8; i < buf.length; ) {
    const len = buf.readUInt32BE(i);
    out.push(buf.subarray(i + 4, i + 8).toString('latin1'));
    i += 12 + len;
  }
  return out;
}

describe('ui/static', () => {
  it('contains the full icon set', () => {
    for (const f of ['logo.svg', 'favicon.svg', 'favicon.ico', 'favicon-16.png', 'favicon-32.png', 'apple-touch-icon.png'])
      expect(files).toContain(f);
  });
  it.each(files)('%s has no c2pa content', (f) => {
    expect(readFileSync(join(dir, f)).toString('latin1').toLowerCase()).not.toContain('c2pa');
  });
  it.each(files.filter((f) => f.endsWith('.svg')))('%s is under 1 KB with no <metadata>', (f) => {
    const text = readFileSync(join(dir, f), 'utf8');
    expect(Buffer.byteLength(text)).toBeLessThan(1024);
    expect(text).not.toMatch(/<metadata/i);
  });
  it('favicon.svg keeps its dark-mode style', () => {
    expect(readFileSync(join(dir, 'favicon.svg'), 'utf8')).toContain('prefers-color-scheme: dark');
  });
  it.each(files.filter((f) => f.endsWith('.png')))('%s has only critical chunks', (f) => {
    const chunks = pngChunks(readFileSync(join(dir, f)));
    expect(chunks.every((c) => ['IHDR', 'PLTE', 'IDAT', 'IEND'].includes(c))).toBe(true);
    for (const bad of ['caBX', 'iTXt', 'tEXt', 'zTXt']) expect(chunks).not.toContain(bad);
  });
  it('favicon.ico holds the cleaned 16 and 32 px PNGs', () => {
    const ico = readFileSync(join(dir, 'favicon.ico'));
    const count = ico.readUInt16LE(4);
    expect(count).toBe(2);
    for (let k = 0; k < count; k++) {
      const size = ico.readUInt32LE(6 + 16 * k + 8);
      const offset = ico.readUInt32LE(6 + 16 * k + 12);
      const png = ico.subarray(offset, offset + size);
      const expected = readFileSync(join(dir, k === 0 ? 'favicon-16.png' : 'favicon-32.png'));
      expect(Buffer.compare(png, expected)).toBe(0);
    }
  });
});
