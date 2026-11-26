// SPDX-License-Identifier: AGPL-3.0-only
// The live event stream, GET /v1/ui/events. A header must be sent, so it is
// read with fetch and a streamed body, not EventSource. Reconnects with
// back-off from 0.5 to 15 seconds. A 204 (the store is not open) stops it until
// the next successful store request; `rate_limited` stops it for good.
import { observe, parseError, storeHasAnswered, token } from './api/client';
import type { WireEvent } from './api/shapes';

export type Listener = (e: WireEvent, receivedAt: number) => void;

/** Splits a server-sent event stream into events. Exported for its unit test. */
export class SseParser {
  private buf = '';
  private event = '';
  private data: string[] = [];
  constructor(private onEvent: (name: string, data: string) => void) {}
  push(chunk: string) {
    this.buf += chunk;
    let nl: number;
    while ((nl = this.buf.search(/\r\n|\r|\n/)) >= 0) {
      const line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + (this.buf.startsWith('\r\n', nl) ? 2 : 1));
      this.line(line);
    }
  }
  private line(line: string) {
    if (line === '') {
      if (this.data.length) this.onEvent(this.event || 'message', this.data.join('\n'));
      this.event = '';
      this.data = [];
      return;
    }
    if (line.startsWith(':')) return; // keep-alive comment
    const i = line.indexOf(':');
    const field = i < 0 ? line : line.slice(0, i);
    const value = i < 0 ? '' : line.slice(i + 1).replace(/^ /, '');
    if (field === 'event') this.event = value;
    else if (field === 'data') this.data.push(value);
  }
}

export function connectEvents(
  storeId: () => string | null,
  onEvent: Listener,
  onState?: (connected: boolean) => void,
): () => void {
  let delay = 500;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let closed = false;
  let abort: AbortController | null = null;
  /** Waiting for a successful store request before (re)subscribing. */
  let parked = true;
  let refused = false;

  const stopWatch = observe((o) => {
    if (o.kind === 'ok' && o.store && o.path !== '/v1/ui/events' && parked && !refused && !closed) {
      parked = false;
      void open();
    }
  });

  const retry = () => {
    if (closed) return;
    timer = setTimeout(() => void open(), delay);
    delay = Math.min(delay * 2, 15_000);
  };

  async function open() {
    if (closed) return;
    abort = new AbortController();
    const headers = new Headers();
    const t = token.get();
    if (t) headers.set('Authorization', `Bearer ${t}`);
    const id = storeId();
    if (id) headers.set('Knoggin-Store', id);
    let res: Response;
    try {
      res = await fetch('/v1/ui/events', { headers, signal: abort.signal, cache: 'no-store', credentials: 'same-origin' });
    } catch {
      onState?.(false);
      retry();
      return;
    }
    if (res.status === 204) {
      // The store is not open: subscribe again after the next successful store request.
      onState?.(false);
      parked = true;
      return;
    }
    if (!res.ok || !res.body) {
      onState?.(false);
      const e = parseError(res.status, await res.json().catch(() => null), res.headers.get('Retry-After'));
      if (e.code === 'rate_limited' || e.code === 'permission_denied' || e.code === 'unauthenticated') {
        refused = true; // a refused stream stops asking and shows nothing
        return;
      }
      retry();
      return;
    }
    delay = 500;
    onState?.(true);
    const parser = new SseParser((_name, data) => {
      try {
        onEvent(JSON.parse(data) as WireEvent, performance.now());
      } catch {
        /* a malformed frame is skipped */
      }
    });
    const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        parser.push(value);
      }
    } catch {
      /* aborted or dropped */
    }
    onState?.(false);
    retry();
  }

  if (storeHasAnswered()) {
    parked = false;
    void open();
  }

  return () => {
    closed = true;
    stopWatch();
    clearTimeout(timer);
    abort?.abort();
  };
}
