// Pure helpers for the layout-preserving translated page (ADR 0005).
import type { Block } from '../../ipc/generated/Block';
import { pdfRectToCss, type ViewportLike } from './coords';

export type Rgb = [number, number, number];

export interface BlockBox {
  left: number; top: number; width: number; height: number;
  /** Starting font size (CSS px) derived from the original line height. */
  fontSize: number;
  lines: number;
  /** The single line sits roughly centred in a wide page (titles). */
  centered: boolean;
}

/** Bounding box of a block's line rects in CSS px, plus a font size estimate. */
export function blockBox(b: Pick<Block, 'location'>, viewport: ViewportLike & { width?: number }): BlockBox | null {
  if (b.location.kind !== 'pdf' || b.location.rects.length === 0) return null;
  const css = b.location.rects.map((r) => pdfRectToCss(r, viewport)).filter((c) => c.width > 0 && c.height > 0);
  if (css.length === 0) return null;
  const left = Math.min(...css.map((c) => c.left));
  const top = Math.min(...css.map((c) => c.top));
  const right = Math.max(...css.map((c) => c.left + c.width));
  const bottom = Math.max(...css.map((c) => c.top + c.height));
  const heights = css.map((c) => c.height).sort((x, y) => x - y);
  const median = heights[Math.floor(heights.length / 2)]!;
  const pageW = viewport.width ?? 0;
  const mid = (left + right) / 2;
  return {
    left, top, width: right - left, height: bottom - top,
    fontSize: Math.max(5, median * 0.8),
    lines: css.length,
    centered: pageW > 0 && Math.abs(mid - pageW / 2) < pageW * 0.04 && left > pageW * 0.15,
  };
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
  return Math.round(lo * 10) / 10;
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
