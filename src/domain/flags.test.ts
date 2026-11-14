import { describe, expect, it } from 'vitest';
import { flagOf, regionOf } from './flags';

describe('flags', () => {
  it('uses the region subtag, then a default region', () => {
    expect(regionOf('en-US')).toBe('US');
    expect(regionOf('en-GB')).toBe('GB');
    expect(regionOf('ko')).toBe('KR');
    expect(regionOf('zh-Hant-TW')).toBe('TW');
    expect(regionOf('xx')).toBeNull();
    expect(flagOf('ko')).toBe('🇰🇷');
    expect(flagOf('en-US')).toBe('🇺🇸');
    expect(flagOf('xx')).toBeNull();
  });
});
