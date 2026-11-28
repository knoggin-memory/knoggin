// SPDX-License-Identifier: AGPL-3.0-only
// The map's physics, off the main thread: d3-force with
// Barnes–Hut many-body, link, collide and gentle centering. Positions are
// streamed back as transferable Float32Arrays.
import { forceCollide, forceLink, forceManyBody, forceSimulation, forceX, forceY, type SimulationNodeDatum } from 'd3-force';
import { hubness, nodeSize, setDegreeScale, type FromWorker, type ToWorker } from './protocol';

interface Node extends SimulationNodeDatum {
  i: number;
  r: number;
  /** 0..1, how connected this memory is relative to the graph. */
  hub: number;
  deg: number;
  /** Forgotten while the map is open: exerts no force, pinned in place. */
  gone?: boolean;
}

let sim: ReturnType<typeof forceSimulation<Node>> | null = null;
let nodes: Node[] = [];
let adjacency: number[][] = [];
let links: { source: number | Node; target: number | Node; w: number }[] = [];
let running = false;
let dragging = false;
let dragAt: { x: number; y: number } | null = null;
/** One tick per frame and high friction, so every change after the first
 *  layout moves slowly, as if through thick fluid. The first layout of a
 *  view runs flat out instead, so a large map is ready quickly. */
let paced = false;
/** The page is hidden: hold the simulation where it is until it's back. */
let paused = false;
function fluid() {
  paced = true;
  sim?.velocityDecay(0.72).alphaDecay(0.015);
}
let pinned: number[] = [];
// The offset that keeps the layout centered. Recomputed only until the first
// settle; afterwards fixed, so dragging one node never shifts the rest.
let offset: [number, number] | null = null;

const post = (m: FromWorker, transfer: Transferable[]) => (self as unknown as Worker).postMessage(m, transfer);

function positions() {
  const n = nodes.length;
  const x = new Float32Array(n), y = new Float32Array(n);
  let cx = 0, cy = 0;
  if (offset) {
    [cx, cy] = offset;
  } else {
    for (const d of nodes) {
      cx += d.x!;
      cy += d.y!;
    }
    cx /= n || 1;
    cy /= n || 1;
  }
  for (let k = 0; k < n; k++) {
    x[k] = nodes[k].x! - cx;
    y[k] = nodes[k].y! - cy;
  }
  return { x, y };
}

async function run() {
  if (running || !sim) return;
  running = true;
  let frames = 0;
  while (sim && !paused && sim.alpha() > sim.alphaMin()) {
    sim.tick(paced ? 1 : 2);
    separate(2);
    if (paced || ++frames % 2 === 0) {
      const { x, y } = positions();
      post({ type: 'tick', x, y, alpha: sim.alpha() }, [x.buffer, y.buffer]);
    }
    // Yield so drag messages arrive between ticks.
    await new Promise((r) => setTimeout(r, paced ? 16 : 0));
  }
  running = false;
  if (paused) return; // picks up where it left off on resume
  if (!paced) fluid();
  if (sim) {
    // A local reheat is done: let the rest of the map move again.
    if (!dragging) release();
    separate(200);
    const { x, y } = positions();
    freezeOffset();
    post({ type: 'settled', x, y }, [x.buffer, y.buffer]);
  }
}

function freezeOffset() {
  if (offset) return;
  let cx = 0, cy = 0;
  for (const d of nodes) {
    cx += d.x!;
    cy += d.y!;
  }
  offset = [cx / (nodes.length || 1), cy / (nodes.length || 1)];
}

/** Room between memories: 3× the original compact layout, so clusters stay readable. */
const spacing = 3;

const collideRadius = (d: Node) => (d.gone ? 0 : (d.r * 2.5 + 10) * spacing);

/** Every distance-like force scales with `spacing`; the pull to the center
 *  weakens with it so the map can actually grow. */
function applySpacing() {
  if (!sim) return;
  sim
    .force('charge', forceManyBody<Node>().strength((d) => (d.gone ? 0 : (-90 - 160 * d.hub) * spacing * spacing)).theta(0.9).distanceMax(500 * spacing))
    .force('link', forceLink<Node, (typeof links)[number]>(links).id((d) => d.i).distance(70 * spacing)
      // d3's own normalization: a memory with many links isn't pulled in many times as hard.
      .strength((l) => (0.3 + 0.7 * l.w) / Math.min((l.source as unknown as Node).deg, (l.target as unknown as Node).deg)))
    .force('collide', forceCollide<Node>(collideRadius).iterations(2))
    .force('x', forceX<Node>(0).strength((d) => (0.035 + 0.04 * d.hub) / spacing))
    .force('y', forceY<Node>(0).strength((d) => (0.035 + 0.04 * d.hub) / spacing));
}

/** Collision as a hard constraint: moves overlapping nodes apart by position,
 *  leaving every other node and all velocities untouched. Runs after each
 *  physics step (friction would otherwise let links win briefly) and fully on
 *  settle and on opening a saved layout. Drawn nodes sit well inside their
 *  collision circles, so after this no two drawn nodes overlap. */
function separate(passes: number) {
  const collide = forceCollide<Node>(collideRadius).iterations(2);
  collide.initialize(nodes, Math.random);
  const vx = nodes.map((d) => d.vx ?? 0), vy = nodes.map((d) => d.vy ?? 0);
  for (let k = 0; k < passes; k++) {
    for (const d of nodes) d.vx = d.vy = 0;
    collide(1);
    let moved = false;
    for (const d of nodes) {
      if (d.fx != null || (!d.vx && !d.vy)) continue;
      d.x! += d.vx!;
      d.y! += d.vy!;
      moved = true;
    }
    if (!moved) break;
  }
  nodes.forEach((d, i) => {
    d.vx = vx[i];
    d.vy = vy[i];
  });
}

/** Hold everything outside `near` still, so only that part of the map moves. */
function pinAllBut(near: Set<number>) {
  release();
  for (const other of nodes)
    if (!near.has(other.i)) {
      other.fx = other.x;
      other.fy = other.y;
      pinned.push(other.i);
    }
}

/** Only `node`'s surroundings move; the rest of the map stays put. */
function reheatAround(node: Node, alpha: number) {
  if (!sim) return;
  if (!dragging) {
    const near = new Set([node.i]);
    for (const o of nodes) if (!o.gone && Math.hypot(o.x! - node.x!, o.y! - node.y!) < 160) near.add(o.i);
    if (node.gone) near.delete(node.i);
    pinAllBut(near);
  }
  sim.alpha(Math.max(sim.alpha(), alpha));
  run();
}

function release() {
  for (const i of pinned) if (!nodes[i].gone) nodes[i].fx = nodes[i].fy = null;
  pinned = [];
}

self.onmessage = (e: MessageEvent<ToWorker>) => {
  const m = e.data;
  if (m.type === 'init') {
    sim?.stop();
    offset = null;
    setDegreeScale(m.maxDegree);
    const unknown: number[] = [];
    adjacency = Array.from({ length: m.n }, () => []);
    // Each memory's strongest link to a memory whose position is known.
    const best = new Int32Array(m.n).fill(-1), bestW = new Float32Array(m.n).fill(-Infinity);
    for (let k = 0; k < m.src.length; k++) {
      const a = m.src[k], b = m.dst[k], w = m.weight[k];
      adjacency[a].push(b);
      adjacency[b].push(a);
      if (m.known[b] && w > bestW[a]) [best[a], bestW[a]] = [b, w];
      if (m.known[a] && w > bestW[b]) [best[b], bestW[b]] = [a, w];
    }
    nodes = Array.from({ length: m.n }, (_, i) => {
      if (!m.known[i]) unknown.push(i);
      return { i, r: nodeSize(m.degree[i]), hub: hubness(m.degree[i]), deg: Math.max(1, m.degree[i]), x: m.x[i], y: m.y[i] };
    });
    // New memories appear next to their neighbors.
    const anyKnown = unknown.length < m.n;
    for (const i of unknown) {
      // Beside its strongest known link at a random angle, not at the average
      // of its links: that average is the middle of the map for a memory
      // linked to everything stored around the same time.
      if (anyKnown && best[i] >= 0) {
        const a = Math.random() * Math.PI * 2;
        nodes[i].x = nodes[best[i]].x! + 30 * Math.cos(a);
        nodes[i].y = nodes[best[i]].y! + 30 * Math.sin(a);
      } else if (anyKnown) {
        nodes[i].x = (Math.random() - 0.5) * 50;
        nodes[i].y = (Math.random() - 0.5) * 50;
      } else {
        nodes[i].x = undefined;
        nodes[i].y = undefined;
      }
    }
    pinned = [];
    links = Array.from(m.src, (s, k) => ({ source: s, target: m.dst[k], w: m.weight[k] }));
    // Room to read: strong repulsion, long weak links, generous spacing.
    // Hubs push harder for room and pull harder toward the center, so they
    // anchor their clusters and sparse memories drift outward.
    sim = forceSimulation(nodes).stop();
    applySpacing();
    // Cached layouts open settled: barely reheat; a fully new one starts hot.
    sim.alpha(unknown.length === 0 ? 0 : anyKnown ? Math.min(0.3, 0.05 + unknown.length / m.n) : 1);
    paced = false;
    if (anyKnown) fluid();
    if (m.reflow) {
      // Borrowed from the full map: drift into this view's own layout.
      sim.alphaDecay(0.012).alpha(0.5);
      run();
    } else if (unknown.length === 0) {
      offset = [0, 0]; // cached layouts are already centered
      separate(200); // a layout saved by an older build may have overlaps
      const { x, y } = positions();
      post({ type: 'settled', x, y }, [x.buffer, y.buffer]);
    } else run();
  } else if (m.type === 'drag' && sim) {
    const n = nodes[m.index];
    if (m.phase === 'start') {
      dragging = true;
      dragAt = null;
      freezeOffset();
      // A gentle, well-damped reheat of the neighborhood only.
      sim.alphaTarget(0.04).alpha(Math.max(sim.alpha(), 0.06));
    }
    // Free the dragged node's 2-hop neighborhood and whatever is near it now,
    // and pin the rest; re-done as it travels, so it can never be dropped onto
    // a node that isn't allowed to move out of the way.
    const raw = { x: m.x + offset![0], y: m.y + offset![1] };
    if (m.phase !== 'end' && (!dragAt || Math.hypot(raw.x - dragAt.x, raw.y - dragAt.y) > 60)) {
      dragAt = raw;
      const near = new Set([m.index]);
      for (const j of adjacency[m.index]) {
        near.add(j);
        for (const k of adjacency[j]) near.add(k);
      }
      for (const o of nodes) if (!o.gone && Math.hypot(o.x! - raw.x, o.y! - raw.y) < 180) near.add(o.i);
      pinAllBut(near);
    }
    // The main thread works in centered coordinates; the worker in raw ones.
    n.fx = m.x + offset![0];
    n.fy = m.y + offset![1];
    if (m.phase === 'end') {
      n.fx = n.fy = null;
      sim.alphaTarget(0);
      dragging = false; // the pinned nodes are released once it settles
    }
    run();
  } else if (m.type === 'add' && sim) {
    // A new memory joins the physics like any other, so it pushes its
    // neighbors aside (and they it) instead of landing on top of them.
    freezeOffset();
    // Sized by all its links, exactly as drawn, so its collision circle holds it.
    const i = nodes.length, deg = m.degree;
    const node: Node = { i, r: nodeSize(deg), hub: hubness(deg), deg: Math.max(1, deg), x: m.x + offset![0], y: m.y + offset![1] };
    nodes.push(node);
    adjacency.push([]);
    for (const l of m.links) {
      adjacency[i].push(l.to);
      adjacency[l.to].push(i);
      links.push({ source: i, target: l.to, w: l.w });
    }
    sim.nodes(nodes); // re-initializes every force, links included
    reheatAround(node, 0.15);
  } else if (m.type === 'remove' && sim && nodes[m.index]) {
    const node = nodes[m.index];
    node.gone = true;
    node.fx = node.x;
    node.fy = node.y;
    links = links.filter((l) => (l.source as Node).i !== m.index && (l.target as Node).i !== m.index);
    (sim.force('link') as ReturnType<typeof forceLink<Node, (typeof links)[number]>>).links(links);
    sim.nodes(nodes);
    reheatAround(node, 0.12);
  } else if (m.type === 'pause') {
    paused = true;
  } else if (m.type === 'resume') {
    paused = false;
    run();
  } else if (m.type === 'stop') {
    sim?.stop();
    sim = null;
  }
};
