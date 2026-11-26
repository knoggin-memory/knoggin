// Open state of the ⌘K palette, shared by the header field and shortcuts.
class Palette {
  open = $state(false);
  show() {
    this.open = true;
  }
  hide() {
    this.open = false;
  }
}
export const palette = new Palette();

/** ⌘K / Ctrl+K anywhere; `/` when not typing in a field. */
export function isPaletteShortcut(e: KeyboardEvent): boolean {
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') return true;
  if (e.key !== '/' || e.metaKey || e.ctrlKey || e.altKey) return false;
  const t = e.target as HTMLElement | null;
  return !(t && (t.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(t.tagName)));
}
