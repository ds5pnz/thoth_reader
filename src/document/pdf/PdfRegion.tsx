// A rectangle of a PDF page shown as a picture (figures in the reflowed text).
// The region is rendered from the page itself, so photos, diagrams and anything drawn over
// them look exactly as they do on the page.
import { useEffect, useState } from 'react';
import type { PDFPageProxy } from 'pdfjs-dist';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import type { Rect } from '../../ipc/generated/Rect';
import { pdfRectToCss } from './coords';
import { openSessionPdf, releaseSessionPdf } from './sessionPdf';

/** CSS pixels per PDF point at natural size (96 / 72). */
const CSS_PER_PT = 96 / 72;
/** Rendered pixels per point: sharp on a 2x display at natural size. */
const RENDER_PER_PT = 2 * CSS_PER_PT;
const MAX_RENDER_SIDE = 2400;
const MAX_CACHED = 48;

/** Rendered regions, most recently used last. Rows are virtualized and remount often. */
const cache = new Map<string, Promise<string | null>>();

export function regionKey(token: string, pageIndex: number, r: Rect): string {
  return `${token}|${pageIndex}|${Math.round(r.x * 10)},${Math.round(r.y * 10)},${Math.round(r.width * 10)},${Math.round(r.height * 10)}`;
}

/** Scale that renders `rect` at RENDER_PER_PT without exceeding the size cap. */
export function regionScale(rect: Pick<Rect, 'width' | 'height'>): number {
  const longest = Math.max(rect.width, rect.height, 1);
  return Math.min(RENDER_PER_PT, MAX_RENDER_SIDE / longest);
}

async function render(session: ReaderSessionDto, pageIndex: number, rect: Rect): Promise<string | null> {
  const doc = await openSessionPdf(session);
  try {
    const page: PDFPageProxy = await doc.getPage(pageIndex + 1);
    const viewport = page.getViewport({ scale: regionScale(rect) });
    // user space → viewport (handles the page's rotation and flipped y axis)
    const css = pdfRectToCss(rect, viewport);
    const left = css.left;
    const top = css.top;
    const w = Math.max(1, Math.round(css.width));
    const h = Math.max(1, Math.round(css.height));
    const canvas = document.createElement('canvas');
    canvas.width = w;
    canvas.height = h;
    const ctx = canvas.getContext('2d')!;
    ctx.fillStyle = '#fff';
    ctx.fillRect(0, 0, w, h);
    await page.render({ canvasContext: ctx, viewport, transform: [1, 0, 0, 1, -left, -top], canvas } as Parameters<PDFPageProxy['render']>[0]).promise;
    const url = canvas.toDataURL('image/png');
    canvas.width = 0;
    canvas.height = 0;
    page.cleanup();
    return url;
  } catch {
    return null;
  } finally {
    releaseSessionPdf(session);
  }
}

function regionUrl(session: ReaderSessionDto, pageIndex: number, rect: Rect): Promise<string | null> {
  const key = regionKey(session.assetToken, pageIndex, rect);
  const hit = cache.get(key);
  if (hit) {
    cache.delete(key);
    cache.set(key, hit);
    return hit;
  }
  const p = render(session, pageIndex, rect);
  cache.set(key, p);
  void p.then((u) => { if (u === null) cache.delete(key); });
  while (cache.size > MAX_CACHED) cache.delete(cache.keys().next().value as string);
  return p;
}

export function PdfRegionImage({ session, pageIndex, rect, alt = '' }: { session: ReaderSessionDto; pageIndex: number; rect: Rect; alt?: string }) {
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const key = regionKey(session.assetToken, pageIndex, rect);
  useEffect(() => {
    let live = true;
    setUrl(null);
    setFailed(false);
    void regionUrl(session, pageIndex, rect).then((u) => { if (live) { setUrl(u); setFailed(u === null); } });
    return () => { live = false; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
  const width = Math.round(rect.width * CSS_PER_PT);
  // The box keeps the picture's shape while it renders, so the text below does not jump.
  const style = { width, maxWidth: '100%', aspectRatio: `${rect.width} / ${rect.height}` };
  if (failed) return <div className="pdf-region failed faint" style={style}>그림을 표시할 수 없습니다</div>;
  return url
    ? <img className="pdf-region" src={url} alt={alt} style={style} draggable={false} />
    : <div className="pdf-region loading" style={style} aria-hidden="true" />;
}
