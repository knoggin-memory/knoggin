// SPDX-License-Identifier: AGPL-3.0-only
// Apps (create, rename, delete, undelete), keys (create and see the secret once,
// rotate, revoke) and switching stores.
import { expect, test } from '@playwright/test';
import { open } from './support/ui';
import { PERSONAL, SUPPORT } from './support/seed';

test('create, rename, delete and undelete an app', async ({ page }) => {
  await open(page, '/apps');
  await page.getByRole('button', { name: 'New app' }).first().click();
  const dialog = page.getByRole('dialog', { name: 'New app' });
  await dialog.getByLabel('Name').fill('Support bot');
  await dialog.getByRole('button', { name: 'Create app' }).click();
  await expect(dialog.getByText('An app with this name already exists.')).toBeVisible();
  await dialog.getByLabel('Name').fill('Research pipeline');
  await dialog.getByRole('button', { name: 'Create app' }).click();
  await expect(page).toHaveURL(/\/apps\/[0-9a-f-]+$/);
  await expect(page.getByRole('heading', { name: 'Research pipeline' })).toBeVisible();

  await page.getByRole('button', { name: 'Rename' }).click();
  const rename = page.getByRole('dialog', { name: 'Rename Research pipeline' });
  await rename.getByLabel('Name').fill('Research notes');
  await rename.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByRole('heading', { name: 'Research notes' })).toBeVisible();

  await page.getByRole('button', { name: 'More actions' }).click();
  await page.getByRole('menuitem', { name: 'Delete app…' }).click();
  const del = page.getByRole('alertdialog', { name: 'Delete Research notes?' });
  await expect(del.getByRole('button', { name: 'Export first' })).toBeVisible();
  await del.getByRole('button', { name: 'Delete app' }).click();
  await expect(page.getByText(/Scheduled for deletion in 7 days\./)).toBeVisible();

  await page.getByRole('link', { name: 'Apps' }).first().click();
  const row = page.getByRole('row', { name: /Research notes/ });
  await expect(row.getByText('Deletes in 7 days')).toBeVisible();
  await expect(row.getByText('off')).toBeVisible();
  await row.getByRole('button', { name: 'Undo' }).click();
  await expect(row.getByText('Deletes in 7 days')).toHaveCount(0);
});

test('create a key and see the secret once, then rotate and revoke it', async ({ page }) => {
  await open(page, '/keys');
  await page.getByRole('button', { name: 'New key' }).first().click();
  const create = page.getByRole('dialog', { name: /New key for/ });
  await create.getByLabel('Name').fill('home-assistant');
  await create.getByLabel('Write').uncheck();
  await create.getByRole('button', { name: 'Create key' }).click();

  const reveal = page.getByRole('dialog', { name: 'Key created: home-assistant' });
  const secret = (await reveal.locator('code').textContent()) ?? '';
  expect(secret).toMatch(/^knoggin_live_[A-Za-z0-9_-]{43}$/);
  await reveal.getByRole('button', { name: 'Done' }).click();
  // The secret is gone: not on the page, not in the browser's storage.
  await expect(page.getByText(secret)).toHaveCount(0);
  const stored = await page.evaluate(() => JSON.stringify({ ...localStorage, ...sessionStorage }));
  expect(stored).not.toContain(secret);
  const row = page.getByRole('row', { name: /home-assistant/ });
  await expect(row.getByText('Read only')).toBeVisible();
  await expect(row.getByText(`knoggin_live_${secret.slice(13, 21)}…`)).toBeVisible();

  await row.getByRole('button', { name: 'Actions for home-assistant' }).click();
  await page.getByRole('menuitem', { name: 'Rotate…' }).click();
  await page.getByRole('alertdialog', { name: 'Rotate home-assistant?' }).getByRole('button', { name: 'Rotate key' }).click();
  const rotated = page.getByRole('dialog', { name: 'New key for home-assistant' });
  const second = (await rotated.locator('code').textContent()) ?? '';
  expect(second).toMatch(/^knoggin_live_/);
  expect(second).not.toBe(secret);
  await rotated.getByRole('button', { name: 'Done' }).click();
  await expect(page.getByRole('row', { name: /home-assistant/ })).toHaveCount(1);

  await page.getByRole('row', { name: /home-assistant/ }).getByRole('button', { name: 'Actions for home-assistant' }).click();
  await page.getByRole('menuitem', { name: 'Revoke…' }).click();
  await page.getByRole('alertdialog', { name: 'Revoke home-assistant?' }).getByRole('button', { name: 'Revoke key' }).click();
  await expect(page.getByRole('row', { name: /home-assistant/ })).toHaveCount(0);
});

test('switching stores shows that store’s memories', async ({ page }) => {
  await open(page, '/explore');
  await expect(page.getByRole('button', { name: PERSONAL[0] })).toBeVisible();
  await page.getByRole('button', { name: /Switch store/ }).click();
  await page.getByRole('menuitemradio', { name: /Support bot/ }).click();
  await expect(page.getByRole('button', { name: SUPPORT })).toBeVisible();
  await expect(page.getByRole('button', { name: PERSONAL[0] })).toHaveCount(0);
  await expect(page.getByText('1 memory', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: /Switch store/ }).click();
  await page.getByRole('menuitemradio', { name: /Personal/ }).click();
  await expect(page.getByRole('button', { name: PERSONAL[0] })).toBeVisible();
});

test('a cookie session at a local install’s public address manages no keys', async ({ page }) => {
  // A cookie session: sign-in is on and the page holds no token. The test's token rides on each request instead.
  await open(page, '/apps');
  const token = await page.evaluate(() => localStorage.getItem('knoggin.sessionToken'));
  await page.route(/\/v1\//, (r) => r.continue({ headers: { ...r.request().headers(), authorization: `Bearer ${token}` } }));
  await page.route(/\/v1\/session$/, async (r) => {
    const real = await (await r.fetch({ headers: { ...r.request().headers(), authorization: `Bearer ${token}` } })).json();
    await r.fulfill({ json: { ...real, auth: 'oauth', webSignIn: true } });
  });
  await page.evaluate(() => localStorage.removeItem('knoggin.sessionToken'));
  await page.reload();
  const elsewhere = 'Keys are managed on the machine that runs Knoggin. Run knoggin open there.';

  await page.getByRole('link', { name: 'Support bot' }).click();
  await expect(page.getByRole('heading', { name: 'Support bot' })).toBeVisible();
  await expect(page.getByRole('region', { name: 'API keys' }).getByText(elsewhere)).toBeVisible();
  await expect(page.getByRole('button', { name: 'Manage keys' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'More actions' })).toHaveCount(0);

  await page.goto(page.url().replace(/\/apps\/.*$/, '/keys'));
  await expect(page.getByText(elsewhere)).toBeVisible();
  await expect(page.getByRole('button', { name: 'New key' })).toHaveCount(0);
});
