import { expect, test } from '@playwright/test';
import { open } from '../support/ui';

test('signs in with the launch token and lists namespaces from the daemon', async ({ page }) => {
  await open(page, '/explore');
  await expect(page).toHaveTitle('recalld · Explore');
  const scope = page.getByRole('group', { name: 'Namespace scope' });
  await expect(scope.getByRole('button', { name: /All namespaces\s*135/ })).toBeVisible();
  await expect(scope.getByRole('button', { name: /project-recalld\s*98/ })).toBeVisible();
  await expect(scope.getByRole('button', { name: /topic-rust\s*12/ })).toBeVisible();
  await expect(page.getByText('read-only · daemon connected')).toBeVisible();
});

test('the API refuses requests without the session cookie', async ({ browser }) => {
  const ctx = await browser.newContext();
  const res = await ctx.request.get(new URL('/api/ui/namespaces', process.env.RECALLD_UI_URL!).toString());
  expect(res.status()).toBe(403);
  await ctx.close();
});

test('a fresh browser starts on the default namespace', async ({ page }) => {
  const url = new URL(process.env.RECALLD_UI_URL!);
  await page.goto(url.toString());
  await page.goto(new URL('/explore', url.origin).toString());
  await expect(page.getByRole('toolbar', { name: 'Filters' }).getByRole('button', { name: /default/ })).toBeVisible();
  await expect(page.getByText('25 memories')).toBeVisible();
});

test('namespace scope is global and persists', async ({ page }) => {
  await open(page, '/explore');
  await page.getByRole('group', { name: 'Namespace scope' }).getByRole('button', { name: /topic-rust/ }).click();
  await expect(page.getByRole('toolbar', { name: 'Filters' }).getByRole('button', { name: /topic-rust/ })).toBeVisible();
  await expect(page.getByText('12 memories')).toBeVisible();
  await page.reload();
  await expect(page.getByText('12 memories')).toBeVisible();
  await page.getByRole('group', { name: 'Namespace scope' }).getByRole('button', { name: /All namespaces/ }).click();
});

test('the sidebar collapses to a rail and remembers it', async ({ page }) => {
  await open(page, '/map');
  await page.getByRole('button', { name: 'Collapse sidebar' }).click();
  await expect(page.getByRole('button', { name: 'Expand sidebar' })).toBeVisible();
  await page.reload();
  await expect(page.getByRole('button', { name: 'Expand sidebar' })).toBeVisible();
  await page.getByRole('button', { name: 'Expand sidebar' }).click();
});
