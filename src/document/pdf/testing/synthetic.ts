/** Test-only builders for synthetic PDF.js-like text items. */
import type { PdfPageInput, PdfTextItemLike, PdfTextStyleLike } from '../types';

export const STYLES: Record<string, PdfTextStyleLike> = {
  R: { fontFamily: 'serif', fontName: 'Times-Roman', ascent: 0.8, descent: -0.2 },
  I: { fontFamily: 'serif', fontName: 'Times-Italic', ascent: 0.8, descent: -0.2 },
  B: { fontFamily: 'sans-serif', fontName: 'Helvetica-Bold', ascent: 0.9, descent: -0.2 },
  H: { fontFamily: 'sans-serif', fontName: 'Helvetica', ascent: 0.9, descent: -0.2 },
  M: { fontFamily: 'monospace', fontName: 'Courier', isMonospace: true, ascent: 0.8, descent: -0.2 },
  S: { fontFamily: 'sans-serif', fontName: 'Symbol', ascent: 0.8, descent: -0.2 },
};

/** Approximate advance: 0.5 em per UTF-16 unit (0.6 for mono). */
export function item(str: string, x: number, y: number, size = 10, font = 'R', extra: Partial<PdfTextItemLike> = {}): PdfTextItemLike {
  const em = font === 'M' ? 0.6 : 0.5;
  return {
    str,
    dir: 'ltr',
    transform: [size, 0, 0, size, x, y],
    width: str.length * size * em,
    height: size,
    fontName: font,
    hasEOL: false,
    ...extra,
  };
}

/** Item rotated by +90° (baseline runs up the page). */
export function item90(str: string, x: number, y: number, size = 10, font = 'R'): PdfTextItemLike {
  return { ...item(str, x, y, size, font), transform: [0, size, -size, 0, x, y] };
}

/** Lines of a paragraph: each string becomes one item on its own baseline. */
export function para(lines: string[], x: number, y: number, opts: { size?: number; leading?: number; font?: string; indent?: number } = {}): PdfTextItemLike[] {
  const size = opts.size ?? 10;
  const leading = opts.leading ?? size * 1.2;
  return lines.map((l, i) => item(l, x + (i === 0 ? (opts.indent ?? 0) : 0), y - i * leading, size, opts.font ?? 'R'));
}

export function pageInput(items: PdfTextItemLike[], extra: Partial<PdfPageInput> = {}): PdfPageInput {
  return { pageIndex: 0, viewBox: [0, 0, 612, 792], rotation: 0, items, styles: STYLES, imageCount: 0, ...extra };
}

/** Words of filler text of roughly `chars` characters, deterministic. */
export function filler(seed: number, chars: number): string {
  const words = ['alpha', 'beta', 'gamma', 'delta', 'epsilon', 'zeta', 'theta', 'kappa', 'lambda', 'sigma', 'omega', 'river', 'column', 'gutter'];
  let s = '';
  let i = seed;
  while (s.length < chars) {
    s += (s ? ' ' : '') + words[i % words.length];
    i = (i * 7 + 3) % 97;
  }
  return s.slice(0, chars).trimEnd();
}
