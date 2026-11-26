// Per-viewer UI conveniences that persist (collapsed sidebar).
function read(key: string, fallback: boolean) {
  try {
    const v = localStorage.getItem(key);
    return v === null ? fallback : v === '1';
  } catch {
    return fallback;
  }
}
function write(key: string, v: boolean) {
  try {
    localStorage.setItem(key, v ? '1' : '0');
  } catch {
    /* ignore */
  }
}

class Prefs {
  sidebarCollapsed = $state(read('recalld.sidebarCollapsed', false));
  toggleSidebar() {
    this.sidebarCollapsed = !this.sidebarCollapsed;
    write('recalld.sidebarCollapsed', this.sidebarCollapsed);
  }
}
export const prefs = new Prefs();
