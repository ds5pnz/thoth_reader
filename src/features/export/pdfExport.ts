// PDF export. Pages are rasterised here one at a time and streamed to Rust, which writes the
// file: a whole book never sits in memory.
//  - side by side (PDF sources): the original page next to the same page with translated text
//    laid over each paragraph, like the reader's parallel view;
//  - typeset (EPUB, or any source): the translation set as a plain A4 document.
import type { PDFDocumentProxy, PDFPageProxy } from 'pdfjs-dist';
import { call } from '../../ipc/client';
import type { Block } from '../../ipc/generated/Block';
import type { BlockDisplay } from '../../ipc/generated/BlockDisplay';
import type { BlocksPage } from '../../ipc/generated/BlocksPage';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import type { SectionRow } from '../../ipc/generated/SectionRow';
import { blockBox, expandBox, harmonizeFontSizes, pickBackground, resolveVerticalOverlaps, textColorFor, type BlockBox, type ItemSize, type Rgb } from '../../document/pdf/translatedPage';
import { openSessionPdf, releaseSessionPdf } from '../../document/pdf/sessionPdf';
import { draw, fit, runsOf, wrap, EXPORT_FONT, type Run } from './canvasText';

export interface ExportProgress { done: number; total: number }
export interface ExportOptions { onProgress: (p: ExportProgress) => void; cancelled: () => boolean; bilingual?: boolean }
export interface ExportResult { saved: boolean; pages: number; untranslated: number }

const NON_BODY = ['running_header', 'running_footer', 'page_number'];
const translatable = (b: Block) => b.translationPolicy === 'auto' && /\p{L}/u.test(b.sourceText);

async function sectionContent(sessionId: string, sectionId: string): Promise<{ blocks: Block[]; display: Map<string, BlockDisplay> }> {
  const blocks: Block[] = [];
  const display = new Map<string, BlockDisplay>();
  let cursor: number | null = null;
  for (let i = 0; i < 50; i++) {
    const page: BlocksPage = await call<BlocksPage>('blocks_get', { sessionId, sectionId, cursor, limit: 1000 });
    blocks.push(...page.blocks);
    for (const d of page.display) display.set(d.blockId, d);
    if (page.nextCursor == null) break;
    cursor = page.nextCursor;
  }
  return { blocks, display };
}

function jpegBase64(canvas: HTMLCanvasElement, quality: number): string {
  return canvas.toDataURL('image/jpeg', quality).replace(/^data:[^,]*,/, '');
}

async function begin(session: ReaderSessionDto, variant: 'side-by-side' | 'translation'): Promise<string | null> {
  return (await call<{ token: string | null }>('export_pdf_begin', { sessionId: session.sessionId, variant })).token;
}

async function finish(token: string, abort: boolean): Promise<{ saved: boolean; pages?: number }> {
  return call<{ saved: boolean; pages?: number }>('export_pdf_end', { token, abort }).catch(() => ({ saved: false }));
}

// ------------------------------------------------------------------ side by side

const SBS_SCALE = 1.6; // canvas px per PDF point (~115 dpi)
const SBS_GAP = 14; // points between the two pages
const MIN_RATIO = 0.6; // below this share of the original size the original text is kept

/** Draw the translated twin of a page onto `target` (already holding the original raster). */
export function paintTranslatedPage(ctx: CanvasRenderingContext2D, viewport: { width: number; height: number; scale: number } & Parameters<typeof blockBox>[1], blocks: readonly Block[], display: ReadonlyMap<string, BlockDisplay>, items: ItemSize[]): { drawn: number; kept: number } {
  const body = blocks.filter((b) => !b.roles.some((r) => NON_BODY.includes(r)) && b.sourceText.trim().length > 0);
  const boxes = resolveVerticalOverlaps(body.map((b) => ({ b, id: b.id, box: blockBox(b, viewport, items) })).filter((x): x is { b: Block; id: string; box: BlockBox } => !!x.box));
  if (boxes.length === 0) return { drawn: 0, kept: 0 };
  const fonts = harmonizeFontSizes(boxes);
  const img = ctx.getImageData(0, 0, ctx.canvas.width, ctx.canvas.height);
  const at = (x: number, y: number): Rgb => {
    const xi = Math.min(img.width - 1, Math.max(0, Math.round(x)));
    const yi = Math.min(img.height - 1, Math.max(0, Math.round(y)));
    const i = (yi * img.width + xi) * 4;
    return [img.data[i]!, img.data[i + 1]!, img.data[i + 2]!];
  };
  const all = boxes.map((x) => x.box);
  const pageRight = Math.min(viewport.width - 4, Math.max(...all.map((x) => x.left + x.width), 0));
  // Decide every block first (sizes come from the untouched raster), then paint.
  const plans: { box: BlockBox; bg: Rgb; laid: NonNullable<ReturnType<typeof fit>>; lh: number; w: number; h: number }[] = [];
  let kept = 0;
  for (const { b, box } of boxes) {
    const shown = display.get(b.id)?.shown;
    if (!shown?.text.trim()) { if (translatable(b)) kept++; continue; }
    const pts: Rgb[] = [];
    for (const [fx, fy] of [[0, 0], [1, 0], [0, 1], [1, 1], [0.5, 0], [0.5, 1], [0, 0.5], [1, 0.5]] as const) pts.push(at(box.left - 2 + fx * (box.width + 4), box.top - 2 + fy * (box.height + 4)));
    const bg = pickBackground(pts);
    const grown = expandBox(img, 1, box, bg, all, pageRight);
    const base = fonts.get(b.id) ?? box.fontSize;
    const lh = box.pitch > 0 ? Math.max(1.05, Math.min(1.9, box.pitch / base)) : 1.25;
    const heading = b.kind === 'heading';
    const runs: Run[] = runsOf(shown.text, shown.marks, heading ? { bold: true } : {});
    const laid = fit(ctx, runs, grown.width + 2, grown.height + 2, base, lh, Math.max(4.5 * viewport.scale, base * MIN_RATIO));
    if (!laid) { kept++; continue; } // too small to read: the original stays, as on screen
    plans.push({ box, bg, laid, lh, w: grown.width + 2, h: grown.height + 2 });
  }
  // Largest first, so a small label is never covered by a big paragraph's background.
  plans.sort((a, b) => b.box.width * b.box.height - a.box.width * a.box.height);
  for (const p of plans) {
    const textH = p.laid.lines.length * p.laid.px * p.lh;
    const textW = Math.max(...p.laid.lines.map((l) => l.width), 0);
    ctx.fillStyle = `rgb(${p.bg[0]},${p.bg[1]},${p.bg[2]})`;
    ctx.fillRect(p.box.left - 1, p.box.top - 1, Math.max(p.box.width + 2, textW + 1), Math.max(p.box.height + 2, textH));
    draw(ctx, p.laid.lines, p.box.left, p.box.top - 1, p.laid.px, p.lh, textColorFor(p.bg));
  }
  return { drawn: plans.length, kept };
}

async function renderPage(page: PDFPageProxy, scale: number): Promise<{ canvas: HTMLCanvasElement; ctx: CanvasRenderingContext2D; viewport: ReturnType<PDFPageProxy['getViewport']> }> {
  const viewport = page.getViewport({ scale });
  const canvas = document.createElement('canvas');
  canvas.width = Math.floor(viewport.width);
  canvas.height = Math.floor(viewport.height);
  const ctx = canvas.getContext('2d', { willReadFrequently: true })!;
  ctx.fillStyle = '#fff';
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  await page.render({ canvasContext: ctx, viewport, canvas } as Parameters<PDFPageProxy['render']>[0]).promise;
  return { canvas, ctx, viewport };
}

/** Original | translated, one sheet per source page. */
export async function exportSideBySide(session: ReaderSessionDto, sections: readonly SectionRow[], opts: ExportOptions): Promise<ExportResult> {
  const token = await begin(session, 'side-by-side');
  if (!token) return { saved: false, pages: 0, untranslated: 0 };
  let doc: PDFDocumentProxy | null = null;
  let untranslated = 0;
  let ok = false;
  try {
    doc = await openSessionPdf(session);
    const ordered = [...sections].sort((a, b) => Number(a.ordinal) - Number(b.ordinal));
    for (let n = 0; n < ordered.length; n++) {
      if (opts.cancelled()) return { saved: false, pages: 0, untranslated };
      const sec = ordered[n]!;
      const index = Number(sec.ordinal);
      if (index < 0 || index >= doc.numPages) continue;
      const page = await doc.getPage(index + 1);
      try {
        const { canvas, ctx, viewport } = await renderPage(page, SBS_SCALE);
        const original = jpegBase64(canvas, 0.82);
        if (sec.state === 'ready') {
          const tc = await page.getTextContent();
          const sizes: ItemSize[] = [];
          for (const it of tc.items) if ('str' in it) sizes.push({ h: it.height, n: it.str.length });
          const { blocks, display } = await sectionContent(session.sessionId, sec.id);
          untranslated += paintTranslatedPage(ctx, viewport, blocks, display, sizes).kept;
        }
        const translated = jpegBase64(canvas, 0.82);
        const w = viewport.width / SBS_SCALE;
        const h = viewport.height / SBS_SCALE;
        await call('export_pdf_page', { token, width: w * 2 + SBS_GAP, height: h, images: [{ x: 0, y: 0, w, h, jpegBase64: original }, { x: w + SBS_GAP, y: 0, w, h, jpegBase64: translated }] });
        canvas.width = 0;
        canvas.height = 0;
      } finally {
        page.cleanup();
      }
      opts.onProgress({ done: n + 1, total: ordered.length });
      await new Promise((r) => setTimeout(r, 0)); // keep the UI alive
    }
    ok = true;
  } finally {
    if (doc) releaseSessionPdf(session);
    if (!ok) await finish(token, true);
  }
  const end = await finish(token, false);
  return { saved: end.saved, pages: end.pages ?? 0, untranslated };
}

// ------------------------------------------------------------------ typeset document

const A4 = { w: 595, h: 842 };
const TS_SCALE = 2;
const MARGIN = { x: 62, top: 64, bottom: 66 };

interface Para { runs: Run[]; px: number; lh: number; before: number; after: number; indent: number; color: string; mono?: boolean; bullet?: boolean }

function sourceRuns(b: Block): Run[] {
  return [{ text: b.sourceText, bold: false, italic: false }];
}

function parasOf(b: Block, d: BlockDisplay | undefined, bilingual: boolean): Para[] {
  if (b.roles.some((r) => NON_BODY.includes(r))) return [];
  const shown = d?.shown?.text.trim() ? d.shown : null;
  const out: Para[] = [];
  const src = (px: number, indent = 0): Para => ({ runs: sourceRuns(b), px, lh: 1.45, before: 0, after: 3, indent, color: '#8a9099' });
  if (b.kind === 'heading') {
    const level = Number(b.roles.find((r) => /^h[1-6]$/.test(r))?.slice(1) ?? 2);
    const px = level <= 1 ? 20 : level === 2 ? 16.5 : 14;
    out.push({ runs: shown ? runsOf(shown.text, shown.marks, { bold: true }) : [{ text: b.sourceText, bold: true, italic: false }], px, lh: 1.3, before: px * 1.3, after: px * 0.5, indent: 0, color: '#16181c' });
    if (bilingual && shown) out.push({ ...src(9.5), after: 8 });
    return out;
  }
  if (b.kind === 'code') return [{ runs: [{ text: b.rawText, bold: false, italic: false }], px: 8.6, lh: 1.4, before: 4, after: 10, indent: 10, color: '#2a2d33', mono: true }];
  if (b.kind === 'table') return [{ runs: [{ text: b.rawText.replace(/\t/g, '   '), bold: false, italic: false }], px: 9, lh: 1.45, before: 4, after: 10, indent: 0, color: '#2a2d33' }];
  if (!b.sourceText.trim()) return [];
  const indent = b.kind === 'list_item' ? 14 : b.kind === 'quote' ? 16 : 0;
  if (bilingual && shown) out.push(src(9, indent));
  out.push({
    runs: shown ? runsOf(shown.text, shown.marks, b.kind === 'caption' ? { italic: true } : {}) : sourceRuns(b),
    px: b.kind === 'caption' ? 9.5 : 11,
    lh: 1.62,
    before: 0,
    after: 8,
    indent,
    color: shown ? '#1f2328' : '#7a808a',
    bullet: b.kind === 'list_item' && !/^[•\-–·*\d(]/.test((shown?.text ?? b.sourceText).trimStart()),
  });
  return out;
}

/** The translation as a plain A4 document (text only; figures are not reproduced). */
export async function exportTypeset(session: ReaderSessionDto, sections: readonly SectionRow[], opts: ExportOptions): Promise<ExportResult> {
  const token = await begin(session, 'translation');
  if (!token) return { saved: false, pages: 0, untranslated: 0 };
  const canvas = document.createElement('canvas');
  canvas.width = A4.w * TS_SCALE;
  canvas.height = A4.h * TS_SCALE;
  const ctx = canvas.getContext('2d')!;
  const width = A4.w - MARGIN.x * 2;
  let y = MARGIN.top;
  let pageNo = 0;
  let dirty = false;
  let untranslated = 0;
  let ok = false;
  const clear = () => {
    ctx.setTransform(TS_SCALE, 0, 0, TS_SCALE, 0, 0);
    ctx.fillStyle = '#fff';
    ctx.fillRect(0, 0, A4.w, A4.h);
    y = MARGIN.top;
    dirty = false;
  };
  const flush = async () => {
    if (!dirty) return;
    pageNo += 1;
    ctx.font = `400 8.5px ${EXPORT_FONT}`;
    ctx.fillStyle = '#9aa0a8';
    ctx.textAlign = 'center';
    ctx.fillText(String(pageNo), A4.w / 2, A4.h - 34);
    ctx.textAlign = 'left';
    await call('export_pdf_page', { token, width: A4.w, height: A4.h, images: [{ x: 0, y: 0, w: A4.w, h: A4.h, jpegBase64: jpegBase64(canvas, 0.86) }] });
    clear();
  };
  clear();
  try {
    const ordered = [...sections].filter((s) => s.state === 'ready').sort((a, b) => Number(a.ordinal) - Number(b.ordinal));
    for (let n = 0; n < ordered.length; n++) {
      if (opts.cancelled()) return { saved: false, pages: 0, untranslated };
      const { blocks, display } = await sectionContent(session.sessionId, ordered[n]!.id);
      const parents = new Set(blocks.map((b) => b.parentId).filter(Boolean));
      for (const b of blocks) {
        if (parents.has(b.id)) continue; // containers repeat their children
        const d = display.get(b.id);
        if (translatable(b) && !d?.shown?.text.trim()) untranslated++;
        for (const p of parasOf(b, d, !!opts.bilingual)) {
          const family = p.mono ? 'ui-monospace, Menlo, monospace' : EXPORT_FONT;
          const lines = wrap(ctx, p.runs, width - p.indent, p.px, family);
          y += p.before;
          let i = 0;
          while (i < lines.length) {
            const lineH = p.px * p.lh;
            // keep a heading with at least a little of what follows
            const room = Math.floor((A4.h - MARGIN.bottom - y) / lineH);
            if (room < 1 || (i === 0 && p.px > 13 && room < 3)) { await flush(); continue; }
            const take = Math.min(room, lines.length - i);
            if (i === 0 && p.bullet) { ctx.font = `400 ${p.px}px ${family}`; ctx.fillStyle = p.color; ctx.fillText('•', MARGIN.x + p.indent - 10, y + (lineH - p.px) / 2 + p.px * 0.82); }
            draw(ctx, lines.slice(i, i + take), MARGIN.x + p.indent, y, p.px, p.lh, p.color, family);
            dirty = true;
            y += take * lineH;
            i += take;
            if (i < lines.length) await flush();
          }
          y += p.after;
        }
      }
      opts.onProgress({ done: n + 1, total: ordered.length });
      await new Promise((r) => setTimeout(r, 0));
    }
    await flush();
    ok = true;
  } finally {
    canvas.width = 0;
    canvas.height = 0;
    if (!ok) await finish(token, true);
  }
  const end = await finish(token, false);
  return { saved: end.saved, pages: end.pages ?? 0, untranslated };
}
