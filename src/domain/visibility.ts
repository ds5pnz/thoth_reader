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

/** Tags user vs programmatic scrolls so sync handlers don't re-trigger each other. */
export class ScrollOrigin {
  private seq = 0;
  private pending: { seq: number; until: number } | null = null;
  markProgrammatic(windowMs = 120): number {
    this.seq += 1;
    this.pending = { seq: this.seq, until: performance.now() + windowMs };
    return this.seq;
  }
  /** Returns 'program' for scroll events caused by the last programmatic move. */
  classify(): 'user' | 'program' {
    if (this.pending && performance.now() <= this.pending.until) return 'program';
    this.pending = null;
    return 'user';
  }
  get sequence() { return this.seq; }
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
