// SPDX-License-Identifier: AGPL-3.0-only
// The global namespace scope: one setting, shown in the sidebar
// and the first chip of every toolbar. Kept in the browser. Starts on all
// namespaces, which is what the /v1/ui/* routes read when no namespace is sent.
const KEY = 'knoggin.scope';

function load(): string | null {
  try {
    return localStorage.getItem(KEY) || null;
  } catch {
    return null;
  }
}

class Scope {
  /** `null` means All namespaces. */
  current = $state<string | null>(load());

  set(namespace: string | null) {
    this.current = namespace;
    try {
      if (namespace === null) localStorage.removeItem(KEY);
      else localStorage.setItem(KEY, namespace);
    } catch {
      /* storage unavailable: keep the in-memory choice */
    }
  }

  get label() {
    return this.current ?? 'All namespaces';
  }
}

export const scope = new Scope();
