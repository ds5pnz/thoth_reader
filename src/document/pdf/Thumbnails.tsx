// Page thumbnails for the sidebar: rendered lazily, one at a time, only for rows that have
// been scrolled into view; kept as small JPEG object URLs.
import { useEffect, useRef, useState } from 'react';
import type { PDFDocumentProxy, PDFPageProxy } from 'pdfjs-dist';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import { openSessionPdf, releaseSessionPdf } from './sessionPdf';

const THUMB_W = 132;

export function PdfThumbnails({ session, currentPage, onPage }: { session: ReaderSessionDto; currentPage: () => number; onPage: (index: number) => void }) {
  const [doc, setDoc] = useState<PDFDocumentProxy | null>(null);
  const [ratio, setRatio] = useState(1.3);
  const [urls, setUrls] = useState<Map<number, string>>(new Map());
  const [current, setCurrent] = useState(0);
  const listRef = useRef<HTMLUListElement>(null);
  const wanted = useRef(new Set<number>());
  const done = useRef(new Set<number>());
  const made = useRef<string[]>([]);

  useEffect(() => {
    let cancelled = false;
    void openSessionPdf(session).then(async (d) => {
      if (cancelled) return;
      const p = await d.getPage(1);
      const vp = p.getViewport({ scale: 1 });
      if (!cancelled) { setRatio(vp.height / vp.width); setDoc(d); }
    }).catch(() => {});
    return () => {
      cancelled = true;
      releaseSessionPdf(session);
      for (const u of made.current) URL.revokeObjectURL(u);
      made.current = [];
    };
  }, [session.assetToken]); // eslint-disable-line react-hooks/exhaustive-deps

  // Follow the reader's current page.
  const cp = useRef(currentPage);
  cp.current = currentPage;
  useEffect(() => {
    const t = window.setInterval(() => setCurrent((c) => { const n = cp.current(); return n === c ? c : n; }), 500);
    return () => window.clearInterval(t);
  }, []);
  useEffect(() => {
    listRef.current?.querySelector<HTMLElement>(`[data-thumb="${current}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [current, doc]);

  // Render what is visible, one page at a time.
  useEffect(() => {
    const list = listRef.current;
    if (!doc || !list) return;
    let stopped = false;
    let running = false;
    const pump = async () => {
      if (running) return;
      running = true;
      while (!stopped) {
        const next = [...wanted.current].find((i) => !done.current.has(i));
        if (next == null) break;
        done.current.add(next);
        let page: PDFPageProxy | null = null;
        try {
          page = await doc.getPage(next + 1);
          const base = page.getViewport({ scale: 1 });
          const vp = page.getViewport({ scale: (THUMB_W * Math.min(2, window.devicePixelRatio || 1)) / base.width });
          const canvas = document.createElement('canvas');
          canvas.width = Math.floor(vp.width);
          canvas.height = Math.floor(vp.height);
          const ctx = canvas.getContext('2d');
          if (ctx) {
            ctx.fillStyle = '#fff';
            ctx.fillRect(0, 0, canvas.width, canvas.height);
            await page.render({ canvasContext: ctx, viewport: vp, canvas } as Parameters<PDFPageProxy['render']>[0]).promise;
            const blob = await new Promise<Blob | null>((r) => canvas.toBlob(r, 'image/jpeg', 0.8));
            if (blob && !stopped) {
              const url = URL.createObjectURL(blob);
              made.current.push(url);
              setUrls((prev) => new Map(prev).set(next, url));
            }
          }
          canvas.width = 0;
          canvas.height = 0;
        } catch {
          // damaged page: leave the placeholder
        } finally {
          page?.cleanup();
        }
      }
      running = false;
    };
    const io = new IntersectionObserver((entries) => {
      for (const e of entries) {
        const i = Number((e.target as HTMLElement).dataset.thumb);
        if (e.isIntersecting) wanted.current.add(i); else if (!done.current.has(i)) wanted.current.delete(i);
      }
      void pump();
    }, { root: list.closest('.side-body') ?? null, rootMargin: '300px 0px' });
    list.querySelectorAll('[data-thumb]').forEach((n) => io.observe(n));
    return () => { stopped = true; io.disconnect(); };
  }, [doc]);

  if (!doc) return <p className="muted side-empty">쪽 미리보기를 준비하는 중…</p>;
  return (
    <ul ref={listRef} className="thumbs" role="list">
      {Array.from({ length: doc.numPages }, (_, i) => (
        <li key={i}>
          <button data-thumb={i} className={`thumb ${i === current ? 'current' : ''}`} aria-current={i === current ? 'page' : undefined} onClick={() => onPage(i)} aria-label={`${i + 1}쪽으로 이동`}>
            <span className="thumb-img" style={{ width: THUMB_W, height: Math.round(THUMB_W * ratio) }}>
              {urls.has(i) && <img src={urls.get(i)} alt="" draggable={false} />}
            </span>
            <span className="thumb-no">{i + 1}</span>
          </button>
        </li>
      ))}
    </ul>
  );
}
