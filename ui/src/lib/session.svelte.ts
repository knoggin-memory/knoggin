// Daemon connection state for the shell's banner and status line.
import { ApiError, get, post } from './api/client';
import type { UiSession } from './api/types';

class Session {
  value = $state<UiSession | null>(null);
  loading = $state(true);
  starting = $state(false);

  /** The session cookie is missing or from an earlier `recalld ui` launch. */
  signedOut = $state(false);

  get status() {
    if (this.signedOut) return 'signedOut';
    return this.value?.daemon.status ?? (this.loading ? 'loading' : 'unavailable');
  }

  async refresh() {
    try {
      this.value = await get<UiSession>('/session');
      this.signedOut = false;
    } catch (e) {
      this.value = null;
      this.signedOut = e instanceof ApiError && e.status === 403;
    } finally {
      this.loading = false;
    }
  }

  async startDaemon() {
    this.starting = true;
    try {
      await post('/daemon/start');
    } catch {
      /* the banner stays; refresh shows the latest message */
    } finally {
      this.starting = false;
      await this.refresh();
    }
  }
}
export const session = new Session();
