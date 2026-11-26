// SPDX-License-Identifier: AGPL-3.0-only
// Every stored string is shown as text, never as HTML (tab "Security").
import { expect, test } from '@playwright/test';
import { noScriptRan, open } from './support/ui';
import { MARKUP } from './support/seed';

test('markup in a memory, an app name and a client name is shown as text', async ({ page }) => {
  await open(page, '/explore');
  await expect(page.getByRole('button', { name: MARKUP.memory })).toBeVisible();
  await expect(page.locator('img[src="x"]')).toHaveCount(0);

  await page.getByRole('button', { name: /^Agent/ }).click();
  await expect(page.getByRole('menuitemcheckbox', { name: new RegExp(MARKUP.client.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')) })).toBeVisible();
  await page.keyboard.press('Escape');

  await page.goto(page.url().replace(/\/explore.*$/, '/timeline'));
  await expect(page.getByText(`${MARKUP.client} stored`)).toBeVisible();

  await page.goto(page.url().replace('/timeline', '/apps'));
  await expect(page.getByRole('link', { name: MARKUP.app })).toBeVisible();
  await page.getByRole('button', { name: /Switch store/ }).click();
  await expect(page.getByRole('menuitemradio', { name: new RegExp('Ops') })).toContainText(MARKUP.app);
  await page.keyboard.press('Escape');
  await noScriptRan(page);
});

test('markup in the answers of Users and Server status is shown as text', async ({ page }) => {
  const evil = '<img src=x onerror="window.__xss=1">';
  const now = new Date().toISOString();
  await page.route('**/v1/session', async (r) => {
    const real = await (await r.fetch()).json();
    await r.fulfill({ json: { ...real, mode: 'server', auth: 'oauth', provider: { name: 'Test issuer' }, idleSeconds: 600, webSignIn: true, user: { ...real.user, name: 'Admin', email: 'admin@example.com', isAdmin: true } } });
  });
  const other = { id: '0193b7c2-5e1a-7d40-8f3b-2a6c9e0d4b17', name: `Eve ${evil}`, email: `eve${evil}@example.com`, isAdmin: false, createdAt: now, lastActiveOn: now.slice(0, 10), appCount: 1, container: { state: 'parked', since: now }, deletedAt: null, deletedBy: null, deleteAfter: null };
  await page.route('**/v1/users?*', (r) => r.fulfill({ json: { items: [other], nextCursor: null } }));
  await page.route('**/v1/users', (r) => r.fulfill({ json: { items: [other], nextCursor: null } }));
  await page.route('**/v1/admin/status', (r) =>
    r.fulfill({
      json: {
        version: '0.1.0', imageDigest: null, mode: 'server', backends: { stateDb: 'dynamodb', objectStorage: 's3', keyProvider: 'kms', launcher: 'ecs' },
        storage: { bucket: 'b', region: 'r' }, dataDir: null, containers: { claimed: 1, ready: 2, starting: 0 }, pool: { target: 2 },
        rollout: null, users: { total: 1, deleting: 0 }, retireBefore: null,
      },
    }),
  );
  const store = { id: '0193a204-1d6b-7e42-a8c1-5f90b2e47a3c', kind: 'app', name: `Bot ${evil}`, createdAt: now, memoryCount: 3, lastWriteAt: now, lastWriteClient: `agent ${evil}`, namespaceCount: 1, status: 'active', deletedAt: null, deleteAfter: null };
  await page.route(`**/v1/users/${other.id}/stores`, (r) => r.fulfill({ json: { items: [store] } }));
  await page.route(`**/v1/admin/stores/${store.id}/snapshots`, (r) => r.fulfill({ json: { items: [] } }));

  await open(page, '/users');
  await expect(page.getByText(`Eve ${evil}`)).toBeVisible();
  await expect(page.getByText(`eve${evil}@example.com`)).toBeVisible();
  await page.goto(page.url().replace('/users', '/status'));
  await expect(page.getByRole('combobox', { name: 'User', exact: true })).toContainText(`Eve ${evil}`);
  await expect(page.getByRole('combobox', { name: 'Store of this user' })).toContainText(`Bot ${evil}`);
  await expect(page.getByText('No snapshots yet.')).toBeVisible();

  // The last agent: neither Users nor Server status draws one (the prototype's columns have none), so the
  // store's lastWriteClient is checked where it is drawn, the Last write tile of App overview.
  await page.route(/\/v1\/stores$/, (r) => r.fulfill({ json: { items: [store] } }));
  await page.route(`**/v1/stores/${store.id}/keys`, (r) => r.fulfill({ json: { items: [] } }));
  await page.goto(page.url().replace('/status', `/apps/${store.id}`));
  await expect(page.getByRole('heading', { name: `Bot ${evil}` })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Store' }).getByText(`agent ${evil}`)).toBeVisible();
  await expect(page.locator('img[src="x"]')).toHaveCount(0);
  await noScriptRan(page);
});
