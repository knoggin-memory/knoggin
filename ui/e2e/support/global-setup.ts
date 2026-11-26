// SPDX-License-Identifier: AGPL-3.0-only
import type { FullConfig } from '@playwright/test';
import { cleanup, makeHome, start, type DaemonInfo } from './daemon';
import { seed } from './seed';

export default async function globalSetup(_: FullConfig) {
  const bin = process.env.KNOGGIN_BIN;
  if (!bin) throw new Error('set KNOGGIN_BIN to a knoggin binary built from this commit (it serves the built UI)');
  const d: DaemonInfo = { bin, home: makeHome() };
  try {
    await start(d);
    await seed(d);
  } catch (e) {
    cleanup(d);
    throw e;
  }
  // The workers read where the daemon lives from here.
  process.env.KNOGGIN_E2E_HOME = d.home;
  return () => cleanup(d);
}
