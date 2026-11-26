// SPDX-License-Identifier: AGPL-3.0-only
// The local web session of `knoggin open`, the fixed address without a session, and the theme.
import { expect, test } from '@playwright/test';
import { fixedAddress } from './support/daemon';
import { daemon, open } from './support/ui';
import { PERSONAL } from './support/seed';

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

test('no screen, overlay or page breaks the content security policy', async ({ page }) => {
  // Collected in the test, so a navigation does not drop what the page before it reported.
  const violations: string[] = [];
  await page.exposeFunction('__cspViolation', (v: string) => violations.push(v));
  await page.addInitScript(() => {
    document.addEventListener('securitypolicyviolation', (e) =>
      (window as unknown as { __cspViolation(v: string): void }).__cspViolation(`${location.pathname}: ${e.violatedDirective} ${e.blockedURI} ${e.sourceFile}:${e.lineNumber}`),
    );
  });
  const settle = async () => {
    await page.waitForTimeout(300);
    expect(violations, page.url()).toEqual([]);
  };
  const origin = await open(page, '/map');
  const go = async (path: string) => {
    await page.goto(origin + path);
    await expect(page.getByRole('main')).toBeVisible();
  };

  // The map, the palette and the store switcher's popover.
  await expect(page.locator('canvas').first()).toBeVisible();
  await settle();
  await page.getByRole('button', { name: /Search/ }).click();
  await page.getByRole('combobox', { name: 'Search memories' }).fill('window seats');
  await expect(page.getByRole('option', { name: new RegExp(PERSONAL[0]) })).toBeVisible();
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: /Switch store/ }).click();
  await expect(page.getByRole('menuitemradio', { name: /Personal/ })).toBeVisible();
  await page.keyboard.press('Escape');
  await settle();

  // Explore: a menu, an open row, the memory panel's menu and its forget dialog.
  await go('/explore');
  await page.getByRole('button', { name: /^Sort/ }).click();
  await expect(page.getByRole('menuitemradio', { name: 'Strength' })).toBeVisible();
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: PERSONAL[0] }).click();
  await page.getByRole('button', { name: 'More actions' }).click();
  await page.getByRole('menuitem', { name: 'Forget…' }).click();
  await page.getByRole('alertdialog', { name: 'Forget this memory?' }).getByRole('button', { name: 'Keep it' }).click();
  await settle();

  for (const path of ['/timeline', '/health', '/setup', '/account', '/users', '/status']) {
    await go(path);
    await settle();
  }

  // Dialogs on the API keys and Apps pages, and an app's overview.
  await go('/keys');
  await page.getByRole('button', { name: 'New key' }).first().click();
  await page.getByRole('dialog', { name: /New key for/ }).getByRole('button', { name: 'Cancel' }).click();
  await go('/apps');
  await page.getByRole('button', { name: 'New app' }).first().click();
  await page.getByRole('dialog', { name: 'New app' }).getByRole('button', { name: 'Cancel' }).click();
  await page.getByRole('link', { name: 'Support bot' }).click();
  await expect(page.getByRole('heading', { name: 'Support bot' })).toBeVisible();
  await settle();

  // A waiting state that takes the screen.
  await page.route(/\/v1\/stores\/[^/]+\/status$/, (r) => r.fulfill({ json: { state: 'waking', lockedReason: null, full: false, model: null, embedding: null } }));
  await go('/explore');
  await expect(page.getByRole('heading', { name: 'Waking your store' })).toBeVisible();
  await settle();
  await page.unrouteAll();

  // The sign-in page, and the standalone page of the fixed address without a session.
  await page.route(/\/v1\/session$/, async (r) => {
    const real = await (await r.fetch()).json();
    await r.fulfill({ json: { ...real, auth: 'oauth', webSignIn: true, signedIn: false, user: null } });
  });
  await page.goto(`${origin}/signin`);
  await expect(page.getByRole('button', { name: 'Continue to sign in' })).toBeVisible();
  await settle();
  await page.unrouteAll();
  await page.goto(`${fixedAddress(daemon())}/map`);
  await expect(page.getByRole('heading', { name: 'Open Knoggin from your terminal' })).toBeVisible();
  await settle();
});
