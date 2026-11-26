// SPDX-License-Identifier: AGPL-3.0-only
// The four ported screens load with seeded data.
import { expect, test } from '@playwright/test';
import { open } from './support/ui';
import { HARBOR, PERSONAL } from './support/seed';

test('the memory map draws the store and its counts', async ({ page }) => {
  await open(page, '/map');
  await expect(page).toHaveTitle('Knoggin · Memory map');
  await expect(page.getByRole('heading', { name: 'Memory map' })).toBeVisible();
  await expect(page.getByText(/6 memories/)).toBeVisible();
  await expect(page.locator('canvas').first()).toBeVisible();
  const scope = page.getByRole('group', { name: 'Namespace scope' });
  await expect(scope.getByRole('button', { name: /All namespaces\s*6/ })).toBeVisible();
  await expect(scope.getByRole('button', { name: /project-harbor\s*2/ })).toBeVisible();
});

test('the map header and the sidebar count the live namespace counts', async ({ page }) => {
  // The graph still holds six memories; the counts say 106, so the header must read them.
  await page.route(/\/v1\/namespaces$/, async (r) => {
    const real = await (await r.fetch()).json();
    real.items[0].memoryCount += 100;
    await r.fulfill({ json: real });
  });
  await open(page, '/map');
  await expect(page.getByText('106 memories')).toBeVisible();
  await expect(page.getByRole('group', { name: 'Namespace scope' }).getByRole('button', { name: /All namespaces\s*106/ })).toBeVisible();
});

test('Explore lists the memories and opens one', async ({ page }) => {
  await open(page, '/explore');
  await expect(page.getByText('6 memories')).toBeVisible();
  await page.getByRole('button', { name: PERSONAL[0] }).click();
  await expect(page.getByText('Said so while booking the Lisbon trip.')).toBeVisible();
  await expect(page.getByRole('region', { name: 'Memory details' }).getByText('Maya', { exact: true })).toBeVisible();
  await page.getByRole('group', { name: 'Namespace scope' }).getByRole('button', { name: /project-harbor/ }).click();
  await expect(page.getByText('2 memories')).toBeVisible();
  await expect(page.getByRole('button', { name: HARBOR[0] })).toBeVisible();
  await page.getByRole('group', { name: 'Namespace scope' }).getByRole('button', { name: /All namespaces/ }).click();
});

test('the Timeline shows stored, recalled and reinforced rows', async ({ page }) => {
  await open(page, '/timeline');
  const activity = page.getByRole('region', { name: 'Activity' });
  await expect(activity.getByText(PERSONAL[2])).toBeVisible();
  await expect(activity.getByText(/^Recalled$/).first()).toBeVisible();
  await expect(activity.getByText(/^Reinforced$/).first()).toBeVisible();
  await expect(page.getByText(/Memories recalled each (week|month)/)).toBeVisible();
});

test('Health shows the phases, storage and system', async ({ page }) => {
  // "Fading soon" comes from the report's atRisk, with no request of its own.
  const rowReads: string[] = [];
  page.on('request', (r) => r.url().includes('/v1/ui/memories') && rowReads.push(r.url()));
  await open(page, '/health');
  await expect(page.getByRole('region', { name: 'Fading soon' }).getByRole('button', { name: 'Reinforce' })).toHaveCount(5);
  expect(rowReads).toEqual([]);
  const phases = page.getByRole('region', { name: 'Phases' });
  await expect(phases.locator('.display')).toHaveText('6');
  await expect(page.getByRole('region', { name: 'Storage' }).getByText('Vectors')).toBeVisible();
  await expect(page.getByRole('region', { name: 'System' }).getByText('Fake model')).toBeVisible();
});

test('search finds a memory and opens it in Explore', async ({ page }) => {
  await open(page, '/map');
  await page.getByRole('button', { name: /Search/ }).click();
  await page.getByRole('combobox', { name: 'Search memories' }).fill('window seats');
  await page.getByRole('option', { name: new RegExp(PERSONAL[0]) }).click();
  await expect(page).toHaveURL(/\/explore\?id=/);
  // Shown above the list, although the loaded rows hold it; its row stays closed.
  await expect(page.locator('.pinned').getByText('Said so while booking the Lisbon trip.')).toBeVisible();
  await expect(page.locator('[data-id]', { hasText: PERSONAL[0] })).toHaveAttribute('aria-expanded', 'false');
});
