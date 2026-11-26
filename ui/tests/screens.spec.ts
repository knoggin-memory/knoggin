// Screens at 1440×900 on the sample store (the reference screens' content).
// Each run also writes test-results/screens/*.png for side-by-side review
// against docs/ui reference screens; toHaveScreenshot guards regressions.
import { expect, test, type Page } from '@playwright/test';
import { open } from './support/ui';

test.use({ viewport: { width: 1440, height: 900 }, reducedMotion: 'reduce' });

// Relative times drift between runs; mask them.
const masks = (page: Page) => [
  page.locator('.mono.t'), // stream times
  page.locator('td.last'), // "accessed 3d ago"
  page.locator('.status.mono'),
  page.locator('.tv, .tc'), // system tiles (uptime, RSS)
  page.locator('.meta, .caption'), // relative next-change phrases
  page.locator('section[aria-label="Memories over time"] svg'), // weekly buckets move with the date
  page.locator('.facts dd, figcaption'), // an opened row's dates, ages and random ID
];

async function shot(page: Page, name: string) {
  await page.evaluate(() => document.fonts.ready);
  await page.waitForTimeout(400);
  await page.screenshot({ path: `test-results/screens/${name}.png` });
  await expect(page).toHaveScreenshot(`${name}.png`, { mask: masks(page), animations: 'disabled' });
}

test('Memory map', async ({ page }) => {
  await open(page, '/map');
  await page.waitForFunction(() => (window as unknown as { __recalldSettled?: boolean }).__recalldSettled === true, null, { timeout: 60_000 });
  await page.getByRole('button', { name: 'Fit graph to view' }).click();
  await page.waitForTimeout(600);
  await shot(page, 'map');
});

test('Memory map with a memory selected', async ({ page }) => {
  await open(page, '/explore?sort=summary');
  await page.getByRole('button', { name: 'Connection bonus keeps well-linked memories in Full' }).click();
  await page.getByRole('button', { name: 'Show on map' }).click();
  await page.waitForFunction(() => (window as unknown as { __recalldSettled?: boolean }).__recalldSettled === true, null, { timeout: 60_000 });
  // Park the pointer away from the map so no node is hovered.
  await page.mouse.move(0, 0);
  await page.waitForTimeout(1000); // the 300 ms centering animation
  await shot(page, 'map-selected');
});

test('Explore', async ({ page }) => {
  await open(page, '/explore');
  await page.locator('tbody tr.row').first().waitFor();
  await shot(page, 'explore');
});

test('Explore with a row opened', async ({ page }) => {
  await open(page, '/explore?sort=summary');
  await page.getByRole('button', { name: 'Connection bonus keeps well-linked memories in Full' }).click();
  await page.getByRole('img', { name: 'Forgetting curve for the selected memory' }).waitFor();
  await page.getByText(/^Links · \d+/).waitFor();
  await shot(page, 'explore-row');
});

test('Search', async ({ page }) => {
  await open(page, '/explore');
  await page.keyboard.press('ControlOrMeta+k');
  await page.keyboard.type('how does decay interact with graph links');
  await page.getByRole('option').first().waitFor();
  await shot(page, 'search');
});

test('Timeline', async ({ page }) => {
  await open(page, '/timeline');
  await page.getByRole('img', { name: /Memories stored and recalled/ }).waitFor();
  await shot(page, 'timeline');
});

test('Health', async ({ page }) => {
  await open(page, '/health');
  await page.getByText('Connected', { exact: true }).waitFor();
  await shot(page, 'health');
});
