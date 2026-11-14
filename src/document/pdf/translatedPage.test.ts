import { describe, expect, it } from 'vitest';
import { blockBox, fitFontSize, pickBackground, textColorFor, MIN_FONT_PX } from './translatedPage';

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
});
