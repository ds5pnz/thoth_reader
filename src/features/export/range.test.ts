import { describe, expect, it } from 'vitest';
import type { SectionRow } from '../../ipc/generated/SectionRow';
import { chaptersOf, formatEta, parsePageRanges, sectionsInRanges } from './range';

const secs = (n: number): SectionRow[] => Array.from({ length: n }, (_, i) => ({ id: `s${i}`, extractionId: 'e', ordinal: i, locator: { kind: 'pdf', pageIndex: i }, state: 'ready', blockCount: 1 }));

describe('export ranges', () => {
  it('chapters run up to the next chapter and the last one to the end', () => {
    const ch = chaptersOf([{ label: 'Intro', spineIndex: 0 }, { label: 'One', spineIndex: 5 }, { label: 'One (again)', spineIndex: 5 }, { label: 'Two', spineIndex: 12 }, { label: 'Bad', spineIndex: 99 }], 20);
    expect(ch.map((c) => [c.label, c.from, c.to])).toEqual([['Intro', 0, 4], ['One / One (again)', 5, 11], ['Two', 12, 19]]);
  });
  it('page lists', () => {
    expect(parsePageRanges('3-10, 15', 100)).toEqual([{ from: 2, to: 9 }, { from: 14, to: 14 }]);
    expect(parsePageRanges('90–120', 100)).toEqual([{ from: 89, to: 99 }]);
    expect(parsePageRanges('10-3', 100)).toBeNull();
    expect(parsePageRanges('0', 100)).toBeNull();
    expect(parsePageRanges('abc', 100)).toBeNull();
    expect(parsePageRanges('', 100)).toBeNull();
    expect(parsePageRanges('101', 100)).toBeNull();
  });
  it('sections in ranges', () => {
    expect(sectionsInRanges(secs(6), [{ from: 1, to: 2 }, { from: 4, to: 4 }])).toEqual(['s1', 's2', 's4']);
  });
  it('eta text', () => {
    expect(formatEta(null)).toBe('계산 중…');
    expect(formatEta(30)).toBe('1분 미만');
    expect(formatEta(60 * 34)).toBe('약 34분');
    expect(formatEta(3600 * 2 + 60 * 5)).toBe('약 2시간 5분');
    expect(formatEta(3600)).toBe('약 1시간');
  });
});
