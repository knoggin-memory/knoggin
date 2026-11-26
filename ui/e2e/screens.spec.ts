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
  await open(page, '/health');
  const phases = page.getByRole('region', { name: 'Phases' });
  await expect(phases.getByText('6', { exact: true })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Storage' }).getByText('Vectors')).toBeVisible();
  await expect(page.getByRole('region', { name: 'System' }).getByText('Fake model')).toBeVisible();
});

test('search finds a memory and opens it in Explore', async ({ page }) => {
  await open(page, '/map');
  await page.getByRole('button', { name: /Search/ }).click();
  await page.getByRole('combobox').fill('window seats');
  await page.getByRole('option', { name: new RegExp(PERSONAL[0]) }).click();
  await expect(page).toHaveURL(/\/explore\?id=/);
  await expect(page.getByText('Said so while booking the Lisbon trip.')).toBeVisible();
});
