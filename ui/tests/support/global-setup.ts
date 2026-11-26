import type { FullConfig } from '@playwright/test';
import { startStack } from './stack';

export default async function globalSetup(_: FullConfig) {
  if (process.env.RECALLD_UI_URL) return;
  const bin = process.env.RECALLD_BIN;
  if (!bin) throw new Error('set RECALLD_UI_URL (running stack) or RECALLD_BIN (recalld built with --features ui,bench)');
  const stack = await startStack(bin, 'sample');
  process.env.RECALLD_UI_URL = stack.url;
  return () => stack.stop();
}
