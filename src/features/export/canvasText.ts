// Rich text on a 2D canvas for PDF export: wrapping at spaces (long words break anywhere,
// like the on-screen overlays), with bold / italic / coloured runs.

export interface Run { text: string; bold: boolean; italic: boolean; color?: string }
export interface MarkLike { startU16: number; endU16: number; kind: string }
export interface Seg { text: string; x: number; run: Run }
export interface Line { segs: Seg[]; width: number }
export interface Measurer { font: string; measureText(s: string): { width: number } }

export const EXPORT_FONT = '"Apple SD Gothic Neo", "Helvetica Neue", Helvetica, Arial, sans-serif';

export function fontOf(run: Pick<Run, 'bold' | 'italic'>, px: number, family = EXPORT_FONT): string {
  return `${run.italic ? 'italic ' : ''}${run.bold ? '700' : '400'} ${px}px ${family}`;
}

/** Text + mark ranges (UTF-16) → runs with uniform styling. */
export function runsOf(text: string, marks: readonly MarkLike[] | undefined, base: Partial<Run> = {}): Run[] {
  const valid = (marks ?? []).filter((m) => m.startU16 >= 0 && m.endU16 <= text.length && m.startU16 < m.endU16);
  const cuts = [...new Set([0, text.length, ...valid.flatMap((m) => [m.startU16, m.endU16])])].sort((a, b) => a - b);
  const out: Run[] = [];
  for (let i = 0; i + 1 < cuts.length; i++) {
    const a = cuts[i]!;
    const b = cuts[i + 1]!;
    const on = (pred: (k: string) => boolean) => valid.find((m) => pred(m.kind) && m.startU16 <= a && m.endU16 >= b);
    const ink = on((k) => k.startsWith('c:'))?.kind.slice(2);
    out.push({
      text: text.slice(a, b),
      bold: !!base.bold || !!on((k) => k === 'b'),
      italic: !!base.italic || !!on((k) => k === 'i'),
      color: ink && /^#[0-9a-f]{6}$/.test(ink) ? ink : base.color,
    });
  }
  return out;
}

/** Greedy line breaking. */
export function wrap(ctx: Measurer, runs: readonly Run[], width: number, px: number, family = EXPORT_FONT): Line[] {
  const lines: Line[] = [];
  let cur: Line = { segs: [], width: 0 };
  let pendingSpace: { w: number; run: Run } | null = null;
  const push = () => { lines.push(cur); cur = { segs: [], width: 0 }; pendingSpace = null; };
  const add = (text: string, w: number, run: Run) => {
    if (pendingSpace && cur.segs.length) {
      cur.segs.push({ text: ' ', x: cur.width, run: pendingSpace.run });
      cur.width += pendingSpace.w;
    }
    pendingSpace = null;
    const last = cur.segs[cur.segs.length - 1];
    if (last && last.run === run && !last.text.endsWith(' ') ) { last.text += text; } else cur.segs.push({ text, x: cur.width, run });
    cur.width += w;
  };
  for (const run of runs) {
    ctx.font = fontOf(run, px, family);
    const spaceW = ctx.measureText(' ').width;
    for (const tok of run.text.split(/(\n|[^\S\n]+)/)) {
      if (!tok) continue;
      if (tok === '\n') { push(); continue; }
      if (/^\s+$/.test(tok)) { if (cur.segs.length) pendingSpace = { w: spaceW, run }; continue; }
      const w = ctx.measureText(tok).width;
      const need = w + (pendingSpace && cur.segs.length ? (pendingSpace as { w: number }).w : 0);
      if (cur.width + need <= width + 0.01) { add(tok, w, run); continue; }
      if (w <= width) { if (cur.segs.length) push(); add(tok, w, run); continue; }
      // A word wider than the line: break it by characters.
      if (cur.segs.length && pendingSpace) { cur.segs.push({ text: ' ', x: cur.width, run }); cur.width += spaceW; pendingSpace = null; }
      for (const ch of tok) {
        const cw = ctx.measureText(ch).width;
        if (cur.width + cw > width + 0.01 && cur.segs.length) push();
        add(ch, cw, run);
      }
    }
  }
  if (cur.segs.length || lines.length === 0) lines.push(cur);
  return lines;
}

export function draw(ctx: CanvasRenderingContext2D, lines: readonly Line[], x: number, y: number, px: number, lineHeight: number, color: string, family = EXPORT_FONT): void {
  ctx.textBaseline = 'alphabetic';
  lines.forEach((line, i) => {
    // baseline: centre the em box in the line box
    const base = y + i * px * lineHeight + (px * lineHeight - px) / 2 + px * 0.82;
    for (const s of line.segs) {
      ctx.font = fontOf(s.run, px, family);
      ctx.fillStyle = s.run.color ?? color;
      ctx.fillText(s.text, x + s.x, base);
    }
  });
}

/**
 * Largest size ≤ `base` (down to `min`) at which `runs` fit `width` × `height`.
 * Returns null when even `min` does not fit.
 */
export function fit(ctx: Measurer, runs: readonly Run[], width: number, height: number, base: number, lineHeight: number, min: number): { px: number; lines: Line[] } | null {
  let px = base;
  for (let i = 0; i < 40 && px >= min - 0.01; i++) {
    const lines = wrap(ctx, runs, width, px);
    if (lines.length * px * lineHeight <= height + 0.5) return { px, lines };
    // shrink by the area ratio first, then in small steps
    const ratio = Math.sqrt(height / (lines.length * px * lineHeight));
    px = Math.max(min - 0.02, Math.min(px - 0.25, px * Math.max(0.8, ratio)));
  }
  return null;
}
