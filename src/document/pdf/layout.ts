/**
 * Pure layout analysis for one PDF page (spec §4.1):
 *   text items → fragments (normalised per text orientation)
 *   → rows (shared baseline) → line segments (split at wide gaps)
 *   → tables (aligned rows of short cells) → units
 *   → columns/zones (whitespace-river gutters, spanning elements split zones)
 *   → flows (one column inside one zone, in reading order)
 *   → paragraphs (spacing, indentation, fonts, punctuation, list markers).
 *
 * All geometry is computed in a per-orientation "frame": PDF user space rotated
 * so that the text baseline runs along +x and glyph "up" is +y. Nothing here
 * assumes rotation 0 or a view box at the origin, and no `height - y` flips are
 * used; rectangles are rotated back to user space exactly.
 */
import type { Rect } from '../../ipc/generated/Rect';
import type { PdfPageInput, PdfTextStyleLike } from './types';
import { countMissingUnicode } from './text';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface StyleInfo {
  bold: boolean;
  italic: boolean;
  mono: boolean;
  symbol: boolean;
  ascent: number;
  descent: number;
}

export interface Frag {
  itemIndex: number;
  str: string;
  isSpace: boolean;
  /** Orientation group key: baseline angle in user space, degrees in [0, 360). */
  angle: number;
  size: number;
  x0: number;
  x1: number;
  base: number;
  top: number;
  bottom: number;
  style: StyleInfo;
  /** Fill colour per stretch of `str` (UTF-16 offsets), when known. */
  colorRuns?: { start: number; end: number; color: string }[];
  missing: number;
  hasEOL: boolean;
  vertical: boolean;
}

export interface TextPart {
  /** null for synthetic separators inserted by the extractor. */
  frag: Frag | null;
  text: string;
}

export interface Line {
  id: number;
  rowIndex: number;
  angle: number;
  frags: Frag[];
  parts: TextPart[];
  text: string;
  x0: number;
  x1: number;
  bottom: number;
  top: number;
  base: number;
  size: number;
  bold: boolean;
  italic: boolean;
  mono: boolean;
  symbolChars: number;
  missing: number;
  chars: number;
  overlapping: boolean;
  duplicateCount: number;
}

export interface Unit {
  kind: 'line' | 'table';
  rowIndex: number;
  lines: Line[];
  /** For tables: rows of cell lines, left to right. */
  rows?: Line[][];
  x0: number;
  x1: number;
  bottom: number;
  top: number;
  /** Line that could not be clearly assigned to a column or to a spanning zone. */
  intruding?: boolean;
}

export interface Flow {
  units: Unit[];
  unresolved: boolean;
  /** true when this flow is made of zone-spanning units (title, wide caption…). */
  spanning: boolean;
  column: number;
}

export interface Paragraph {
  flow: Flow;
  units: Unit[];
  /** Rule that started this paragraph (diagnostics only). */
  startReason?: string;
}

export interface Frame {
  angle: number;
  cos: number;
  sin: number;
}

export interface OrientationGroup {
  frame: Frame;
  frags: Frag[];
  spaces: Frag[];
  lines: Line[];
  units: Unit[];
  flows: Flow[];
  paragraphs: Paragraph[];
  columnCount: number;
  chars: number;
}

export interface PageLayout {
  groups: OrientationGroup[];
  dominant: OrientationGroup | null;
  /** Page bounds in the dominant frame. */
  pageFrameBox: { x0: number; x1: number; bottom: number; top: number };
  bodySize: number;
  bodyBold: boolean;
  clippedItemCount: number;
  missingUnicodeChars: number;
  totalChars: number;
  textItemCount: number;
  overlappingChars: number;
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

export function makeFrame(angle: number): Frame {
  const rad = (angle * Math.PI) / 180;
  // Snap exact quarter turns so rotations are exact (no 6e-17 noise).
  const snap = (v: number) => (Math.abs(v) < 1e-12 ? 0 : Math.abs(Math.abs(v) - 1) < 1e-12 ? Math.sign(v) : v);
  return { angle, cos: snap(Math.cos(rad)), sin: snap(Math.sin(rad)) };
}

/** User space → frame (rotate by -angle). */
export function toFrame(f: Frame, x: number, y: number): [number, number] {
  return [x * f.cos + y * f.sin, -x * f.sin + y * f.cos];
}

/** Frame → user space (rotate by +angle). */
export function fromFrame(f: Frame, x: number, y: number): [number, number] {
  return [x * f.cos - y * f.sin, x * f.sin + y * f.cos];
}

/** Axis-aligned user-space rect of a frame box. */
export function frameBoxToUserRect(f: Frame, x0: number, bottom: number, x1: number, top: number): Rect {
  const pts = [fromFrame(f, x0, bottom), fromFrame(f, x1, bottom), fromFrame(f, x0, top), fromFrame(f, x1, top)];
  const xs = pts.map((p) => p[0]);
  const ys = pts.map((p) => p[1]);
  const minX = Math.min(...xs);
  const minY = Math.min(...ys);
  return { x: round3(minX), y: round3(minY), width: round3(Math.max(...xs) - minX), height: round3(Math.max(...ys) - minY) };
}

export function userRectToFrameBox(f: Frame, r: Rect): { x0: number; x1: number; bottom: number; top: number } {
  const pts = [toFrame(f, r.x, r.y), toFrame(f, r.x + r.width, r.y), toFrame(f, r.x, r.y + r.height), toFrame(f, r.x + r.width, r.y + r.height)];
  const xs = pts.map((p) => p[0]);
  const ys = pts.map((p) => p[1]);
  return { x0: Math.min(...xs), x1: Math.max(...xs), bottom: Math.min(...ys), top: Math.max(...ys) };
}

const round3 = (v: number) => Math.round(v * 1000) / 1000;

// ---------------------------------------------------------------------------
// Styles
// ---------------------------------------------------------------------------

export function styleInfo(style: PdfTextStyleLike | undefined): StyleInfo {
  const name = (style?.fontName ?? '').replace(/^[A-Z]{6}\+/, '');
  const fam = style?.fontFamily ?? '';
  const mono =
    style?.isMonospace === true ||
    fam === 'monospace' ||
    /courier|mono|consol|menlo|inconsolata|fixed|typewriter|lucidaconsole|lucida console|cmtt|sourcecode|source code|firacode|fira code/i.test(name);
  const bold = /bold|black|heavy|semibold|demi|extrabold|ultrabold|[-,]bd\b|cmbx/i.test(name);
  const italic = /italic|oblique|cmmi|cmti|[-,]it\b/i.test(name);
  const symbol = /symbol|math|cmsy|cmmi|cmex|msam|msbm|stix|euclid|mt ?extra/i.test(name);
  const asc = typeof style?.ascent === 'number' && Number.isFinite(style.ascent) ? style.ascent : 0.8;
  const desc = typeof style?.descent === 'number' && Number.isFinite(style.descent) ? style.descent : -0.2;
  return {
    bold,
    italic,
    mono,
    symbol,
    ascent: clamp(asc, 0.5, 1.1),
    descent: clamp(desc, -0.5, 0),
  };
}

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

// ---------------------------------------------------------------------------
// Fragments
// ---------------------------------------------------------------------------

/** Snap an angle to a quarter turn when within 3°, else round to whole degrees. Result in [0, 360). */
export function orientationKey(rawDeg: number): number {
  let d = ((rawDeg % 360) + 360) % 360;
  const q = Math.round(d / 90) * 90;
  d = Math.abs(d - q) < 3 ? q : Math.round(d);
  return d % 360;
}

export function buildFragments(input: PdfPageInput): { frags: Frag[]; clipped: number; emptyWithWidth: number } {
  const frags: Frag[] = [];
  let clipped = 0;
  let emptyWithWidth = 0;
  const [vx0, vy0, vx1, vy1] = normBox(input.viewBox);
  input.items.forEach((it, itemIndex) => {
    if (!it || typeof it.str !== 'string') return;
    const t = it.transform;
    if (!Array.isArray(t) || t.length < 6 || !t.every((v) => Number.isFinite(v))) return;
    const [a, b, c, d, e, f] = t as [number, number, number, number, number, number];
    if (it.str.length === 0) {
      if (it.width > 0.5) emptyWithWidth++;
      return;
    }
    const st = input.styles[it.fontName];
    const vertical = it.dir === 'ttb' || st?.vertical === true;
    let deg = (Math.atan2(b, a) * 180) / Math.PI;
    if (vertical) deg -= 90;
    const angle = orientationKey(deg);
    const frame = makeFrame(angle);
    const size = Math.hypot(c, d) || it.height || Math.hypot(a, b) || 1;
    const style = styleInfo(st);
    const [fx, fy] = toFrame(frame, e, f);
    let adv = Number.isFinite(it.width) ? it.width : 0;
    if (vertical) adv = Math.max(Math.abs(it.width), Math.abs(it.height));
    const x0 = Math.min(fx, fx + adv);
    const x1 = Math.max(fx, fx + adv);
    const frag: Frag = {
      itemIndex,
      colorRuns: Array.isArray(it.colorRuns) ? it.colorRuns : undefined,
      str: it.str,
      isSpace: /^\s+$/.test(it.str),
      angle,
      size,
      x0,
      x1,
      base: fy,
      top: fy + style.ascent * size,
      bottom: fy + style.descent * size,
      style,
      missing: countMissingUnicode(it.str),
      hasEOL: it.hasEOL,
      vertical,
    };
    // Drop items entirely outside the view box (invisible after cropping).
    const r = frameBoxToUserRect(frame, frag.x0, frag.bottom, Math.max(frag.x1, frag.x0 + 0.01), frag.top);
    const tol = 0.5;
    if (r.x > vx1 + tol || r.x + r.width < vx0 - tol || r.y > vy1 + tol || r.y + r.height < vy0 - tol) {
      if (!frag.isSpace) clipped++;
      return;
    }
    frags.push(frag);
  });
  return { frags, clipped, emptyWithWidth };
}

export function normBox(vb: readonly number[]): [number, number, number, number] {
  const [a = 0, b = 0, c = 0, d = 0] = vb;
  return [Math.min(a, c), Math.min(b, d), Math.max(a, c), Math.max(b, d)];
}

// ---------------------------------------------------------------------------
// Rows and line segments
// ---------------------------------------------------------------------------

interface Row {
  frags: Frag[];
  refBase: number;
  refSize: number;
  bottom: number;
  top: number;
}

export const LIST_MARKER_ONLY = /^(?:[•●◦▪▫‣∙·○■□➢►–—\-*]|\(?\d{1,3}[.)]|\(?[a-zA-Z][.)]|\(?(?:i|ii|iii|iv|v|vi|vii|viii|ix|x|xi|xii)[.)])$/;
export const LIST_MARKER_START = /^(?:[•●◦▪▫‣∙·○■□➢►–—\-*]\s*\S|\(?\d{1,3}[.)]\s+\S|\(?[a-zA-Z][.)]\s+\S|\(?(?:i|ii|iii|iv|v|vi|vii|viii|ix|x|xi|xii)[.)]\s+\S)/;
const EQUATION_NUMBER = /^\(\d{1,3}(?:\.\d{1,3})?[a-z]?\)$/;

export function buildRows(frags: Frag[]): Row[] {
  const sorted = [...frags].sort((p, q) => q.base - p.base || p.x0 - q.x0 || p.itemIndex - q.itemIndex);
  const rows: Row[] = [];
  for (const f of sorted) {
    let target: Row | undefined;
    for (let r = rows.length - 1; r >= 0; r--) {
      const row = rows[r] as Row;
      if (row.refBase - f.base > 3 * Math.max(row.refSize, f.size)) break;
      const ov = Math.min(row.top, f.top) - Math.max(row.bottom, f.bottom);
      const minH = Math.min(row.top - row.bottom, f.top - f.bottom);
      if (ov >= 0.5 * minH && Math.abs(row.refBase - f.base) <= 0.6 * Math.max(row.refSize, f.size)) {
        target = row;
        break;
      }
    }
    if (!target) {
      rows.push({ frags: [f], refBase: f.base, refSize: f.size, bottom: f.bottom, top: f.top });
      continue;
    }
    target.frags.push(f);
    if (f.size > target.refSize * 1.05) {
      target.refSize = f.size;
      target.refBase = f.base;
      target.bottom = f.bottom;
      target.top = f.top;
    }
  }
  rows.sort((p, q) => q.refBase - p.refBase);
  return rows;
}

/** Split rows into line segments at gaps wider than ~0.9 em; attach space items. */
export function buildLines(rows: Row[], spaces: Frag[], startId = 0): Line[] {
  const lines: Line[] = [];
  let id = startId;
  rows.forEach((row, rowIndex) => {
    const fr = [...row.frags].sort((p, q) => p.x0 - q.x0 || p.itemIndex - q.itemIndex);
    const segs: { frags: Frag[]; overlapping: boolean; dup: number }[] = [];
    let cur: { frags: Frag[]; overlapping: boolean; dup: number } | null = null;
    const splitGap = Math.max(0.9 * row.refSize, 3);
    for (const f of fr) {
      if (!cur) {
        cur = { frags: [f], overlapping: false, dup: 0 };
        segs.push(cur);
        continue;
      }
      const prev = cur.frags[cur.frags.length - 1] as Frag;
      const segX1 = Math.max(...cur.frags.map((g) => g.x1));
      const gap = f.x0 - segX1;
      if (gap > splitGap) {
        cur = { frags: [f], overlapping: false, dup: 0 };
        segs.push(cur);
        continue;
      }
      // overlap / duplicate detection (faux bold, double-drawn text)
      const ov = Math.min(prev.x1, f.x1) - Math.max(prev.x0, f.x0);
      const minW = Math.max(0.01, Math.min(prev.x1 - prev.x0, f.x1 - f.x0));
      if (ov > 0.5 * minW) {
        if (prev.str === f.str && Math.abs(prev.x0 - f.x0) < 0.2 * f.size && Math.abs(prev.base - f.base) < 0.2 * f.size) {
          cur.dup++;
          continue; // drop exact duplicate
        }
        cur.overlapping = true;
      }
      cur.frags.push(f);
    }
    // Merge list markers / equation numbers with their neighbours on the row.
    for (let i = 0; i < segs.length - 1; i++) {
      const a = segs[i] as (typeof segs)[number];
      const b = segs[i + 1] as (typeof segs)[number];
      const aText = a.frags.map((g) => g.str).join('').trim();
      const aX1 = Math.max(...a.frags.map((g) => g.x1));
      const bX0 = Math.min(...b.frags.map((g) => g.x0));
      if (LIST_MARKER_ONLY.test(aText) && bX0 - aX1 <= 3 * row.refSize) {
        a.frags.push(...b.frags);
        a.overlapping ||= b.overlapping;
        a.dup += b.dup;
        segs.splice(i + 1, 1);
        i--;
      }
    }
    for (let i = segs.length - 1; i > 0; i--) {
      const b = segs[i] as (typeof segs)[number];
      const bText = b.frags.map((g) => g.str).join('').trim();
      if (EQUATION_NUMBER.test(bText)) {
        const a = segs[i - 1] as (typeof segs)[number];
        a.frags.push(...b.frags);
        segs.splice(i, 1);
      }
    }
    for (const s of segs) {
      const x0 = Math.min(...s.frags.map((g) => g.x0));
      const x1 = Math.max(...s.frags.map((g) => g.x1));
      // Attach pdf.js space items that sit strictly inside this segment on this row.
      const inner = spaces.filter(
        (sp) => Math.abs(sp.base - row.refBase) <= 0.35 * row.refSize && sp.x0 >= x0 - 0.5 && sp.x0 <= x1 + 0.5 && sp.x0 < x1 - 0.01,
      );
      const all = [...s.frags, ...inner].sort((p, q) => p.x0 - q.x0 || Number(p.isSpace) - Number(q.isSpace) || p.itemIndex - q.itemIndex);
      // drop space items not between two real frags
      const firstReal = all.findIndex((g) => !g.isSpace);
      let lastReal = -1;
      for (let k = all.length - 1; k >= 0; k--) if (!(all[k] as Frag).isSpace) { lastReal = k; break; }
      const frags = all.filter((g, k) => !g.isSpace || (k > firstReal && k < lastReal));
      for (const sp of inner) spaces.splice(spaces.indexOf(sp), 1);
      lines.push(makeLine(id++, rowIndex, frags, s.overlapping, s.dup));
    }
  });
  return lines;
}

function makeLine(id: number, rowIndex: number, frags: Frag[], overlapping: boolean, duplicateCount: number): Line {
  const real = frags.filter((f) => !f.isSpace);
  const parts: TextPart[] = [];
  let text = '';
  let prev: Frag | null = null;
  for (const f of frags) {
    if (prev && !f.isSpace && !prev.isSpace) {
      const gap = f.x0 - prev.x1;
      const needs = gap > 0.2 * Math.min(prev.size, f.size) && !/\s$/.test(prev.str) && !/^\s/.test(f.str);
      if (needs) {
        parts.push({ frag: null, text: ' ' });
        text += ' ';
      }
    }
    parts.push({ frag: f, text: f.str });
    text += f.str;
    prev = f;
  }
  let chars = 0;
  let boldC = 0;
  let italC = 0;
  let monoC = 0;
  let symC = 0;
  let missing = 0;
  const sizes: [number, number][] = [];
  for (const f of real) {
    const n = Math.max(1, f.str.trim().length);
    chars += n;
    sizes.push([f.size, n]);
    if (f.style.bold) boldC += n;
    if (f.style.italic) italC += n;
    if (f.style.mono) monoC += n;
    if (f.style.symbol) symC += n;
    missing += f.missing;
  }
  // dominant size = size with most characters (superscripts don't win)
  const bySize = new Map<number, number>();
  for (const [s, n] of sizes) {
    const k = Math.round(s * 4) / 4;
    bySize.set(k, (bySize.get(k) ?? 0) + n);
  }
  let size = real[0]?.size ?? 10;
  let best = -1;
  for (const [s, n] of bySize) if (n > best || (n === best && s > size)) { best = n; size = s; }
  // baseline/extent of the dominant-size frags
  const main = real.filter((f) => Math.abs(f.size - size) <= 0.3);
  const ref = main.length ? main : real;
  return {
    id,
    rowIndex,
    angle: real[0]?.angle ?? 0,
    frags,
    parts,
    text,
    x0: Math.min(...real.map((f) => f.x0)),
    x1: Math.max(...real.map((f) => f.x1)),
    bottom: Math.min(...real.map((f) => f.bottom)),
    top: Math.max(...real.map((f) => f.top)),
    base: median(ref.map((f) => f.base)),
    size,
    bold: boldC > chars / 2,
    italic: italC > chars / 2,
    mono: monoC > chars * 0.8,
    symbolChars: symC,
    missing,
    chars,
    overlapping,
    duplicateCount,
  };
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

/**
 * Detect simple tables: ≥2 consecutive rows, each with ≥3 short cells, whose
 * cell spans overlap horizontally. Returns table units and the remaining lines.
 */
export function detectTables(lines: Line[], contentWidth: number): { tables: Unit[]; rest: Line[] } {
  const byRow = new Map<number, Line[]>();
  for (const l of lines) {
    const arr = byRow.get(l.rowIndex) ?? [];
    arr.push(l);
    byRow.set(l.rowIndex, arr);
  }
  const words = (l: Line) => l.text.trim().split(/\s+/).filter(Boolean).length;
  const isCell = (l: Line) => l.text.trim().length <= 30 && words(l) <= 4 && l.x1 - l.x0 <= Math.max(0.3 * contentWidth, 60) && !l.mono;
  const rowIdx = [...byRow.keys()].sort((a, b) => a - b);
  const candidates = new Map<number, Line[]>();
  for (const r of rowIdx) {
    const cells = (byRow.get(r) ?? []).filter(isCell).sort((a, b) => a.x0 - b.x0);
    if (cells.length >= 3) candidates.set(r, cells);
  }
  const tables: Unit[] = [];
  const used = new Set<number>();
  let run: Line[][] = [];
  // Narrow text columns look like rows of short cells; real tables have
  // uneven cell widths and terse cells.
  const proseLike = (rows: Line[][]) => {
    const cells = rows.flat();
    const meanWords = cells.reduce((s, l) => s + words(l), 0) / Math.max(1, cells.length);
    const widths = cells.map((l) => l.x1 - l.x0);
    const mean = widths.reduce((a, b) => a + b, 0) / Math.max(1, widths.length);
    const sd = Math.sqrt(widths.reduce((a, b) => a + (b - mean) ** 2, 0) / Math.max(1, widths.length));
    return meanWords >= 3 && sd / Math.max(1, mean) < 0.15;
  };
  const flush = () => {
    if (run.length >= 2 && !proseLike(run)) {
      const all = run.flat();
      all.forEach((l) => used.add(l.id));
      tables.push({
        kind: 'table',
        rowIndex: (run[0] as Line[])[0]?.rowIndex ?? 0,
        lines: all,
        rows: run,
        x0: Math.min(...all.map((l) => l.x0)),
        x1: Math.max(...all.map((l) => l.x1)),
        bottom: Math.min(...all.map((l) => l.bottom)),
        top: Math.max(...all.map((l) => l.top)),
      });
    }
    run = [];
  };
  let prevRow = -2;
  for (const r of rowIdx) {
    const cells = candidates.get(r);
    if (!cells) {
      flush();
      prevRow = -2;
      continue;
    }
    const last = run.at(-1);
    if (last && r === prevRow + 1) {
      const lb = median(last.map((l) => l.base));
      const cb = median(cells.map((l) => l.base));
      const size = median(cells.map((l) => l.size));
      const lx0 = Math.min(...last.map((l) => l.x0));
      const lx1 = Math.max(...last.map((l) => l.x1));
      const cx0 = Math.min(...cells.map((l) => l.x0));
      const cx1 = Math.max(...cells.map((l) => l.x1));
      const ov = Math.min(lx1, cx1) - Math.max(lx0, cx0);
      if (lb - cb <= 2.5 * size && ov >= 0.5 * Math.min(lx1 - lx0, cx1 - cx0)) {
        run.push(cells);
      } else {
        flush();
        run = [cells];
      }
    } else {
      flush();
      run = [cells];
    }
    prevRow = r;
  }
  flush();
  return { tables, rest: lines.filter((l) => !used.has(l.id)) };
}

// ---------------------------------------------------------------------------
// Columns, zones and flows
// ---------------------------------------------------------------------------

export function lineUnit(l: Line): Unit {
  return { kind: 'line', rowIndex: l.rowIndex, lines: [l], x0: l.x0, x1: l.x1, bottom: l.bottom, top: l.top };
}

interface Gutter {
  g0: number;
  g1: number;
  left: Unit[];
  right: Unit[];
  span: Unit[];
  sideBySide: number;
}

const unitSort = (a: Unit, b: Unit) => a.rowIndex - b.rowIndex || a.x0 - b.x0;
const cy = (u: Unit) => (u.top + u.bottom) / 2;

export function findGutter(units: Unit[], minGutter: number): Gutter | null {
  const n = units.length;
  if (n < 4) return null;
  const minX = Math.min(...units.map((u) => u.x0));
  const maxX = Math.max(...units.map((u) => u.x1));
  const edges = [...new Set(units.flatMap((u) => [u.x0, u.x1]).filter((x) => x > minX && x < maxX))].sort((a, b) => a - b);
  edges.unshift(minX);
  edges.push(maxX);
  const allowance = Math.max(1, Math.floor(n * 0.25));
  const spans: [number, number][] = [];
  let open: [number, number] | null = null;
  for (let k = 0; k < edges.length - 1; k++) {
    const a = edges[k] as number;
    const b = edges[k + 1] as number;
    if (b - a <= 1e-6) continue;
    const mid = (a + b) / 2;
    let c = 0;
    for (const u of units) if (u.x0 < mid && u.x1 > mid) c++;
    if (c <= allowance) {
      if (open && Math.abs(open[1] - a) < 1e-6) open[1] = b;
      else {
        open = [a, b];
        spans.push(open);
      }
    } else open = null;
  }
  let best: Gutter | null = null;
  let bestScore = 0;
  for (const [g0, g1] of spans) {
    if (g1 - g0 < minGutter || g0 <= minX + 1e-6 || g1 >= maxX - 1e-6) continue;
    const tol = 1;
    const left: Unit[] = [];
    const right: Unit[] = [];
    const span: Unit[] = [];
    for (const u of units) {
      if (u.x1 <= g0 + tol) left.push(u);
      else if (u.x0 >= g1 - tol) right.push(u);
      else span.push(u);
    }
    if (left.length < 2 || right.length < 2) continue;
    const overl = (a: Unit, b: Unit) => Math.min(a.top, b.top) - Math.max(a.bottom, b.bottom) > 0;
    const sbL = left.filter((l) => right.some((r) => overl(l, r))).length;
    const sbR = right.filter((r) => left.some((l) => overl(l, r))).length;
    const sideBySide = Math.min(sbL, sbR);
    if (sideBySide < 2) continue;
    const score = (g1 - g0) * sideBySide;
    if (score > bestScore) {
      bestScore = score;
      best = { g0, g1, left, right, span, sideBySide };
    }
  }
  return best;
}

/** Order units into flows (columns within zones). Recursive for 3+ columns. */
export function orderUnits(units: Unit[], minGutter: number, depth = 0): Flow[] {
  if (units.length === 0) return [];
  const g = depth < 4 ? findGutter(units, minGutter) : null;
  if (!g) return [{ units: [...units].sort(unitSort), unresolved: false, spanning: false, column: 0 }];

  const lx0 = Math.min(...g.left.map((u) => u.x0));
  const lx1 = Math.max(...g.left.map((u) => u.x1));
  const rx0 = Math.min(...g.right.map((u) => u.x0));
  const rx1 = Math.max(...g.right.map((u) => u.x1));
  const gw = g.g1 - g.g0;
  const spanning: Unit[] = [];
  const left = [...g.left];
  const right = [...g.right];
  for (const u of g.span) {
    const ovL = Math.max(0, Math.min(u.x1, lx1) - Math.max(u.x0, lx0)) / Math.max(1, lx1 - lx0);
    const ovR = Math.max(0, Math.min(u.x1, rx1) - Math.max(u.x0, rx0)) / Math.max(1, rx1 - rx0);
    const c = (u.x0 + u.x1) / 2;
    const centred = c >= g.g0 - 0.5 * gw && c <= g.g1 + 0.5 * gw;
    if ((ovL >= 0.25 && ovR >= 0.25) || centred) spanning.push(u);
    else {
      u.intruding = true;
      (ovL >= ovR ? left : right).push(u);
    }
  }
  const shortSide = (us: Unit[]) => median(us.flatMap((u) => u.lines.map((l) => l.text.trim().length))) < 15;
  const weak = g.sideBySide < 3 || (shortSide(g.left) && shortSide(g.right));

  spanning.sort(unitSort);
  const zoneOf = (u: Unit) => spanning.filter((s) => cy(s) > cy(u)).length;
  const flows: Flow[] = [];
  let lastWasSpan = false;
  for (let k = 0; k <= spanning.length; k++) {
    const zl = left.filter((u) => zoneOf(u) === k);
    const zr = right.filter((u) => zoneOf(u) === k);
    if (zl.length || zr.length) {
      const sub = [...orderUnits(zl, minGutter, depth + 1), ...orderUnits(zr, minGutter, depth + 1).map((f) => ({ ...f, column: f.column + 1 }))];
      for (const f of sub) {
        if (weak || f.units.some((u) => u.intruding)) f.unresolved = true;
        flows.push(f);
      }
      lastWasSpan = false;
    }
    const s = spanning[k];
    if (s) {
      const last = flows.at(-1);
      if (lastWasSpan && last) last.units.push(s);
      else flows.push({ units: [s], unresolved: false, spanning: true, column: 0 });
      lastWasSpan = true;
    }
  }
  return flows;
}

export function median(xs: number[]): number {
  if (xs.length === 0) return 0;
  const s = [...xs].sort((a, b) => a - b);
  const m = Math.floor(s.length / 2);
  return s.length % 2 ? (s[m] as number) : ((s[m - 1] as number) + (s[m] as number)) / 2;
}

function percentile(xs: number[], p: number): number {
  if (xs.length === 0) return 0;
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.max(0, Math.round(p * (s.length - 1))))] as number;
}

// ---------------------------------------------------------------------------
// Paragraphs
// ---------------------------------------------------------------------------

const SENTENCE_END = /[.!?。！？:;…]["'”’»)\]]*$/;

export function groupParagraphs(flow: Flow): Paragraph[] {
  const lines = flow.units.filter((u) => u.kind === 'line').map((u) => u.lines[0] as Line);
  const sizeMed = median(lines.map((l) => l.size));
  const bodyLines = lines.filter((l) => Math.abs(l.size - sizeMed) <= 0.12 * sizeMed);
  const rightEdge = percentile(bodyLines.map((l) => l.x1), 0.9);
  const leftEdge = percentile(bodyLines.map((l) => l.x0), 0.1);
  const colWidth = Math.max(1, rightEdge - leftEdge);
  const deltas: number[] = [];
  for (let i = 1; i < lines.length; i++) {
    const a = lines[i - 1] as Line;
    const b = lines[i] as Line;
    const d = a.base - b.base;
    if (Math.abs(a.size - b.size) <= 0.12 * a.size && d > 0.8 * a.size && d < 2.5 * a.size) deltas.push(d);
  }
  // Lower quartile: paragraph gaps inflate the median on short flows.
  const leading = deltas.length ? percentile(deltas, 0.25) : 1.2 * (sizeMed || 10);

  const paras: Paragraph[] = [];
  let cur: Unit[] = [];
  let curLines: Line[] = [];
  let curWhy = 'start';
  const push = () => {
    if (cur.length) paras.push({ flow, units: cur, startReason: curWhy });
    cur = [];
    curLines = [];
  };
  for (const u of flow.units) {
    if (u.kind === 'table') {
      push();
      paras.push({ flow, units: [u], startReason: 'table' });
      continue;
    }
    const l = u.lines[0] as Line;
    const prev = curLines.at(-1);
    const why = prev ? breakReason(curLines, prev, l, { leading, rightEdge, colWidth, leftEdge }) : 'start';
    if (why === null) {
      cur.push(u);
      curLines.push(l);
      continue;
    }
    push();
    cur = [u];
    curLines = [l];
    curWhy = why;
  }
  push();
  return paras;
}

interface FlowCtx {
  leading: number;
  rightEdge: number;
  leftEdge: number;
  colWidth: number;
}

export function shouldBreak(para: Line[], prev: Line, cur: Line, ctx: FlowCtx): boolean {
  return breakReason(para, prev, cur, ctx) !== null;
}

/** Why a new paragraph starts at `cur` (null: it continues the paragraph). The name of the
 *  rule is kept on the paragraph for diagnostics. */
export function breakReason(para: Line[], prev: Line, cur: Line, ctx: FlowCtx): string | null {
  const size = Math.max(prev.size, cur.size);
  if (Math.abs(cur.size - prev.size) > 0.12 * size) return 'size';
  if (cur.bold !== prev.bold) return 'bold';
  if (cur.mono !== prev.mono) return 'mono';
  if (cur.angle !== prev.angle) return 'angle';
  const delta = prev.base - cur.base;
  if (delta <= 0.3 * size) return 'same_row'; // same row, separate segment
  const expected = Math.max(ctx.leading, 0.9 * size);
  if (delta > expected * 1.35 + 0.5) return 'gap';
  // Mono (code) blocks keep their own indentation and punctuation.
  if (cur.mono && prev.mono) return null;
  if (LIST_MARKER_START.test(cur.text.trimStart())) return 'list_marker';
  const first = para[0] as Line;
  const isList = LIST_MARKER_START.test(first.text.trimStart());
  if (SENTENCE_END.test(prev.text.trimEnd()) && prev.x1 < ctx.rightEdge - Math.max(2 * size, 0.08 * ctx.colWidth)) return 'short_last_line';
  const tol = 0.8 * size;
  if (para.length === 1) {
    const dx = cur.x0 - first.x0;
    if (Math.abs(dx) <= tol) return null;
    if (dx < 0 && -dx <= 4 * size) return null; // first-line indent
    if (dx > 0 && isList && dx <= 4 * size) return null; // hanging indent
    if (centred(prev, cur, tol)) return null;
    return 'indent_first';
  }
  const bodyX = (para[1] as Line).x0;
  if (Math.abs(cur.x0 - bodyX) <= tol) return null;
  if (centred(prev, cur, tol)) return null;
  return 'indent';
}

function centred(a: Line, b: Line, tol: number): boolean {
  return Math.abs((a.x0 + a.x1) / 2 - (b.x0 + b.x1) / 2) <= tol && Math.abs(a.x0 - b.x0) > tol;
}

// ---------------------------------------------------------------------------
// Page orchestration
// ---------------------------------------------------------------------------

export function analyzeLayout(input: PdfPageInput): PageLayout {
  const { frags, clipped, emptyWithWidth } = buildFragments(input);
  const byAngle = new Map<number, Frag[]>();
  for (const f of frags) {
    const arr = byAngle.get(f.angle) ?? [];
    arr.push(f);
    byAngle.set(f.angle, arr);
  }
  let totalChars = 0;
  let missing = emptyWithWidth;
  for (const f of frags) {
    if (f.isSpace) continue;
    totalChars += f.str.length;
    missing += f.missing;
  }
  totalChars += emptyWithWidth;

  const groups: OrientationGroup[] = [];
  let lineId = 0;
  let overlappingChars = 0;
  for (const [angle, gf] of byAngle) {
    const frame = makeFrame(angle);
    const real = gf.filter((f) => !f.isSpace);
    if (real.length === 0) continue;
    const spaces = gf.filter((f) => f.isSpace);
    const rows = buildRows(real);
    const lines = buildLines(rows, spaces, lineId);
    lineId += lines.length;
    for (const l of lines) if (l.overlapping || l.duplicateCount) overlappingChars += l.chars;
    const contentWidth = Math.max(1, Math.max(...lines.map((l) => l.x1)) - Math.min(...lines.map((l) => l.x0)));
    const { tables, rest } = detectTables(lines, contentWidth);
    const units = [...tables, ...rest.map(lineUnit)].sort(unitSort);
    const sizeMed = median(real.map((f) => f.size));
    const flows = orderUnits(units, Math.max(0.9 * sizeMed, 4));
    const paragraphs = flows.flatMap((fl) => groupParagraphs(fl));
    const columnCount = Math.max(1, ...flows.filter((f) => !f.spanning).map((f) => f.column + 1));
    const chars = real.reduce((s, f) => s + f.str.length, 0);
    groups.push({ frame, frags: real, spaces, lines, units, flows, paragraphs, columnCount, chars });
  }
  groups.sort((a, b) => b.chars - a.chars || a.frame.angle - b.frame.angle);
  const dominant = groups[0] ?? null;
  const frame = dominant?.frame ?? makeFrame(0);
  const [vx0, vy0, vx1, vy1] = normBox(input.viewBox);
  const pageFrameBox = userRectToFrameBox(frame, { x: vx0, y: vy0, width: vx1 - vx0, height: vy1 - vy0 });

  // char-weighted body font size / boldness over the dominant group
  let bodySize = 0;
  let bodyBold = false;
  if (dominant) {
    const bySize = new Map<number, number>();
    let bold = 0;
    let all = 0;
    for (const f of dominant.frags) {
      const n = f.str.trim().length;
      const k = Math.round(f.size * 4) / 4;
      bySize.set(k, (bySize.get(k) ?? 0) + n);
      all += n;
      if (f.style.bold) bold += n;
    }
    let best = -1;
    for (const [s, n] of bySize) if (n > best) { best = n; bodySize = s; }
    bodyBold = bold > all / 2;
  }
  return {
    groups,
    dominant,
    pageFrameBox,
    bodySize,
    bodyBold,
    clippedItemCount: clipped,
    missingUnicodeChars: missing,
    totalChars,
    textItemCount: input.items.length,
    overlappingChars,
  };
}
