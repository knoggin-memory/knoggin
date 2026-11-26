import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vitest/config';

// `pnpm dev` proxies the API to a running `recalld ui --port 7690`. Open the
// dev server at http://127.0.0.1:5173 after visiting recalld's printed link
// once, so the (host-scoped) session cookie is present.
export default defineConfig({
  plugins: [
    sveltekit({
      // A single-page app: recalld serves index.html for every non-API route.
      adapter: adapter({ pages: 'dist', assets: 'dist', fallback: 'index.html', strict: true }),
      files: { assets: 'static' },
      alias: { $lib: 'src/lib' },
      output: { bundleStrategy: 'split' },
      version: { pollInterval: 0 },
    }),
  ],
  server: {
    host: '127.0.0.1',
    fs: { allow: ['..'] },
    proxy: { '/api/ui': { target: 'http://127.0.0.1:7690', changeOrigin: true } },
  },
  build: { target: 'es2022', reportCompressedSize: true },
  worker: { format: 'es' },
  test: { include: ['src/**/*.test.ts', 'tests/unit/**/*.test.ts'], environment: 'node' },
});
