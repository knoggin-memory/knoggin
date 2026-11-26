// The global namespace scope (DESIGN §2): one setting, shown in the sidebar
// and the first chip of every toolbar. Persisted across sessions. Starts on
// `default`, the lighter view on a store split into project namespaces;
// All namespaces is stored as `*`.
const KEY = 'recalld.scope';
const ALL = '*';

function load(): string | null {
  try {
    const stored = localStorage.getItem(KEY);
    return stored === ALL ? null : stored || 'default';
  } catch {
    return 'default';
  }
}

class Scope {
  /** `null` means All namespaces. */
  current = $state<string | null>(load());

  set(namespace: string | null) {
    this.current = namespace;
    try {
      localStorage.setItem(KEY, namespace ?? ALL);
    } catch {
      /* storage unavailable: keep the in-memory choice */
    }
  }

  get label() {
    return this.current ?? 'All namespaces';
  }
}

export const scope = new Scope();
