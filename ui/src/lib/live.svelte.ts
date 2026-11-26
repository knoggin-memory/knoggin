// SPDX-License-Identifier: AGPL-3.0-only
// Live activity shared by the map (light, pill) and the Timeline. Events arrive
// with names (phase, edge type); the map works in recalld's numbers, so they are
// turned into numbers here, once.
import type { WireEvent } from './api/shapes';
import { connectEvents } from './events';
import { EDGE_CODE, PHASE_CODE } from './map/encoding';
import { queryClient } from './query';
import { store } from './store.svelte';

type Spread = { from: string; to: string; edgeType: number; amount: number; order: number };
type Link = { to: string; edgeType: number; weight: number };

/** A live event with edge types and phases as numbers. */
export type UiEvent =
  | (Omit<Extract<WireEvent, { kind: 'recall' }>, 'spreads'> & { spreads: Spread[] })
  | (Omit<Extract<WireEvent, { kind: 'store' }>, 'phase' | 'edges'> & { phase: number; edges: Link[] })
  | (Omit<Extract<WireEvent, { kind: 'link' }>, 'edgeType'> & { edgeType: number })
  | Extract<WireEvent, { kind: 'reinforce' }>
  | Extract<WireEvent, { kind: 'forget' }>
  | (Omit<Extract<WireEvent, { kind: 'phase_change' }>, 'from' | 'to'> & { from: number; to: number })
  | Extract<WireEvent, { kind: 'sweep' }>
  | Extract<WireEvent, { kind: 'lagged' }>;

export function normalize(e: WireEvent): UiEvent {
  switch (e.kind) {
    case 'recall':
      return { ...e, spreads: e.spreads.map((s) => ({ ...s, edgeType: EDGE_CODE[s.edgeType] ?? 0 })) };
    case 'store':
      return { ...e, phase: PHASE_CODE[e.phase] ?? 1, edges: e.edges.map((x) => ({ ...x, edgeType: EDGE_CODE[x.edgeType] ?? 0 })) };
    case 'link':
      return { ...e, edgeType: EDGE_CODE[e.edgeType] ?? 0 };
    case 'phase_change':
      return { ...e, from: PHASE_CODE[e.from] ?? 1, to: (e.to as string) === 'deleted' ? 4 : (PHASE_CODE[e.to] ?? 1) };
    default:
      return e;
  }
}

export interface Received {
  event: UiEvent;
  at: number; // performance.now() when it arrived
}

type Sink = (r: Received, replay?: { speed: number }) => void;

class Live {
  /** The last 300 events, newest last. */
  recent = $state<Received[]>([]);
  connected = $state(false);
  /** Client → color slot, in order of first appearance. */
  clients = $state<string[]>([]);
  private sinks = new Set<Sink>();
  private stop: (() => void) | null = null;
  private users = 0;
  private storeId: string | null = null;

  start() {
    if (this.users++ > 0) return;
    this.connect();
  }

  /** Reconnects with the current store (after a store switch). */
  connect() {
    this.stop?.();
    this.storeId = store.id;
    this.recent = [];
    this.stop = connectEvents(
      () => this.storeId,
      (event, at) => this.receive({ event: normalize(event), at }),
      (c) => (this.connected = c),
    );
  }

  ensureStore(id: string | null) {
    if (this.users > 0 && id !== this.storeId) this.connect();
  }

  release() {
    if (--this.users > 0) return;
    this.stop?.();
    this.stop = null;
  }

  /** Takes one event from the stream. */
  receive(r: Received) {
    // Events were missed: read again everything the screen shows from the store.
    if (r.event.kind === 'lagged') void queryClient.invalidateQueries({ queryKey: ['store'] });
    const client = 'client' in r.event ? (r.event.client ?? null) : null;
    if (client && !this.clients.includes(client)) this.clients = [...this.clients, client];
    this.recent.push(r);
    if (this.recent.length > 300) this.recent.splice(0, this.recent.length - 300);
    for (const s of this.sinks) s(r);
  }

  replay(r: Received, speed = 3) {
    for (const s of this.sinks) s({ event: r.event, at: performance.now() }, { speed });
  }

  subscribe(s: Sink) {
    this.sinks.add(s);
    return () => this.sinks.delete(s);
  }

  clientSlot(client: string | null | undefined): number {
    const i = client ? this.clients.indexOf(client) : -1;
    return i < 0 ? 0 : i % 4;
  }

  get latest(): Received | undefined {
    return this.recent.at(-1);
  }
}

export const live = new Live();
