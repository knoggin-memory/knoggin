// SPEC §6 budgets against a synthetic 10k-memory / 100k-edge store.
// `pnpm perf` with RECALLD_BIN (built --features ui,bench, release recommended).
// Results go to perf/results/perf.json; CI runs a smaller smoke variant.
import { expect, test, type Page } from '@playwright/test';
import { execSync, spawn } from 'node:child_process';
import { mkdirSync, writeFileSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { gzipSync } from 'node:zlib';
import { join } from 'node:path';
import { startStack, type Stack } from '../tests/support/stack';
import { allNamespaces } from '../tests/support/ui';
import { DaemonSocket } from './daemon';

const smoke = !!process.env.PERF_SMOKE;
const results: Record<string, number | string | boolean> = {};
let stack: Stack;

const p95 = (xs: number[]) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(s.length * 0.95))] ?? NaN;
};

async function frameTimes(page: Page, during: () => Promise<void>): Promise<number[]> {
  await page.evaluate(() => {
    const w = window as unknown as { __frames: number[]; __rec: boolean };
    w.__frames = [];
    w.__rec = true;
    const loop = (t: number) => {
      if (!w.__rec) return;
      w.__frames.push(t);
      requestAnimationFrame(loop);
    };
    requestAnimationFrame(loop);
  });
  await during();
  const frames = await page.evaluate(() => {
    const w = window as unknown as { __frames: number[]; __rec: boolean };
    w.__rec = false;
    return w.__frames;
  });
  return frames.slice(1).map((t, i) => t - frames[i]);
}

test.describe.configure({ mode: 'serial' });

// PERF_EXTERNAL=1 drives an already running stack at RECALLD_UI_URL (e.g. an
// isolated container, so the browser can be the host's GPU Chrome) and
// triggers agent recalls with PERF_RECALL_CMD instead of the local socket.
const external = !!process.env.PERF_EXTERNAL;

test.beforeAll(async () => {
  if (external) {
    stack = { url: process.env.RECALLD_UI_URL!, dir: '', stop() {} };
    results.storeMemories = Number(process.env.PERF_MEMORIES ?? 10000);
    results.storeEdgesTarget = Number(process.env.PERF_EDGES ?? 100000);
    return;
  }
  const bin = process.env.RECALLD_BIN!;
  if (smoke) {
    // CI generates the store with a debug binary; 1k / 10k took over 10 min
    // there (past the hook timeout). 200 / 2k takes ~40 s and still
    // exercises every measurement; budgets come from the full 10k / 100k run.
    process.env.PERF_MEMORIES ??= '200';
    process.env.PERF_EDGES ??= '2000';
  }
  const t = performance.now();
  stack = await startStack(bin, 'perf');
  results.storeMemories = Number(process.env.PERF_MEMORIES ?? 10000);
  results.storeEdgesTarget = Number(process.env.PERF_EDGES ?? 100000);
  results.stackStartSeconds = (performance.now() - t) / 1000;
});

test.afterAll(async () => {
  stack?.stop();
  mkdirSync(join(import.meta.dirname, 'results'), { recursive: true });
  writeFileSync(join(import.meta.dirname, 'results', smoke ? 'perf-smoke.json' : 'perf.json'), JSON.stringify(results, null, 2));
  console.log(JSON.stringify(results, null, 2));
});

// Budgets are for the whole store, across every namespace.
test.beforeEach(({ page }) => allNamespaces(page));

test('initial-route JS bundle (gzip)', async () => {
  const dir = join(import.meta.dirname, '../dist/_app/immutable');
  const files: string[] = [];
  const walk = (d: string) => readdirSync(d).forEach((f) => (statSync(join(d, f)).isDirectory() ? walk(join(d, f)) : files.push(join(d, f))));
  walk(dir);
  const gz = (f: string) => gzipSync(readFileSync(f)).length;
  const js = files.filter((f) => f.endsWith('.js'));
  results.jsTotalGzipKB = Math.round(js.reduce((a, f) => a + gz(f), 0) / 1024);
  results.jsLargestChunkGzipKB = Math.round(Math.max(...js.map(gz)) / 1024);
});

test('launch → first paint, map open, cached reopen', async ({ page }) => {
  const url = new URL(stack.url);
  if (external) {
    // Launch timing is measured inside the stack (see PERF_LAUNCH_MS).
    results.launchToReadyMs = Number(process.env.PERF_LAUNCH_MS ?? NaN);
    await page.goto(url.toString());
    await page.goto(new URL('/map', url.origin).toString());
    const fcp = await page.evaluate(() => performance.getEntriesByName('first-contentful-paint')[0]?.startTime ?? NaN);
    results.firstPaintMs = Math.round(fcp);
    results.launchToFirstPaintMs = Math.round(results.launchToReadyMs + fcp);
    return mapTimings(page, url);
  }
  // Warm daemon: launch a second `recalld ui` and time it to first paint.
  const env = { ...process.env, HOME: join(stack.dir, 'home'), RECALLD_CONFIG: join(stack.dir, 'config.toml'), RECALLD_DAEMON_SOCKET: join(stack.dir, 'recalld.sock') };
  const t0 = performance.now();
  const ui = spawn(process.env.RECALLD_BIN!, ['--config', env.RECALLD_CONFIG, 'ui', '--no-open'], { env, stdio: ['ignore', 'ignore', 'pipe'] });
  const link = await new Promise<string>((resolve) => {
    let out = '';
    ui.stderr.on('data', (d) => {
      out += d;
      const m = out.match(/http:\/\/127\.0\.0\.1:\d+\/\?t=[0-9a-f]+/);
      if (m) resolve(m[0]);
    });
  });
  const tLink = performance.now();
  await page.goto(link);
  const fcp = await page.evaluate(() => performance.getEntriesByName('first-contentful-paint')[0]?.startTime ?? NaN);
  results.launchToFirstPaintMs = Math.round(tLink - t0 + fcp);
  ui.kill();

  await mapTimings(page, url);
});

async function mapTimings(page: Page, url: URL) {
  await page.goto(url.toString());
  await page.evaluate(() => indexedDB.deleteDatabase('keyval-store'));
  await page.goto(new URL('/map', url.origin).toString());
  await page.waitForFunction(() => performance.getEntriesByName('recalld:map-ready').length > 0, null, { timeout: 300_000 });
  results.mapFirstOpenMs = Math.round(await page.evaluate(() => performance.getEntriesByName('recalld:map-ready')[0].startTime));
  // Let the layout settle and cache positions.
  await page.waitForFunction(() => (window as unknown as { __recalldSettled?: boolean }).__recalldSettled === true, null, { timeout: 300_000 });
  await page.reload();
  await page.waitForFunction(() => performance.getEntriesByName('recalld:map-ready').length > 0, null, { timeout: 60_000 });
  results.mapCachedInteractiveMs = Math.round(await page.evaluate(() => performance.getEntriesByName('recalld:map-ready')[0].startTime));
  results.mapReopenedFromCache = await page.evaluate(() => (window as unknown as { __recalldCached?: boolean }).__recalldCached === true);
  expect(results.mapCachedInteractiveMs as number).toBeLessThan(smoke ? 5000 : 1500);
}

test('pan, zoom and drag frame times', async ({ page }) => {
  await page.goto(stack.url);
  await page.goto(new URL('/map', stack.url).toString());
  await page.waitForFunction(() => performance.getEntriesByName('recalld:map-ready').length > 0);
  await page.waitForTimeout(500);
  const box = (await page.locator('.canvas-card').boundingBox())!;
  const cx = box.x + box.width / 2, cy = box.y + box.height / 2;
  const deltas = await frameTimes(page, async () => {
    await page.mouse.move(cx, cy);
    await page.mouse.down();
    for (let i = 0; i < 60; i++) await page.mouse.move(cx + Math.sin(i / 6) * 200, cy + Math.cos(i / 6) * 120);
    await page.mouse.up();
    for (let i = 0; i < 20; i++) await page.mouse.wheel(0, i % 2 ? 120 : -120);
    await page.waitForTimeout(600);
  });
  results.panZoomFrames = deltas.length;
  results.panZoomP95FrameMs = Number(p95(deltas).toFixed(1));
});

test('hover → tooltip', async ({ page }) => {
  await page.goto(stack.url);
  await page.goto(new URL('/map', stack.url).toString());
  await page.waitForFunction(() => performance.getEntriesByName('recalld:map-ready').length > 0);
  // Hover a node that has stopped moving, at its exact page position.
  await page.waitForFunction(() => (window as unknown as { __recalldSettled?: boolean }).__recalldSettled === true);
  const pos = await page.evaluate(() => (window as unknown as { __recalldNodeAt: () => { x: number; y: number } }).__recalldNodeAt());
  const t = await page.evaluate(() => performance.now());
  await page.mouse.move(pos.x, pos.y);
  await page.locator('[role=tooltip]').waitFor();
  const shown = await page.evaluate(() => performance.now());
  results.hoverToTooltipMs = Math.round(shown - t);
  results.hoverToTooltipBeyondDelayMs = Math.round(shown - t - 120);
});

test('search keystroke → results', async ({ page }) => {
  await page.goto(stack.url);
  await page.goto(new URL('/explore', stack.url).toString());
  // The palette shortcut works once the shell is up.
  await page.getByText('read-only · daemon connected').waitFor();
  await page.keyboard.press('ControlOrMeta+k');
  const input = page.getByRole('dialog', { name: 'Search memories' }).getByRole('combobox');
  await input.waitFor();
  const times: number[] = [];
  for (const q of ['kalo', 'mira tu', 'zone pasi', 'orveka', 'dori', 'lomi ra', 'tuve', 'sika', 'ranelo', 'pado']) {
    await input.fill('');
    const before = await page.evaluate(() => performance.getEntriesByName('recalld:search-results').length);
    const t = await page.evaluate(() => performance.now());
    await input.pressSequentially(q, { delay: 0 });
    await page.waitForFunction((n) => performance.getEntriesByName('recalld:search-results').length > n, before);
    const done = await page.evaluate(() => performance.getEntriesByName('recalld:search-results').at(-1)!.startTime);
    times.push(done - t);
  }
  results.searchEndToEndP95Ms = Math.round(p95(times));
  results.searchAfterDebounceP95Ms = Math.round(p95(times.map((x) => x - 120)));
});

test('Explore scroll frame times', async ({ page }) => {
  await page.goto(stack.url);
  await page.goto(new URL('/explore', stack.url).toString());
  await page.locator('tbody tr.row').first().waitFor();
  const table = page.locator('.table-card');
  await table.hover();
  const deltas = await frameTimes(page, async () => {
    for (let i = 0; i < 80; i++) await page.mouse.wheel(0, 600);
    await page.waitForTimeout(400);
  });
  results.exploreRows = Number(results.storeMemories);
  results.exploreScrollP95FrameMs = Number(p95(deltas).toFixed(1));
});

test('recall event → light, and 50 recalls/s', async ({ page }) => {
  await page.goto(stack.url);
  await page.goto(new URL('/map', stack.url).toString());
  await page.waitForFunction(() => performance.getEntriesByName('recalld:map-ready').length > 0);
  await page.waitForTimeout(1000);
  // A real agent recall through the daemon, as `recalld mcp` would send it.
  const sock = external ? null : await DaemonSocket.open(join(stack.dir, 'recalld.sock'));
  await sock?.call('ping', { protocolVersion: 2, maxMessageSize: 8 * 1024 * 1024 });
  await sock?.call('hello', { agent: 'perf-agent', kind: 'mcp' });
  const lat: number[] = [];
  for (let i = 0; i < 10; i++) {
    const before = await page.evaluate(() => (window as unknown as { __recalldLit: number[] }).__recalldLit?.length ?? 0);
    if (sock) {
      const r = await sock.call('search', { query: ['kalo mira', 'tuve sika', 'dori pa'][i % 3], namespace: 'default', limit: 5, tags: [], entities: [], topics: [], emotions: [], minStrength: null, depth: 1, timeRangeStart: null, timeRangeEnd: null });
      expect(r.error).toBeUndefined();
    } else {
      execSync(process.env.PERF_RECALL_CMD!, { stdio: 'ignore' });
    }
    const sent = Date.now();
    await page.waitForFunction((n) => ((window as unknown as { __recalldLit: number[] }).__recalldLit?.length ?? 0) > n, before);
    // __recalldLit holds wall-clock ms when each recall's first light frame was drawn;
    // compared with the event's daemon timestamp (same machine clock).
    const [litAt, eventAt] = await page.evaluate(() => (window as unknown as { __recalldLitPair: [number, number] }).__recalldLitPair);
    lat.push(litAt - eventAt);
    void sent;
  }
  sock?.close();
  results.recallToLightP95Ms = Math.round(p95(lat));

  await page.goto(new URL('/map?simulate=50', stack.url).toString());
  await page.waitForFunction(() => performance.getEntriesByName('recalld:map-ready').length > 0);
  const deltas = await frameTimes(page, () => page.waitForTimeout(5000));
  results.recalls50PerSecP95FrameMs = Number(p95(deltas).toFixed(1));
});

test('idle CPU on the map', async ({ page }) => {
  await page.goto(stack.url);
  await page.goto(new URL('/map', stack.url).toString());
  await page.waitForFunction(() => (window as unknown as { __recalldSettled?: boolean }).__recalldSettled === true, null, { timeout: 120_000 });
  await page.waitForTimeout(1000);
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('Performance.enable');
  const m = async () => Object.fromEntries((await cdp.send('Performance.getMetrics')).metrics.map((x) => [x.name, x.value]));
  const a = await m();
  await page.waitForTimeout(10_000);
  const b = await m();
  results.idleMainThreadCpuPercent = Number((((b.TaskDuration - a.TaskDuration) / (b.Timestamp - a.Timestamp)) * 100).toFixed(2));
});
