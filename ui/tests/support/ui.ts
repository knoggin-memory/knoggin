import { expect, type Page } from '@playwright/test';

/**
 * Start on All namespaces, as these tests were written for, unless a test has
 * already chosen a scope. A fresh browser otherwise starts on `default`.
 */
export async function allNamespaces(page: Page) {
  await page.addInitScript(() => {
    if (!localStorage.getItem('recalld.scope')) localStorage.setItem('recalld.scope', '*');
  });
}

/** Sign in with the launch token, open `path`, and wait for the shell. */
export async function open(page: Page, path = '/map') {
  await allNamespaces(page);
  const url = new URL(process.env.RECALLD_UI_URL!);
  await page.goto(url.toString());
  await page.goto(new URL(path, url.origin).toString());
  await expect(page.getByText('read-only · daemon connected')).toBeVisible();
}
