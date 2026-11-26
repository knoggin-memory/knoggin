// SPDX-License-Identifier: AGPL-3.0-only
import { defineConfig, devices } from '@playwright/test';

// Runs against a real `recalld ui`: either RECALLD_UI_URL (an already
// running stack, e.g. in a container) or a stack tests/support/stack.ts
// spawns from RECALLD_BIN with an isolated temp store.
export default defineConfig({
  testDir: 'tests',
  outputDir: 'test-results',
  timeout: 60_000,
  expect: { timeout: 10_000, toHaveScreenshot: { maxDiffPixelRatio: 0.03 } },
  fullyParallel: false,
  workers: 1,
  reporter: [['list']],
  globalSetup: './tests/support/global-setup.ts',
  use: {
    viewport: { width: 1440, height: 900 },
    ...(process.env.PW_CHANNEL ? { channel: process.env.PW_CHANNEL } : {}),
  },
  projects: [
    { name: 'e2e', testMatch: /e2e\/.*\.spec\.ts/, use: { ...devices['Desktop Chrome'], viewport: { width: 1440, height: 900 } } },
    { name: 'screens', testMatch: /screens\.spec\.ts/, use: { viewport: { width: 1440, height: 900 } } },
    { name: 'perf', testMatch: /perf\.spec\.ts/, testDir: 'perf', timeout: 600_000, use: { viewport: { width: 1440, height: 900 } } },
  ],
});
