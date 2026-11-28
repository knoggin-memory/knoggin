// SPDX-License-Identifier: AGPL-3.0-only
// The waiting and failure states.
// They come from the store status route, which may be polled because it is a
// control route, and from the error codes of the UI's other requests. Store
// routes are never polled on a timer.
import { ApiError, get, observe } from './api/client';
import type { StoreStatus } from './api/shapes';
import { queryClient } from './query';
import { session } from './session.svelte';
import { store } from './store.svelte';
import { scope } from './scope.svelte';

export type Indicator =
  | { kind: 'not-running'; remote: boolean }
  | { kind: 'locked'; reason: string }
  | { kind: 'updating'; retryIn: number | null }
  | { kind: 'unavailable'; reason: string | null; exhausted: boolean }
  | { kind: 'waking' }
  | { kind: 'full' }
  | { kind: 'model-unavailable'; reason: string | null }
  | { kind: 'downloading'; done: number; total: number; name: string }
  | { kind: 'catching-up'; pending: number };

/** Banners keep the screen usable; the others take the screen inside the shell. */
export const WHOLE_SCREEN = new Set(['locked', 'updating', 'waking']);

const MAX_UNAVAILABLE = 5;
const MINUTE = 60_000;

/** The first state that applies, in priority order. Exported for its unit test. */
export function pick(s: {
  notRunning: boolean;
  remote: boolean;
  locked: string | null;
  upgrading: number | null | undefined;
  unavailable: { reason: string | null; attempts: number } | null;
  waking: boolean;
  full: boolean;
  status: StoreStatus | null;
}): Indicator | null {
  if (s.notRunning) return { kind: 'not-running', remote: s.remote };
  if (s.locked) return { kind: 'locked', reason: s.locked };
  if (s.upgrading !== undefined) return { kind: 'updating', retryIn: s.upgrading };
  if (s.unavailable) {
    const keeps = s.unavailable.reason === 'out_of_memory' || s.unavailable.reason === 'crashed';
    return { kind: 'unavailable', reason: s.unavailable.reason, exhausted: !keeps && s.unavailable.attempts >= MAX_UNAVAILABLE };
  }
  if (s.waking) return { kind: 'waking' };
  if (s.full) return { kind: 'full' };
  const m = s.status?.model;
  if (m?.state === 'unavailable') return { kind: 'model-unavailable', reason: m.reason ?? null };
  if (m?.state === 'downloading') return { kind: 'downloading', done: m.bytesDone ?? 0, total: m.bytesTotal ?? 0, name: m.name };
  const pending = s.status?.embedding?.pending ?? 0;
  if (pending > 0) return { kind: 'catching-up', pending };
  return null;
}

/** How long until the next status read: every 2 s while waking or downloading, 5 s while
 *  memories wait for vectors, 60 s while locked; otherwise none. */
export function pollDelay(st: StoreStatus | null, slow: boolean): number | null {
  if (slow || st?.state === 'waking' || st?.model?.state === 'downloading') return 2_000;
  if (st?.state === 'locked') return MINUTE;
  if ((st?.embedding?.pending ?? 0) > 0) return 5_000;
  return null;
}

class Status {
  status = $state<StoreStatus | null>(null);
  /** A store request has had no answer for a second. */
  slow = $state(false);
  notRunning = $state(false);
  lockedByError = $state<string | null>(null);
  /** Seconds until a retry while `upgrading`; undefined when not upgrading. */
  upgrading = $state<number | null | undefined>(undefined);
  unavailable = $state<{ reason: string | null; attempts: number } | null>(null);
  fullByWrite = $state(false);
  /** The short notice for `rate_limited`: when to try again, as a time. */
  rateLimitedUntil = $state<number | null>(null);
  /** The reason of that `rate_limited`, if it gave one. */
  rateLimitedReason = $state<string | null>(null);

  private pollTimer: ReturnType<typeof setTimeout> | undefined;
  private retryTimer: ReturnType<typeof setTimeout> | undefined;
  private reachTimer: ReturnType<typeof setTimeout> | undefined;
  /** A retry round is scheduled and has not run yet. */
  private retryPending = false;
  private polledFor: string | null = null;
  private started = false;

  get indicator(): Indicator | null {
    return pick({
      notRunning: this.notRunning,
      remote: session.remoteLocal,
      locked: this.lockedByError ?? (this.status?.state === 'locked' ? (this.status.lockedReason ?? 'master_key_missing') : null),
      upgrading: this.upgrading,
      unavailable: this.unavailable,
      waking: this.status?.state === 'waking' || (this.slow && this.status?.state !== 'ready'),
      full: !!this.status?.full || this.fullByWrite,
      status: this.status,
    });
  }

  start() {
    if (this.started) return;
    this.started = true;
    observe((o) => {
      if (o.kind === 'slow') {
        this.slow = o.started;
        if (o.started) this.poll();
        return;
      }
      if (o.kind === 'ok') {
        this.notRunning = false;
        if (o.store) {
          this.lockedByError = null;
          this.upgrading = undefined;
          this.unavailable = null;
        }
        return;
      }
      this.onError(o.error, o.store, o.scoped, o.write);
    });
    // Polling stops while the tab is hidden and starts again when it is back.
    document.addEventListener('visibilitychange', () => {
      if (document.hidden) {
        clearTimeout(this.pollTimer);
        clearTimeout(this.reachTimer);
      } else {
        this.poll();
        if (this.notRunning) this.markNotRunning();
      }
    });
  }

  private onError(e: ApiError, isStore: boolean, scoped: boolean, write: boolean) {
    switch (e.code) {
      case 'unreachable':
        // On a server a network error is an ordinary load failure ("Could not load this.").
        if (session.local) this.markNotRunning();
        return;
      case 'rate_limited':
        this.rateLimitedUntil = Date.now() + (e.retryAfter ?? 30) * 1000;
        this.rateLimitedReason = e.reason;
        return;
      case 'store_locked':
        if (isStore) this.lockedByError = e.reason ?? 'master_key_missing';
        break;
      case 'upgrading':
        this.upgrading = e.retryAfter;
        this.retry((e.retryAfter ?? 5) * 1000);
        break;
      case 'unavailable': {
        // Parallel requests fail together: the errors of one retry round count once.
        const prev = this.unavailable?.attempts ?? 0;
        const attempts = this.retryPending ? prev : Math.min(prev + 1, MAX_UNAVAILABLE);
        this.unavailable = { reason: e.reason, attempts };
        if (this.retryPending) break;
        const keeps = e.reason === 'out_of_memory' || e.reason === 'crashed';
        if (keeps) this.retry(MINUTE);
        else if (attempts < MAX_UNAVAILABLE) this.retry((e.retryAfter ?? 2) * 1000);
        break;
      }
      case 'resource_exhausted':
        if (write) this.fullByWrite = true;
        break;
      case 'not_found':
        if (scoped && scope.current !== null) scope.set(null);
        break;
    }
    // After any store error, read the store status once.
    if (isStore) this.poll();
  }

  private markNotRunning() {
    this.notRunning = true;
    clearTimeout(this.reachTimer);
    // Ask the session route again every 60 seconds; the banner goes when it answers.
    const again = () => {
      if (document.hidden) return;
      this.reachTimer = setTimeout(async () => {
        try {
          await get('/v1/session');
          this.notRunning = false;
          void queryClient.refetchQueries({ type: 'active' });
        } catch {
          again();
        }
      }, MINUTE);
    };
    again();
  }

  /** The Retry button of "Temporarily unavailable", once its retries are used up. */
  retryNow() {
    this.unavailable = null;
    void queryClient.refetchQueries({ type: 'active' });
  }

  private retry(ms: number) {
    clearTimeout(this.retryTimer);
    this.retryPending = true;
    this.retryTimer = setTimeout(() => {
      this.retryPending = false;
      void queryClient.refetchQueries({ type: 'active' });
    }, ms);
  }

  /** Reads the current store's status now, and schedules the next read if one is due. */
  async poll() {
    clearTimeout(this.pollTimer);
    const id = store.id;
    if (!id || session.phase !== 'ready' || !session.value?.signedIn) return;
    if (this.polledFor !== id) {
      this.status = null;
      this.polledFor = id;
    }
    try {
      const st = await get<StoreStatus>(`/v1/stores/${id}/status`);
      if (store.id !== id) return;
      const wasBlocked = this.status?.state === 'locked' || this.status?.state === 'waking' || this.lockedByError;
      this.status = st;
      this.fullByWrite = false;
      if (st.state !== 'locked') this.lockedByError = null;
      if (wasBlocked && st.state === 'ready') void queryClient.refetchQueries({ type: 'active' });
    } catch (e) {
      if (e instanceof ApiError && (e.code === 'not_found' || e.code === 'store_deleting')) {
        store.gone();
        return;
      }
    }
    if (document.hidden) return;
    const next = pollDelay(this.status, this.slow);
    if (next !== null) this.pollTimer = setTimeout(() => this.poll(), next);
  }
}

export const status = new Status();
