// Spawns an isolated recalld stack: embedding stub, a synthetic store and
// `recalld ui`. Never touches ~/.recalld: HOME, the socket, the config and
// the data directory all live in a fresh temp dir.
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { existsSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export interface Stack {
  url: string;
  dir: string;
  stop(): void;
}

export async function startStack(bin: string, kind: 'sample' | 'perf'): Promise<Stack> {
  const dir = mkdtempSync(join(tmpdir(), `recalld-ui-${kind}-`));
  const stubPort = 20000 + Math.floor(Math.random() * 20000);
  const config = join(dir, 'config.toml');
  writeFileSync(
    config,
    `[storage]\ndata_dir = "${join(dir, 'data')}"\n[embedding]\nprovider = "ollama"\nmodel_name = "hash-embed"\n` +
      `base_url = "http://127.0.0.1:${stubPort}"\ndimensions = 64\napi_key_env = "RECALLD_UI_TEST_UNUSED_KEY"\n[decay]\ndisable_sweep = true\n`,
  );
  const env = { ...process.env, HOME: join(dir, 'home'), RECALLD_CONFIG: config, RECALLD_DAEMON_SOCKET: join(dir, 'recalld.sock') };
  const children: ChildProcess[] = [];
  children.push(spawn(bin, ['bench', 'embed-stub', '--addr', `127.0.0.1:${stubPort}`, '--dim', '64'], { env, stdio: 'ignore' }));
  const synthetic = kind === 'perf' ? ['--memories', process.env.PERF_MEMORIES ?? '10000', '--edges', process.env.PERF_EDGES ?? '100000'] : ['--sample'];
  const gen = spawnSync(bin, ['--config', config, 'bench', 'synthetic', ...synthetic], { env, encoding: 'utf8' });
  if (gen.status !== 0) throw new Error(`synthetic store failed: ${gen.stderr}`);
  const ui = spawn(bin, ['--config', config, 'ui', '--no-open'], { env, stdio: ['ignore', 'ignore', 'pipe'] });
  children.push(ui);
  const url = await new Promise<string>((resolve, reject) => {
    let out = '';
    const t = setTimeout(() => reject(new Error(`recalld ui did not start: ${out}`)), 120_000);
    ui.stderr!.on('data', (d) => {
      out += d;
      const m = out.match(/http:\/\/127\.0\.0\.1:\d+\/\?t=[0-9a-f]+/);
      if (m) {
        clearTimeout(t);
        resolve(m[0]);
      }
    });
  });
  // `recalld ui` starts the daemon but stops waiting after 10 s; a debug
  // daemon takes ~40 s to open its store on a fast machine and well over two
  // minutes on a CI runner already running another stack. Wait for its socket.
  const socket = join(dir, 'recalld.sock');
  for (let i = 0; i < 720 && !existsSync(socket); i++) await new Promise((r) => setTimeout(r, 500));
  if (!existsSync(socket)) throw new Error('the daemon never opened its socket');
  const stop = () => {
    children.forEach((c) => c.kill());
    spawnSync(bin, ['daemon', 'stop'], { env });
  };
  return { url, dir, stop };
}
