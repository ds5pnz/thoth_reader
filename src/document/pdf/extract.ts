/**
 * PDF page → Block[] (spec §3.2, §4.1, §4.2). Pure: no DOM, no PDF.js — runs in
 * a Web Worker (see extract.worker.ts) or in tests. Hashing is injected so ids
 * match Rust (`domain::derive_block_id`, `derive_section_id`).
 */
import type { Block } from '../../ipc/generated/Block';
import type { BlockKind } from '../../ipc/generated/BlockKind';
import type { ItemRange } from '../../ipc/generated/ItemRange';
import type { QualityLevel } from '../../ipc/generated/QualityLevel';
import type { Rect } from '../../ipc/generated/Rect';
import type { SafeNode } from '../../ipc/generated/SafeNode';
import type { TextNodeRef } from '../../ipc/generated/TextNodeRef';
import type { TranslationPolicy } from '../../ipc/generated/TranslationPolicy';
import { canonicalJson } from '../../domain/canonicalJson';
import {
  analyzeLayout,
  frameBoxToUserRect,
  LIST_MARKER_START,
  normBox,
  type Line,
  type OrientationGroup,
  type PageLayout,
  type Paragraph,
  userRectToFrameBox,
} from './layout';
import { buildVocabulary, decideLineEndHyphen, normalizeWithMapping, type RawEdit } from './text';
import type { ExtractDeps, PdfPageDiagnostics, PdfPageInput, PdfPageResult, QualityReason } from './types';

/** Block kinds never sent to automatic translation (spec §4.2). */
export const PRESERVE_KINDS: ReadonlySet<BlockKind> = new Set<BlockKind>(['code', 'formula', 'table', 'figure']);

export const ROLE_FOOTNOTE = 'footnote';
export const ROLE_RUNNING_HEADER = 'running_header';
export const ROLE_RUNNING_FOOTER = 'running_footer';
export const ROLE_CONTINUES = 'continues_on_next_page';
/** A bare page number in the top/bottom band (never translated). */
export const ROLE_PAGE_NUMBER = 'page_number';

/** A block before ids/hashes are assigned. */
export interface DraftBlock {
  kind: BlockKind;
  roles: string[];
  rawText: string;
  sourceText: string;
  offsets: Block['mapping']['offsets'];
  textNodes: TextNodeRef[];
  rects: Rect[];
  itemRanges: ItemRange[];
  content: SafeNode[];
  reasons: Set<QualityReason>;
  level: QualityLevel;
  policy: TranslationPolicy;
  /** Bounding box in the dominant frame (for band tests). */
  frameBox: { x0: number; x1: number; bottom: number; top: number };
}

export interface PageAnalysis {
  layout: PageLayout;
  drafts: DraftBlock[];
  diagnostics: PdfPageDiagnostics;
}

// ---------------------------------------------------------------------------
// Classification helpers
// ---------------------------------------------------------------------------

const CAPTION_STRONG = /^(?:figure|fig\.|table|tab\.|listing|algorithm|chart|plate|그림|표|도표|사진)\s*[0-9IVXivx]+(?:[.\-–][0-9]+)*\s*[.:—–|]/i;
const CAPTION_WEAK = /^(?:figure|fig\.|table|tab\.|그림|표)\s*[0-9]+/i;
const MATH_CHARS = /[=+−×÷±∓≤≥≠≈≡∝∞∑∏∫∮∂∇√∈∉⊂⊃⊆⊇∪∩∧∨¬∀∃→←↔⇒⇔⟨⟩‖^_Α-Ωα-ω\u{1d400}-\u{1d7ff}]/gu;
const MATH_FUNCS = new Set(['sin', 'cos', 'tan', 'log', 'exp', 'lim', 'max', 'min', 'sup', 'inf', 'det', 'mod', 'arg', 'sinh', 'cosh', 'tanh', 'ln']);
const PAGE_NUMBER = /^(?:(?:page|p\.|페이지)\s*)?[-–—]?\s*(?:\d{1,4}|[ivxlcdm]{1,6})(?:\s*(?:of|\/)\s*\d{1,4})?\s*[-–—]?$/i;
const FOOTNOTE_MARKER = /^(?:\d{1,3}|[*†‡§¶]|[¹²³⁴⁵⁶⁷⁸⁹⁰]+|\[\d{1,3}\]|\(\d{1,3}\))\s*\S/;

export function isFormulaText(text: string, symbolShare: number, italicSingleLetters: number): boolean {
  const t = text.trim();
  if (t.length === 0 || t.length > 240) return false;
  const words = (t.match(/\p{L}{3,}/gu) ?? []).filter((w) => !MATH_FUNCS.has(w.toLowerCase()));
  const math = t.match(MATH_CHARS)?.length ?? 0;
  const nonSpace = t.replace(/\s+/g, '').length;
  if (symbolShare >= 0.3 && words.length <= 2) return true;
  if (math >= 1 && words.length <= 1 && nonSpace >= 3) return true;
  if (italicSingleLetters >= 3 && words.length <= 1 && /[=<>()+\-]/.test(t)) return true;
  return false;
}

function levelOf(reasons: Set<QualityReason>, hard: Set<QualityReason>): QualityLevel {
  if ([...hard].some((r) => reasons.has(r))) return 'low';
  return reasons.size ? 'medium' : 'high';
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

/** Layout + classification for one page; no hashing (sync, unit-testable). */
export function analyzePage(input: PdfPageInput): PageAnalysis {
  const layout = analyzeLayout(input);
  const [vx0, vy0, vx1, vy1] = normBox(input.viewBox);
  const pageArea = Math.max(1, (vx1 - vx0) * (vy1 - vy0));
  const imageCoverage = computeImageCoverage(input, pageArea);
  const pageReasons = new Set<QualityReason>();
  const realChars = layout.totalChars - layout.missingUnicodeChars;

  // Scan: no usable text but painted images → no blocks at all (spec §4.2).
  if (realChars <= 0 && input.imageCount > 0) {
    pageReasons.add('likely_scan');
  }
  const pageMissingRatio = layout.totalChars ? layout.missingUnicodeChars / layout.totalChars : 0;
  if (layout.missingUnicodeChars > 0) pageReasons.add('missing_unicode');
  // Scan + OCR layer: near-full-page image AND invisible text. A designed page with a
  // full-bleed background image and visible text is not a scan.
  const ocrLayer = realChars > 0 && imageCoverage >= 0.85 && input.invisibleText === true;
  if (ocrLayer) pageReasons.add('likely_scan');
  if (realChars > 0 && input.imageCount > 0 && realChars < pageArea * 0.0004) pageReasons.add('low_text_density');

  const drafts: DraftBlock[] = [];
  const scanNoText = realChars <= 0 && input.imageCount > 0;
  if (!scanNoText && layout.dominant) {
    const vocab = buildVocabulary(layout.groups.flatMap((g) => g.lines.map((l) => l.text)));
    for (const group of layout.groups) {
      const isDominant = group === layout.dominant;
      for (const para of group.paragraphs) {
        const d = buildDraft(para, group, layout, isDominant, vocab);
        if (!d) continue;
        if (pageMissingRatio >= 0.3) d.reasons.add('missing_unicode');
        if (ocrLayer) d.reasons.add('likely_scan');
        if (pageReasons.has('low_text_density')) d.reasons.add('low_text_density');
        finalizeQuality(d, pageMissingRatio >= 0.3, !isDominant);
        drafts.push(d);
      }
    }
    markContinuation(drafts, layout);
  }
  for (const d of drafts) for (const r of d.reasons) pageReasons.add(r);

  const diagnostics: PdfPageDiagnostics = {
    pageIndex: input.pageIndex,
    reasons: [...pageReasons].sort(),
    textItemCount: layout.textItemCount,
    clippedItemCount: layout.clippedItemCount,
    missingUnicodeChars: layout.missingUnicodeChars,
    totalChars: layout.totalChars,
    imageCount: input.imageCount,
    imageCoverage: Math.round(imageCoverage * 1000) / 1000,
    columnCount: layout.dominant?.columnCount ?? 0,
    dominantTextAngle: layout.dominant?.frame.angle ?? 0,
    blockCount: drafts.length,
    translatableBlockCount: drafts.filter((d) => d.policy === 'auto').length,
    tagged: !!input.structTree,
  };
  return { layout, drafts, diagnostics };
}

function computeImageCoverage(input: PdfPageInput, pageArea: number): number {
  if (!input.imageRects?.length) return 0;
  const [vx0, vy0, vx1, vy1] = normBox(input.viewBox);
  let area = 0;
  for (const r of input.imageRects) {
    const w = Math.max(0, Math.min(r.x + r.width, vx1) - Math.max(r.x, vx0));
    const h = Math.max(0, Math.min(r.y + r.height, vy1) - Math.max(r.y, vy0));
    area += w * h;
  }
  return Math.min(1, area / pageArea);
}

function finalizeQuality(d: DraftBlock, pageMissing: boolean, minorOrientation: boolean): void {
  const hard = new Set<QualityReason>(['unresolved_columns']);
  const missingRatio = d.rawText.length ? countMissing(d) / Math.max(1, d.rawText.replace(/\s+/g, '').length) : 0;
  if (missingRatio >= 0.1 || pageMissing) hard.add('missing_unicode');
  if (d.reasons.has('overlapping_text') && d.level === 'low') hard.add('overlapping_text');
  d.level = levelOf(d.reasons, hard);
  if (PRESERVE_KINDS.has(d.kind)) d.policy = 'preserve';
  else if (d.roles.some((r) => r === ROLE_RUNNING_HEADER || r === ROLE_RUNNING_FOOTER || r === ROLE_PAGE_NUMBER)) d.policy = 'preserve';
  else if (d.level === 'low') d.policy = 'manual_only';
  // OCR text over a scan is readable but never auto-translated (spec §4.2).
  else if (d.reasons.has('likely_scan')) d.policy = 'manual_only';
  else if (minorOrientation) d.policy = 'manual_only';
  else d.policy = 'auto';
}

function countMissing(d: DraftBlock): number {
  return (d.rawText.match(/[\ufffd\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f\ue000-\uf8ff]/g) ?? []).length;
}

function buildDraft(
  para: Paragraph,
  group: OrientationGroup,
  layout: PageLayout,
  isDominant: boolean,
  vocab: ReadonlySet<string>,
): DraftBlock | null {
  const frame = group.frame;
  const reasons = new Set<QualityReason>();
  if (para.flow.unresolved || para.units.some((u) => u.intruding)) reasons.add('unresolved_columns');
  if (!isDominant || frame.angle % 90 !== 0) reasons.add('rotated_text');

  const unit0 = para.units[0];
  if (!unit0) return null;
  const isTable = unit0.kind === 'table';
  const lines: Line[] = para.units.flatMap((u) => u.lines);
  if (lines.some((l) => l.overlapping || l.duplicateCount > 0)) reasons.add('overlapping_text');
  if (lines.some((l) => l.frags.some((f) => f.vertical))) reasons.add('rotated_text');

  const chars = lines.reduce((s, l) => s + l.chars, 0);
  const missing = lines.reduce((s, l) => s + l.missing, 0);
  if (missing > 0) reasons.add('missing_unicode');
  const missingRatio = chars ? missing / chars : 0;

  const fb = {
    x0: Math.min(...para.units.map((u) => u.x0)),
    x1: Math.max(...para.units.map((u) => u.x1)),
    bottom: Math.min(...para.units.map((u) => u.bottom)),
    top: Math.max(...para.units.map((u) => u.top)),
  };
  // frame box relative to the dominant frame (for band tests)
  const userBox = frameBoxToUserRect(frame, fb.x0, fb.bottom, fb.x1, fb.top);
  const domFrame = layout.dominant?.frame ?? frame;
  const frameBox = userRectToFrameBox(domFrame, userBox);

  // ---- kind ----
  let kind: BlockKind;
  const roles: string[] = [];
  const text = lines.map((l) => l.text).join(' ').trim();
  const size = medianWeighted(lines.map((l) => [l.size, l.chars]));
  const bold = lines.every((l) => l.bold);
  const mono = lines.every((l) => l.mono);
  const symbolShare = chars ? lines.reduce((s, l) => s + l.symbolChars, 0) / chars : 0;
  const italicSingles = lines.reduce(
    (s, l) => s + l.frags.filter((f) => f.style.italic && /^\s*\p{L}\s*$/u.test(f.str)).length,
    0,
  );
  const page = layout.pageFrameBox;
  const pageH = Math.max(1, page.top - page.bottom);
  if (isTable) kind = 'table';
  else if (missingRatio >= 0.3) kind = 'unknown';
  else if (mono) kind = 'code';
  else if (isFormulaText(text, symbolShare, italicSingles)) kind = 'formula';
  else if (CAPTION_STRONG.test(text)) kind = 'caption';
  else if (
    lines.length <= 3 &&
    text.length <= 200 &&
    layout.bodySize > 0 &&
    (size >= 1.15 * layout.bodySize || (bold && !layout.bodyBold && size >= 0.95 * layout.bodySize)) &&
    !(lines.length > 1 && /[.!?]$/.test(text) && size < 1.15 * layout.bodySize)
  ) {
    kind = 'heading';
  } else if (
    layout.bodySize > 0 &&
    size <= 0.88 * layout.bodySize &&
    frameBox.top <= page.bottom + 0.3 * pageH &&
    FOOTNOTE_MARKER.test(text) &&
    text.length > 4
  ) {
    kind = 'paragraph';
    roles.push(ROLE_FOOTNOTE);
  } else if (LIST_MARKER_START.test(text) && !PAGE_NUMBER.test(text)) kind = 'list_item';
  else if (CAPTION_WEAK.test(text) && text.length <= 300) kind = 'caption';
  else kind = 'paragraph';
  if (
    kind === 'paragraph' &&
    PAGE_NUMBER.test(text) &&
    (frameBox.top >= page.top - 0.12 * pageH || frameBox.bottom <= page.bottom + 0.12 * pageH)
  ) {
    roles.push(ROLE_PAGE_NUMBER);
  }

  // ---- raw text, text nodes, item ranges, rects ----
  const textNodes: TextNodeRef[] = [];
  const itemRanges: ItemRange[] = [];
  const rects: Rect[] = [];
  const lineStarts: number[] = [];
  const lineTexts: string[] = [];
  let raw = '';
  const appendLine = (parts: Line['parts'], prefix: string) => {
    if (prefix) raw += prefix;
    for (const p of parts) {
      if (p.frag) {
        textNodes.push({ path: `item:${p.frag.itemIndex}`, rawStartU16: raw.length, rawLenU16: p.text.length, nodeOffsetU16: 0 });
        itemRanges.push({ itemIndex: p.frag.itemIndex, startU16: 0, endU16: p.frag.str.length });
      }
      raw += p.text;
    }
  };
  const content: SafeNode[] = [];
  if (isTable && unit0.rows) {
    unit0.rows.forEach((row, ri) => {
      if (ri > 0) raw += '\n';
      lineStarts.push(raw.length);
      const cells: SafeNode[] = [];
      row.forEach((cell, ci) => {
        if (ci > 0) raw += '\t';
        appendLine(cell.parts, '');
        cells.push({ t: 'table_cell', header: ri === 0, c: [{ t: 'text', v: cell.text.trim() }] });
      });
      lineTexts.push(row.map((c) => c.text).join('\t'));
      const x0 = Math.min(...row.map((c) => c.x0));
      const x1 = Math.max(...row.map((c) => c.x1));
      rects.push(frameBoxToUserRect(frame, x0, Math.min(...row.map((c) => c.bottom)), x1, Math.max(...row.map((c) => c.top))));
      if (ri > 0) content.push({ t: 'break' });
      content.push(...cells);
    });
  } else {
    const minX = Math.min(...lines.map((l) => l.x0));
    lines.forEach((l, i) => {
      if (i > 0) raw += '\n';
      lineStarts.push(raw.length);
      let prefix = '';
      if (kind === 'code') {
        const cw = monoCharWidth(l);
        const n = cw > 0 ? Math.round((l.x0 - minX) / cw) : 0;
        if (n > 0 && n < 200) prefix = ' '.repeat(n);
      }
      appendLine(l.parts, prefix);
      lineTexts.push(prefix + l.text);
      rects.push(frameBoxToUserRect(frame, l.x0, l.bottom, l.x1, l.top));
    });
  }

  // ---- hyphenation (prose only, recorded as raw edits → mapping.offsets) ----
  const edits: RawEdit[] = [];
  if (!PRESERVE_KINDS.has(kind) && kind !== 'unknown') {
    for (let i = 0; i + 1 < lineTexts.length; i++) {
      const a = lineTexts[i] as string;
      const b = lineTexts[i + 1] as string;
      const decision = decideLineEndHyphen(a, b, vocab);
      if (decision === 'none') continue;
      const aStart = lineStarts[i] as number;
      const bStart = lineStarts[i + 1] as number;
      const aTrim = a.trimEnd().length;
      const bLead = b.length - b.trimStart().length;
      const from = decision === 'remove' ? aStart + aTrim - 1 : aStart + aTrim;
      edits.push({ rawStart: from, rawEnd: bStart + bLead, replacement: '' });
    }
  }
  const norm = normalizeWithMapping(raw, kind, edits);
  if (norm.text.length === 0 && kind !== 'code') return null;

  if (kind === 'code') content.push({ t: 'inline_code', v: raw });
  else if (!isTable) content.push({ t: 'text', v: norm.text });

  return {
    kind,
    roles,
    rawText: raw,
    sourceText: norm.text,
    offsets: norm.offsets,
    textNodes,
    rects,
    itemRanges,
    content,
    reasons,
    level: 'high',
    policy: 'auto',
    frameBox,
  };
}

function monoCharWidth(l: Line): number {
  let w = 0;
  let n = 0;
  for (const f of l.frags) {
    if (f.isSpace || f.str.length === 0) continue;
    w += f.x1 - f.x0;
    n += f.str.length;
  }
  return n ? w / n : 0;
}

function medianWeighted(pairs: [number, number][]): number {
  const s = pairs.filter((p) => p[1] > 0).sort((a, b) => a[0] - b[0]);
  const total = s.reduce((acc, p) => acc + p[1], 0);
  let acc = 0;
  for (const [v, w] of s) {
    acc += w;
    if (acc >= total / 2) return v;
  }
  return s.at(-1)?.[0] ?? 0;
}

/** Flag the last body block of the page as a cross-page continuation candidate (never merged). */
function markContinuation(drafts: DraftBlock[], layout: PageLayout): void {
  const page = layout.pageFrameBox;
  const h = Math.max(1, page.top - page.bottom);
  const inBand = (d: DraftBlock) =>
    d.sourceText.length <= 60 && (d.frameBox.top >= page.top - 0.08 * h || d.frameBox.bottom <= page.bottom + 0.08 * h);
  for (let i = drafts.length - 1; i >= 0; i--) {
    const d = drafts[i] as DraftBlock;
    if (d.roles.includes(ROLE_FOOTNOTE) || inBand(d) || d.reasons.has('rotated_text')) continue;
    if ((d.kind === 'paragraph' || d.kind === 'list_item') && !/[.!?。！？:]["'”’»)\]]*$/.test(d.sourceText)) {
      d.roles.push(ROLE_CONTINUES);
    }
    break;
  }
}

// ---------------------------------------------------------------------------
// Ids and Block objects
// ---------------------------------------------------------------------------

export function pageLocator(pageIndex: number): string {
  return `page:${pageIndex}`;
}

export function rawRangeOf(ranges: readonly ItemRange[]): string {
  const first = ranges[0];
  const last = ranges[ranges.length - 1];
  if (!first || !last) return '0:0-0:0';
  return `${first.itemIndex}:${first.startU16}-${last.itemIndex}:${last.endU16}`;
}

/** Must match Rust `domain::derive_block_id`. */
export async function deriveBlockId(
  deps: ExtractDeps,
  extractionId: string,
  locator: string,
  rawRange: string,
  kind: BlockKind,
  occurrence: number,
): Promise<string> {
  return deps.sha256Hex(canonicalJson({ extractionId, locator, rawRange, kind, occurrence }));
}

/** Must match Rust `domain::derive_section_id`. */
export async function deriveSectionId(deps: ExtractDeps, extractionId: string, ordinal: number, locator: string): Promise<string> {
  return deps.sha256Hex(canonicalJson({ extractionId, ordinal, locator }));
}

/** Extract one page into Block objects with deterministic ids. */
export async function extractPage(input: PdfPageInput, extractionId: string, deps: ExtractDeps): Promise<PdfPageResult> {
  const { drafts, diagnostics } = analyzePage(input);
  const locator = pageLocator(input.pageIndex);
  const sectionId = await deriveSectionId(deps, extractionId, input.pageIndex, locator);
  const occ = new Map<string, number>();
  const blocks: Block[] = [];
  const viewBox = [...input.viewBox];
  for (const [ordinal, d] of drafts.entries()) {
    const rawRange = rawRangeOf(d.itemRanges);
    const key = `${rawRange}|${d.kind}`;
    const occurrence = occ.get(key) ?? 0;
    occ.set(key, occurrence + 1);
    const [id, sourceHash] = await Promise.all([
      deriveBlockId(deps, extractionId, locator, rawRange, d.kind, occurrence),
      deps.sha256Hex(d.sourceText),
    ]);
    blocks.push({
      id,
      extractionId,
      sectionId,
      parentId: null,
      ordinal,
      kind: d.kind,
      rawText: d.rawText,
      sourceText: d.sourceText,
      sourceHash,
      content: d.content,
      location: {
        kind: 'pdf',
        pageIndex: input.pageIndex,
        viewBox,
        rotation: input.rotation,
        rects: d.rects,
        itemRanges: d.itemRanges,
      },
      quality: { level: d.level, reasons: [...d.reasons].sort() },
      translationPolicy: d.policy,
      mapping: { offsets: d.offsets, textNodes: d.textNodes },
      roles: [...d.roles],
    });
  }
  return { pageIndex: input.pageIndex, blocks, diagnostics };
}
