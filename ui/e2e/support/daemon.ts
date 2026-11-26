// SPDX-License-Identifier: AGPL-3.0-only
// A Knoggin daemon in test mode in a temporary home: never ~/.knoggin. The
// daemon is started and stopped with `knoggin start` and `knoggin stop`, so a
// test may restart it and the teardown still finds it.
import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { request as httpRequest } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export interface DaemonInfo {
  bin: string;
  home: string;
}

export function env(d: DaemonInfo): NodeJS.ProcessEnv {
  return {
    PATH: process.env.PATH,
    HOME: d.home,
    KNOGGIN_HOME: d.home,
    KNOGGIN_TEST_MODE: '1',
    KNOGGIN_LOCAL__MASTER_KEY: 'key_file',
    KNOGGIN_SERVER__BIND: '127.0.0.1:0',
  };
}

const run = (d: DaemonInfo, args: string[]) => execFileSync(d.bin, args, { env: env(d), encoding: 'utf8', timeout: 60_000 }).trim();

export function makeHome(): string {
  // Short, so the socket path stays under 100 bytes.
  return mkdtempSync(join(tmpdir(), 'kg-e2e-'));
}

export async function start(d: DaemonInfo) {
  run(d, ['start']);
  const lock = join(d.home, 'run', 'daemon.lock');
  for (let i = 0; i < 240; i++) {
    if (existsSync(lock)) {
      try {
        if (JSON.parse(readFileSync(lock, 'utf8')).address) return;
      } catch {
        /* written as we read it */
      }
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error('the daemon did not start');
}

export function stop(d: DaemonInfo) {
  try {
    run(d, ['stop']);
  } catch {
    /* not running */
  }
}

export function cleanup(d: DaemonInfo) {
  stop(d);
  rmSync(d.home, { recursive: true, force: true });
}

/** The fixed address (agents' listener), from the lock file. */
export function fixedAddress(d: DaemonInfo): string {
  return `http://${JSON.parse(readFileSync(join(d.home, 'run', 'daemon.lock'), 'utf8')).address}`;
}

/** A page address with a fresh one-time code: `knoggin open --print-url`. */
export const openUrl = (d: DaemonInfo) => run(d, ['open', '--print-url']);

/** A request on the daemon's Unix socket: the local user, no credential. */
export function api<T>(d: DaemonInfo, method: string, path: string, body?: unknown, headers: Record<string, string> = {}): Promise<{ status: number; headers: Record<string, string | string[] | undefined>; body: T }> {
  return new Promise((resolve, reject) => {
    const data = body === undefined ? undefined : JSON.stringify(body);
    const req = httpRequest(
      {
        socketPath: join(d.home, 'run', 'daemon.sock'),
        path,
        method,
        headers: { Host: 'localhost', ...(data ? { 'Content-Type': 'application/json' } : {}), ...headers },
      },
      (res) => {
        let text = '';
        res.on('data', (c) => (text += c));
        res.on('end', () => {
          const parsed = text ? JSON.parse(text) : undefined;
          if ((res.statusCode ?? 500) >= 400) reject(new Error(`${method} ${path}: ${res.statusCode} ${text}`));
          else resolve({ status: res.statusCode ?? 0, headers: res.headers, body: parsed as T });
        });
      },
    );
    req.on('error', reject);
    if (data) req.write(data);
    req.end();
  });
}
