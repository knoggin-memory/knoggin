// Decodes graph_snapshot's base64 little-endian columns into typed arrays.
import type { GraphSnapshot, NamespaceRef } from './types';
import { get } from './client';
import { DAY_MS, phaseFor, retrievability } from '$lib/fsrs';

function bytes(b64: string): Uint8Array<ArrayBuffer> {
  const bin = atob(b64);
  const out = new Uint8Array(new ArrayBuffer(bin.length));
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
const u32 = (s: string) => new Uint32Array(bytes(s).buffer);
const f32 = (s: string) => new Float32Array(bytes(s).buffer);
const f64 = (s: string) => new Float64Array(bytes(s).buffer);

export interface Graph {
  generatedAt: number;
  namespaces: NamespaceRef[];
  ids: string[];
  index: Map<string, number>;
  ns: Uint32Array;
  phase: Uint8Array;
  strength: Float32Array;
  decayStrength: Float32Array;
  stability: Float32Array;
  createdAt: Float64Array;
  lastAccess: Float64Array;
  degree: Uint32Array;
  permastore: Uint8Array;
  src: Uint32Array;
  dst: Uint32Array;
  type: Uint8Array;
  weight: Float32Array;
}

export function decode(first: GraphSnapshot, pages: GraphSnapshot[] = []): Graph {
  const n = first.nodes;
  if (!n) throw new Error('first snapshot page has no nodes');
  const concat = <T extends Uint32Array | Uint8Array | Float32Array>(
    make: (s: string) => T,
    pick: (p: GraphSnapshot) => string,
    Ctor: { new (len: number): T },
  ): T => {
    const parts = [first, ...pages].map((p) => make(pick(p)));
    if (parts.length === 1) return parts[0];
    const out = new Ctor(parts.reduce((a, p) => a + p.length, 0));
    let o = 0;
    for (const p of parts) {
      out.set(p as never, o);
      o += p.length;
    }
    return out;
  };
  return {
    generatedAt: first.generatedAt,
    namespaces: first.namespaces,
    ids: n.ids,
    index: new Map(n.ids.map((id, i) => [id, i])),
    ns: u32(n.ns),
    phase: bytes(n.phase),
    strength: f32(n.strength),
    decayStrength: f32(n.decayStrength),
    stability: f32(n.stability),
    createdAt: f64(n.createdAt),
    lastAccess: f64(n.lastAccess),
    degree: u32(n.degree),
    permastore: bytes(n.permastore),
    src: concat(u32, (p) => p.edges.src, Uint32Array),
    dst: concat(u32, (p) => p.edges.dst, Uint32Array),
    type: concat(bytes, (p) => p.edges.type, Uint8Array),
    weight: concat(f32, (p) => p.edges.weight, Float32Array),
  };
}

/** Fetch every page of the snapshot for `namespace` (null = all). */
export async function fetchGraph(namespace: string | null): Promise<Graph> {
  const first = await get<GraphSnapshot>('/graph', { namespace });
  const pages: GraphSnapshot[] = [];
  let next = first.nextEdgeOffset;
  while (next !== null) {
    const page = await get<GraphSnapshot>('/graph', { namespace, edgeOffset: next });
    if (page.nodeCount !== first.nodeCount) return fetchGraph(namespace); // graph changed mid-read
    pages.push(page);
    next = page.nextEdgeOffset;
  }
  return decode(first, pages);
}

/**
 * The part of `g` active since `since` (epoch ms): memories created or
 * accessed since then, and the edges between them. Fewer nodes for the map's
 * physics on large stores; `degree` keeps each memory's full link count.
 */
export function activeSince(g: Graph, since: number): Graph {
  const keep: number[] = [];
  for (let i = 0; i < g.ids.length; i++) if (Math.max(g.createdAt[i], g.lastAccess[i]) >= since) keep.push(i);
  if (keep.length === g.ids.length) return g;
  const remap = new Int32Array(g.ids.length).fill(-1);
  keep.forEach((old, i) => (remap[old] = i));
  const edges: number[] = [];
  for (let e = 0; e < g.src.length; e++) if (remap[g.src[e]] >= 0 && remap[g.dst[e]] >= 0) edges.push(e);
  const pick = <T extends Uint8Array | Uint32Array | Float32Array | Float64Array>(a: T, at: number[]): T => {
    const out = new (a.constructor as { new (n: number): T })(at.length);
    at.forEach((j, i) => (out[i] = a[j]));
    return out;
  };
  const ids = keep.map((i) => g.ids[i]);
  return {
    ...g,
    ids,
    index: new Map(ids.map((id, i) => [id, i])),
    ns: pick(g.ns, keep),
    phase: pick(g.phase, keep),
    strength: pick(g.strength, keep),
    decayStrength: pick(g.decayStrength, keep),
    stability: pick(g.stability, keep),
    createdAt: pick(g.createdAt, keep),
    lastAccess: pick(g.lastAccess, keep),
    degree: pick(g.degree, keep),
    permastore: pick(g.permastore, keep),
    src: Uint32Array.from(edges, (e) => remap[g.src[e]]),
    dst: Uint32Array.from(edges, (e) => remap[g.dst[e]]),
    type: pick(g.type, edges),
    weight: pick(g.weight, edges),
  };
}

/** The decay multiplier the sweep applies to memory `i`'s namespace. */
export function decayMultiplier(g: Graph, i: number): number {
  return g.namespaces.find((n) => n.id === g.ns[i])?.decayMultiplier ?? 1;
}

/** Retrievability of memory `i` at `at` (the Explore "strength"). */
export function strengthAt(g: Graph, i: number, at = Date.now()): number {
  if (i < 0 || g.permastore[i]) return 1;
  if (g.stability[i] <= 0) return g.strength[i];
  return retrievability(Math.max(0, at - g.lastAccess[i]) / DAY_MS, g.stability[i], decayMultiplier(g, i));
}

/** The phase memory `i` would be in at `at` (the map's Decay scrubber). */
export function projectedPhase(g: Graph, i: number, at: number): number {
  if (g.permastore[i] || g.phase[i] === 4 || g.stability[i] <= 0) return g.phase[i];
  const ns = g.namespaces.find((n) => n.id === g.ns[i]);
  // The projection never makes a memory stronger than recalld says it is now.
  return Math.max(g.phase[i], phaseFor(strengthAt(g, i, at), (ns?.thresholds as [number, number, number]) ?? undefined));
}
