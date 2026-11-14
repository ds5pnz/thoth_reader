import { describe, expect, it } from 'vitest';
import { blockBox, dominantItemHeight, expandBox, fitFontSize, harmonizeFontSizes, pickBackground, textColorFor, MIN_FONT_PX, type BlockBox } from './translatedPage';

// Identity-like viewport: PDF y grows upward on a 100x200 page.
const vp = {
  width: 100,
  convertToViewportPoint: (x: number, y: number) => [x, 200 - y],
  convertToPdfPoint: (x: number, y: number) => [x, 200 - y],
} as never;

describe('translated page helpers', () => {
  it('computes the block bounding box and font estimate from line rects', () => {
    const b = { location: { kind: 'pdf' as const, pageIndex: 0, viewBox: [0, 0, 100, 200], rotation: 0, itemRanges: [], rects: [{ x: 10, y: 150, width: 80, height: 10 }, { x: 10, y: 138, width: 60, height: 10 }] } };
    const box = blockBox(b, vp)!;
    expect(box.left).toBe(10);
    expect(box.width).toBe(80);
    expect(box.top).toBe(40); // 200 - (150 + 10)
    expect(box.height).toBe(22);
    expect(box.lines).toBe(2);
    expect(box.fontSize).toBeCloseTo(7.4, 5);
    expect(box.pitch).toBe(12);
  });
  it('fits the font size by binary search and respects the minimum', () => {
    expect(fitFontSize(12, () => true)).toBe(12);
    const fitted = fitFontSize(12, (px) => px <= 7.3);
    expect(fitted).toBeLessThanOrEqual(7.3);
    expect(fitted).toBeGreaterThan(7);
    expect(fitFontSize(12, () => false)).toBe(MIN_FONT_PX);
  });
  it('picks the dominant background and a readable text colour', () => {
    expect(pickBackground([[255, 255, 255], [254, 254, 254], [0, 0, 0], [255, 255, 255]])).toEqual([255, 255, 255]);
    expect(textColorFor([255, 255, 255])).toBe('#141414');
    expect(textColorFor([20, 30, 60])).toBe('#f4f4f4');
  });
  it('takes the font size from the dominant text item, not an oversized list numeral', () => {
    const b = { location: { kind: 'pdf' as const, pageIndex: 0, viewBox: [0, 0, 100, 200], rotation: 0, rects: [{ x: 10, y: 150, width: 80, height: 20 }], itemRanges: [{ itemIndex: 0, startU16: 0, endU16: 2 }, { itemIndex: 1, startU16: 0, endU16: 40 }] } };
    const items = [{ h: 18, n: 2 }, { h: 10, n: 40 }];
    expect(dominantItemHeight(b, items)).toBe(10);
    expect(blockBox(b, { ...(vp as object), scale: 1 } as never, items)!.fontSize).toBeCloseTo(9.2, 5);
  });
  it('snaps near-equal sizes to the body size but keeps headings', () => {
    const mk = (fontSize: number, width = 100, lines = 3): BlockBox => ({ left: 0, top: 0, width, height: 30, fontSize, lines, pitch: 12 });
    const m = harmonizeFontSizes([{ id: 'a', box: mk(10) }, { id: 'b', box: mk(10.4) }, { id: 'c', box: mk(9.3, 100, 1) }, { id: 'h', box: mk(16, 60, 1) }]);
    expect(m.get('a')).toBe(m.get('b'));
    expect(m.get('c')).toBe(m.get('a'));
    expect(m.get('h')).toBe(16);
  });
  it('grows into empty space and stops at rules and other blocks', () => {
    // 200x100 white raster with a vertical black rule at x=150 and a block below at y=60.
    const w = 200, h = 100;
    const data = new Uint8ClampedArray(w * h * 4).fill(255);
    for (let y = 0; y < h; y++) { const i = (y * w + 150) * 4; data[i] = data[i + 1] = data[i + 2] = 0; }
    const box: BlockBox = { left: 10, top: 10, width: 40, height: 10, fontSize: 8, lines: 1, pitch: 0 };
    const below: BlockBox = { left: 10, top: 60, width: 100, height: 10, fontSize: 8, lines: 1, pitch: 0 };
    const g = expandBox({ data, width: w, height: h }, 1, box, [255, 255, 255], [box, below], 190, 10);
    expect(g.width).toBeGreaterThan(120);          // reached the rule…
    expect(10 + g.width).toBeLessThan(150);        // …but not across it
    expect(10 + g.height).toBeLessThanOrEqual(58); // stops above the next block
    expect(g.height).toBeGreaterThan(30);
  });
});
