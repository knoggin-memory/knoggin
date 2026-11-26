// Thin client for recalld ui's /api/ui/*. Every read here is read-only on the
// daemon side; the only writes are reinforce() and forget().
import type { UiError } from './types';

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
  get daemonDown() {
    return this.code === 'daemonUnavailable';
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let res: Response;
  try {
    res = await fetch(`/api/ui${path}`, { credentials: 'same-origin', ...init });
  } catch (e) {
    throw new ApiError(0, 'daemonUnavailable', 'recalld ui is not reachable');
  }
  if (res.status === 204) return undefined as T;
  const body = await res.json().catch(() => null);
  if (!res.ok) {
    const err = (body ?? {}) as Partial<UiError>;
    throw new ApiError(res.status, err.error ?? 'internal', err.message ?? res.statusText);
  }
  return body as T;
}

function query(params: Record<string, string | number | boolean | null | undefined | string[]>) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v === null || v === undefined || v === '') continue;
    if (Array.isArray(v)) v.forEach((x) => q.append(k, x));
    else q.set(k, String(v));
  }
  const s = q.toString();
  return s ? `?${s}` : '';
}

export const get = <T>(path: string, params: Parameters<typeof query>[0] = {}) =>
  request<T>(path + query(params));

/** Mutations carry the header recalld ui requires on every write. */
export const post = <T>(path: string, body?: unknown) =>
  request<T>(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-Recalld-UI': '1' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
