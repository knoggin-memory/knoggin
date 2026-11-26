// SPDX-License-Identifier: AGPL-3.0-only
// The local web session of `knoggin open`, the fixed address without a session, and the theme.
import { expect, test } from '@playwright/test';
import { fixedAddress } from './support/daemon';
import { daemon, open } from './support/ui';

test('a knoggin open session sets no cookie and keeps its token in local storage', async ({ page, context }) => {
  const origin = await open(page, '/map');
  expect(await context.cookies()).toEqual([]);
  const token = await page.evaluate(() => localStorage.getItem('knoggin.sessionToken'));
  expect(token).toMatch(/^knoggin_session_/);
  expect(new URL(origin).hostname).toMatch(/^(127\.0\.0\.1|.*\.localhost)$/);
});

test('the fixed address without a session says to run knoggin open', async ({ page }) => {
  await page.goto(`${fixedAddress(daemon())}/map`);
  await expect(page.getByRole('heading', { name: 'Open Knoggin from your terminal' })).toBeVisible();
  await expect(page.locator('code', { hasText: 'knoggin open' })).toBeVisible();
  await expect(page.getByRole('button', { name: /Sign in/ })).toHaveCount(0);
  await expect(page.getByRole('link', { name: 'Source code' })).toBeVisible();
});

test('the theme choice persists across a reload', async ({ page }) => {
  await open(page, '/account');
  await page.getByRole('group', { name: 'Theme' }).getByRole('button', { name: 'Dark' }).click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await page.reload();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await expect(page.getByRole('group', { name: 'Theme' }).getByRole('button', { name: 'Dark' })).toHaveAttribute('aria-pressed', 'true');
  await page.getByRole('group', { name: 'Theme' }).getByRole('button', { name: 'Light' }).click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
});

test('a local install shows Settings, and the server pages say they are not for it', async ({ page }) => {
  await open(page, '/users');
  await expect(page.getByText('The local install has one user, so it has no Users page.')).toBeVisible();
  await page.goto(page.url().replace('/users', '/status'));
  await expect(page.getByText('Server status belongs to a Knoggin server. The local install shows its system details under Health and in Settings.')).toBeVisible();
  await page.getByRole('link', { name: 'Back to the Memory map' }).click();
  await expect(page.getByRole('link', { name: 'Settings' })).toBeVisible();
});
