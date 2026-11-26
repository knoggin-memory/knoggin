// Live activity shared by the map (light, pill) and the activity lists.
import type { UiEvent } from './api/types';
import { connectEvents } from './events';

export interface Received {
  event: UiEvent;
  at: number; // performance.now() when it arrived
}

type Sink = (r: Received, replay?: { speed: number }) => void;

class Live {
  /** The last ~300 events, newest last (DESIGN §7: kept for Replay). */
  recent = $state<Received[]>([]);
  connected = $state(false);
  /** Agent → color slot, in order of first appearance. */
  agents = $state<string[]>([]);
  private sinks = new Set<Sink>();
  private stop: (() => void) | null = null;
  private users = 0;

  start() {
    if (this.users++ > 0) return;
    this.stop = connectEvents(
      (event, at) => this.receive({ event, at }),
      (c) => (this.connected = c),
    );
  }

  release() {
    if (--this.users > 0) return;
    this.stop?.();
    this.stop = null;
  }

  /** Inject an event (dev simulator and perf harness use this path too). */
  receive(r: Received) {
    const agent = 'agent' in r.event ? (r.event.agent ?? null) : null;
    if (agent && !this.agents.includes(agent)) this.agents = [...this.agents, agent];
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

  agentSlot(agent: string | null | undefined): number {
    const i = agent ? this.agents.indexOf(agent) : -1;
    return i < 0 ? 0 : i % 4;
  }

  get latest(): Received | undefined {
    return this.recent.at(-1);
  }
}

export const live = new Live();
