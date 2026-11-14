// Visibility & anchor rules (spec §3.4).

export const VISIBLE_DEBOUNCE_MS = 150;
export const VISIBLE_MIN_FRACTION = 0.1;
export const VISIBLE_MIN_PX = 24;
export const ANCHOR_LINE = 0.25;

export interface Box { top: number; bottom: number; height: number }

/** Visible when ≥10% of the element's area OR ≥24 CSS px of it intersects the viewport. */
export function isVisible(el: Box, viewport: Box): boolean {
  const visible = Math.max(0, Math.min(el.bottom, viewport.bottom) - Math.max(el.top, viewport.top));
  if (visible <= 0) return false;
  if (visible >= VISIBLE_MIN_PX) return true;
  return el.height > 0 && visible / el.height >= VISIBLE_MIN_FRACTION;
}

export interface AnchorCandidate { id: string; top: number; bottom: number }

/** Representative anchor: block closest to 25% from the viewport top; ratio within it. */
export function pickAnchor(cands: AnchorCandidate[], viewportTop: number, viewportHeight: number): { id: string; ratio: number } | null {
  const line = viewportTop + viewportHeight * ANCHOR_LINE;
  let best: AnchorCandidate | null = null;
  let bestDist = Infinity;
  for (const c of cands) {
    const d = line < c.top ? c.top - line : line > c.bottom ? line - c.bottom : 0;
    if (d < bestDist) { bestDist = d; best = c; }
  }
  if (!best) return null;
  const h = Math.max(1, best.bottom - best.top);
  const ratio = Math.min(1, Math.max(0, (line - best.top) / h));
  return { id: best.id, ratio };
}

/** Scroll target that puts `ratio` of the block at the 25% line. */
export function scrollTopForAnchor(blockTop: number, blockHeight: number, ratio: number, viewportHeight: number): number {
  return Math.max(0, blockTop + blockHeight * ratio - viewportHeight * ANCHOR_LINE);
}

/**
 * Classifies scroll events. A scroll is the user's only when real input (wheel, touch,
 * scroll keys, pointer held on the scroller) happened just before it; everything else —
 * programmatic restores, virtual-list offset corrections, layout shifts — is 'program'.
 * Sequence numbers tag programmatic moves so sync handlers don't re-trigger each other.
 */
export class ScrollOrigin {
  private seq = 0;
  private programUntil = 0;
  private userUntil = 0;
  private held = false;
  markProgrammatic(windowMs = 120): number {
    this.seq += 1;
    this.programUntil = performance.now() + windowMs;
    return this.seq;
  }
  noteUserInput(windowMs = 1200) {
    this.userUntil = performance.now() + windowMs;
    this.programUntil = 0; // real input always wins over a pending programmatic window
  }
  setHeld(held: boolean) {
    this.held = held;
    if (held) this.programUntil = 0;
    else this.userUntil = performance.now() + 300;
  }
  classify(): 'user' | 'program' {
    const now = performance.now();
    if (now <= this.programUntil) return 'program';
    return this.held || now <= this.userUntil ? 'user' : 'program';
  }
  get sequence() { return this.seq; }
}

const SCROLL_KEYS = new Set(['ArrowUp', 'ArrowDown', 'PageUp', 'PageDown', 'Home', 'End', ' ', 'Spacebar']);

/** Wire real-input listeners on a scroll container. Returns a cleanup function. */
export function attachUserIntent(el: HTMLElement, origin: ScrollOrigin): () => void {
  const input = () => origin.noteUserInput();
  const key = (e: KeyboardEvent) => { if (SCROLL_KEYS.has(e.key)) origin.noteUserInput(); };
  const down = () => origin.setHeld(true);
  const up = () => origin.setHeld(false);
  el.addEventListener('wheel', input, { passive: true });
  el.addEventListener('touchstart', input, { passive: true });
  el.addEventListener('touchmove', input, { passive: true });
  el.addEventListener('keydown', key);
  el.addEventListener('pointerdown', down);
  window.addEventListener('pointerup', up);
  window.addEventListener('pointercancel', up);
  return () => {
    el.removeEventListener('wheel', input);
    el.removeEventListener('touchstart', input);
    el.removeEventListener('touchmove', input);
    el.removeEventListener('keydown', key);
    el.removeEventListener('pointerdown', down);
    window.removeEventListener('pointerup', up);
    window.removeEventListener('pointercancel', up);
  };
}

export class Debouncer {
  private t: ReturnType<typeof setTimeout> | null = null;
  constructor(private ms: number) {}
  run(fn: () => void) {
    if (this.t) clearTimeout(this.t);
    this.t = setTimeout(() => { this.t = null; fn(); }, this.ms);
  }
  cancel() { if (this.t) clearTimeout(this.t); this.t = null; }
}
