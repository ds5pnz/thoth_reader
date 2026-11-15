import { describe, expect, it } from 'vitest';
import { figureRegions, insertFigures, type DraftBlock } from './extract';
import type { PdfPageInput } from './types';
import { regionKey, regionScale } from './PdfRegion';

const page = (imageRects: PdfPageInput['imageRects']): PdfPageInput => ({ pageIndex: 0, viewBox: [0, 0, 600, 800], rotation: 0, items: [], styles: {}, imageCount: imageRects?.length ?? 0, imageRects });

function text(x0: number, x1: number, bottom: number, top: number, roles: string[] = []): DraftBlock {
  return { kind: 'paragraph', roles, rawText: 't', sourceText: 't', offsets: [], textNodes: [], rects: [], itemRanges: [], content: [], reasons: new Set(), level: 'high', policy: 'auto', frameBox: { x0, x1, bottom, top } };
}

describe('PDF picture regions', () => {
  it('keeps real pictures and drops icons, backgrounds and band logos', () => {
    const r = figureRegions(page([
      { x: 100, y: 300, width: 300, height: 200 }, // a figure
      { x: 50, y: 700, width: 12, height: 12 }, // bullet icon
      { x: 0, y: 0, width: 600, height: 800 }, // page background
      { x: 40, y: 750, width: 120, height: 44 }, // header logo
    ]));
    expect(r).toEqual([{ x: 100, y: 300, width: 300, height: 200 }]);
    expect(figureRegions(page(undefined))).toEqual([]);
  });

  it('joins the strips of one picture', () => {
    const r = figureRegions(page([
      { x: 100, y: 400, width: 300, height: 50 },
      { x: 100, y: 350, width: 300, height: 50 },
      { x: 100, y: 300, width: 300, height: 50 },
    ]));
    expect(r).toEqual([{ x: 100, y: 300, width: 300, height: 150 }]);
  });

  it('skips an image that text is set on, and orders pictures top to bottom', () => {
    const backdrop = { x: 50, y: 100, width: 500, height: 300 };
    const drafts = [text(60, 540, 150, 380)];
    expect(figureRegions(page([backdrop]), drafts)).toEqual([]);
    const two = figureRegions(page([{ x: 100, y: 100, width: 200, height: 100 }, { x: 100, y: 500, width: 200, height: 100 }]));
    expect(two.map((x) => x.y)).toEqual([500, 100]);
  });

  it('clips to the page and ignores what is left when too small', () => {
    expect(figureRegions(page([{ x: 580, y: 300, width: 200, height: 200 }]))).toEqual([]);
    expect(figureRegions(page([{ x: 400, y: 300, width: 400, height: 200 }]))).toEqual([{ x: 400, y: 300, width: 200, height: 200 }]);
  });

  it('puts a picture between the paragraph above and the one below it', () => {
    const drafts = [text(50, 550, 700, 760), text(50, 550, 600, 680), text(50, 550, 200, 280)];
    insertFigures(drafts, [{ x: 100, y: 320, width: 300, height: 240 }]);
    expect(drafts.map((d) => d.kind)).toEqual(['paragraph', 'paragraph', 'figure', 'paragraph']);
    expect(drafts[2]!.policy).toBe('preserve');
    expect(drafts[2]!.sourceText).toBe('');
    expect(drafts[2]!.rects).toEqual([{ x: 100, y: 320, width: 300, height: 240 }]);
  });

  it('keeps a picture in its own column and after the text when nothing follows', () => {
    // two columns: left 50-290, right 310-550; the picture sits in the right column
    const drafts = [text(50, 290, 600, 760), text(50, 290, 100, 580), text(310, 550, 600, 760), text(310, 550, 100, 300)];
    insertFigures(drafts, [{ x: 320, y: 320, width: 220, height: 260 }]);
    expect(drafts.map((d) => d.kind)).toEqual(['paragraph', 'paragraph', 'paragraph', 'figure', 'paragraph']);
    const tail = [text(50, 550, 600, 760)];
    insertFigures(tail, [{ x: 100, y: 100, width: 300, height: 300 }]);
    expect(tail.map((d) => d.kind)).toEqual(['paragraph', 'figure']);
    // a page number below the picture is not what the picture is read before
    const withFooter = [text(50, 550, 600, 760), text(280, 320, 20, 34, ['page_number'])];
    insertFigures(withFooter, [{ x: 100, y: 100, width: 300, height: 300 }]);
    expect(withFooter.map((d) => d.kind)).toEqual(['paragraph', 'figure', 'paragraph']);
  });

  it('renders at a bounded size and caches by page and rectangle', () => {
    expect(regionScale({ width: 300, height: 200 })).toBeCloseTo((96 / 72) * 2);
    expect(regionScale({ width: 3000, height: 200 })).toBeCloseTo(0.8);
    const r = { x: 1, y: 2, width: 3, height: 4 };
    expect(regionKey('tok', 5, r)).toBe(regionKey('tok', 5, { ...r }));
    expect(regionKey('tok', 5, r)).not.toBe(regionKey('tok', 6, r));
    expect(regionKey('tok', 5, r)).not.toBe(regionKey('other', 5, r));
  });
});
