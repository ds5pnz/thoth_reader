// PDF original view (spec §1.1, §3.3, §3.4): rendered pages on the left (max ~5 canvases
// around the viewport), the translation panel on the right synced to the anchor block.
// The original page content is never modified; highlights are overlays from stored
// PDF-space rects converted with the PDF.js viewport.
import { forwardRef, useCallback, useEffect, useImperativeHandle, useMemo, useRef, useState } from 'react';
import type { PDFDocumentProxy, PDFPageProxy, PageViewport } from 'pdfjs-dist';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import type { Block } from '../../ipc/generated/Block';
import type { ReaderModel } from '../../features/reader/useReaderSession';
import { TranslationCell, type CellActions } from '../../features/translation/TranslationCell';
import { pdfRectToCss, cssPointToPdf, hitTest } from './coords';
import { isVisible, pickAnchor, ScrollOrigin, Debouncer, VISIBLE_DEBOUNCE_MS } from '../../domain/visibility';
import { openSessionPdf } from './sessionPdf';

export interface PdfOriginalHandle {
  getAnchor: () => { blockId: string; ratio: number } | null;
  scrollToBlock: (blockId: string, ratio?: number, highlight?: boolean) => boolean;
  scrollToSection: (sectionId: string) => void;
}

const MAX_LIVE_CANVASES = 5;
const SCALE = 1.35;

type PageMeta = { index: number; width: number; height: number; viewport: PageViewport };

export const PdfOriginalView = forwardRef<PdfOriginalHandle, {
  session: ReaderSessionDto;
  model: ReaderModel;
  loadSection: (id: string) => void;
  mode: 'parallel' | 'original';
  onVisible: (ids: string[], direction: 'forward' | 'backward') => void;
  onScrolled: () => void;
  actions: CellActions;
  autoActive: boolean;
  targetLanguage: string;
  syncScroll: boolean;
  highlightBlockId: string | null;
}>(function PdfOriginalView(props, ref) {
  const { session, model, loadSection, mode, onVisible, onScrolled, actions, autoActive, targetLanguage, syncScroll, highlightBlockId } = props;
  const [doc, setDoc] = useState<PDFDocumentProxy | null>(null);
  const [pages, setPages] = useState<PageMeta[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [live, setLive] = useState<Set<number>>(new Set([0]));
  const [anchorId, setAnchorId] = useState<string | null>(null);
  const [userScrollingPanel, setUserScrollingPanel] = useState(false);
  const pagesRef = useRef<HTMLDivElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const origin = useRef(new ScrollOrigin());
  const panelOrigin = useRef(new ScrollOrigin());
  const lastTop = useRef(0);
  const dir = useRef<'forward' | 'backward'>('forward');
  const deb = useRef(new Debouncer(VISIBLE_DEBOUNCE_MS));

  // Load the PDF via the thoth-doc protocol (Range requests, bundled worker, eval off).
  useEffect(() => {
    let cancelled = false;
    let opened: PDFDocumentProxy | null = null;
    openSessionPdf(session).then(async (d) => {
      if (cancelled) { void d.destroy(); return; }
      opened = d;
      const metas: PageMeta[] = [];
      for (let i = 0; i < d.numPages; i++) {
        // Page viewports are cheap; sizes are needed for layout before rendering.
        const p = await d.getPage(i + 1);
        const vp = p.getViewport({ scale: SCALE });
        metas.push({ index: i, width: vp.width, height: vp.height, viewport: vp });
        if (i > 2000) break;
      }
      if (!cancelled) { setDoc(d); setPages(metas); }
    }).catch((e: unknown) => setError(String((e as Error)?.message ?? e)));
    return () => { cancelled = true; void opened?.destroy(); };
  }, [session]);

  const sectionByPage = useMemo(() => {
    const m = new Map<number, string>();
    for (const s of model.sections) m.set(Number(s.ordinal), s.id);
    return m;
  }, [model.sections]);

  const blocksByPage = useMemo(() => {
    const m = new Map<number, Block[]>();
    for (const [, bs] of model.blocks) {
      for (const b of bs) {
        if (b.location.kind !== 'pdf') continue;
        const arr = m.get(b.location.pageIndex) ?? [];
        arr.push(b);
        m.set(b.location.pageIndex, arr);
      }
    }
    return m;
  }, [model.blocks]);

  const translatable = useCallback((b: Block) => !b.roles.includes('running_header') && !b.roles.includes('running_footer') && b.sourceText.trim().length > 0, []);

  // Which pages are near the viewport -> render canvases (≤5) and load their blocks.
  const updateLive = useCallback(() => {
    const el = pagesRef.current;
    if (!el || pages.length === 0) return;
    const vp = el.getBoundingClientRect();
    const near: { i: number; d: number }[] = [];
    el.querySelectorAll<HTMLElement>('[data-page]').forEach((n) => {
      const r = n.getBoundingClientRect();
      const d = r.bottom < vp.top ? vp.top - r.bottom : r.top > vp.bottom ? r.top - vp.bottom : 0;
      near.push({ i: Number(n.dataset.page), d });
    });
    near.sort((a, b) => a.d - b.d);
    const next = new Set(near.slice(0, MAX_LIVE_CANVASES).map((x) => x.i));
    setLive((prev) => (prev.size === next.size && [...next].every((x) => prev.has(x)) ? prev : next));
    for (const i of next) { const sid = sectionByPage.get(i); if (sid) loadSection(sid); }
  }, [pages.length, sectionByPage, loadSection]);

  // Visibility: viewport ∩ block rects (same rule as the reflow view).
  const report = useCallback(() => {
    const el = pagesRef.current;
    if (!el) return;
    const vp = el.getBoundingClientRect();
    const ids: string[] = [];
    const cands: { id: string; top: number; bottom: number }[] = [];
    for (const pm of pages) {
      if (!live.has(pm.index)) continue;
      const node = el.querySelector<HTMLElement>(`[data-page="${pm.index}"]`);
      if (!node) continue;
      const pr = node.getBoundingClientRect();
      for (const b of blocksByPage.get(pm.index) ?? []) {
        if (b.location.kind !== 'pdf' || !b.location.rects.length || !translatable(b)) continue;
        const css = b.location.rects.map((r) => pdfRectToCss(r, pm.viewport));
        const top = pr.top + Math.min(...css.map((c) => c.top));
        const bottom = pr.top + Math.max(...css.map((c) => c.top + c.height));
        if (isVisible({ top, bottom, height: bottom - top }, { top: vp.top, bottom: vp.bottom, height: vp.height })) ids.push(b.id);
        cands.push({ id: b.id, top: top - vp.top, bottom: bottom - vp.top });
      }
    }
    onVisible(ids.slice(0, 100), dir.current);
    const a = pickAnchor(cands, 0, vp.height);
    if (a) setAnchorId(a.id);
  }, [pages, live, blocksByPage, onVisible, translatable]);

  useEffect(() => { updateLive(); deb.current.run(report); }, [pages, model.blocks]); // eslint-disable-line react-hooks/exhaustive-deps

  const onScroll = () => {
    const el = pagesRef.current;
    if (!el) return;
    dir.current = el.scrollTop >= lastTop.current ? 'forward' : 'backward';
    lastTop.current = el.scrollTop;
    if (origin.current.classify() === 'user') onScrolled();
    updateLive();
    deb.current.run(report);
  };

  // Panel follows the anchor block unless the user is scrolling the panel.
  useEffect(() => {
    if (!syncScroll || userScrollingPanel || !anchorId || mode === 'original') return;
    const row = panelRef.current?.querySelector<HTMLElement>(`[data-trow="${CSS.escape(anchorId)}"]`);
    if (row && panelRef.current) {
      panelOrigin.current.markProgrammatic(300);
      panelRef.current.scrollTo({ top: Math.max(0, row.offsetTop - panelRef.current.clientHeight * 0.25), behavior: 'auto' });
    }
  }, [anchorId, syncScroll, userScrollingPanel, mode]);

  const scrollToBlockImpl = useCallback((blockId: string, ratio = 0) => {
    const el = pagesRef.current;
    if (!el) return false;
    for (const pm of pages) {
      const b = (blocksByPage.get(pm.index) ?? []).find((x) => x.id === blockId);
      if (!b || b.location.kind !== 'pdf' || !b.location.rects.length) continue;
      const node = el.querySelector<HTMLElement>(`[data-page="${pm.index}"]`);
      if (!node) return false;
      const css = b.location.rects.map((r) => pdfRectToCss(r, pm.viewport));
      const top = Math.min(...css.map((c) => c.top));
      const bottom = Math.max(...css.map((c) => c.top + c.height));
      origin.current.markProgrammatic(300);
      el.scrollTop = Math.max(0, node.offsetTop + top + (bottom - top) * ratio - el.clientHeight * 0.25);
      setAnchorId(blockId);
      setUserScrollingPanel(false);
      return true;
    }
    return false;
  }, [pages, blocksByPage]);

  useImperativeHandle(ref, () => ({
    getAnchor() {
      const el = pagesRef.current;
      if (!el) return null;
      if (anchorId) return { blockId: anchorId, ratio: 0 };
      return null;
    },
    scrollToBlock: scrollToBlockImpl,
    scrollToSection(sectionId) {
      const s = model.sections.find((x) => x.id === sectionId);
      const node = s ? pagesRef.current?.querySelector<HTMLElement>(`[data-page="${Number(s.ordinal)}"]`) : null;
      if (node && pagesRef.current) { origin.current.markProgrammatic(300); pagesRef.current.scrollTop = node.offsetTop; }
    },
  }), [anchorId, scrollToBlockImpl, model.sections]);

  if (error) return <div className="banner warn">PDF를 열 수 없습니다: {error}</div>;

  const panelBlocks = mode === 'original' ? [] : [...live].sort((a, b) => a - b).flatMap((i) => (blocksByPage.get(i) ?? []).filter(translatable).sort((a, b) => a.ordinal - b.ordinal));
  return (
    <div className={`pdf-split ${mode === 'original' ? 'only' : ''}`}>
      <div ref={pagesRef} className="pdf-pages" onScroll={onScroll} tabIndex={0} aria-label="PDF 원본 페이지">
        {pages.map((pm) => (
          <PdfPage
            key={pm.index}
            doc={doc}
            meta={pm}
            live={live.has(pm.index)}
            blocks={blocksByPage.get(pm.index) ?? []}
            sectionState={model.sections.find((s) => Number(s.ordinal) === pm.index)?.state}
            highlightBlockId={highlightBlockId ?? (mode !== 'original' ? anchorId : null)}
            onPick={(b) => { setAnchorId(b.id); setUserScrollingPanel(false); }}
          />
        ))}
      </div>
      {mode !== 'original' && (
        <div
          ref={panelRef}
          className="pdf-trn"
          onScroll={() => { if (panelOrigin.current.classify() === 'user') setUserScrollingPanel(true); }}
          aria-label="번역"
        >
          {userScrollingPanel && syncScroll && (
            <div className="sync-chip"><button className="btn small primary" onClick={() => setUserScrollingPanel(false)}>현재 원문에 맞춤</button></div>
          )}
          {panelBlocks.map((b) => (
            <div key={b.id} data-trow={b.id} className={`trow ${b.id === anchorId ? 'anchor' : ''}`} onClick={() => scrollToBlockImpl(b.id)}>
              <div className="src-mini" lang="en">{b.sourceText.slice(0, 120)}</div>
              <TranslationCell block={b} display={model.display.get(b.id)} job={model.jobs.get(b.id)} preview={model.previews.get(b.id)} autoActive={autoActive} targetLanguage={targetLanguage} actions={actions} />
            </div>
          ))}
        </div>
      )}
    </div>
  );
});

function PdfPage({ doc, meta, live, blocks, sectionState, highlightBlockId, onPick }: {
  doc: PDFDocumentProxy | null; meta: PageMeta; live: boolean; blocks: Block[]; sectionState?: string; highlightBlockId: string | null; onPick: (b: Block) => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const textRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!live || !doc || !canvasRef.current) return;
    let cancelled = false;
    let task: { cancel(): void; promise: Promise<void> } | null = null;
    let page: PDFPageProxy | null = null;
    void (async () => {
      page = await doc.getPage(meta.index + 1);
      if (cancelled || !canvasRef.current) return;
      const dpr = window.devicePixelRatio || 1;
      const vp = page.getViewport({ scale: SCALE * dpr });
      const c = canvasRef.current;
      c.width = Math.floor(vp.width);
      c.height = Math.floor(vp.height);
      const ctx = c.getContext('2d');
      if (!ctx) return;
      task = page.render({ canvasContext: ctx, viewport: vp, canvas: c } as Parameters<PDFPageProxy['render']>[0]);
      await task.promise.catch(() => {});
      if (cancelled || !textRef.current) return;
      // Text layer for native selection (original-mode text selection).
      const pdfjs = await import('pdfjs-dist');
      const tc = await page.getTextContent();
      textRef.current.replaceChildren();
      const tl = new pdfjs.TextLayer({ textContentSource: tc, container: textRef.current, viewport: page.getViewport({ scale: SCALE }) });
      await tl.render().catch(() => {});
    })();
    return () => {
      cancelled = true;
      task?.cancel();
      // Free the canvas backing store when the page leaves the live window.
      if (canvasRef.current) { canvasRef.current.width = 0; canvasRef.current.height = 0; }
      textRef.current?.replaceChildren();
      page?.cleanup();
    };
  }, [live, doc, meta.index]);

  const hl = blocks.find((b) => b.id === highlightBlockId);
  const onClick = (e: React.MouseEvent<HTMLDivElement>) => {
    if (window.getSelection()?.toString()) return; // text selection, not a block pick
    const r = (e.currentTarget as HTMLDivElement).getBoundingClientRect();
    const p = cssPointToPdf(e.clientX - r.left, e.clientY - r.top, meta.viewport);
    const b = hitTest(blocks, p, 2);
    if (b) onPick(b);
  };
  return (
    <div className="pdf-page" data-page={meta.index} style={{ width: meta.width, height: meta.height }} onClick={onClick}>
      {live ? <canvas ref={canvasRef} aria-hidden="true" /> : null}
      <div ref={textRef} className="textLayer" />
      {hl && hl.location.kind === 'pdf' && hl.location.rects.map((r, i) => {
        const c = pdfRectToCss(r, meta.viewport);
        return <div key={i} className="pdf-hl" style={{ left: c.left, top: c.top, width: c.width, height: c.height }} />;
      })}
      {sectionState === 'failed' && <span className="badge danger page-badge">추출 실패</span>}
      {blocks.length > 0 && blocks.every((b) => b.translationPolicy !== 'auto') && <span className="badge warn page-badge">자동 번역 제한</span>}
    </div>
  );
}
