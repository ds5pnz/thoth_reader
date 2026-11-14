// Pure helpers for the layout-preserving translated page (ADR 0005).
import type { Block } from '../../ipc/generated/Block';
import { pdfRectToCss, type ViewportLike } from './coords';

export type Rgb = [number, number, number];

export interface BlockBox {
  left: number; top: number; width: number; height: number;
  /** Font size (CSS px): the dominant real glyph size of the block when the page's text
   *  items are known, else an estimate from the line height. */
  fontSize: number;
  lines: number;
  /** Distance between consecutive line tops (CSS px); 0 for single-line blocks. */
  pitch: number;
}

/** Size info of one PDF.js text item: font height in PDF units and its text length. */
export interface ItemSize { h: number; n: number }

/**
 * Dominant glyph height (PDF units) among a block's text items, weighted by text length, so
 * one oversized list numeral or drop cap does not inflate the block's size.
 */
export function dominantItemHeight(b: Pick<Block, 'location'>, items: readonly (ItemSize | undefined)[]): number | null {
  if (b.location.kind !== 'pdf') return null;
  const weight = new Map<number, number>();
  for (const r of b.location.itemRanges) {
    const it = items[r.itemIndex];
    if (!it || !(it.h > 0)) continue;
    const key = Math.round(it.h * 4) / 4;
    weight.set(key, (weight.get(key) ?? 0) + Math.max(1, r.endU16 - r.startU16));
  }
  let best: number | null = null;
  let bestW = 0;
  for (const [h, w] of weight) if (w > bestW) { best = h; bestW = w; }
  return best;
}

/** Bounding box of a block's line rects in CSS px, with font size and line pitch. */
export function blockBox(b: Pick<Block, 'location'>, viewport: ViewportLike & { width?: number; scale?: number }, items?: readonly (ItemSize | undefined)[] | null): BlockBox | null {
  if (b.location.kind !== 'pdf' || b.location.rects.length === 0) return null;
  const css = b.location.rects.map((r) => pdfRectToCss(r, viewport)).filter((c) => c.width > 0 && c.height > 0);
  if (css.length === 0) return null;
  const left = Math.min(...css.map((c) => c.left));
  const top = Math.min(...css.map((c) => c.top));
  const right = Math.max(...css.map((c) => c.left + c.width));
  const bottom = Math.max(...css.map((c) => c.top + c.height));
  const heights = css.map((c) => c.height).sort((x, y) => x - y);
  const tops = css.map((c) => c.top).sort((x, y) => x - y);
  const gaps = tops.slice(1).map((t, i) => t - tops[i]!).filter((g) => g > 1).sort((x, y) => x - y);
  const real = items ? dominantItemHeight(b, items) : null;
  const scale = viewport.scale ?? 1;
  // Hangul glyphs fill the em box more than Latin x-height text: slightly smaller reads equal.
  // Some PDFs report item heights larger than the real em size, so never exceed what the
  // original line geometry allows (line pitch for paragraphs, line box for single lines).
  const pitch0 = gaps.length ? gaps[Math.floor(gaps.length / 2)]! : 0;
  const geomCap = pitch0 > 0 ? pitch0 / 1.2 : heights[0]! * 0.8;
  const fontSize = Math.min(real != null ? real * scale * 0.92 : heights[0]! * 0.74, geomCap);
  return {
    left, top, width: right - left, height: bottom - top,
    fontSize: Math.max(5, fontSize),
    lines: css.length,
    pitch: pitch0,
  };
}

/** Snap near-equal sizes to the page's body size so paragraphs don't differ by fractions. */
export function harmonizeFontSizes(boxes: readonly { id: string; box: BlockBox }[]): Map<string, number> {
  const weight = new Map<number, number>();
  for (const { box } of boxes) {
    const key = Math.round(box.fontSize * 2) / 2;
    weight.set(key, (weight.get(key) ?? 0) + box.width * box.lines);
  }
  let body = 0;
  let bestW = -1;
  for (const [k, w] of weight) if (w > bestW) { body = k; bestW = w; }
  const out = new Map<string, number>();
  for (const { id, box } of boxes) out.set(id, body > 0 && Math.abs(box.fontSize - body) <= body * 0.12 ? body : box.fontSize);
  return out;
}

export interface Raster { data: Uint8ClampedArray; width: number; height: number }

function near(d: Uint8ClampedArray, i: number, bg: Rgb, tol: number): boolean {
  return Math.abs(d[i]! - bg[0]) <= tol && Math.abs(d[i + 1]! - bg[1]) <= tol && Math.abs(d[i + 2]! - bg[2]) <= tol;
}

/**
 * Grow a block's box into empty page space so a longer translation does not have to wrap or
 * shrink: to the right and then downward, only across pixels that match the block's
 * background (so table rules, figures and other text stop the growth), and never into
 * another block. `k` = raster px per CSS px. Returns the grown width/height in CSS px.
 */
export function expandBox(img: Raster, k: number, box: BlockBox, bg: Rgb, others: readonly BlockBox[], pageRight: number, maxExtraLines = 2): { width: number; height: number } {
  const tol = 14;
  const px = (v: number) => Math.round(v * k);
  const uniformCol = (xCss: number, y0: number, y1: number) => {
    const x = px(xCss);
    if (x < 0 || x >= img.width) return false;
    for (let y = Math.max(0, px(y0)); y < Math.min(img.height, px(y1)); y += 2) if (!near(img.data, (y * img.width + x) * 4, bg, tol)) return false;
    return true;
  };
  const uniformRow = (yCss: number, x0: number, x1: number) => {
    const y = px(yCss);
    if (y < 0 || y >= img.height) return false;
    for (let x = Math.max(0, px(x0)); x < Math.min(img.width, px(x1)); x += 2) if (!near(img.data, (y * img.width + x) * 4, bg, tol)) return false;
    return true;
  };
  const bottom = box.top + box.height;
  // Right limit: page content edge, or the nearest block to the right on the same band.
  let limitR = pageRight;
  for (const o of others) {
    if (o === box) continue;
    const overlapsV = o.top < bottom && o.top + o.height > box.top;
    if (overlapsV && o.left >= box.left + box.width - 1) limitR = Math.min(limitR, o.left - 4);
  }
  let right = box.left + box.width;
  while (right + 2 <= limitR && uniformCol(right + 2, box.top, bottom)) right += 2;
  // Down limit: the nearest block below that overlaps horizontally, and a few extra lines.
  const lineH = box.pitch > 0 ? box.pitch : box.height / Math.max(1, box.lines);
  let limitB = bottom + lineH * maxExtraLines;
  for (const o of others) {
    if (o === box) continue;
    const overlapsH = o.left < right && o.left + o.width > box.left;
    if (overlapsH && o.top >= bottom - 1) limitB = Math.min(limitB, o.top - 2);
  }
  let down = bottom;
  while (down + 2 <= limitB && uniformRow(down + 2, box.left, right)) down += 2;
  return { width: right - box.left, height: down - box.top };
}

export const MIN_FONT_PX = 4.5;

/** Largest font size ≤ base for which `fits(px)` holds (binary search); never below the
 *  minimum, in which case the text overflows and the full text is available on click. */
export function fitFontSize(base: number, fits: (px: number) => boolean): number {
  if (fits(base)) return base;
  let lo = MIN_FONT_PX;
  let hi = base;
  if (!fits(lo)) return lo;
  for (let i = 0; i < 8; i++) {
    const mid = (lo + hi) / 2;
    if (fits(mid)) lo = mid; else hi = mid;
  }
  // Round *down* and confirm: rounding up could step past the largest size that fits.
  let out = Math.floor(lo * 10) / 10;
  while (out > MIN_FONT_PX && !fits(out)) out = Math.round((out - 0.1) * 10) / 10;
  return Math.max(MIN_FONT_PX, out);
}

/**
 * Adjacent blocks' line boxes often overlap by a few pixels (ascent/descent exceed the line
 * pitch). Trim the upper block to end where the lower one starts so their overlays do not
 * overlap; the lower block's opaque box covers the shared strip. Larger overlaps (nested or
 * side-by-side geometry) are left alone. Returns new boxes in the same order.
 */
export function resolveVerticalOverlaps<T extends { box: BlockBox }>(items: readonly T[]): T[] {
  const out = items.map((x) => ({ ...x, box: { ...x.box } }));
  const order = out.map((_, i) => i).sort((a, b) => out[a]!.box.top - out[b]!.box.top);
  for (let i = 0; i < order.length; i++) {
    const a = out[order[i]!]!.box;
    for (let j = i + 1; j < order.length; j++) {
      const b = out[order[j]!]!.box;
      if (b.top >= a.top + a.height) break;
      const overlapW = Math.min(a.left + a.width, b.left + b.width) - Math.max(a.left, b.left);
      const overlapH = a.top + a.height - b.top;
      if (overlapW <= 2 || overlapH <= 0 || b.top <= a.top) continue;
      const lineA = a.height / Math.max(1, a.lines);
      if (overlapH <= Math.max(lineA * 0.75, 4) && b.top - a.top >= lineA * 0.5) a.height = b.top - a.top;
    }
  }
  return out;
}

/** Most common colour among samples taken around a block (quantised), i.e. its background. */
export function pickBackground(samples: Rgb[]): Rgb {
  if (samples.length === 0) return [255, 255, 255];
  const buckets = new Map<string, { n: number; sum: Rgb }>();
  for (const s of samples) {
    const key = s.map((v) => v >> 4).join(',');
    const b = buckets.get(key) ?? { n: 0, sum: [0, 0, 0] as Rgb };
    b.n += 1;
    b.sum = [b.sum[0] + s[0], b.sum[1] + s[1], b.sum[2] + s[2]];
    buckets.set(key, b);
  }
  let best: { n: number; sum: Rgb } | null = null;
  for (const b of buckets.values()) if (!best || b.n > best.n) best = b;
  return [Math.round(best!.sum[0] / best!.n), Math.round(best!.sum[1] / best!.n), Math.round(best!.sum[2] / best!.n)];
}

/** Dark text on light backgrounds and vice versa (relative luminance). */
export function textColorFor(bg: Rgb): string {
  const lum = (0.2126 * bg[0] + 0.7152 * bg[1] + 0.0722 * bg[2]) / 255;
  return lum < 0.5 ? '#f4f4f4' : '#141414';
}
