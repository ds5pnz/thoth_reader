import type { BlockKind } from '../../ipc/generated/BlockKind';
import type { OffsetMapEntry } from '../../ipc/generated/OffsetMapEntry';

/**
 * Exactly Rust's `char::is_whitespace` (Unicode White_Space). Note this differs
 * from JS `\s`: it includes U+0085 and excludes U+FEFF.
 */
const RUST_WS = /[\t\n\v\f\r \u0085\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]/;
const RUST_WS_RUN = /[\t\n\v\f\r \u0085\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]+/g;

export function isRustWhitespace(ch: string): boolean {
  return RUST_WS.test(ch);
}

const PRESERVED_KINDS: ReadonlySet<BlockKind> = new Set<BlockKind>(['code', 'formula']);

/**
 * Port of Rust `domain::normalize_source_text`: NFC, collapse every whitespace
 * run to one ASCII space, trim. Code and formula blocks are returned untouched.
 */
export function normalizeSourceText(raw: string, kind: BlockKind): string {
  if (PRESERVED_KINDS.has(kind)) return raw;
  // No String#trim(): JS trims U+FEFF, Rust does not. After the replace every
  // Rust-whitespace run is a single ASCII space.
  return raw.normalize('NFC').replace(RUST_WS_RUN, ' ').replace(/^ | $/g, '');
}

/** A raw-text edit applied before normalization (e.g. removing "-\n" of a hyphenated line break). */
export interface RawEdit {
  rawStart: number;
  rawEnd: number;
  replacement: string;
}

export interface NormalizedText {
  text: string;
  /** Piecewise raw<->source mapping covering the whole source and raw strings. */
  offsets: OffsetMapEntry[];
}

interface Piece {
  rawStart: number;
  rawEnd: number;
  text: string;
  /** true for text produced by a RawEdit (never NFC'd / split). */
  edit: boolean;
}

/**
 * Applies `edits` to `raw`, then the same normalization as
 * {@link normalizeSourceText}, recording a raw<->source mapping in UTF-16
 * code units. Length-preserving runs are merged, so a typical paragraph has
 * only a handful of entries (one per hyphen removal / whitespace change).
 *
 * Invariant (tested): for no edits, `text === normalizeSourceText(raw, kind)`.
 */
export function normalizeWithMapping(raw: string, kind: BlockKind, edits: readonly RawEdit[] = []): NormalizedText {
  const pieces = applyEdits(raw, edits);
  if (PRESERVED_KINDS.has(kind)) {
    const text = pieces.map((p) => p.text).join('');
    return { text, offsets: mergeEntries(piecesToEntries(pieces)) };
  }
  // Split copy pieces into whitespace / non-whitespace runs.
  type Unit = Piece & { ws: boolean };
  const units: Unit[] = [];
  for (const p of pieces) {
    if (p.edit) {
      // Edit replacements are literal (in practice always '').
      units.push({ ...p, ws: false });
      continue;
    }
    let i = 0;
    while (i < p.text.length) {
      const ws = RUST_WS.test(p.text[i] as string);
      let j = i + 1;
      while (j < p.text.length && RUST_WS.test(p.text[j] as string) === ws) j++;
      units.push({ rawStart: p.rawStart + i, rawEnd: p.rawStart + j, text: p.text.slice(i, j), edit: false, ws });
      i = j;
    }
  }
  const entries: OffsetMapEntry[] = [];
  let out = '';
  let pendingWs: { rawStart: number; rawEnd: number } | null = null;
  for (const u of units) {
    if (u.ws) {
      pendingWs = pendingWs ? { rawStart: pendingWs.rawStart, rawEnd: u.rawEnd } : { rawStart: u.rawStart, rawEnd: u.rawEnd };
      continue;
    }
    if (u.text.length === 0) {
      // zero-length edit: keep pending whitespace pending
      if (pendingWs && out.length === 0) {
        entries.push({ sourceStart: 0, sourceEnd: 0, rawStart: pendingWs.rawStart, rawEnd: pendingWs.rawEnd });
        pendingWs = null;
      }
      entries.push({ sourceStart: out.length, sourceEnd: out.length, rawStart: u.rawStart, rawEnd: u.rawEnd });
      continue;
    }
    if (pendingWs) {
      if (out.length === 0) {
        entries.push({ sourceStart: 0, sourceEnd: 0, rawStart: pendingWs.rawStart, rawEnd: pendingWs.rawEnd });
      } else {
        entries.push({ sourceStart: out.length, sourceEnd: out.length + 1, rawStart: pendingWs.rawStart, rawEnd: pendingWs.rawEnd });
        out += ' ';
      }
      pendingWs = null;
    }
    const t = u.edit ? u.text : u.text.normalize('NFC');
    entries.push({ sourceStart: out.length, sourceEnd: out.length + t.length, rawStart: u.rawStart, rawEnd: u.rawEnd });
    out += t;
  }
  if (pendingWs) entries.push({ sourceStart: out.length, sourceEnd: out.length, rawStart: pendingWs.rawStart, rawEnd: pendingWs.rawEnd });
  return { text: out, offsets: mergeEntries(entries) };
}

function applyEdits(raw: string, edits: readonly RawEdit[]): Piece[] {
  const sorted = [...edits].sort((a, b) => a.rawStart - b.rawStart);
  const pieces: Piece[] = [];
  let pos = 0;
  for (const e of sorted) {
    if (e.rawStart < pos || e.rawEnd < e.rawStart || e.rawEnd > raw.length) throw new RangeError('overlapping or invalid raw edit');
    if (e.rawStart > pos) pieces.push({ rawStart: pos, rawEnd: e.rawStart, text: raw.slice(pos, e.rawStart), edit: false });
    pieces.push({ rawStart: e.rawStart, rawEnd: e.rawEnd, text: e.replacement, edit: true });
    pos = e.rawEnd;
  }
  if (pos < raw.length) pieces.push({ rawStart: pos, rawEnd: raw.length, text: raw.slice(pos), edit: false });
  return pieces;
}

function piecesToEntries(pieces: Piece[]): OffsetMapEntry[] {
  const out: OffsetMapEntry[] = [];
  let s = 0;
  for (const p of pieces) {
    out.push({ sourceStart: s, sourceEnd: s + p.text.length, rawStart: p.rawStart, rawEnd: p.rawEnd });
    s += p.text.length;
  }
  return out;
}

/** Merge contiguous, length-preserving entries (1:1 offsets) into single spans. */
function mergeEntries(entries: OffsetMapEntry[]): OffsetMapEntry[] {
  const out: OffsetMapEntry[] = [];
  for (const e of entries) {
    const last = out.at(-1);
    const eIdentity = e.sourceEnd - e.sourceStart === e.rawEnd - e.rawStart && e.rawEnd > e.rawStart;
    if (
      last &&
      eIdentity &&
      last.sourceEnd - last.sourceStart === last.rawEnd - last.rawStart &&
      last.rawEnd > last.rawStart &&
      last.sourceEnd === e.sourceStart &&
      last.rawEnd === e.rawStart
    ) {
      last.sourceEnd = e.sourceEnd;
      last.rawEnd = e.rawEnd;
    } else {
      out.push({ ...e });
    }
  }
  return out;
}

/** Map a source offset back to a raw offset using the mapping (start-biased). */
export function sourceToRawOffset(offsets: readonly OffsetMapEntry[], sourceOffset: number): number {
  for (const e of offsets) {
    if (sourceOffset >= e.sourceStart && sourceOffset < e.sourceEnd) {
      const len = e.sourceEnd - e.sourceStart;
      if (len === e.rawEnd - e.rawStart) return e.rawStart + (sourceOffset - e.sourceStart);
      return sourceOffset === e.sourceStart ? e.rawStart : e.rawEnd;
    }
  }
  const last = offsets.at(-1);
  return last ? last.rawEnd : 0;
}

// ---------------------------------------------------------------------------
// Missing Unicode / UTF-16 boundaries
// ---------------------------------------------------------------------------

/** U+FFFD, C0/C1 controls (except TAB/LF/CR), and BMP private-use: glyphs without a real Unicode mapping. */
const MISSING_RE = /[\ufffd\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f\ue000-\uf8ff]/g;

export function countMissingUnicode(s: string): number {
  return s.match(MISSING_RE)?.length ?? 0;
}

let segmenter: Intl.Segmenter | null | undefined;
function graphemeSegmenter(): Intl.Segmenter | null {
  if (segmenter === undefined) {
    segmenter = typeof Intl !== 'undefined' && 'Segmenter' in Intl ? new Intl.Segmenter(undefined, { granularity: 'grapheme' }) : null;
  }
  return segmenter;
}

/**
 * True when `index` (UTF-16) does not split a surrogate pair, a combining
 * sequence, or an emoji ZWJ / modifier / variation-selector sequence.
 */
export function isSafeU16Boundary(s: string, index: number): boolean {
  if (index <= 0 || index >= s.length) return index === 0 || index === s.length;
  const seg = graphemeSegmenter();
  if (seg) {
    for (const g of seg.segment(s)) {
      if (g.index === index) return true;
      if (g.index > index) return false;
    }
    return false;
  }
  const c = s.charCodeAt(index);
  const p = s.charCodeAt(index - 1);
  if (c >= 0xdc00 && c <= 0xdfff && p >= 0xd800 && p <= 0xdbff) return false;
  const ch = String.fromCodePoint(s.codePointAt(index) ?? 0);
  if (/[\p{M}\u200d︀-️]/u.test(ch) || p === 0x200d) return false;
  const cp = s.codePointAt(index) ?? 0;
  if (cp >= 0x1f3fb && cp <= 0x1f3ff) return false;
  return true;
}

/** Trim whitespace at both ends but only at grapheme-safe boundaries. Returns [start, end). */
export function safeTrimRange(s: string): [number, number] {
  let start = 0;
  let end = s.length;
  while (start < end && RUST_WS.test(s[start] as string) && isSafeU16Boundary(s, start + 1)) start++;
  while (end > start && RUST_WS.test(s[end - 1] as string) && isSafeU16Boundary(s, end - 1)) end--;
  return [start, end];
}

// ---------------------------------------------------------------------------
// Line-end hyphenation
// ---------------------------------------------------------------------------

/**
 * Prefixes that usually form real hyphenated compounds; a line-end hyphen after
 * them is kept (the line break is removed without inserting a space).
 */
const COMPOUND_PREFIXES = new Set([
  'self', 'non', 'well', 'ex', 'all', 'half', 'quasi', 'cross', 'co', 'anti', 'multi', 'semi', 'pseudo', 'post',
  'high', 'low', 'long', 'short', 'state', 'real', 'open', 'built', 'so', 'one', 'two', 'three', 'end', 'user',
]);

export type HyphenDecision =
  /** Soft/syllable hyphen: remove hyphen and line break ("recon-\nstruction" → "reconstruction"). */
  | 'remove'
  /** Real hyphen: keep it, drop the line break ("Jean-\nPaul" → "Jean-Paul"). */
  | 'keep'
  /** Not a hyphenated line end. */
  | 'none';

const LINE_END_HYPHEN = /(\p{L}+)([-\u00ad‐])$/u;
const LINE_START_WORD = /^(\p{L}+)/u;

/**
 * Decide what to do with a hyphen at the end of `prevLine` followed by
 * `nextLine`. Only removes when confident: both halves lowercase letters, the
 * head is not a common compound prefix, and (when a vocabulary is given) the
 * hyphenated form was not seen elsewhere on the page. A soft hyphen (U+00AD)
 * is always removed. Capitalised / digit continuations keep the hyphen.
 */
export function decideLineEndHyphen(prevLine: string, nextLine: string, vocabulary?: ReadonlySet<string>): HyphenDecision {
  const pm = LINE_END_HYPHEN.exec(prevLine.trimEnd());
  if (!pm) return 'none';
  const head = pm[1] as string;
  const mark = pm[2] as string;
  const next = nextLine.trimStart();
  const nm = LINE_START_WORD.exec(next);
  if (mark === '\u00ad') return nm ? 'remove' : 'none';
  if (!nm) return /^\d/.test(next) ? 'keep' : 'none';
  const tail = nm[1] as string;
  const lower = (w: string) => w === w.toLowerCase() && w !== w.toUpperCase();
  if (!lower(tail) || !lower(head)) return 'keep';
  if (COMPOUND_PREFIXES.has(head.toLowerCase())) return 'keep';
  if (vocabulary) {
    const joined = (head + tail).toLowerCase();
    const hyph = `${head}-${tail}`.toLowerCase();
    if (vocabulary.has(hyph) && !vocabulary.has(joined)) return 'keep';
  }
  if (head.length < 2 || tail.length < 2) return 'keep';
  return 'remove';
}

/** Lower-cased word vocabulary of a page (used to confirm hyphenation decisions). */
export function buildVocabulary(texts: Iterable<string>): Set<string> {
  const vocab = new Set<string>();
  for (const t of texts) {
    for (const m of t.matchAll(/\p{L}+(?:-\p{L}+)*/gu)) vocab.add(m[0].toLowerCase());
  }
  return vocab;
}
