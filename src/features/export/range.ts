// Turning "pages 10–40", "these chapters" or "everything" into section ids.
import type { SectionRow } from '../../ipc/generated/SectionRow';

export interface TocLike { label: string; spineIndex: number; children?: TocLike[] }
export interface Chapter { label: string; from: number; to: number; depth: number }

const ordinalOf = (s: SectionRow) => {
  const l = s.locator as { spineIndex?: number; pageIndex?: number };
  return Number(l.spineIndex ?? l.pageIndex ?? s.ordinal);
};

/**
 * Chapters as ordinal ranges: each top-level TOC entry runs up to (not including) the next
 * one; the last runs to the end of the book. Entries pointing at the same place are merged.
 */
export function chaptersOf(toc: readonly TocLike[], sectionCount: number): Chapter[] {
  const tops = toc.filter((e) => Number.isInteger(e.spineIndex) && e.spineIndex >= 0 && e.spineIndex < sectionCount).map((e) => ({ label: e.label, from: e.spineIndex }));
  tops.sort((a, b) => a.from - b.from);
  const out: Chapter[] = [];
  for (let i = 0; i < tops.length; i++) {
    const cur = tops[i]!;
    const next = tops.slice(i + 1).find((x) => x.from > cur.from);
    const to = (next ? next.from : sectionCount) - 1;
    const prev = out[out.length - 1];
    if (prev && prev.from === cur.from) { prev.label = `${prev.label} / ${cur.label}`; continue; }
    out.push({ label: cur.label, from: cur.from, to: Math.max(cur.from, to), depth: 0 });
  }
  return out;
}

export function sectionsInRanges(sections: readonly SectionRow[], ranges: readonly { from: number; to: number }[]): string[] {
  return sections.filter((s) => { const o = ordinalOf(s); return ranges.some((r) => o >= r.from && o <= r.to); }).map((s) => s.id);
}

/** "3-10, 15" style page list (1-based, inclusive) → 0-based ranges; null when malformed. */
export function parsePageRanges(input: string, pageCount: number): { from: number; to: number }[] | null {
  const parts = input.split(',').map((p) => p.trim()).filter(Boolean);
  if (parts.length === 0) return null;
  const out: { from: number; to: number }[] = [];
  for (const p of parts) {
    const m = /^(\d{1,6})(?:\s*[-–~]\s*(\d{1,6}))?$/.exec(p);
    if (!m) return null;
    const a = Number(m[1]);
    const b = m[2] ? Number(m[2]) : a;
    if (a < 1 || b < a || a > pageCount) return null;
    out.push({ from: a - 1, to: Math.min(pageCount, b) - 1 });
  }
  return out;
}

export function formatEta(seconds: number | null | undefined): string {
  if (seconds == null) return '계산 중…';
  if (seconds < 60) return '1분 미만';
  const m = Math.round(seconds / 60);
  if (m < 60) return `약 ${m}분`;
  const h = Math.floor(m / 60);
  const rest = m % 60;
  return rest ? `약 ${h}시간 ${rest}분` : `약 ${h}시간`;
}

/** Clock time at which the run should end, e.g. "오후 3:42". */
export function etaClock(seconds: number | null | undefined, now = Date.now()): string | null {
  if (seconds == null) return null;
  return new Date(now + seconds * 1000).toLocaleTimeString('ko-KR', { hour: 'numeric', minute: '2-digit' });
}
