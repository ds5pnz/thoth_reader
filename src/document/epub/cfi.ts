// Canonical CFI helpers. Ranges look like epubcfi(/6/4[id]!/4/2,/1:0,/3:12) (epub.js 0.3.93).
import { EpubCFI } from 'epubjs';
import type { Block } from '../../ipc/generated/Block';

export function splitRange(cfiRange: string): { base: string; start: string; end: string } | null {
  const m = /^epubcfi\((.*)\)$/.exec(cfiRange);
  if (!m) return null;
  const parts = m[1]!.split(',');
  if (parts.length !== 3) return { base: m[1]!, start: '', end: '' };
  return { base: parts[0]!, start: parts[1]!, end: parts[2]! };
}

export function cfiStart(b: Block): string | null {
  if (b.location.kind !== 'epub') return null;
  const r = splitRange(b.location.cfiRange);
  if (!r) return null;
  // Point CFIs (e.g. figures without text) have no range parts.
  return r.start ? `epubcfi(${r.base}${r.start})` : `epubcfi(${r.base})`;
}

/** CFI for a UTF-16 offset inside the block's raw text, via the stored text-node runs. */
export function cfiAtOffset(b: Block, offsetU16: number): string | null {
  if (b.location.kind !== 'epub') return null;
  const r = splitRange(b.location.cfiRange);
  if (!r) return null;
  const spine = r.base.split('!')[0];
  const runs = b.mapping.textNodes;
  for (const t of runs) {
    if (offsetU16 >= t.rawStartU16 && offsetU16 <= t.rawStartU16 + t.rawLenU16) {
      return `epubcfi(${spine}!${t.path}:${t.nodeOffsetU16 + (offsetU16 - t.rawStartU16)})`;
    }
  }
  return cfiStart(b);
}

const cmp = new EpubCFI();

/** The block whose start is the last one at or before `cfi` (same spine item). */
export function blockForCfi(blocks: Block[], cfi: string): Block | null {
  let best: Block | null = null;
  let bestStart: string | null = null;
  for (const b of blocks) {
    const s = cfiStart(b);
    if (!s) continue;
    try {
      if (cmp.compare(s, cfi) <= 0 && (!bestStart || cmp.compare(s, bestStart) > 0)) {
        best = b;
        bestStart = s;
      }
    } catch {
      /* malformed: skip */
    }
  }
  return best;
}
