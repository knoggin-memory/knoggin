// SPDX-License-Identifier: AGPL-3.0-only
// The one module that talks to Knoggin's API. It adds the credential it holds
// (a local session token from `knoggin open`, or nothing: a cookie travels by
// itself), `X-Knoggin-UI: 1` on every request that is not a GET, and the
// current store on store routes. No screen knows which credential is in use.
import type { ApiErrorBody } from './shapes';

const TOKEN = 'knoggin.sessionToken';

export class ApiError extends Error {
  constructor(
    readonly status: number,
    /** The API's code, or `unreachable` when no answer in the API's shape came back. */
    readonly code: string,
    message: string,
    readonly reason: string | null = null,
    readonly field: string | null = null,
    /** Seconds, from `Retry-After`. */
    readonly retryAfter: number | null = null,
    readonly docsUrl: string | null = null,
  ) {
    super(message);
  }
}

/** What a request led to, for the modules that track waiting states. */
export type Outcome =
  | { kind: 'ok'; store: boolean; path: string }
  | { kind: 'error'; store: boolean; path: string; error: ApiError; scoped: boolean; write: boolean }
  | { kind: 'slow'; started: boolean };

const listeners = new Set<(o: Outcome) => void>();
export function observe(fn: (o: Outcome) => void) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}
/** A store request has succeeded since the page loaded: the event stream may open. */
let storeAnswered = false;
export const storeHasAnswered = () => storeAnswered;
const emit = (o: Outcome) => {
  if (o.kind === 'ok' && o.store) storeAnswered = true;
  listeners.forEach((fn) => fn(o));
};

// ── The credential ────────────────────────────────────────────────────────
function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
export const token = {
  /** The stored token, or the one this page holds when storage is blocked. */
  get: () => read(TOKEN) ?? memoryToken,
  set(value: string) {
    try {
      localStorage.setItem(TOKEN, value);
    } catch {
      /* storage blocked: the token lives for this page only */
      memoryToken = value;
    }
  },
  drop() {
    memoryToken = null;
    try {
      localStorage.removeItem(TOKEN);
    } catch {
      /* nothing stored */
    }
  },
};
let memoryToken: string | null = null;
const credential = () => token.get();
export const holdsToken = () => credential() !== null;

// ── The current store, sent on store routes as Knoggin-Store ───────────────
let storeId: string | null = null;
export const setStoreHeader = (id: string | null) => (storeId = id);

type Value = string | number | boolean | null | undefined | string[];
export function query(params: Record<string, Value>) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v === null || v === undefined || v === '') continue;
    if (Array.isArray(v)) v.forEach((x) => q.append(k, x));
    else q.set(k, String(v));
  }
  const s = q.toString();
  return s ? `?${s}` : '';
}

export interface Options {
  query?: Record<string, Value>;
  body?: unknown;
  /** A store route: carries Knoggin-Store and counts for the waking state. */
  store?: boolean;
  /** Sends `namespace` from the scope: a `not_found` means the namespace is gone. */
  scoped?: boolean;
}

function headers(method: string, opts: Options): Headers {
  const h = new Headers();
  const t = credential();
  if (t) h.set('Authorization', `Bearer ${t}`);
  if (method !== 'GET') h.set('X-Knoggin-UI', '1');
  if (opts.body !== undefined) h.set('Content-Type', 'application/json');
  if (opts.store && storeId) h.set('Knoggin-Store', storeId);
  return h;
}

export function parseError(status: number, body: unknown, retryAfter: string | null): ApiError {
  const e = (body as Partial<ApiErrorBody> | null)?.error;
  if (!e || typeof e.code !== 'string') {
    // Not the API's shape: a tunnel or proxy answered for a Knoggin that is down.
    return new ApiError(status, 'unreachable', 'Knoggin cannot be reached.');
  }
  const after = retryAfter !== null && /^\d+$/.test(retryAfter) ? Number(retryAfter) : null;
  return new ApiError(status, e.code, e.message ?? '', e.reason ?? null, e.field ?? null, after, e.docsUrl ?? null);
}

const SLOW_MS = 1_000;
let slowCount = 0;

export async function request<T>(method: string, path: string, opts: Options = {}): Promise<T> {
  const url = path + query(opts.query ?? {});
  const isStore = !!opts.store;
  let slow: ReturnType<typeof setTimeout> | undefined;
  let wasSlow = false;
  if (isStore) {
    slow = setTimeout(() => {
      wasSlow = true;
      if (slowCount++ === 0) emit({ kind: 'slow', started: true });
    }, SLOW_MS);
  }
  const done = () => {
    clearTimeout(slow);
    if (wasSlow && --slowCount === 0) emit({ kind: 'slow', started: false });
  };
  let res: Response;
  try {
    res = await fetch(url, {
      method,
      headers: headers(method, opts),
      body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
      credentials: 'same-origin',
      cache: 'no-store',
    });
  } catch {
    done();
    const error = new ApiError(0, 'unreachable', 'Knoggin cannot be reached.');
    emit({ kind: 'error', store: isStore, path, error, scoped: !!opts.scoped, write: method !== 'GET' });
    throw error;
  }
  done();
  if (res.ok) {
    emit({ kind: 'ok', store: isStore, path });
    if (res.status === 204) return undefined as T;
    const text = await res.text();
    return (text ? JSON.parse(text) : undefined) as T;
  }
  const body = await res.json().catch(() => null);
  const error = parseError(res.status, body, res.headers.get('Retry-After'));
  emit({ kind: 'error', store: isStore, path, error, scoped: !!opts.scoped, write: method !== 'GET' });
  throw error;
}

export const get = <T>(path: string, opts: Omit<Options, 'body'> = {}) => request<T>('GET', path, opts);
export const post = <T>(path: string, body?: unknown, opts: Options = {}) => request<T>('POST', path, { ...opts, body });
export const patch = <T>(path: string, body?: unknown) => request<T>('PATCH', path, { body });
export const del = <T>(path: string, opts: Options = {}) => request<T>('DELETE', path, opts);

/** A store route, with the current store. */
export const sget = <T>(path: string, q: Record<string, Value> = {}, scoped = false) => get<T>(path, { query: q, store: true, scoped });

/**
 * One store's export. Without a local session token it is a plain link, which
 * streams to disk; with one, a link cannot carry the token, so the file is
 * fetched and handed to the browser.
 */
export function exportHref(id: string) {
  return `/v1/export${query({ store: id })}`;
}
export async function downloadExport(id: string, fallbackName: string) {
  const t = credential();
  if (!t) {
    const a = document.createElement('a');
    a.href = exportHref(id);
    a.download = '';
    a.click();
    return;
  }
  const res = await fetch(exportHref(id), { headers: { Authorization: `Bearer ${t}` }, cache: 'no-store' });
  if (!res.ok) throw parseError(res.status, await res.json().catch(() => null), res.headers.get('Retry-After'));
  const name = /filename="([^"]+)"/.exec(res.headers.get('Content-Disposition') ?? '')?.[1] ?? fallbackName;
  const url = URL.createObjectURL(await res.blob());
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
}
