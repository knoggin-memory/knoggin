// SPDX-License-Identifier: AGPL-3.0-only
// Per-viewer conveniences kept in the browser: the collapsed sidebar and the theme.
function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
function write(key: string, v: string) {
  try {
    localStorage.setItem(key, v);
  } catch {
    /* ignore */
  }
}

export type Theme = 'system' | 'light' | 'dark';
const THEME = 'knoggin.theme';

const dark = () => typeof matchMedia !== 'undefined' && matchMedia('(prefers-color-scheme: dark)').matches;

/** The `data-theme` value for a choice: System follows the device. */
export const resolveTheme = (t: Theme, systemDark: boolean) => (t === 'dark' || (t === 'system' && systemDark) ? 'dark' : 'light');

class Prefs {
  sidebarCollapsed = $state(read('knoggin.sidebarCollapsed') === '1');
  theme = $state<Theme>((['light', 'dark'] as const).find((t) => t === read(THEME)) ?? 'system');

  toggleSidebar() {
    this.sidebarCollapsed = !this.sidebarCollapsed;
    write('knoggin.sidebarCollapsed', this.sidebarCollapsed ? '1' : '0');
  }

  setTheme(t: Theme) {
    this.theme = t;
    write(THEME, t);
    this.apply();
  }

  /** Sets `data-theme` on <html>; app.html does the same before first paint. */
  apply() {
    document.documentElement.dataset.theme = resolveTheme(this.theme, dark());
  }

  watchSystem() {
    const mq = matchMedia('(prefers-color-scheme: dark)');
    const on = () => this.apply();
    mq.addEventListener('change', on);
    return () => mq.removeEventListener('change', on);
  }
}
export const prefs = new Prefs();
