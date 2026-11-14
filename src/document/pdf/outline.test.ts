import { describe, expect, it } from 'vitest';
import type { PDFDocumentProxy } from 'pdfjs-dist';
import { cleanLabel, readPdfOutline } from './outline';

const fakeDoc = (outline: unknown, pages = 10) => ({
  numPages: pages,
  getOutline: async () => outline,
  getDestination: async (name: string) => (name === 'ch2' ? [{ num: 7, gen: 0 }] : null),
  getPageIndex: async (ref: { num: number }) => { if (ref.num === 99) throw new Error('bad ref'); return ref.num - 3; },
}) as unknown as PDFDocumentProxy;

describe('pdf outline', () => {
  it('resolves explicit, named and numeric destinations', async () => {
    const toc = await readPdfOutline(fakeDoc([
      { title: 'One', dest: [{ num: 3, gen: 0 }, { name: 'XYZ' }], items: [{ title: ' 1.1\n Sub ', dest: [5], items: [] }] },
      { title: 'Two', dest: 'ch2', items: [] },
    ]));
    expect(toc.map((e) => [e.label, e.spineIndex])).toEqual([['One', 0], ['Two', 4]]);
    expect(toc[0]!.children.map((e) => [e.label, e.spineIndex])).toEqual([['1.1 Sub', 5]]);
  });
  it('dangling or out-of-range targets fall back to the previous entry', async () => {
    const toc = await readPdfOutline(fakeDoc([
      { title: 'A', dest: [6], items: [] },
      { title: 'B', dest: [{ num: 99, gen: 0 }], items: [] },
      { title: 'C', dest: [500], items: [] },
      { title: 'D', dest: 'missing', items: [] },
    ]));
    expect(toc.map((e) => e.spineIndex)).toEqual([6, 6, 6, 6]);
  });
  it('no outline → empty', async () => {
    expect(await readPdfOutline(fakeDoc(null))).toEqual([]);
    expect(await readPdfOutline({ numPages: 1, getOutline: async () => { throw new Error('x'); } } as unknown as PDFDocumentProxy)).toEqual([]);
  });
  it('labels are plain single-line text', () => {
    expect(cleanLabel('a\u0000b\n\tc')).toBe('a b c');
    expect(cleanLabel('x'.repeat(500))).toHaveLength(300);
  });
});
