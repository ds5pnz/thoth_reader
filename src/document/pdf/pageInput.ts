/**
 * Builds the structured-clone-friendly extractor input from a PDF.js page.
 * Runs on the thread that owns the PDFDocumentProxy (main thread in the app);
 * the heavy layout work then happens in extract.worker.ts.
 *
 * Works with both `pdfjs-dist/build/pdf.mjs` and the legacy build (tests).
 */
import type { PDFPageProxy } from 'pdfjs-dist';
import type { PdfImageRect, PdfPageInput, PdfStructNodeLike, PdfTextItemLike, PdfTextStyleLike } from './types';

/** PDF.js `OPS` codes (stable for pdfjs-dist 6.5.x; asserted in tests against the real `OPS`). */
export const PDF_OPS = {
  save: 10,
  restore: 11,
  transform: 12,
  setTextRenderingMode: 38,
  paintFormXObjectBegin: 74,
  paintFormXObjectEnd: 75,
  paintImageMaskXObject: 83,
  paintImageMaskXObjectGroup: 84,
  paintImageXObject: 85,
  paintInlineImageXObject: 86,
  paintInlineImageXObjectGroup: 87,
  paintImageXObjectRepeat: 88,
  paintImageMaskXObjectRepeat: 89,
  paintSolidColorImageMask: 90,
} as const;

const SINGLE_IMAGE_OPS = new Set<number>([
  PDF_OPS.paintImageXObject,
  PDF_OPS.paintInlineImageXObject,
  PDF_OPS.paintImageMaskXObject,
]);
const OTHER_IMAGE_OPS = new Set<number>([
  PDF_OPS.paintImageMaskXObjectGroup,
  PDF_OPS.paintInlineImageXObjectGroup,
  PDF_OPS.paintImageXObjectRepeat,
  PDF_OPS.paintImageMaskXObjectRepeat,
  PDF_OPS.paintSolidColorImageMask,
]);

export interface PageInputOptions {
  /** Read the struct tree (tagged PDF). Default false (it can be slow on big documents). */
  structTree?: boolean;
  /** Analyse the operator list for images (scan detection). Default true. */
  operatorList?: boolean;
}

type Matrix = [number, number, number, number, number, number];
const IDENTITY: Matrix = [1, 0, 0, 1, 0, 0];

function mul(m: Matrix, n: Matrix): Matrix {
  // n applied first, then m (PDF `cm` semantics: CTM' = n × CTM)
  return [
    n[0] * m[0] + n[1] * m[2],
    n[0] * m[1] + n[1] * m[3],
    n[2] * m[0] + n[3] * m[2],
    n[2] * m[1] + n[3] * m[3],
    n[4] * m[0] + n[5] * m[2] + m[4],
    n[4] * m[1] + n[5] * m[3] + m[5],
  ];
}

function asMatrix(v: unknown): Matrix | null {
  if (!v || typeof (v as ArrayLike<number>).length !== 'number' || (v as ArrayLike<number>).length < 6) return null;
  const a = Array.from(v as ArrayLike<number>).slice(0, 6);
  return a.every((x) => Number.isFinite(x)) ? (a as Matrix) : null;
}

/** Count image painting operators and compute unit-square placements under the CTM. */
const TEXT_OPS = new Set([44, 45, 46, 47]); // showText, showSpacedText, nextLineShowText, nextLineSetSpacingShowText
const OP_SET_FILL_RGB = 59;
const OP_SET_FILL_OTHER = new Set([54, 55, 57, 61, 93]); // setFillColor(N), gray, CMYK, transparent

export interface PaintedText { text: string; color: string | null }

function hexColor(args: unknown): string | null {
  const a = Array.isArray(args) ? args : [];
  if (typeof a[0] === 'string' && /^#[0-9a-f]{6}$/i.test(a[0])) return a[0].toLowerCase();
  if (a.length >= 3 && a.slice(0, 3).every((v) => typeof v === 'number' && Number.isFinite(v))) {
    return '#' + (a.slice(0, 3) as number[]).map((v) => Math.max(0, Math.min(255, Math.round(v))).toString(16).padStart(2, '0')).join('');
  }
  return null;
}

/** Text painting operators in order, each with the fill colour in effect. */
export function paintedText(fnArray: ArrayLike<number>, argsArray: ArrayLike<unknown>): PaintedText[] {
  const out: PaintedText[] = [];
  let color: string | null = '#000000';
  const stack: (string | null)[] = [];
  for (let i = 0; i < fnArray.length; i++) {
    const fn = fnArray[i]!;
    const args = argsArray[i] as unknown[] | null | undefined;
    if (fn === PDF_OPS.save) stack.push(color);
    else if (fn === PDF_OPS.restore) color = stack.length ? stack.pop()! : color;
    else if (fn === OP_SET_FILL_RGB) color = hexColor(args);
    else if (OP_SET_FILL_OTHER.has(fn)) color = null; // pattern / unknown space: do not guess
    else if (TEXT_OPS.has(fn)) {
      const glyphs = Array.isArray(args) ? (Array.isArray(args[0]) ? args[0] : Array.isArray(args[args.length - 1]) ? args[args.length - 1] : null) : null;
      if (!Array.isArray(glyphs)) continue;
      let text = '';
      for (const g of glyphs as unknown[]) {
        const u = (g as { unicode?: unknown } | null)?.unicode;
        if (typeof u === 'string') text += u;
      }
      if (text) out.push({ text, color });
    }
  }
  return out;
}

const INK = (s: string) => s.normalize('NFKC').replace(/\s+/gu, '');

/**
 * Give each text item the colour it was painted with by walking the painted glyphs and the
 * extracted items in parallel (both are in content-stream order). When the two streams do
 * not line up well enough the page gets no colours at all rather than wrong ones.
 */
export function assignItemColors(items: { str: string; color?: string | null; colorRuns?: { start: number; end: number; color: string }[] }[], painted: readonly PaintedText[]): boolean {
  const pc: string[] = [];
  const cols: (string | null)[] = [];
  for (const p of painted) for (const ch of INK(p.text)) { pc.push(ch); cols.push(p.color); }
  // Item ink characters, each remembering its item and its UTF-16 position in `str`.
  const ic: string[] = [];
  const owner: number[] = [];
  const at: number[] = [];
  items.forEach((it, idx) => {
    let pos = 0;
    for (const cp of it.str) {
      for (const ch of INK(cp)) { ic.push(ch); owner.push(idx); at.push(pos); }
      pos += cp.length;
    }
  });
  if (ic.length === 0) return false;
  const got: (string | null | undefined)[] = new Array(ic.length);
  // Both streams are in content order but differ in places (glyphs the extractor drops or
  // adds, hyphens, ligatures). Never jump on a single matching letter: that walks ahead of
  // the real position and every later colour lands too early. Resynchronise only where the
  // next few characters agree, skipping whichever side needs the shorter skip.
  const GRAM = 5;
  const gram = (arr: string[], p: number) => (p + GRAM <= arr.length ? arr.slice(p, p + GRAM).join('') : null);
  const index = (arr: string[]) => {
    const m = new Map<string, number[]>();
    for (let p = 0; p + GRAM <= arr.length; p++) {
      const g = arr.slice(p, p + GRAM).join('');
      const l = m.get(g);
      if (l) l.push(p); else m.set(g, [p]);
    }
    return m;
  };
  const pcIndex = index(pc);
  const icIndex = index(ic);
  // first occurrence of `g` at or after `from` (positions are ascending)
  const next = (m: Map<string, number[]>, g: string | null, from: number) => {
    const l = g ? m.get(g) : undefined;
    if (!l) return -1;
    let lo = 0; let hi = l.length;
    while (lo < hi) { const mid = (lo + hi) >> 1; if (l[mid]! < from) lo = mid + 1; else hi = mid; }
    return lo < l.length ? l[lo]! : -1;
  };
  let i = 0;
  let k = 0;
  let matched = 0;
  while (i < ic.length && k < pc.length) {
    if (ic[i] === pc[k]) {
      got[i] = cols[k];
      matched++; i++; k++;
      continue;
    }
    // Where does the text at `i` continue in the paint stream, and where does the paint at
    // `k` continue in the text? Take the shorter skip (a block may be missing on either side).
    const kTo = next(pcIndex, gram(ic, i), k + 1);
    const iTo = next(icIndex, gram(pc, k), i + 1);
    if (kTo >= 0 && (iTo < 0 || kTo - k <= iTo - i)) k = kTo;
    else if (iTo >= 0) i = iTo;
    else { i++; k++; } // a substituted character
  }
  if (matched / ic.length < 0.9) return false;
  // Per item: runs of one colour (bridging whitespace and unmatched characters), plus the
  // majority colour.
  const runs: { start: number; end: number; color: string }[][] = items.map(() => []);
  for (let j = 0; j < ic.length; j++) {
    const c = got[j];
    if (!c) continue;
    const list = runs[owner[j]!]!;
    const last = list[list.length - 1];
    const end = at[j]! + ic[j]!.length;
    if (last && last.color === c) last.end = Math.max(last.end, end);
    else {
      if (last) last.end = Math.min(last.end, at[j]!); // a bridged run stops where the next begins
      list.push({ start: at[j]!, end, color: c });
    }
  }
  items.forEach((it, idx) => {
    const list = runs[idx]!;
    const weight = new Map<string, number>();
    for (const r of list) weight.set(r.color, (weight.get(r.color) ?? 0) + (r.end - r.start));
    let best: string | null = null;
    let n = 0;
    for (const [c, v] of weight) if (v > n) { best = c; n = v; }
    it.color = best;
    it.colorRuns = list;
  });
  return true;
}

export function analyzeOperatorList(fnArray: ArrayLike<number>, argsArray: ArrayLike<unknown>): { imageCount: number; imageRects: PdfImageRect[]; invisibleText: boolean } {
  let invisibleText = false;
  let ctm: Matrix = IDENTITY;
  const stack: Matrix[] = [];
  let imageCount = 0;
  const imageRects: PdfImageRect[] = [];
  for (let i = 0; i < fnArray.length; i++) {
    const fn = fnArray[i];
    const args = argsArray[i] as unknown[] | null | undefined;
    if (fn === PDF_OPS.setTextRenderingMode && Array.isArray(args) && args[0] === 3) invisibleText = true;
    switch (fn) {
      case PDF_OPS.save:
        stack.push(ctm);
        break;
      case PDF_OPS.restore:
        ctm = stack.pop() ?? IDENTITY;
        break;
      case PDF_OPS.transform: {
        const m = asMatrix(args);
        if (m) ctm = mul(ctm, m);
        break;
      }
      case PDF_OPS.paintFormXObjectBegin: {
        stack.push(ctm);
        const m = asMatrix(args?.[0]);
        if (m) ctm = mul(ctm, m);
        break;
      }
      case PDF_OPS.paintFormXObjectEnd:
        ctm = stack.pop() ?? IDENTITY;
        break;
      default:
        if (fn !== undefined && SINGLE_IMAGE_OPS.has(fn)) {
          imageCount++;
          const pts = [
            [ctm[4], ctm[5]],
            [ctm[0] + ctm[4], ctm[1] + ctm[5]],
            [ctm[2] + ctm[4], ctm[3] + ctm[5]],
            [ctm[0] + ctm[2] + ctm[4], ctm[1] + ctm[3] + ctm[5]],
          ] as const;
          const xs = pts.map((p) => p[0]);
          const ys = pts.map((p) => p[1]);
          const x = Math.min(...xs);
          const y = Math.min(...ys);
          imageRects.push({ x, y, width: Math.max(...xs) - x, height: Math.max(...ys) - y });
        } else if (fn !== undefined && OTHER_IMAGE_OPS.has(fn)) {
          imageCount++;
        }
    }
  }
  return { imageCount, imageRects, invisibleText };
}

interface FontLike {
  name?: unknown;
  isMonospace?: unknown;
}

/**
 * Build the worker input for one page. Real font names come from
 * `page.commonObjs` and require the document to be opened with
 * `fontExtraProperties: true` (see pdfjs.ts); without them bold/monospace
 * detection falls back to PDF.js' generic `fontFamily`.
 */
export async function buildPageInput(page: PDFPageProxy, opts: PageInputOptions = {}): Promise<PdfPageInput> {
  const viewport = page.getViewport({ scale: 1 });
  let imageCount = 0;
  let imageRects: PdfImageRect[] = [];
  let invisibleText = false;
  let painted: PaintedText[] = [];
  if (opts.operatorList !== false) {
    // The operator list also makes PDF.js resolve the page fonts into commonObjs.
    const ops = await page.getOperatorList();
    ({ imageCount, imageRects, invisibleText } = analyzeOperatorList(ops.fnArray, ops.argsArray));
    painted = paintedText(ops.fnArray, ops.argsArray);
  }
  const tc = await page.getTextContent({ includeMarkedContent: false });
  const items: PdfTextItemLike[] = [];
  for (const it of tc.items) {
    if (!('str' in it)) continue; // marked content (not requested, defensive)
    items.push({
      str: it.str,
      dir: it.dir,
      transform: Array.from(it.transform as ArrayLike<number>),
      width: it.width,
      height: it.height,
      fontName: it.fontName,
      hasEOL: it.hasEOL,
    });
  }
  if (painted.length) assignItemColors(items, painted);
  const styles: Record<string, PdfTextStyleLike> = {};
  for (const [key, st] of Object.entries(tc.styles)) {
    let fontName: string | null = null;
    let isMonospace: boolean | null = null;
    try {
      if (page.commonObjs.has(key)) {
        const f = page.commonObjs.get(key) as FontLike | null;
        if (f && typeof f.name === 'string') fontName = f.name;
        if (f && typeof f.isMonospace === 'boolean') isMonospace = f.isMonospace;
      }
    } catch {
      // font not resolved: keep generic family
    }
    styles[key] = {
      fontFamily: st.fontFamily,
      ascent: st.ascent,
      descent: st.descent,
      vertical: st.vertical,
      fontName,
      isMonospace,
    };
  }
  let structTree: PdfStructNodeLike | null = null;
  if (opts.structTree) {
    try {
      structTree = (await page.getStructTree()) as unknown as PdfStructNodeLike | null;
    } catch {
      structTree = null;
    }
  }
  const rot = ((page.rotate % 360) + 360) % 360;
  return {
    pageIndex: page.pageNumber - 1,
    viewBox: Array.from(viewport.viewBox),
    rotation: rot,
    items,
    styles,
    imageCount,
    imageRects,
    invisibleText,
    structTree,
  };
}
