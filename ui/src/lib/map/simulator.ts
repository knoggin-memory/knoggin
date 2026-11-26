// Dev simulator (DESIGN Phase 4): synthetic recalls built from the loaded
// graph, fed through the same path as real events. `/map?simulate=5` emits
// five recalls a second; the perf harness uses 50.
import type { Graph } from '$lib/api/snapshot';
import type { UiEvent } from '$lib/api/types';
import { live } from '$lib/live.svelte';

export function simulate(g: Graph, perSecond: number): () => void {
  const n = g.ids.length;
  if (!n || perSecond <= 0) return () => {};
  const adj: number[][] = Array.from({ length: n }, () => []);
  for (let k = 0; k < g.src.length; k++) {
    if (g.type[k] === 4) continue;
    adj[g.src[k]].push(k);
    adj[g.dst[k]].push(k);
  }
  let seq = 1_000_000;
  const agents = ['claude-code', 'claude-desktop', 'cursor'];
  const emit = () => {
    const seed = Math.floor(Math.random() * n);
    const seeds = [{ id: g.ids[seed], score: 0.6 + Math.random() * 0.4 }];
    const trace: { from: string; to: string; edgeType: number; amount: number; order: number }[] = [];
    let frontier = [seed];
    const seen = new Set([seed]);
    for (let order = 0; order < 3 && frontier.length; order++) {
      const next: number[] = [];
      for (const i of frontier.slice(0, 4)) {
        for (const k of adj[i].slice(0, 4)) {
          const j = g.src[k] === i ? g.dst[k] : g.src[k];
          if (seen.has(j)) continue;
          seen.add(j);
          next.push(j);
          trace.push({ from: g.ids[i], to: g.ids[j], edgeType: g.type[k], amount: 0.1 + Math.random() * 0.4, order });
        }
      }
      frontier = next;
    }
    const event: UiEvent = {
      kind: 'recall', seq: seq++, at: Date.now(), agent: agents[seq % agents.length], namespace: 'default', query: 'simulated',
      seeds, trace, returned: [seeds[0].id, ...trace.slice(0, 3).map((t) => t.to)], suppressed: trace.slice(3, 5).map((t) => t.to),
    };
    live.receive({ event, at: performance.now() });
  };
  const timer = setInterval(emit, 1000 / perSecond);
  return () => clearInterval(timer);
}
