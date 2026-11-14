import { describe, expect, it } from 'vitest';
import { splitRange, cfiAtOffset, cfiStart } from './cfi';
import type { Block } from '../../ipc/generated/Block';

const block = (cfiRange: string, textNodes: Block['mapping']['textNodes']): Block => ({
  id: 'b', extractionId: 'e', sectionId: 's', parentId: null, ordinal: 0, kind: 'paragraph', rawText: 'Hello world', sourceText: 'Hello world', sourceHash: 'h', content: [],
  location: { kind: 'epub', spineIndex: 1, href: 'x/1.xhtml', canonicalRevision: 'r', cfiRange, textPath: [4, 2], startU16: 0, endU16: 11, quote: { exact: 'Hello world', prefix: '', suffix: '' } },
  quality: { level: 'high', reasons: [] }, translationPolicy: 'auto', mapping: { offsets: [], textNodes }, roles: [],
});

describe('cfi helpers', () => {
  it('splits a range', () => {
    expect(splitRange('epubcfi(/6/4[c1]!/4/2,/1:0,/3:5)')).toEqual({ base: '/6/4[c1]!/4/2', start: '/1:0', end: '/3:5' });
  });
  it('maps offsets through text-node runs', () => {
    const b = block('epubcfi(/6/4[c1]!/4/2,/1:0,/3:5)', [
      { path: '/4/2/1', rawStartU16: 0, rawLenU16: 6, nodeOffsetU16: 0 },
      { path: '/4/2/2/1', rawStartU16: 6, rawLenU16: 5, nodeOffsetU16: 0 },
    ]);
    expect(cfiStart(b)).toBe('epubcfi(/6/4[c1]!/4/2/1:0)');
    expect(cfiAtOffset(b, 3)).toBe('epubcfi(/6/4[c1]!/4/2/1:3)');
    expect(cfiAtOffset(b, 8)).toBe('epubcfi(/6/4[c1]!/4/2/2/1:2)');
  });
});
