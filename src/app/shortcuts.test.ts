import { describe, expect, it } from 'vitest';
import { matches } from './shortcuts';

const ev = (init: KeyboardEventInit) => new KeyboardEvent('keydown', init);

describe('shortcut matching', () => {
  it('matches by key', () => {
    expect(matches(ev({ key: 'd', metaKey: true }), 'mod+d')).toBe(true);
    expect(matches(ev({ key: 'd' }), 'mod+d')).toBe(false);
    expect(matches(ev({ key: 'D', metaKey: true, shiftKey: true }), 'mod+d')).toBe(false);
  });
  it('falls back to the physical key with a Korean input source', () => {
    expect(matches(ev({ key: 'ㅇ', code: 'KeyD', metaKey: true }), 'mod+d')).toBe(true);
    expect(matches(ev({ key: 'ㄹ', code: 'KeyF', metaKey: true }), 'mod+f')).toBe(true);
    expect(matches(ev({ key: '₩', code: 'Backslash', metaKey: true }), 'mod+\\')).toBe(true);
    expect(matches(ev({ key: 'ㅇ', code: 'KeyD', metaKey: true }), 'mod+f')).toBe(false);
  });
});
