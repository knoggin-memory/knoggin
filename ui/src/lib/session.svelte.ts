// SPDX-License-Identifier: AGPL-3.0-only
// The session: who is here, which install this is, and which pages to show.
// `GET /v1/session` comes first; every other screen follows from it.
import { ApiError, get, holdsToken, observe, post, token } from './api/client';
import type { Session } from './api/shapes';

export type Phase = 'loading' | 'ready' | 'locked' | 'unreachable';

const LOCK_POLL_MS = 60_000;

class SessionState {
  value = $state<Session | null>(null);
  phase = $state<Phase>('loading');
  /** `store_locked` from the session route: the whole local install is locked. */
  lockReason = $state<string | null>(null);
  lockDocsUrl = $state<string | null>(null);
  /** Set when a request answered `permission_denied` with `account_removed` or `not_allowed`. */
  deniedReason = $state<string | null>(null);
  private timer: ReturnType<typeof setTimeout> | undefined;
  private loading: Promise<void> | null = null;

  constructor() {
    observe((o) => {
      if (o.kind !== 'error') return;
      const e = o.error;
      if (e.code === 'unauthenticated') {
        token.drop();
        void this.load();
      } else if (e.code === 'permission_denied' && (e.reason === 'account_removed' || e.reason === 'not_allowed')) {
        token.drop();
        this.deniedReason = e.reason;
        void this.load();
      } else if (e.code === 'store_deleting' && !o.store) {
        // A control route says the account is scheduled for deletion.
        void this.load();
      }
    });
    // Polling stops while the tab is hidden and starts again when it is back.
    if (typeof document !== 'undefined')
      document.addEventListener('visibilitychange', () => {
        if (document.hidden) clearTimeout(this.timer);
        else if (this.phase === 'locked' || this.phase === 'unreachable') void this.load();
      });
  }

  private again() {
    if (!document.hidden) this.timer = setTimeout(() => this.load(), LOCK_POLL_MS);
  }

  /** Exchanges the one-time code of `knoggin open` (`/?t=<code>`) for a session token. */
  async exchange(code: string) {
    try {
      const r = await post<{ sessionToken: string }>('/v1/local/session', { code });
      token.set(r.sessionToken);
    } catch {
      /* a used or expired code: the session route then says there is no session */
    }
  }

  load(): Promise<void> {
    this.loading ??= this.fetch().finally(() => (this.loading = null));
    return this.loading;
  }

  private async fetch() {
    clearTimeout(this.timer);
    try {
      this.value = await get<Session>('/v1/session');
      this.phase = 'ready';
      this.lockReason = null;
    } catch (e) {
      if (e instanceof ApiError && e.code === 'store_locked') {
        this.phase = 'locked';
        this.lockReason = e.reason;
        this.lockDocsUrl = e.docsUrl;
        this.again();
      } else if (!this.value) {
        this.phase = 'unreachable';
        this.again();
      }
    }
  }

  replace(s: Session) {
    this.value = s;
  }

  get s() {
    return this.value;
  }
  get local() {
    return this.value?.mode === 'local';
  }
  get server() {
    return this.value?.mode === 'server';
  }
  get signInOn() {
    return this.value?.auth === 'oauth';
  }
  /** A `knoggin open` session: the API module holds a local session token. */
  get openSession() {
    return holdsToken();
  }
  /** A cookie session: sign-in is on, the session is signed in and no token is held. */
  get cookieSession() {
    return this.value?.auth === 'oauth' && !!this.value.signedIn && !holdsToken();
  }
  /** A cookie session at a local install's public address: the browser may be on another machine,
   *  and keys, store deletes and the account's deletion need `knoggin open`. */
  get remoteLocal() {
    return this.local && this.cookieSession;
  }
  get isAdmin() {
    return !!this.value?.user?.isAdmin;
  }
  docs(file: string) {
    return `${this.value?.docsUrl ?? this.lockDocsUrl ?? ''}/${file}`;
  }
}

export const session = new SessionState();

/** "today", "yesterday" or the date, for the date-only fields `lastUsedOn` and `lastActiveOn` (UTC dates). */
export function dayLabel(date: string | null, now = new Date()): string {
  if (!date) return 'Never';
  const today = now.toISOString().slice(0, 10);
  const y = new Date(now.getTime() - 86_400_000).toISOString().slice(0, 10);
  if (date === today) return 'today';
  if (date === y) return 'yesterday';
  const [yy, mm, dd] = date.split('-').map(Number);
  return new Date(Date.UTC(yy, mm - 1, dd)).toLocaleDateString('en-US', { month: 'short', day: 'numeric', year: 'numeric', timeZone: 'UTC' });
}

/** A `return_to` the UI follows itself: a path on this site (one leading `/`, no backslash,
 *  no control character), the same rule the server applies, else `/map`. A `//host` or backslash form
 *  would leave the site. */
export function safeReturnTo(p: string | null): string {
  // eslint-disable-next-line no-control-regex
  return p && p.startsWith('/') && p[1] !== '/' && !p.includes('\\') && !/[\u0000-\u001f\u007f-\u009f]/.test(p) ? p : '/map';
}

/** The name to show for a user: the name, else the email, else "User" and 8 characters of the ID. */
export function who(u: { id: string; name?: string | null; email?: string | null }) {
  const text = u.name || u.email || `User ${u.id.slice(0, 8)}`;
  const initials = (u.name ? text.split(/\s+/).map((w) => w[0]).join('') : text.charAt(0)).slice(0, 2).toUpperCase();
  return { text, initials };
}
