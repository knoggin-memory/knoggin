// SPDX-License-Identifier: AGPL-3.0-only
// The waiting and failure states, driven by a stubbed store status route and stubbed errors.
import { expect, test, type Page } from '@playwright/test';
import { open } from './support/ui';

const ready = { state: 'ready', lockedReason: null, full: false, model: { state: 'ready', identity: 'fake-64', name: 'Fake model' }, embedding: { pending: 0, total: 6 } };

async function withStatus(page: Page, status: object) {
  await page.route(/\/v1\/stores\/[^/]+\/status$/, (r) => r.fulfill({ json: { ...ready, ...status } }));
}

test('waking takes the screen', async ({ page }) => {
  await withStatus(page, { state: 'waking', model: null, embedding: null });
  await open(page, '/explore');
  await expect(page.getByRole('heading', { name: 'Waking your store' })).toBeVisible();
  // A local install has no idle parking, so no parking sentence.
  await expect(page.getByText('This takes a few seconds.', { exact: true })).toBeVisible();
});

test('a locked store shows one sentence for its reason', async ({ page }) => {
  await withStatus(page, { state: 'locked', lockedReason: 'format_too_new', model: null, embedding: null });
  await open(page, '/explore');
  await expect(page.getByRole('heading', { name: 'This store is locked' })).toBeVisible();
  await expect(page.getByText('This store was saved by a newer version of Knoggin. Upgrade Knoggin to open it.')).toBeVisible();
});

test('a locked store links to the recovery guide', async ({ page }) => {
  await withStatus(page, { state: 'locked', lockedReason: 'snapshot_unreadable', model: null, embedding: null });
  await open(page, '/health');
  await expect(page.getByText(/The saved copies of this store could not be read\. Nothing was overwritten\. See the/)).toBeVisible();
  await expect(page.getByRole('link', { name: 'recovery guide' })).toHaveAttribute('href', /\/docs\/local-install\.md$/);
});

test('the model banners and catching up keep the screen usable', async ({ page }) => {
  await withStatus(page, { model: { state: 'downloading', identity: 'x', name: 'EmbeddingGemma 2 · q4', bytesDone: 64_000_000, bytesTotal: 217_000_000 } });
  await open(page, '/explore');
  await expect(page.getByText('Downloading the model.')).toBeVisible();
  await expect(page.getByRole('progressbar', { name: 'Model download' })).toHaveAttribute('aria-valuenow', '29');
  await expect(page.getByRole('heading', { name: 'Explore' })).toBeVisible();

  await page.unrouteAll();
  await withStatus(page, { model: { state: 'unavailable', identity: 'x', name: 'x', reason: 'offline' } });
  await page.reload();
  await expect(page.getByText('No connection. Saving and keyword search already work. Knoggin tries again by itself.')).toBeVisible();

  await page.unrouteAll();
  await withStatus(page, { embedding: { pending: 214, total: 900 } });
  await page.reload();
  await expect(page.getByText('Catching up: 214 memories left to index.')).toBeVisible();

  await page.unrouteAll();
  await withStatus(page, { full: true });
  await page.reload();
  await expect(page.getByText('This store is full.')).toBeVisible();
});

test('updating, temporarily unavailable and too many requests come from error codes', async ({ page }) => {
  // Every store request answers the same, as a container being replaced would.
  const fail = (code: string, extra: object = {}, retry = '1') =>
    page.route(/\/v1\/(ui\/(?!events)|namespaces)/, (r) =>
      r.fulfill({ status: code === 'rate_limited' ? 429 : 503, headers: { 'Retry-After': retry }, json: { error: { code, message: 'stubbed', ...extra } } }),
    );
  await fail('upgrading');
  await open(page, '/explore');
  await expect(page.getByRole('heading', { name: 'Knoggin is updating' })).toBeVisible();
  await expect(page.getByText('Your store is moving to the new version. This page carries on by itself.')).toBeVisible();

  await page.unrouteAll();
  await fail('unavailable', { reason: 'crashed' }, '60');
  await page.reload();
  await expect(page.getByText('This store stopped unexpectedly several times in a row. Knoggin tries again every minute.')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Retry' })).toHaveCount(0);

  await page.unrouteAll();
  await fail('rate_limited', {}, '30');
  await page.reload();
  await expect(page.getByText(/Too many requests\. Try again in (30|29|28) seconds\./)).toBeVisible();
});

test('a screen whose data cannot load says so, with Retry', async ({ page }) => {
  await page.route(/\/v1\/ui\/health\?|\/v1\/ui\/health$/, (r) => r.fulfill({ status: 500, json: { error: { code: 'internal', message: 'Internal error' } } }));
  await open(page, '/health');
  await expect(page.getByText('Could not load this.')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Retry' })).toBeVisible();
});
