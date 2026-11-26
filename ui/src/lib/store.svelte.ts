// SPDX-License-Identifier: AGPL-3.0-only
// The current store: one global setting, shown in the sidebar switcher, kept in
// the browser and sent on every store request as `Knoggin-Store`.
import { observe, setStoreHeader } from './api/client';
import { queryClient } from './query';
import { session } from './session.svelte';
import { scope } from './scope.svelte';
import { selection } from './selection.svelte';

const KEY = 'knoggin.store';

function load(): string | null {
  try {
    return localStorage.getItem(KEY);
  } catch {
    return null;
  }
}

class CurrentStore {
  private chosen = $state<string | null>(load());

  constructor() {
    observe((o) => {
      if (o.kind !== 'error' || !o.store || o.error.code !== 'store_deleting') return;
      this.gone();
    });
  }

  /** The store in use: the chosen one, else the personal store. */
  get id(): string | null {
    const id = this.chosen ?? session.value?.personalStoreId ?? null;
    setStoreHeader(id);
    return id;
  }

  get isPersonal() {
    return this.id === session.value?.personalStoreId;
  }

  set(id: string) {
    if (id === this.id) return;
    this.chosen = id === session.value?.personalStoreId ? null : id;
    try {
      if (this.chosen) localStorage.setItem(KEY, this.chosen);
      else localStorage.removeItem(KEY);
    } catch {
      /* storage blocked: keep the choice for this page */
    }
    setStoreHeader(this.id);
    // Changing the store clears cached store data; its namespaces are its own.
    queryClient.removeQueries({ queryKey: ['store'] });
    scope.set(null);
    selection.select(null);
  }

  /** The current store answered `not_found` or `store_deleting`: fall back to the personal store.
   *  If the personal store itself is being deleted, the account was deleted somewhere else. */
  gone() {
    if (this.isPersonal) void session.load();
    else this.set(session.value?.personalStoreId ?? '');
  }
}

export const store = new CurrentStore();
