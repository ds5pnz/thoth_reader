/**
 * Cross-page pass: running headers / footers (spec §4.1). Blocks are never
 * removed; repeated top/bottom-band blocks get a role and `preserve` policy.
 */
import type { Block } from '../../ipc/generated/Block';
import { makeFrame, normBox, userRectToFrameBox } from './layout';
import { ROLE_CONTINUES, ROLE_RUNNING_FOOTER, ROLE_RUNNING_HEADER } from './extract';
import type { PdfPageResult, RunningElementUpdate } from './types';

export interface RunningOptions {
  /** Fraction of the page height that counts as the top / bottom band. Default 0.12. */
  band?: number;
  /** Minimum number of distinct pages a pattern must appear on. Default 3. */
  minPages?: number;
}

/** Normalise text for repetition matching: NFKC, lower case, digit runs and lone roman numerals → '#'. */
export function runningKey(text: string): string {
  return text
    .normalize('NFKC')
    .toLowerCase()
    .replace(/\d+/g, '#')
    .replace(/(^|\s)[ivxlcdm]{1,6}(?=\s|$)/g, '$1#')
    .replace(/\s+/g, ' ')
    .trim();
}

type Band = 'top' | 'bottom';

/**
 * Find blocks that repeat (after {@link runningKey}) in the same band across at
 * least `minPages` pages. Pure: returns updates, see {@link applyRunningUpdates}.
 */
export function detectRunningElements(pages: readonly Pick<PdfPageResult, 'pageIndex' | 'blocks' | 'diagnostics'>[], opts: RunningOptions = {}): RunningElementUpdate[] {
  const band = opts.band ?? 0.12;
  const minPages = opts.minPages ?? 3;
  const candidates = new Map<string, { pageIndex: number; blockId: string; band: Band }[]>();
  for (const p of pages) {
    const frame = makeFrame(p.diagnostics.dominantTextAngle ?? 0);
    for (const b of p.blocks) {
      if (b.location.kind !== 'pdf' || b.location.rects.length === 0 || b.location.rects.length > 2) continue;
      if (b.sourceText.length === 0 || b.sourceText.length > 150) continue;
      if (b.quality.reasons.includes('rotated_text')) continue;
      const [vx0, vy0, vx1, vy1] = normBox(b.location.viewBox);
      const pageBox = userRectToFrameBox(frame, { x: vx0, y: vy0, width: vx1 - vx0, height: vy1 - vy0 });
      const h = pageBox.top - pageBox.bottom;
      let top = -Infinity;
      let bottom = Infinity;
      for (const r of b.location.rects) {
        const fb = userRectToFrameBox(frame, r);
        top = Math.max(top, fb.top);
        bottom = Math.min(bottom, fb.bottom);
      }
      let which: Band | null = null;
      if (bottom >= pageBox.top - band * h) which = 'top';
      else if (top <= pageBox.bottom + band * h) which = 'bottom';
      if (!which) continue;
      const key = `${which}|${runningKey(b.sourceText)}`;
      const arr = candidates.get(key) ?? [];
      arr.push({ pageIndex: p.pageIndex, blockId: b.id, band: which });
      candidates.set(key, arr);
    }
  }
  const updates: RunningElementUpdate[] = [];
  for (const arr of candidates.values()) {
    const distinct = new Set(arr.map((a) => a.pageIndex));
    if (distinct.size < minPages) continue;
    for (const a of arr) {
      updates.push({
        pageIndex: a.pageIndex,
        blockId: a.blockId,
        role: a.band === 'top' ? ROLE_RUNNING_HEADER : ROLE_RUNNING_FOOTER,
        translationPolicy: 'preserve',
      });
    }
  }
  updates.sort((a, b) => a.pageIndex - b.pageIndex || a.blockId.localeCompare(b.blockId));
  return updates;
}

/** Apply updates in place (roles + policy + diagnostics counters). Returns the touched blocks. */
export function applyRunningUpdates(pages: PdfPageResult[], updates: readonly RunningElementUpdate[]): Block[] {
  const byId = new Map(updates.map((u) => [u.blockId, u]));
  const touched: Block[] = [];
  for (const p of pages) {
    let changed = false;
    for (const b of p.blocks) {
      const u = byId.get(b.id);
      if (!u) continue;
      if (!b.roles.includes(u.role)) b.roles.push(u.role);
      b.roles = b.roles.filter((r) => r !== ROLE_CONTINUES);
      b.translationPolicy = u.translationPolicy;
      touched.push(b);
      changed = true;
    }
    if (changed) p.diagnostics.translatableBlockCount = p.blocks.filter((b) => b.translationPolicy === 'auto').length;
  }
  return touched;
}
