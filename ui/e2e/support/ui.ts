// SPDX-License-Identifier: AGPL-3.0-only
import { expect, type Page } from '@playwright/test';
import { openUrl, type DaemonInfo } from './daemon';

export const daemon = (): DaemonInfo => ({ bin: process.env.KNOGGIN_BIN!, home: process.env.KNOGGIN_E2E_HOME! });

/** Opens a page the way `knoggin open` does and waits for the shell, then goes to `path`. */
export async function open(page: Page, path = '/map'): Promise<string> {
  const url = new URL(openUrl(daemon()));
  await page.goto(url.toString());
  await expect(page.getByText('daemon connected')).toBeVisible();
  // The page posts the code and takes it out of the address bar.
  await expect(page).not.toHaveURL(/[?&]t=/);
  if (path !== '/map') await page.goto(new URL(path, url.origin).toString());
  return url.origin;
}

/** Nothing stored was run as markup. */
export async function noScriptRan(page: Page) {
  expect(await page.evaluate(() => (window as unknown as { __xss?: number }).__xss)).toBeUndefined();
}
