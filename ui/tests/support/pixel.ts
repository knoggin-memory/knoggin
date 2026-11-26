import { inflateSync } from 'node:zlib';

/** RGB of the centre pixel of a small PNG screenshot (8-bit RGB or RGBA). */
export function centrePixel(png: Buffer): [number, number, number] {
  let i = 8, w = 0, h = 0, channels = 4;
  const idat: Buffer[] = [];
  while (i < png.length) {
    const len = png.readUInt32BE(i);
    const type = png.toString('ascii', i + 4, i + 8);
    const data = png.subarray(i + 8, i + 8 + len);
    if (type === 'IHDR') {
      w = data.readUInt32BE(0);
      h = data.readUInt32BE(4);
      channels = data[9] === 6 ? 4 : 3;
    } else if (type === 'IDAT') idat.push(data);
    i += 12 + len;
  }
  const raw = inflateSync(Buffer.concat(idat));
  const stride = w * channels;
  let prev = new Uint8Array(stride);
  let row = new Uint8Array(stride);
  for (let y = 0, pos = 0; y < h; y++, pos += stride + 1) {
    const filter = raw[pos];
    row = new Uint8Array(raw.subarray(pos + 1, pos + 1 + stride));
    for (let x = 0; x < stride; x++) {
      const a = x >= channels ? row[x - channels] : 0, b = prev[x], c = x >= channels ? prev[x - channels] : 0;
      const p = a + b - c, pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
      const pred = [0, a, b, (a + b) >> 1, pa <= pb && pa <= pc ? a : pb <= pc ? b : c][filter];
      row[x] = (row[x] + pred) & 255;
    }
    if (y === Math.floor(h / 2)) {
      const o = Math.floor(w / 2) * channels;
      return [row[o], row[o + 1], row[o + 2]];
    }
    prev = row;
  }
  throw new Error('empty image');
}
