import { describe, expect, it } from 'vitest';
import type { DocumentRow } from '../../ipc/generated/DocumentRow';
import { coverUrl, displayImprint, needsPdfCover, relativeDay, sortDocuments, splitAuthors } from './covers';

const doc = (p: Partial<DocumentRow>): DocumentRow => ({
  id: 'a', sha256: '', format: 'epub', title: 't', byteSize: 1, state: 'ready', activeExtractionId: null, metadata: {}, createdAt: 0, lastOpenedAt: null, ...p,
});

describe('library sorting', () => {
  const docs = [
    doc({ id: '1', title: '나무', createdAt: 10, lastOpenedAt: 50 }),
    doc({ id: '2', title: 'Zebra', createdAt: 30 }),
    doc({ id: '3', title: 'apple 10', createdAt: 20, lastOpenedAt: 90 }),
    doc({ id: '4', title: 'Apple 2', createdAt: 40 }),
  ];
  it('recent: last read first, then unread by newest import', () => {
    expect(sortDocuments(docs, 'recent').map((d) => d.id)).toEqual(['3', '1', '4', '2']);
  });
  it('title: natural, case-insensitive, Hangul first, metadata title wins', () => {
    expect(sortDocuments(docs, 'title').map((d) => d.id)).toEqual(['1', '4', '3', '2']); // 가나다 before ABC
    const renamed = [doc({ id: 'x', title: 'zzz', metadata: { title: 'Alpha' } }), doc({ id: 'y', title: 'Beta' })];
    expect(sortDocuments(renamed, 'title').map((d) => d.id)).toEqual(['x', 'y']);
  });
  it('does not mutate its input', () => {
    const before = docs.map((d) => d.id);
    sortDocuments(docs, 'title');
    expect(docs.map((d) => d.id)).toEqual(before);
  });
});

describe('library metadata helpers', () => {
  it('cover url only when a cover exists', () => {
    expect(coverUrl(doc({}))).toBeNull();
    expect(coverUrl(doc({ id: 'abc', metadata: { hasCover: true, coverVersion: 7 } }))).toBe('thoth-doc://localhost/cover/abc?v=7');
  });
  it('pdf cover is made once', () => {
    expect(needsPdfCover(doc({ format: 'pdf' }))).toBe(true);
    expect(needsPdfCover(doc({ format: 'pdf', metadata: { libraryMeta: 1 } }))).toBe(false);
    expect(needsPdfCover(doc({ format: 'epub' }))).toBe(false);
  });
  it('splits author lists', () => {
    expect(splitAuthors('A. One; B. Two and C. Three')).toEqual(['A. One', 'B. Two', 'C. Three']);
    expect(splitAuthors('Anderson')).toEqual(['Anderson']);
    expect(splitAuthors(undefined)).toEqual([]);
  });
  it('imprint from publisher and year', () => {
    expect(displayImprint(doc({ metadata: { publisher: 'Pub', published: '2021-03-04T00:00:00Z' } }))).toBe('Pub · 2021');
    expect(displayImprint(doc({ metadata: { published: 'n/a' } }))).toBe('');
  });
  it('relative days', () => {
    const now = new Date(2026, 10, 14, 12).getTime();
    expect(relativeDay(now - 3600_000, now)).toBe('오늘');
    expect(relativeDay(new Date(2026, 10, 13, 23).getTime(), now)).toBe('어제');
    expect(relativeDay(new Date(2026, 10, 10, 9).getTime(), now)).toBe('4일 전');
  });
});
