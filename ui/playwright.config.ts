// SPDX-License-Identifier: AGPL-3.0-only
import { defineConfig, devices } from '@playwright/test';

// The end-to-end tier (tab "Testing, CI, releases"): a daemon built from the
// same commit, given by path in KNOGGIN_BIN, started in test mode in a
// temporary home. The harness builds nothing; the daemon serves the built UI.
export default defineConfig({
  testDir: 'e2e',
  outputDir: 'test-results',
  timeout: 60_000,
  expect: { timeout: 10_000 },
  // One daemon is shared, and one file restarts it: run the files one at a time.
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  reporter: [['list']],
  globalSetup: './e2e/support/global-setup.ts',
  use: { ...devices['Desktop Chrome'], viewport: { width: 1440, height: 900 }, trace: 'retain-on-failure' },
});
