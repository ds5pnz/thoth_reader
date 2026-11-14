import { describe, expect, it } from 'vitest';
import {
  buildVocabulary,
  countMissingUnicode,
  decideLineEndHyphen,
  isSafeU16Boundary,
  normalizeSourceText,
  normalizeWithMapping,
  safeTrimRange,
  sourceToRawOffset,
} from './text';

describe('normalizeSourceText (port of Rust normalize_source_text)', () => {
  it('matches the Rust unit test cases', () => {
    expect(normalizeSourceText('  a \n\t b  ', 'paragraph')).toBe('a b');
    expect(normalizeSourceText('  a \n b', 'code')).toBe('  a \n b');
    expect(normalizeSourceText('e\u0301', 'paragraph')).toBe('é');
  });

  it('uses Rust char::is_whitespace, not JS \\s', () => {
    // U+0085 (NEL) is whitespace in Rust but not in JS \s; U+FEFF is the reverse.
    expect(normalizeSourceText('a\u0085b', 'paragraph')).toBe('a b');
    expect(normalizeSourceText('\ufeffa', 'paragraph')).toBe('\ufeffa');
    expect(normalizeSourceText('a\u3000\u2028b', 'heading')).toBe('a b');
  });

  it('leaves formula text untouched (no NFC)', () => {
    expect(normalizeSourceText(' e\u0301 ', 'formula')).toBe(' e\u0301 ');
  });
});

describe('normalizeWithMapping', () => {
  const sample = ['', '   ', 'abc', '  a  b  ', 'line one\nline two', 'e\u0301te\u0301 \t x', 'x\u00a0\u00a0y\u2003z', '😀 a\n\nb 👍🏽'];

  it('produces the same text as normalizeSourceText', () => {
    for (const s of sample) expect(normalizeWithMapping(s, 'paragraph').text).toBe(normalizeSourceText(s, 'paragraph'));
  });

  it('fuzz: same text and a contiguous, complete mapping', () => {
    const alphabet = ['a', 'b', ' ', '\n', '\t', '\u00a0', 'e\u0301', '😀', '\u0085', '-', '\u3000'];
    let seed = 7;
    const rnd = () => (seed = (seed * 1103515245 + 12345) % 2 ** 31) / 2 ** 31;
    for (let n = 0; n < 300; n++) {
      let s = '';
      const len = Math.floor(rnd() * 20);
      for (let i = 0; i < len; i++) s += alphabet[Math.floor(rnd() * alphabet.length)];
      const { text, offsets } = normalizeWithMapping(s, 'paragraph');
      expect(text).toBe(normalizeSourceText(s, 'paragraph'));
      let src = 0;
      let raw = 0;
      for (const e of offsets) {
        expect(e.sourceStart).toBe(src);
        expect(e.rawStart).toBe(raw);
        src = e.sourceEnd;
        raw = e.rawEnd;
      }
      expect(src).toBe(text.length);
      expect(raw).toBe(s.length);
    }
  });

  it('merges length-preserving runs (one entry for plain text)', () => {
    const { offsets } = normalizeWithMapping('plain text with single spaces', 'paragraph');
    expect(offsets).toEqual([{ sourceStart: 0, sourceEnd: 29, rawStart: 0, rawEnd: 29 }]);
  });

  it('records hyphen removal edits', () => {
    const raw = 'a recon-\nstruction b';
    const { text, offsets } = normalizeWithMapping(raw, 'paragraph', [{ rawStart: 7, rawEnd: 9, replacement: '' }]);
    expect(text).toBe('a reconstruction b');
    expect(offsets).toEqual([
      { sourceStart: 0, sourceEnd: 7, rawStart: 0, rawEnd: 7 },
      { sourceStart: 7, sourceEnd: 7, rawStart: 7, rawEnd: 9 },
      { sourceStart: 7, sourceEnd: 18, rawStart: 9, rawEnd: 20 },
    ]);
    // "struction" starts at source 7 and raw 9
    expect(sourceToRawOffset(offsets, 7)).toBe(9);
    expect(sourceToRawOffset(offsets, 2)).toBe(2);
  });

  it('maps collapsed whitespace and NFC changes', () => {
    const raw = 'e\u0301  x';
    const { text, offsets } = normalizeWithMapping(raw, 'paragraph');
    expect(text).toBe('é x');
    expect(offsets[0]).toEqual({ sourceStart: 0, sourceEnd: 1, rawStart: 0, rawEnd: 2 });
    expect(sourceToRawOffset(offsets, 2)).toBe(4);
  });

  it('keeps code raw but still applies edits', () => {
    expect(normalizeWithMapping('  x  \n y', 'code').text).toBe('  x  \n y');
  });
});

describe('line-end hyphenation', () => {
  it('removes syllable hyphens between lowercase halves', () => {
    expect(decideLineEndHyphen('the recon-', 'struction of')).toBe('remove');
    expect(decideLineEndHyphen('the recon-  ', '  struction')).toBe('remove');
  });
  it('keeps hyphens of capitalised compounds and digits', () => {
    expect(decideLineEndHyphen('such as Thoth-', 'Reader keeps')).toBe('keep');
    expect(decideLineEndHyphen('Jean-', 'Paul')).toBe('keep');
    expect(decideLineEndHyphen('version-', '2 is')).toBe('keep');
  });
  it('keeps hyphens after common compound prefixes', () => {
    expect(decideLineEndHyphen('a self-', 'evident claim')).toBe('keep');
    expect(decideLineEndHyphen('a non-', 'trivial case')).toBe('keep');
  });
  it('uses the page vocabulary when the hyphenated form is attested', () => {
    const vocab = buildVocabulary(['we use a pre-trained model', 'the pre-trained weights']);
    expect(decideLineEndHyphen('the pre-', 'trained model', vocab)).toBe('keep');
    expect(decideLineEndHyphen('the pre-', 'trained model')).toBe('remove');
  });
  it('always removes soft hyphens, ignores dashes and non-hyphen endings', () => {
    expect(decideLineEndHyphen('recon\u00ad', 'struction')).toBe('remove');
    expect(decideLineEndHyphen('a dash —', 'next')).toBe('none');
    expect(decideLineEndHyphen('no hyphen', 'next')).toBe('none');
    expect(decideLineEndHyphen('x -', 'y')).toBe('none');
  });
});

describe('UTF-16 boundaries and missing glyphs', () => {
  it('rejects boundaries inside surrogate pairs, combining sequences and emoji sequences', () => {
    const s = 'a😀e\u0301👍🏽👩\u200d💻b';
    expect(isSafeU16Boundary(s, 0)).toBe(true);
    expect(isSafeU16Boundary(s, 1)).toBe(true); // before 😀
    expect(isSafeU16Boundary(s, 2)).toBe(false); // inside 😀
    expect(isSafeU16Boundary(s, 3)).toBe(true); // before e
    expect(isSafeU16Boundary(s, 4)).toBe(false); // before combining acute
    expect(isSafeU16Boundary(s, 5)).toBe(true); // before 👍
    expect(isSafeU16Boundary(s, 7)).toBe(false); // before skin tone modifier
    expect(isSafeU16Boundary(s, 9)).toBe(true); // before 👩
    expect(isSafeU16Boundary(s, 11)).toBe(false); // before ZWJ
    expect(isSafeU16Boundary(s, s.length - 1)).toBe(true); // before b
    expect(isSafeU16Boundary(s, s.length)).toBe(true);
  });

  it('trims whitespace without splitting a cluster', () => {
    expect(safeTrimRange('  ab  ')).toEqual([2, 4]);
    // a space followed by a combining mark forms one cluster: not trimmed apart
    expect(safeTrimRange(' \u0301x')).toEqual([0, 3]);
  });

  it('counts unmapped glyph indicators', () => {
    expect(countMissingUnicode('abc')).toBe(0);
    expect(countMissingUnicode('a\ufffdb\u0001\ue000')).toBe(3);
    expect(countMissingUnicode('tab\tnl\n')).toBe(0);
  });
});
