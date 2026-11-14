/**
 * PDF user space ⇄ CSS pixel conversions (spec §3.3, §3.4).
 *
 * Every conversion goes through a PDF.js `PageViewport` (its affine transform
 * already accounts for /Rotate, the crop/view box origin, userUnit and scale).
 * Manual `height - y` flips are forbidden. pdfjs-dist 6.x no longer ships
 * `PageViewport.convertToViewportRectangle`, so {@link convertToViewportRectangle}
 * reproduces it from `convertToViewportPoint` (corner mapping + normalisation),
 * which is exactly what the removed method did.
 *
 * Viewport units are CSS pixels when the viewport was created with the CSS
 * scale. For a HiDPI canvas create the canvas from `scale * devicePixelRatio`
 * but keep CSS geometry on the CSS viewport — or divide canvas pixels by
 * `devicePixelRatio` first (see {@link canvasPointToCss}).
 */
import type { Block } from '../../ipc/generated/Block';
import type { Rect } from '../../ipc/generated/Rect';

/** The part of PDF.js `PageViewport` used here (keeps this module testable and build-agnostic). */
export interface ViewportLike {
  readonly width: number;
  readonly height: number;
  readonly scale: number;
  readonly rotation: number;
  readonly viewBox: readonly number[];
  convertToViewportPoint(x: number, y: number): number[];
  convertToPdfPoint(x: number, y: number): number[];
}

/** A rectangle in CSS pixels relative to the page's top-left corner. */
export interface CssRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** Same as the former `PageViewport.convertToViewportRectangle([x1, y1, x2, y2])`. */
export function convertToViewportRectangle(viewport: ViewportLike, rect: readonly [number, number, number, number]): [number, number, number, number] {
  const [ax, ay] = viewport.convertToViewportPoint(rect[0], rect[1]) as [number, number];
  const [bx, by] = viewport.convertToViewportPoint(rect[2], rect[3]) as [number, number];
  return [Math.min(ax, bx), Math.min(ay, by), Math.max(ax, bx), Math.max(ay, by)];
}

/** PDF user-space rect → CSS rect on the page (axis aligned for any quarter-turn rotation). */
export function pdfRectToCss(rect: Rect, viewport: ViewportLike): CssRect {
  const [l, t, r, b] = convertToViewportRectangle(viewport, [rect.x, rect.y, rect.x + rect.width, rect.y + rect.height]);
  return { left: l, top: t, width: r - l, height: b - t };
}

/** CSS point (page-relative) → PDF user space point. */
export function cssPointToPdf(x: number, y: number, viewport: ViewportLike): { x: number; y: number } {
  const [px, py] = viewport.convertToPdfPoint(x, y) as [number, number];
  return { x: px, y: py };
}

/** PDF user space point → CSS point (page-relative). */
export function pdfPointToCss(x: number, y: number, viewport: ViewportLike): { x: number; y: number } {
  const [cx, cy] = viewport.convertToViewportPoint(x, y) as [number, number];
  return { x: cx, y: cy };
}

/** CSS rect → PDF user space rect (inverse of {@link pdfRectToCss}). */
export function cssRectToPdf(css: CssRect, viewport: ViewportLike): Rect {
  const a = cssPointToPdf(css.left, css.top, viewport);
  const b = cssPointToPdf(css.left + css.width, css.top + css.height, viewport);
  const x = Math.min(a.x, b.x);
  const y = Math.min(a.y, b.y);
  return { x, y, width: Math.abs(b.x - a.x), height: Math.abs(b.y - a.y) };
}

/** Device (canvas backing store) pixels → CSS pixels. */
export function canvasPointToCss(x: number, y: number, devicePixelRatio: number): { x: number; y: number } {
  const dpr = devicePixelRatio > 0 ? devicePixelRatio : 1;
  return { x: x / dpr, y: y / dpr };
}

/**
 * Hit test a PDF user-space point against blocks (same page). Returns the block
 * whose rect contains the point; on ties the smallest rect wins. `tolerance`
 * is in user space units (default 1).
 */
export function hitTest<B extends Pick<Block, 'location'>>(blocks: readonly B[], pdfPoint: { x: number; y: number }, tolerance = 1): B | null {
  let best: B | null = null;
  let bestArea = Infinity;
  for (const b of blocks) {
    if (b.location.kind !== 'pdf') continue;
    for (const r of b.location.rects) {
      if (
        pdfPoint.x >= r.x - tolerance &&
        pdfPoint.x <= r.x + r.width + tolerance &&
        pdfPoint.y >= r.y - tolerance &&
        pdfPoint.y <= r.y + r.height + tolerance
      ) {
        const area = r.width * r.height;
        if (area < bestArea) {
          best = b;
          bestArea = area;
        }
      }
    }
  }
  return best;
}

/** Visibility thresholds (spec §3.4): ≥10% of the area or ≥24 CSS px visible. */
export const VISIBLE_AREA_FRACTION = 0.1;
export const VISIBLE_MIN_CSS_PX = 24;

export interface Visibility {
  /** Visible area / total area of the rects (0..1). */
  fraction: number;
  /** Visible extent in CSS px along the vertical axis (envelope of the rects' visible parts). */
  visiblePx: number;
  visible: boolean;
}

/**
 * Intersect block rects (PDF user space) with the visible part of the page.
 * `scrollRect` is the visible region in page-relative CSS pixels (e.g. the
 * scroll container's viewport translated into the page element's box).
 */
export function rectsIntersectViewport(rects: readonly Rect[], viewport: ViewportLike, scrollRect: CssRect): Visibility {
  let total = 0;
  let visibleArea = 0;
  const spans: [number, number][] = [];
  const sr = { l: scrollRect.left, t: scrollRect.top, r: scrollRect.left + scrollRect.width, b: scrollRect.top + scrollRect.height };
  for (const rect of rects) {
    const c = pdfRectToCss(rect, viewport);
    total += c.width * c.height;
    const l = Math.max(c.left, sr.l);
    const t = Math.max(c.top, sr.t);
    const r = Math.min(c.left + c.width, sr.r);
    const b = Math.min(c.top + c.height, sr.b);
    if (r > l && b > t) {
      visibleArea += (r - l) * (b - t);
      spans.push([t, b]);
    }
  }
  // Visible extent of the block like a DOM box would report it: from the
  // highest to the lowest visible part (inter-line gaps included).
  const visiblePx = spans.length ? Math.max(...spans.map((s) => s[1])) - Math.min(...spans.map((s) => s[0])) : 0;
  const fraction = total > 0 ? visibleArea / total : 0;
  return { fraction, visiblePx, visible: fraction >= VISIBLE_AREA_FRACTION || visiblePx >= VISIBLE_MIN_CSS_PX };
}
