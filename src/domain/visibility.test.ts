import { describe, expect, it } from 'vitest';
import { isVisible, pickAnchor, scrollTopForAnchor } from './visibility';

const vp = { top: 0, bottom: 800, height: 800 };

describe('visibility thresholds', () => {
  it('24px visible counts even for tall blocks', () => {
    expect(isVisible({ top: 776, bottom: 3000, height: 2224 }, vp)).toBe(true);
    expect(isVisible({ top: 790, bottom: 3000, height: 2210 }, vp)).toBe(false);
  });
  it('10% of a short block counts', () => {
    expect(isVisible({ top: 798, bottom: 818, height: 20 }, vp)).toBe(true); // 2px of 20 = 10%
    expect(isVisible({ top: 799, bottom: 819, height: 20 }, vp)).toBe(false);
  });
  it('block taller than the viewport is visible', () => {
    expect(isVisible({ top: -500, bottom: 2000, height: 2500 }, vp)).toBe(true);
  });
});

describe('anchor', () => {
  it('chooses block under the 25% line with ratio', () => {
    const a = pickAnchor([{ id: 'a', top: 0, bottom: 100 }, { id: 'b', top: 100, bottom: 400 }], 0, 800);
    expect(a).toEqual({ id: 'b', ratio: (200 - 100) / 300 });
  });
  it('round-trips through scrollTopForAnchor', () => {
    const st = scrollTopForAnchor(1000, 300, 0.5, 800);
    const a = pickAnchor([{ id: 'x', top: 1000, bottom: 1300 }], st, 800);
    expect(a?.id).toBe('x');
    expect(a?.ratio).toBeCloseTo(0.5, 6);
  });
});
