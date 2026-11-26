// SPDX-License-Identifier: AGPL-3.0-only
import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vitest/config';

// `pnpm dev` proxies the API to a running Knoggin at KNOGGIN_DEV_API: the
// local web address that `knoggin open --print-url` prints (without `/?t=...`);
// open the dev server with that URL's `?t=<code>`.
const api = process.env.KNOGGIN_DEV_API ?? 'http://127.0.0.1:7690';

export default defineConfig({
  plugins: [
    sveltekit({
      // A single-page app: Knoggin serves index.html for every path that is not a file or the API.
      adapter: adapter({ pages: 'dist', assets: 'dist', fallback: 'index.html', strict: true }),
      files: { assets: 'static' },
      alias: { $lib: 'src/lib' },
      output: { bundleStrategy: 'split' },
      version: { pollInterval: 0 },
    }),
  ],
  server: {
    host: '127.0.0.1',
    // The daemon allows only its own origin; the dev server is another one, so the header goes.
    proxy: Object.fromEntries(
      ['/v1', '/auth'].map((p) => [p, { target: api, changeOrigin: true, configure: (proxy) => proxy.on('proxyReq', (req) => req.removeHeader('origin')) }]),
    ),
  },
  build: { target: 'es2022', reportCompressedSize: true },
  worker: { format: 'es' },
  test: { include: ['src/**/*.test.ts', 'tests/unit/**/*.test.ts'], environment: 'node', maxWorkers: 2 },
});
