import { describe, expect, it } from 'vitest';
import { accentRuns, colorsDiffer, styledContent } from './extract';
import { assignItemColors, paintedText } from './pageInput';

const identity = (n: number) => [{ sourceStart: 0, sourceEnd: n, rawStart: 0, rawEnd: n }];
const g = (s: string) => [...s].map((unicode) => ({ unicode }));

describe('painted text colours', () => {
  it('tracks the fill colour across save/restore and spacing entries', () => {
    const fn = [59, 44, 10, 59, 45, 11, 44, 57, 44];
    const args = [['#000000'], [g('See ')], null, ['#B00000'], [[...g('Cloud'), -120, ...g(' Versus')]], null, [g(' end')], [0.5], [g('?')]];
    expect(paintedText(fn, args)).toEqual([
      { text: 'See ', color: '#000000' },
      { text: 'Cloud Versus', color: '#b00000' },
      { text: ' end', color: '#000000' },
      { text: '?', color: null },
    ]);
  });
  it('assigns colours to items whose boundaries differ from the paint operators', () => {
    const items = [{ str: 'See Clo' }, { str: 'ud Versus' }, { str: ' ' }, { str: 'end' }] as { str: string; color?: string | null }[];
    const ok = assignItemColors(items, [{ text: 'See ', color: '#000000' }, { text: 'Cloud Versus', color: '#b00000' }, { text: ' end', color: '#000000' }]);
    expect(ok).toBe(true);
    expect(items.map((i) => i.color)).toEqual(['#000000', '#b00000', null, '#000000']);
  });
  it('ligatures and dropped glyphs still line up; unrelated streams give no colours', () => {
    const items = [{ str: 'office' }, { str: 'files' }] as { str: string; color?: string | null }[];
    expect(assignItemColors(items, [{ text: 'oﬃce', color: '#112233' }, { text: 'x', color: '#000000' }, { text: 'ﬁles', color: '#445566' }])).toBe(true);
    expect(items.map((i) => i.color)).toEqual(['#112233', '#445566']);
    const other = [{ str: 'completely different text' }] as { str: string; color?: string | null }[];
    expect(assignItemColors(other, [{ text: 'zzzz qqqq', color: '#ff0000' }])).toBe(false);
    expect(other[0]!.color).toBeUndefined();
  });
});

describe('accent colour runs', () => {
  it('picks the minority colour that is visibly different', () => {
    const runs = [
      { rawStart: 0, rawEnd: 40, color: '#000000' },
      { rawStart: 40, rawEnd: 52, color: '#b00000' },
      { rawStart: 52, rawEnd: 60, color: '#0a0a0a' },
    ];
    expect(accentRuns(runs)).toEqual([{ rawStart: 40, rawEnd: 52, color: '#b00000' }]);
    expect(accentRuns([{ rawStart: 0, rawEnd: 9, color: '#b00000' }])).toEqual([]);
    expect(accentRuns([{ rawStart: 0, rawEnd: 9, color: '#000000' }, { rawStart: 9, rawEnd: 20, color: '#111111' }])).toEqual([]);
    expect(colorsDiffer('#000000', '#231f20')).toBe(false);
    expect(colorsDiffer('#231f20', '#b00000')).toBe(true);
  });
  it('becomes a colour node around exactly those words', () => {
    const text = 'See Cloud Versus Self-Hosting on page 12 for more.';
    const nodes = styledContent(text, identity(text.length), [], 'paragraph', false, [{ rawStart: 4, rawEnd: 40, color: '#b00000' }]);
    expect(nodes).toEqual([
      { t: 'text', v: 'See ' },
      { t: 'color', v: '#b00000', c: [{ t: 'text', v: 'Cloud Versus Self-Hosting on page 12' }] },
      { t: 'text', v: ' for more.' },
    ]);
  });
  it('colour combines with bold, colour innermost', () => {
    const text = 'a bold red z';
    const nodes = styledContent(text, identity(text.length), [{ rawStart: 2, rawEnd: 10, s: 1 }], 'paragraph', false, [{ rawStart: 7, rawEnd: 10, color: '#cc0000' }]);
    expect(nodes).toEqual([
      { t: 'text', v: 'a ' },
      { t: 'strong', c: [{ t: 'text', v: 'bold ' }] },
      { t: 'strong', c: [{ t: 'color', v: '#cc0000', c: [{ t: 'text', v: 'red' }] }] },
      { t: 'text', v: ' z' },
    ]);
  });
});
