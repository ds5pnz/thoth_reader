import { describe, expect, it } from 'vitest';
import { imageRefs, placeImage } from './pdfExport';

describe('pictures in the typeset PDF', () => {
  it('collects image references in reading order, also nested ones', () => {
    expect(imageRefs([
      { t: 'text', v: 'a' },
      { t: 'image_ref', resourceId: 'r1', alt: '' },
      { t: 'link', href: 'https://example.com/', c: [{ t: 'strong', c: [{ t: 'image_ref', resourceId: 'r2', alt: 'x' }] }] },
      { t: 'math_ref', resourceId: 'm1', alt: 'x^2' },
    ])).toEqual(['r1', 'r2', 'm1']);
    expect(imageRefs([{ t: 'text', v: 'plain' }])).toEqual([]);
  });

  it('keeps small pictures at natural size and fits large ones to the text area', () => {
    expect(placeImage(100, 40, 471, 712)).toEqual({ w: 75, h: 30 });
    const wide = placeImage(2000, 1000, 471, 712);
    expect(wide.w).toBeCloseTo(471);
    expect(wide.h).toBeCloseTo(235.5);
    const tall = placeImage(1000, 4000, 471, 712);
    expect(tall.h).toBeCloseTo(712);
    expect(tall.w).toBeCloseTo(178);
    expect(placeImage(0, 100, 471, 712)).toEqual({ w: 0, h: 0 });
  });
});
