// @vitest-environment node
import { describe, expect, it } from 'vitest';
import type { Block } from '../../ipc/generated/Block';
import type { Rect } from '../../ipc/generated/Rect';
import { sha256Hex } from '../../domain/canonicalJson';
import {
  canvasPointToCss,
  convertToViewportRectangle,
  cssPointToPdf,
  cssRectToPdf,
  hitTest,
  pdfPointToCss,
  pdfRectToCss,
  rectsIntersectViewport,
  VISIBLE_MIN_CSS_PX,
  type ViewportLike,
} from './coords';
import { extractPage } from './extract';
import { buildPageInput } from './pageInput';
import { openFixture } from './testing/fixtures';

const close = (a: number, b: number, eps = 1e-6) => expect(Math.abs(a - b)).toBeLessThan(eps);

function rectClose(a: Rect, b: Rect, eps = 1e-6) {
  close(a.x, b.x, eps);
  close(a.y, b.y, eps);
  close(a.width, b.width, eps);
  close(a.height, b.height, eps);
}

async function pagesWithBlocks(name: string) {
  const doc = await openFixture(name);
  const out = [];
  for (let i = 1; i <= doc.numPages; i++) {
    const page = await doc.getPage(i);
    const res = await extractPage(await buildPageInput(page), 'ext', { sha256Hex });
    out.push({ page, blocks: res.blocks });
  }
  return out;
}

describe('PDF ⇄ CSS conversions with real PDF.js viewports (T04)', () => {
  it('round-trips every block rect under rotation 0/90/180/270, crop offsets, zoom and devicePixelRatio; hit test returns the same block', async () => {
    let checked = 0;
    for (const name of ['cropbox.pdf', 'rotated.pdf', 'single-column.pdf']) {
      for (const { page, blocks } of await pagesWithBlocks(name)) {
        for (const extra of [0, 90, 180, 270]) {
          for (const scale of [0.5, 1, 1.75, 3]) {
            for (const dpr of [1, 2, 3]) {
              const rotation = (page.rotate + extra) % 360;
              const cssVp = page.getViewport({ scale, rotation });
              const canvasVp = page.getViewport({ scale: scale * dpr, rotation });
              for (const b of blocks) {
                if (b.location.kind !== 'pdf') continue;
                for (const r of b.location.rects) {
                  const css = pdfRectToCss(r, cssVp);
                  expect(css.width).toBeGreaterThan(0);
                  expect(css.height).toBeGreaterThan(0);
                  // inside the rendered page
                  expect(css.left).toBeGreaterThanOrEqual(-1e-6);
                  expect(css.top).toBeGreaterThanOrEqual(-1e-6);
                  expect(css.left + css.width).toBeLessThanOrEqual(cssVp.width + 1e-6);
                  expect(css.top + css.height).toBeLessThanOrEqual(cssVp.height + 1e-6);
                  rectClose(cssRectToPdf(css, cssVp), r, 1e-6);
                  // click in the middle of the highlight on a HiDPI canvas
                  const canvasCx = (css.left + css.width / 2) * dpr;
                  const canvasCy = (css.top + css.height / 2) * dpr;
                  const back = canvasPointToCss(canvasCx, canvasCy, dpr);
                  const p = cssPointToPdf(back.x, back.y, cssVp);
                  expect(hitTest(blocks, p, 0)?.id).toBe(b.id);
                  // the canvas viewport is the CSS viewport scaled by dpr
                  const viaCanvas = pdfPointToCss(p.x, p.y, canvasVp);
                  close(viaCanvas.x, canvasCx, 1e-6);
                  close(viaCanvas.y, canvasCy, 1e-6);
                  checked++;
                }
              }
            }
          }
        }
      }
    }
    expect(checked).toBeGreaterThan(1000);
  });

  it('respects the view box origin (crop box) instead of assuming (0,0)', async () => {
    const { page, blocks } = (await pagesWithBlocks('cropbox.pdf'))[0]!;
    const vp = page.getViewport({ scale: 1 });
    expect(vp.viewBox).toEqual([50, 100, 562, 742]);
    expect(vp.width).toBe(512);
    expect(vp.height).toBe(642);
    // heading baseline at y=700, x=80 in user space → (30, 42) CSS px from the crop origin
    const p = pdfPointToCss(80, 700, vp);
    close(p.x, 30);
    close(p.y, 42);
    expect(blocks[0]!.location.kind === 'pdf' && blocks[0]!.location.viewBox).toEqual([50, 100, 562, 742]);
    // text outside the crop box never becomes a block
    expect(blocks.some((b) => b.sourceText.includes('HIDDEN'))).toBe(false);
  });

  it('rotated pages: /Rotate 90 with rotated text reads upright on screen in the right order', async () => {
    const pages = await pagesWithBlocks('rotated.pdf');
    const { page, blocks } = pages[2]!;
    const vp = page.getViewport({ scale: 1 });
    expect(vp.rotation).toBe(90);
    const [heading, body] = blocks.map((b) => (b.location.kind === 'pdf' ? pdfRectToCss(b.location.rects[0]!, vp) : null));
    // horizontal on screen, heading above the paragraph
    expect(heading!.width).toBeGreaterThan(heading!.height);
    expect(heading!.top).toBeLessThan(body!.top);
    // the upright-in-user-space page displays sideways
    const p0 = pages[0]!;
    const css0 = p0.blocks[0]!.location.kind === 'pdf' ? pdfRectToCss(p0.blocks[0]!.location.rects[0]!, p0.page.getViewport({ scale: 1 })) : null;
    expect(css0!.height).toBeGreaterThan(css0!.width);
  });

  it('convertToViewportRectangle normalises corners like the removed PDF.js method', async () => {
    const { page } = (await pagesWithBlocks('cropbox.pdf'))[0]!;
    const vp = page.getViewport({ scale: 2, rotation: 180 });
    const [l, t, r, b] = convertToViewportRectangle(vp, [100, 200, 300, 250]);
    expect(l).toBeLessThan(r);
    expect(t).toBeLessThan(b);
    close(r - l, 400);
    close(b - t, 100);
  });
});

describe('hitTest', () => {
  const mk = (id: string, rects: Rect[]) =>
    ({ id, location: { kind: 'pdf', pageIndex: 0, viewBox: [0, 0, 612, 792], rotation: 0, rects, itemRanges: [] } }) as unknown as Block;
  it('returns the smallest containing block and null outside', () => {
    const big = mk('big', [{ x: 0, y: 0, width: 500, height: 500 }]);
    const small = mk('small', [{ x: 10, y: 10, width: 20, height: 10 }]);
    expect(hitTest([big, small], { x: 15, y: 15 })?.id).toBe('small');
    expect(hitTest([big, small], { x: 400, y: 400 })?.id).toBe('big');
    expect(hitTest([big, small], { x: 600, y: 600 })).toBeNull();
    expect(hitTest([small], { x: 30.5, y: 15 }, 1)?.id).toBe('small');
    expect(hitTest([small], { x: 30.5, y: 15 }, 0)).toBeNull();
  });
});

describe('visibility (spec §3.4: ≥10% area or ≥24 CSS px)', () => {
  // A plain unrotated, unscaled viewport implemented through a PDF.js-style affine transform.
  const vp: ViewportLike = {
    width: 612,
    height: 792,
    scale: 1,
    rotation: 0,
    viewBox: [0, 0, 612, 792],
    convertToViewportPoint: (x, y) => [x, 792 - y],
    convertToPdfPoint: (x, y) => [x, 792 - y],
  };
  const line = (top: number, h = 12): Rect => ({ x: 72, y: 792 - top - h, width: 400, height: h });

  it('fully visible block', () => {
    const v = rectsIntersectViewport([line(100), line(114)], vp, { left: 0, top: 0, width: 612, height: 500 });
    expect(v.fraction).toBeCloseTo(1, 6);
    expect(v.visiblePx).toBeCloseTo(26, 6);
    expect(v.visible).toBe(true);
  });

  it('a long block taller than the screen is visible through the 10% rule', () => {
    const rects = Array.from({ length: 100 }, (_, i) => line(i * 14));
    const v = rectsIntersectViewport(rects, vp, { left: 0, top: 0, width: 612, height: 300 });
    expect(v.fraction).toBeGreaterThan(0.1);
    expect(v.visible).toBe(true);
  });

  it('24 px of a huge block is enough even below 10% of its area', () => {
    const rects = Array.from({ length: 60 }, (_, i) => line(i * 12));
    const v = rectsIntersectViewport(rects, vp, { left: 0, top: 690, width: 612, height: 100 });
    expect(v.fraction).toBeLessThan(0.1);
    expect(v.visiblePx).toBeGreaterThanOrEqual(VISIBLE_MIN_CSS_PX);
    expect(v.visible).toBe(true);
  });

  it('a sliver below both thresholds is not visible', () => {
    const rects = Array.from({ length: 30 }, (_, i) => line(i * 12));
    const v = rectsIntersectViewport(rects, vp, { left: 0, top: 355, width: 612, height: 400 });
    expect(v.visiblePx).toBeCloseTo(5, 6);
    expect(v.fraction).toBeLessThan(0.1);
    expect(v.visible).toBe(false);
  });

  it('works with a real rotated, scaled viewport', async () => {
    const { page, blocks } = (await pagesWithBlocks('cropbox.pdf'))[0]!;
    const v = page.getViewport({ scale: 2, rotation: 90 });
    const b = blocks[1]!;
    if (b.location.kind !== 'pdf') throw new Error('pdf');
    const all = rectsIntersectViewport(b.location.rects, v, { left: 0, top: 0, width: v.width, height: v.height });
    expect(all.fraction).toBeCloseTo(1, 6);
    const none = rectsIntersectViewport(b.location.rects, v, { left: 0, top: v.height + 10, width: v.width, height: 100 });
    expect(none.visible).toBe(false);
  });
});
