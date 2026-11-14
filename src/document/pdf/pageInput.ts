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
export function analyzeOperatorList(fnArray: ArrayLike<number>, argsArray: ArrayLike<unknown>): { imageCount: number; imageRects: PdfImageRect[] } {
  let ctm: Matrix = IDENTITY;
  const stack: Matrix[] = [];
  let imageCount = 0;
  const imageRects: PdfImageRect[] = [];
  for (let i = 0; i < fnArray.length; i++) {
    const fn = fnArray[i];
    const args = argsArray[i] as unknown[] | null | undefined;
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
  return { imageCount, imageRects };
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
  if (opts.operatorList !== false) {
    // The operator list also makes PDF.js resolve the page fonts into commonObjs.
    const ops = await page.getOperatorList();
    ({ imageCount, imageRects } = analyzeOperatorList(ops.fnArray, ops.argsArray));
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
    structTree,
  };
}
