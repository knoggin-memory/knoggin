// Map colors, read from the design tokens at runtime so tokens.css stays the
// only source of values (WebGL needs concrete colors).
export interface MapTheme {
  node: string;
  /** Stage greens: Full fades from `node` toward `nodeFading`, then Summary, Ghost. */
  nodeFading: string;
  nodeSummary: string;
  nodeGhost: string;
  nodeLit: string;
  edge: string;
  contradicts: string;
  permastore: string;
  text: string;
  accent: string;
  suppressed: string;
  halo: string;
  canvasDot: string;
  /** The map's background: node colors are blended onto it so nodes are solid. */
  canvas: string;
  moss: string;
  agents: string[];
  fontSans: string;
}

const css = (name: string) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

export function readTheme(): MapTheme {
  return {
    node: css('--color-node'),
    nodeFading: css('--color-node-fading'),
    nodeSummary: css('--color-node-summary'),
    nodeGhost: css('--color-node-ghost'),
    nodeLit: css('--color-node-lit'),
    edge: css('--color-edge'),
    contradicts: css('--color-contradicts'),
    permastore: css('--color-permastore'),
    text: css('--color-text'),
    accent: css('--color-accent'),
    suppressed: css('--color-suppressed'),
    halo: css('--color-label-halo'),
    canvasDot: css('--color-canvas-dot'),
    canvas: css('--color-canvas'),
    moss: css('--color-agent-2'),
    agents: [1, 2, 3, 4].map((i) => css(`--color-agent-${i}`)),
    fontSans: css('--font-sans'),
  };
}

/** Blend two `#RRGGBB` colors: t = 0 gives `a`, t = 1 gives `b`. */
export function mix(a: string, b: string, t: number): string {
  const p = parseInt(a.slice(1), 16), q = parseInt(b.slice(1), 16), k = Math.min(1, Math.max(0, t));
  const ch = (s: number) => Math.round(((p >> s) & 255) + (((q >> s) & 255) - ((p >> s) & 255)) * k);
  return '#' + [16, 8, 0].map((s) => ch(s).toString(16).padStart(2, '0')).join('');
}

/** `color` at opacity `a` as it looks over `ground`, but fully opaque, so
 *  nothing drawn underneath (links) shows through. */
export function solid(ground: string, color: string, a: number): string {
  return mix(ground, color, a);
}

/** `#RRGGBB` + alpha → `rgba(r, g, b, a)`. */
export function alpha(hex: string, a: number): string {
  const h = hex.replace('#', '');
  const n = parseInt(h.length === 3 ? h.replace(/./g, (c) => c + c) : h, 16);
  return `rgba(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}, ${a})`;
}

export function rgb(hex: string): [number, number, number] {
  const h = hex.replace('#', '');
  const n = parseInt(h, 16);
  return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255];
}
