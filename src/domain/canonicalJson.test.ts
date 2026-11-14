import { describe, expect, it } from 'vitest';
import { canonicalJson, sha256Hex } from './canonicalJson';

describe('canonicalJson (RFC 8785, matches Rust canonical_json)', () => {
  it('golden: key order and numbers', () => {
    const v = { b: 1, a: [3, null, true], é: 'x', A: 1.5, z: { y: 0, x: -0 } };
    expect(canonicalJson(v)).toBe('{"A":1.5,"a":[3,null,true],"b":1,"z":{"x":0,"y":0},"é":"x"}');
  });

  it('golden: UTF-16 code unit key order (astral before U+FF61)', () => {
    expect(canonicalJson({ '\uff61': 1, '\u{1f600}': 2 })).toBe('{"\u{1f600}":2,"\uff61":1}');
  });

  it('golden: escapes', () => {
    expect(canonicalJson({ k: 'a"b\\c\n\u0001/' })).toBe('{"k":"a\\"b\\\\c\\n\\u0001/"}');
  });

  it('numbers use ECMAScript shortest form', () => {
    expect(canonicalJson([1e21, 1.5e-7, 123, 0.1, -0])).toBe('[1e+21,1.5e-7,123,0.1,0]');
  });

  it('rejects values JSON cannot carry', () => {
    expect(() => canonicalJson({ x: Number.NaN })).toThrow();
    expect(() => canonicalJson({ x: Number.POSITIVE_INFINITY })).toThrow();
    expect(() => canonicalJson({ x: '\ud800' })).toThrow();
  });

  it('omits undefined members like JSON.stringify', () => {
    expect(canonicalJson({ a: undefined, b: 1 })).toBe('{"b":1}');
  });

  it('block id preimage shape', () => {
    const s = canonicalJson({
      extractionId: 'e',
      locator: 'page:0',
      rawRange: '0:0-3:10',
      kind: 'paragraph',
      occurrence: 0,
    });
    expect(s).toBe('{"extractionId":"e","kind":"paragraph","locator":"page:0","occurrence":0,"rawRange":"0:0-3:10"}');
  });
});

describe('sha256Hex', () => {
  it('hashes UTF-8 and returns lower-case hex', async () => {
    expect(await sha256Hex('')).toBe('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855');
    expect(await sha256Hex('abc')).toBe('ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad');
  });
});
