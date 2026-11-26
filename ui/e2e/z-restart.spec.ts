// SPDX-License-Identifier: AGPL-3.0-only
// After the daemon restarts, the old page cannot connect (its address is new on
// every run) and says to run knoggin open; a new knoggin open gives a working page.
// Runs last: it restarts the shared daemon.
import { expect, test } from '@playwright/test';
import { start, stop } from './support/daemon';
import { daemon, open } from './support/ui';

test('the old page shows Knoggin is not running, and knoggin open gives a working page', async ({ page }) => {
  await open(page, '/map');
  stop(daemon());
  await start(daemon());
  // Any request from the old page now fails to connect.
  await page.getByRole('link', { name: 'Explore' }).click();
  await expect(page.getByText('Knoggin is not running.')).toBeVisible();
  await expect(page.getByText('Run this to start it and open a new page:')).toBeVisible();
  await expect(page.locator('code', { hasText: 'knoggin open' })).toBeVisible();

  await open(page, '/explore');
  await expect(page.getByText('Knoggin is not running.')).toHaveCount(0);
  await expect(page.getByText(/memories?$/).first()).toBeVisible();
});
