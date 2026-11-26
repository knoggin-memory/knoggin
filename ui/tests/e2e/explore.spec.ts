import { expect, test } from '@playwright/test';
import { open } from '../support/ui';

test('lists memories, filters by phase and sorts via the URL', async ({ page }) => {
  await open(page, '/explore');
  await expect(page.getByText('135 memories')).toBeVisible();
  const rows = page.locator('tbody tr.row');
  await expect(rows.first()).toBeVisible();
  // Toggle Full and Summary off: only Ghosts remain, with "Summary dropped".
  const toolbar = page.getByRole('toolbar', { name: 'Filters' });
  await toolbar.getByRole('button', { name: 'Full', exact: true }).click();
  await toolbar.getByRole('button', { name: 'Summary', exact: true }).click();
  await expect(page).toHaveURL(/phase=3/);
  await expect(rows.first().getByRole('button')).toHaveText('Summary dropped');
  await page.goto(page.url().replace(/\?.*/, '?sort=summary'));
  await expect(page).toHaveURL(/sort=summary/);
  await expect(toolbar.getByText('Memory', { exact: false })).toBeVisible();
});

test('works from the keyboard alone', async ({ page }) => {
  await open(page, '/explore');
  const first = page.locator('tbody tr.row').first().getByRole('button');
  await first.focus();
  await page.keyboard.press('Enter');
  // The row opens in place with its details.
  const details = page.getByRole('region', { name: 'Memory details' });
  await expect(details).toBeVisible();
  const selected = page.locator('tbody tr.row.selected');
  await expect(selected).toHaveCount(1);
  // Compare ids, not text: two memories can share a summary.
  const id = await selected.getByRole('button').getAttribute('data-id');
  await page.keyboard.press('ArrowDown');
  await expect(selected.getByRole('button')).not.toHaveAttribute('data-id', id!);
  await expect(selected).toHaveCount(1);
  await expect(details).toHaveCount(1);
  await page.keyboard.press('Escape');
  await expect(details).toBeHidden();
});

test('an opened row shows the decay curve, links and actions', async ({ page }) => {
  await open(page, '/explore?sort=summary');
  await page.getByRole('button', { name: 'Connection bonus keeps well-linked memories in Full' }).click();
  const panel = page.getByRole('region', { name: 'Memory details' });
  await expect(panel.getByRole('img', { name: 'Forgetting curve for the selected memory' })).toBeVisible();
  await expect(panel.getByText(/^R \d\.\d\d · S /)).toBeVisible();
  await expect(panel.getByText(/^Links · \d+/)).toBeVisible();
  await expect(panel.getByRole('button', { name: 'Reinforce' })).toBeVisible();
  await panel.getByRole('button', { name: 'More actions' }).click();
  await page.getByRole('menuitem', { name: 'Forget…' }).click();
  await expect(page.getByRole('alertdialog')).toContainText('Forget this memory?');
  await page.getByRole('button', { name: 'Keep it' }).click();
  await expect(page.getByRole('alertdialog')).toBeHidden();
});
