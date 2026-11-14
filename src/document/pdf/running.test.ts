import { describe, expect, it } from 'vitest';
import { sha256Hex } from '../../domain/canonicalJson';
import { extractPage } from './extract';
import { applyRunningUpdates, detectRunningElements, runningKey } from './running';
import { filler, item, pageInput, para } from './testing/synthetic';
import type { PdfPageResult } from './types';

async function doc(n: number, header: (i: number) => string, footer: (i: number) => string, rotation = 0): Promise<PdfPageResult[]> {
  const out: PdfPageResult[] = [];
  for (let i = 0; i < n; i++) {
    const items = [item(header(i), 72, 760, 9, 'H'), ...para([filler(i, 90), filler(i + 1, 40) + '.'], 72, 700), item(footer(i), 290, 30, 9, 'H')];
    out.push(await extractPage(pageInput(items, { pageIndex: i, rotation }), 'ext', { sha256Hex }));
  }
  return out;
}

describe('running headers / footers', () => {
  it('normalises digits and roman numerals', () => {
    expect(runningKey('Chapter 12 — Page 3')).toBe(runningKey('Chapter 7 — Page 41'));
    expect(runningKey('xiv  Preface')).toBe('# preface');
  });

  it('flags repeated band text on ≥3 pages without removing it', async () => {
    const pages = await doc(4, () => 'A Book Title', (i) => `Page ${i + 1} of 4`);
    const updates = detectRunningElements(pages);
    expect(updates).toHaveLength(8);
    applyRunningUpdates(pages, updates);
    for (const p of pages) {
      expect(p.blocks).toHaveLength(3);
      expect(p.blocks[0]!.roles).toContain('running_header');
      expect(p.blocks[2]!.roles).toContain('running_footer');
      expect(p.blocks[0]!.translationPolicy).toBe('preserve');
      expect(p.blocks[1]!.translationPolicy).toBe('auto');
      expect(p.diagnostics.translatableBlockCount).toBe(1);
    }
  });

  it('needs at least three pages', async () => {
    const pages = await doc(2, () => 'A Book Title', () => 'Footer text');
    expect(detectRunningElements(pages)).toEqual([]);
  });

  it('does not flag varying band text or body text', async () => {
    const pages = await doc(4, (i) => `Different header ${'abcd'[i]}`, (i) => ['alpha', 'beta', 'gamma', 'delta'][i]!);
    expect(detectRunningElements(pages)).toEqual([]);
  });

  it('ids do not change when roles are applied (roles are not part of the id)', async () => {
    const pages = await doc(3, () => 'Header', () => 'Footer');
    const before = pages.map((p) => p.blocks.map((b) => b.id));
    applyRunningUpdates(pages, detectRunningElements(pages));
    expect(pages.map((p) => p.blocks.map((b) => b.id))).toEqual(before);
  });
});
