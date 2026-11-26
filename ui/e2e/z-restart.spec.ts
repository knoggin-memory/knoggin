// SPDX-License-Identifier: AGPL-3.0-only
// After the daemon restarts, the old page cannot connect (its address is new on
// every run) and says to run knoggin open; a new knoggin open gives a working page.
// This test runs its own daemon, so the other files can share theirs in parallel.
import { expect, test } from '@playwright/test';
import { api, cleanup, makeHome, openUrl, start, stop, type DaemonInfo } from './support/daemon';

test('the old page shows Knoggin is not running, and knoggin open gives a working page', async ({ page }) => {
  const d: DaemonInfo = { bin: process.env.KNOGGIN_BIN!, home: makeHome() };
  try {
    await start(d);
    await api(d, 'POST', '/v1/memories', { summary: 'Kept across a restart' });
    await page.goto(openUrl(d));
    await expect(page.getByText('daemon connected')).toBeVisible();
    await expect(page.getByText('1 memory', { exact: true })).toBeVisible(); // the map's header

    stop(d);
    await start(d);
    // Any request from the old page now fails to connect.
    await page.getByRole('link', { name: 'Explore' }).click();
    await expect(page.getByText('Knoggin is not running.')).toBeVisible();
    await expect(page.getByText('Run this to start it and open a new page:')).toBeVisible();
    await expect(page.locator('code', { hasText: 'knoggin open' })).toBeVisible();
    await expect(page.getByText('daemon unavailable')).toBeVisible();

    await page.goto(openUrl(d));
    await expect(page.getByText('daemon connected')).toBeVisible();
    await expect(page.getByText('Knoggin is not running.')).toHaveCount(0);
    await page.getByRole('link', { name: 'Explore' }).click();
    await expect(page.getByRole('button', { name: 'Kept across a restart' })).toBeVisible();
  } finally {
    cleanup(d);
  }
});
