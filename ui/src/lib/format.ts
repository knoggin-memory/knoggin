// Words and numbers as DESIGN §9 spells them.
const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

/** "just now", "14m ago", "3d ago", "2w ago", "4mo ago", "1y ago". */
export function ago(ms: number, now = Date.now()): string {
  const d = Math.max(0, now - ms);
  if (d < MIN) return 'just now';
  if (d < HOUR) return `${Math.floor(d / MIN)}m ago`;
  if (d < DAY) return `${Math.floor(d / HOUR)}h ago`;
  if (d < 14 * DAY) return `${Math.floor(d / DAY)}d ago`;
  if (d < 60 * DAY) return `${Math.floor(d / (7 * DAY))}w ago`;
  if (d < 365 * DAY) return `${Math.floor(d / (30 * DAY))}mo ago`;
  return `${Math.floor(d / (365 * DAY))}y ago`;
}

/** "in 3d", "in 2w"… for future times. */
export function inFuture(ms: number, now = Date.now()): string {
  const d = ms - now;
  if (d <= 0) return 'next sweep';
  return `in ${duration(d / DAY)}`;
}

/** Durations given in days: "16 d", "3 mo", "4 yr". */
export function duration(days: number): string {
  if (!Number.isFinite(days)) return '—';
  if (days < 1) return `${Math.max(1, Math.round(days * 24))} h`;
  if (days < 60) return `${Math.round(days)} d`;
  if (days < 730) return `${Math.round(days / 30)} mo`;
  return `${Math.round(days / 365)} yr`;
}

export const fixed2 = (x: number) => x.toFixed(2);

export function count(n: number): string {
  return n.toLocaleString('en-US');
}

export function bytes(n: number): string {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v < 10 && i > 0 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/** Truncate at a word boundary to about `max` characters with "…". */
export function truncateWords(text: string, max = 26): string {
  if (text.length <= max) return text;
  const cut = text.slice(0, max + 1);
  const space = cut.lastIndexOf(' ');
  return (space > max * 0.5 ? cut.slice(0, space) : text.slice(0, max)).trimEnd() + '…';
}

export const PHASES = ['', 'Full', 'Summary', 'Ghost', 'Tombstone'] as const;
export const phaseName = (code: number) => PHASES[code] ?? 'Unknown';
