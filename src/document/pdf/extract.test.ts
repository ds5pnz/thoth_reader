import { describe, expect, it } from 'vitest';
import { sha256Hex } from '../../domain/canonicalJson';
import { analyzePage, deriveBlockId, deriveSectionId, extractPage, isFormulaText, rawRangeOf } from './extract';
import { filler, item, item90, pageInput, para } from './testing/synthetic';
import type { PdfTextItemLike } from './types';

const deps = { sha256Hex };

describe('ids (match Rust derive_block_id / derive_section_id)', () => {
  it('hashes the canonical JSON preimage', async () => {
    const id = await deriveBlockId(deps, 'ext', 'page:0', '0:0-3:10', 'paragraph', 0);
    expect(id).toBe(await sha256Hex('{"extractionId":"ext","kind":"paragraph","locator":"page:0","occurrence":0,"rawRange":"0:0-3:10"}'));
    const sid = await deriveSectionId(deps, 'ext', 4, 'page:4');
    expect(sid).toBe(await sha256Hex('{"extractionId":"ext","locator":"page:4","ordinal":4}'));
    expect(id).toMatch(/^[0-9a-f]{64}$/);
  });

  it('is deterministic and distinguishes identical text at different positions', async () => {
    const items = [item('Same sentence here.', 72, 700), item('Same sentence here.', 72, 600)];
    const a = await extractPage(pageInput(items), 'ext', deps);
    const b = await extractPage(pageInput(items), 'ext', deps);
    expect(a.blocks.map((x) => x.id)).toEqual(b.blocks.map((x) => x.id));
    expect(a.blocks).toHaveLength(2);
    expect(a.blocks[0]!.id).not.toBe(a.blocks[1]!.id);
    expect(a.blocks[0]!.sourceHash).toBe(a.blocks[1]!.sourceHash);
    expect(a.blocks[0]!.sectionId).toBe(await deriveSectionId(deps, 'ext', 0, 'page:0'));
    const c = await extractPage(pageInput(items), 'other-extraction', deps);
    expect(c.blocks[0]!.id).not.toBe(a.blocks[0]!.id);
  });

  it('rawRange covers first..last item in reading order', () => {
    expect(rawRangeOf([{ itemIndex: 3, startU16: 0, endU16: 5 }, { itemIndex: 9, startU16: 0, endU16: 12 }])).toBe('3:0-9:12');
  });
});

describe('block shape', () => {
  it('produces Block DTOs with PDF locations and UTF-16 item ranges (emoji, combining marks)', async () => {
    const s1 = 'Emoji 😀 and nai\u0308ve text';
    const s2 = 'continues 👩\u200d💻 here.';
    const res = await extractPage(pageInput([item(s1, 72, 700), item(s2, 72, 688)]), 'ext', deps);
    expect(res.blocks).toHaveLength(1);
    const b = res.blocks[0]!;
    expect(b.location.kind).toBe('pdf');
    if (b.location.kind !== 'pdf') return;
    expect(b.location.itemRanges).toEqual([
      { itemIndex: 0, startU16: 0, endU16: s1.length },
      { itemIndex: 1, startU16: 0, endU16: s2.length },
    ]);
    expect(s1.length).toBe(24); // UTF-16 units (😀 = 2), not code points (23)
    expect(b.rawText).toBe(`${s1}\n${s2}`);
    expect(b.sourceText).toBe(`Emoji 😀 and naïve text continues 👩\u200d💻 here.`);
    // textNodes point back into rawText exactly
    for (const tn of b.mapping.textNodes) {
      const idx = Number(tn.path.replace('item:', ''));
      expect(b.rawText.slice(tn.rawStartU16, tn.rawStartU16 + tn.rawLenU16)).toBe([s1, s2][idx]);
    }
    expect(b.location.rects).toHaveLength(2);
    expect(b.location.viewBox).toEqual([0, 0, 612, 792]);
    expect(b.parentId).toBeNull();
    expect(b.ordinal).toBe(0);
    expect(b.content).toEqual([{ t: 'text', v: b.sourceText }]);
    expect(b.sourceHash).toBe(await sha256Hex(b.sourceText));
  });

  it('removes confident hyphenation and records it in mapping.offsets', async () => {
    const res = await extractPage(pageInput(para(['the recon-', 'struction of text.'], 72, 700)), 'ext', deps);
    const b = res.blocks[0]!;
    expect(b.rawText).toBe('the recon-\nstruction of text.');
    expect(b.sourceText).toBe('the reconstruction of text.');
    expect(b.mapping.offsets).toEqual([
      { sourceStart: 0, sourceEnd: 9, rawStart: 0, rawEnd: 9 },
      { sourceStart: 9, sourceEnd: 9, rawStart: 9, rawEnd: 11 },
      { sourceStart: 9, sourceEnd: 27, rawStart: 11, rawEnd: 29 },
    ]);
  });

  it('keeps hyphens of capitalised compounds without inserting a space', async () => {
    const res = await extractPage(pageInput(para(['names like Jean-', 'Paul stay intact.'], 72, 700)), 'ext', deps);
    expect(res.blocks[0]!.sourceText).toBe('names like Jean-Paul stay intact.');
  });
});

describe('classification and policy (spec §4.2)', () => {
  it('preserves code with indentation and never auto-translates it', async () => {
    const res = await extractPage(
      pageInput([item('if (x) {', 90, 500, 9, 'M'), item('return y;', 90 + 4 * 5.4, 489, 9, 'M'), item('}', 90, 478, 9, 'M')]),
      'ext',
      deps,
    );
    const b = res.blocks[0]!;
    expect(b.kind).toBe('code');
    expect(b.translationPolicy).toBe('preserve');
    expect(b.rawText).toBe('if (x) {\n    return y;\n}');
    expect(b.sourceText).toBe(b.rawText);
  });

  it('detects formulas', async () => {
    expect(isFormulaText('f(x) = Σ a_i x^i (1)', 0, 0)).toBe(true);
    expect(isFormulaText('E = mc2', 0, 0)).toBe(true);
    expect(isFormulaText('The value x = 5 is the answer we wanted.', 0, 0)).toBe(false);
    const res = await extractPage(pageInput([...para([filler(1, 80)], 72, 700), item('a + b = c', 250, 670, 12, 'I')]), 'ext', deps);
    const f = res.blocks.find((b) => b.kind === 'formula')!;
    expect(f.translationPolicy).toBe('preserve');
  });

  it('classifies tables as preserved', async () => {
    const rows = [
      ['Name', 'Value', 'Unit'],
      ['alpha', '1.0', 'm'],
      ['beta', '2.5', 's'],
    ];
    const items = rows.flatMap((r, ri) => r.map((c, ci) => item(c, 90 + ci * 120, 600 - ri * 14, 9)));
    const res = await extractPage(pageInput(items), 'ext', deps);
    expect(res.blocks).toHaveLength(1);
    const t = res.blocks[0]!;
    expect(t.kind).toBe('table');
    expect(t.translationPolicy).toBe('preserve');
    expect(t.rawText).toBe('Name\tValue\tUnit\nalpha\t1.0\tm\nbeta\t2.5\ts');
    expect(t.content.filter((n) => n.t === 'table_cell')).toHaveLength(9);
  });

  it('separates strong captions from prose that mentions a table', async () => {
    const res = await extractPage(
      pageInput([
        ...para(['Table 2: Results of the run.'], 72, 700),
        ...para([filler(1, 90) + ' x', 'Table 2 shows that the method works well on all of the inputs we tried.'], 72, 650),
      ]),
      'ext',
      deps,
    );
    expect(res.blocks.map((b) => b.kind)).toEqual(['caption', 'paragraph']);
  });

  it('marks small bottom notes with a marker as footnotes and big text as headings', async () => {
    const items: PdfTextItemLike[] = [
      item('Chapter Title', 72, 720, 16, 'B'),
      ...para([filler(1, 90), filler(2, 90), filler(3, 90)], 72, 690, { size: 11, leading: 13 }),
      item('1 A footnote in small type at the bottom of the page.', 72, 80, 8),
    ];
    const res = await extractPage(pageInput(items), 'ext', deps);
    expect(res.blocks.map((b) => [b.kind, b.roles])).toEqual([
      ['heading', []],
      ['paragraph', ['continues_on_next_page']],
      ['paragraph', ['footnote']],
    ]);
  });

  it('flags the last unfinished body paragraph as a continuation candidate only', async () => {
    const res = await extractPage(pageInput(para([filler(1, 90), 'and it stops in the middle of'], 72, 200)), 'ext', deps);
    expect(res.blocks).toHaveLength(1);
    expect(res.blocks[0]!.roles).toEqual(['continues_on_next_page']);
    const done = await extractPage(pageInput(para([filler(1, 90), 'and it ends here.'], 72, 200)), 'ext', deps);
    expect(done.blocks[0]!.roles).toEqual([]);
  });

  it('page numbers in the bands are preserved', async () => {
    const res = await extractPage(pageInput([...para([filler(1, 90) + '.'], 72, 700), item('- 12 -', 290, 30, 9)]), 'ext', deps);
    expect(res.blocks[1]!.roles).toEqual(['page_number']);
    expect(res.blocks[1]!.translationPolicy).toBe('preserve');
  });
});

describe('quality reasons (spec §4.1)', () => {
  it('likely_scan: no text + images → no blocks at all (T03)', async () => {
    const res = await extractPage(pageInput([], { imageCount: 1, imageRects: [{ x: 0, y: 0, width: 612, height: 792 }] }), 'ext', deps);
    expect(res.blocks).toEqual([]);
    expect(res.diagnostics.reasons).toEqual(['likely_scan']);
    expect(res.diagnostics.translatableBlockCount).toBe(0);
    expect(res.diagnostics.imageCoverage).toBe(1);
  });

  it('missing_unicode: unmapped glyphs → low + manual_only (T03)', async () => {
    const res = await extractPage(pageInput([item('\u0001\u0002\u0003\u0004 \u0005\u0006', 72, 700), item('\ufffd\ufffd\ufffd', 72, 688)]), 'ext', deps);
    expect(res.blocks.every((b) => b.translationPolicy !== 'auto')).toBe(true);
    expect(res.blocks[0]!.quality).toEqual({ level: 'low', reasons: ['missing_unicode'] });
    expect(res.diagnostics.missingUnicodeChars).toBe(9);
  });

  it('a few missing glyphs only lower the block to medium', async () => {
    const res = await extractPage(pageInput(para([filler(1, 90) + ' \ufffd', filler(2, 60) + '.'], 72, 700)), 'ext', deps);
    expect(res.blocks[0]!.quality).toEqual({ level: 'medium', reasons: ['missing_unicode'] });
    expect(res.blocks[0]!.translationPolicy).toBe('auto');
  });

  it('empty strings with width count as missing unicode', () => {
    const a = analyzePage(pageInput([item('', 72, 700, 10, 'R', { width: 30 }), item('text here', 72, 650)]));
    expect(a.diagnostics.missingUnicodeChars).toBe(1);
  });

  it('unresolved_columns → low + manual_only', async () => {
    const rows = Array.from({ length: 5 }, (_, i) => [item(`Key ${i}`, 72, 700 - i * 14), item(`value ${i}`, 300, 700 - i * 14)]).flat();
    const res = await extractPage(pageInput(rows), 'ext', deps);
    expect(res.blocks.length).toBeGreaterThan(0);
    for (const b of res.blocks) {
      expect(b.quality.reasons).toContain('unresolved_columns');
      expect(b.quality.level).toBe('low');
      expect(b.translationPolicy).toBe('manual_only');
    }
  });

  it('rotated_text: minor orientation block is manual_only and ordered last', async () => {
    const res = await extractPage(pageInput([...para([filler(1, 90) + '.'], 72, 700), item90('rotated label', 40, 300)]), 'ext', deps);
    expect(res.blocks.map((b) => b.sourceText.startsWith('rotated'))).toEqual([false, true]);
    expect(res.blocks[1]!.quality.reasons).toEqual(['rotated_text']);
    expect(res.blocks[1]!.translationPolicy).toBe('manual_only');
    expect(res.diagnostics.reasons).toContain('rotated_text');
  });

  it('overlapping_text is reported for double-drawn text', async () => {
    const res = await extractPage(pageInput([item('Overlap', 72, 700), item('Overlap', 72.2, 700), item(' tail.', 107, 700)]), 'ext', deps);
    expect(res.blocks[0]!.quality.reasons).toContain('overlapping_text');
    expect(res.blocks[0]!.sourceText).toBe('Overlap tail.');
  });

  it('low_text_density on image pages with very little text', async () => {
    const res = await extractPage(pageInput([item('Fig', 72, 700)], { imageCount: 2, imageRects: [{ x: 72, y: 300, width: 300, height: 300 }] }), 'ext', deps);
    expect(res.diagnostics.reasons).toContain('low_text_density');
    expect(res.blocks[0]!.quality.level).toBe('medium');
  });

  it('a full-page image with a text layer (OCR) is likely_scan but still readable', async () => {
    const res = await extractPage(
      pageInput(para([filler(1, 90), filler(2, 90) + '.'], 72, 700), { imageCount: 1, imageRects: [{ x: 0, y: 0, width: 612, height: 792 }] }),
      'ext',
      deps,
    );
    expect(res.diagnostics.reasons).toContain('likely_scan');
    expect(res.blocks[0]!.quality.level).toBe('medium');
  });
});
