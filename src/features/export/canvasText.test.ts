import { describe, expect, it } from 'vitest';
import { fit, runsOf, wrap, type Measurer } from './canvasText';

/** Every character is half an em wide; bold is 10% wider. */
const measurer = (): Measurer => {
  const m = { font: '', measureText(s: string) { const px = Number(/(\d+(?:\.\d+)?)px/.exec(m.font)?.[1] ?? 10); return { width: [...s].length * px * 0.5 * (m.font.includes('700') ? 1.1 : 1) }; } };
  return m;
};
const text = (lines: ReturnType<typeof wrap>) => lines.map((l) => l.segs.map((s) => s.text).join(''));

describe('canvas text layout', () => {
  it('splits runs at mark boundaries', () => {
    const r = runsOf('지금 클라우드를 보세요', [{ startU16: 3, endU16: 7, kind: 'b' }, { startU16: 3, endU16: 7, kind: 'c:#b00000' }, { startU16: 0, endU16: 99, kind: 'i' }]);
    expect(r.map((x) => [x.text, x.bold, x.color])).toEqual([['지금 ', false, undefined], ['클라우드', true, '#b00000'], ['를 보세요', false, undefined]]);
  });
  it('wraps at spaces and never starts a line with one', () => {
    const lines = wrap(measurer(), runsOf('aaaa bbbb cccc dddd', undefined), 50, 10);
    expect(text(lines)).toEqual(['aaaa bbbb', 'cccc dddd']);
    expect(lines.every((l) => l.width <= 50)).toBe(true);
  });
  it('breaks a word that is wider than the line, and honours newlines', () => {
    expect(text(wrap(measurer(), runsOf('abcdefghijkl', undefined), 20, 10))).toEqual(['abcd', 'efgh', 'ijkl']);
    expect(text(wrap(measurer(), runsOf('one\ntwo', undefined), 100, 10))).toEqual(['one', 'two']);
  });
  it('keeps styled runs as separate segments with correct positions', () => {
    const lines = wrap(measurer(), runsOf('ab cd', [{ startU16: 3, endU16: 5, kind: 'b' }]), 100, 10);
    expect(lines).toHaveLength(1);
    const segs = lines[0]!.segs;
    expect(segs.map((s) => s.text)).toEqual(['ab', ' ', 'cd']);
    expect(segs[2]!.run.bold).toBe(true);
    expect(segs[2]!.x).toBe(15);
  });
  it('fits by shrinking, and gives up below the minimum', () => {
    const runs = runsOf('aaaa bbbb cccc dddd eeee ffff', undefined);
    const ok = fit(measurer(), runs, 60, 30, 12, 1.2, 6)!;
    expect(ok.px).toBeLessThanOrEqual(12);
    expect(ok.lines.length * ok.px * 1.2).toBeLessThanOrEqual(30.5);
    expect(fit(measurer(), runs, 20, 8, 12, 1.2, 9)).toBeNull();
  });
});
