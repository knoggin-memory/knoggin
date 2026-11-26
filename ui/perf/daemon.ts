// Minimal framed JSON-RPC client for the daemon socket (4-byte big-endian
// length prefix), so the perf harness can trigger real agent recalls.
import { connect, type Socket } from 'node:net';

export class DaemonSocket {
  private buf = Buffer.alloc(0);
  private waiting: ((v: unknown) => void)[] = [];
  private id = 1;
  private constructor(private sock: Socket) {
    sock.on('data', (d) => {
      this.buf = Buffer.concat([this.buf, d]);
      while (this.buf.length >= 4) {
        const len = this.buf.readUInt32BE(0);
        if (this.buf.length < 4 + len) break;
        const msg = JSON.parse(this.buf.subarray(4, 4 + len).toString('utf8'));
        this.buf = this.buf.subarray(4 + len);
        this.waiting.shift()?.(msg);
      }
    });
  }
  static open(path: string): Promise<DaemonSocket> {
    return new Promise((resolve, reject) => {
      const s = connect(path, () => resolve(new DaemonSocket(s)));
      s.on('error', reject);
    });
  }
  call(method: string, params: unknown): Promise<{ result?: unknown; error?: { message: string } }> {
    const body = Buffer.from(JSON.stringify({ jsonrpc: '2.0', id: this.id++, method, params }));
    const head = Buffer.alloc(4);
    head.writeUInt32BE(body.length);
    this.sock.write(Buffer.concat([head, body]));
    return new Promise((r) => this.waiting.push(r as never));
  }
  close() {
    this.sock.end();
  }
}
