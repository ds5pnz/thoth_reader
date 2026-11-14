// PDF original view (spec §1.1, §3.3, §3.4; ADR 0005): original pages on the left and, in
// side-by-side mode, a *translated page* on the right that keeps the page layout — the same
// raster (figures, table rules, backgrounds) with each text block's translation fitted into
// that block's own region. Both pages share one scroll container, so they always line up.
// The original page is never modified; everything is an overlay positioned from the stored
// PDF-space rects through the PDF.js viewport.
import { forwardRef, useCallback, useEffect, useImperativeHandle, useLayoutEffect, useMemo, useRef, useState } from 'react';
import type { PDFDocumentProxy, PDFPageProxy, PageViewport } from 'pdfjs-dist';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import type { Block } from '../../ipc/generated/Block';
import type { ReaderModel } from '../../features/reader/useReaderSession';
import { TranslationCell, type CellActions } from '../../features/translation/TranslationCell';
import { pdfRectToCss, cssPointToPdf, hitTest } from './coords';
import { isVisible, pickAnchor, ScrollOrigin, attachUserIntent, Debouncer, VISIBLE_DEBOUNCE_MS } from '../../domain/visibility';
import { openSessionPdf, releaseSessionPdf } from './sessionPdf';
import { setExtractionFocus } from './usePdfExtraction';
import { blockBox, expandBox, fitFontSize, harmonizeFontSizes, resolveVerticalOverlaps, pickBackground, textColorFor, type BlockBox, type ItemSize, type Rgb } from './translatedPage';

export interface PdfOriginalHandle {
  getAnchor: () => { blockId: string; ratio: number } | null;
  scrollToBlock: (blockId: string, ratio?: number, highlight?: boolean) => boolean;
  scrollToSection: (sectionId: string) => void;
  /** 0 when the 25% line is inside the block's region (spec §10.4), else the distance. */
  anchorErrorPx: (blockId: string, ratio: number) => number | null;
}

const NO_BLOCKS: Block[] = [];
const MAX_LIVE_CANVASES = 5;
const MAX_SCALE = 1.8;
const GUTTER = 16;
/** Below this share of the original size a fitted translation is not worth showing. */
const MIN_READABLE_RATIO = 0.6;

/** Sizes are kept at scale 1; `viewport` is derived for the current fit-to-width scale. */
type PageBase = { index: number; bw: number; bh: number; proxy: PDFPageProxy | null };
type PageMeta = { index: number; width: number; height: number; viewport: PageViewport | null };

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
  const [bases, setBases] = useState<PageBase[]>([]);
  const [paneWidth, setPaneWidth] = useState(0);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [rendered, setRendered] = useState<Map<number, number>>(new Map());
  const canvases = useRef(new Map<number, HTMLCanvasElement>());
  const itemSizes = useRef(new Map<number, ItemSize[]>());
  // Fit to width: one page in original mode, two pages side by side otherwise.
  const scale = useMemo(() => {
    const bw = bases[0]?.bw ?? 0;
    if (!bw || !paneWidth) return 1;
    const cols = mode === 'original' ? 1 : 2;
    const avail = (paneWidth - GUTTER * (cols + 1)) / cols;
    return Math.max(0.3, Math.min(MAX_SCALE, Math.round((avail / bw) * 100) / 100));
  }, [bases, paneWidth, mode]);
  const pages: PageMeta[] = useMemo(() => bases.map((b) => ({
    index: b.index,
    width: b.bw * scale,
    height: b.bh * scale,
    viewport: b.proxy ? b.proxy.getViewport({ scale }) : null,
  })), [bases, scale]);
  const [error, setError] = useState<string | null>(null);
  const [live, setLive] = useState<Set<number>>(new Set([0]));
  const [anchorId, setAnchorId] = useState<string | null>(null);
  const pagesRef = useRef<HTMLDivElement>(null);
  const origin = useRef(new ScrollOrigin());
  const lastTop = useRef(0);
  const dir = useRef<'forward' | 'backward'>('forward');
  const deb = useRef(new Debouncer(VISIBLE_DEBOUNCE_MS));

  // Load the PDF via the thoth-doc protocol (Range requests, bundled worker, eval off).
  useEffect(() => {
    let cancelled = false;
    openSessionPdf(session).then(async (d) => {
      if (cancelled) return;
      // Only page 1 is measured eagerly; other pages use its size until they come near
      // the viewport (2,000-page documents must not touch every page up front).
      const p1 = await d.getPage(1);
      const vp1 = p1.getViewport({ scale: 1 });
      const metas: PageBase[] = Array.from({ length: d.numPages }, (_, i) => ({ index: i, bw: vp1.width, bh: vp1.height, proxy: i === 0 ? p1 : null }));
      if (!cancelled) { setDoc(d); setBases(metas); }
    }).catch((e: unknown) => setError(String((e as Error)?.message ?? e)));
    return () => { cancelled = true; releaseSessionPdf(session); };
  }, [session.assetToken]); // eslint-disable-line react-hooks/exhaustive-deps

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
    if (near[0]) setExtractionFocus(session.sessionId, near[0].i);
    setLive((prev) => (prev.size === next.size && [...next].every((x) => prev.has(x)) ? prev : next));
    for (const i of next) { const sid = sectionByPage.get(i); if (sid) loadSection(sid); }
    // Measure real viewports for live pages (rotation/crop may differ per page).
    if (doc) {
      for (const i of next) {
        if (pages[i] && !pages[i]!.viewport) {
          void doc.getPage(i + 1).then((pg) => {
            const vp = pg.getViewport({ scale: 1 });
            setBases((prev) => prev.map((m) => (m.index === i ? { ...m, proxy: pg, bw: vp.width, bh: vp.height } : m)));
          });
        }
      }
    }
  }, [pages, doc, sectionByPage, loadSection, session.sessionId]);

  // Visibility: viewport ∩ block rects (same rule as the reflow view).
  const report = useCallback(() => {
    const el = pagesRef.current;
    if (!el) return;
    const vp = el.getBoundingClientRect();
    const ids: string[] = [];
    const cands: { id: string; top: number; bottom: number }[] = [];
    for (const pm of pages) {
      if (!live.has(pm.index) || !pm.viewport) continue;
      const pv = pm.viewport;
      const node = el.querySelector<HTMLElement>(`[data-page="${pm.index}"]`);
      if (!node) continue;
      const pr = node.getBoundingClientRect();
      for (const b of blocksByPage.get(pm.index) ?? []) {
        if (b.location.kind !== 'pdf' || !b.location.rects.length || !translatable(b)) continue;
        const css = b.location.rects.map((r) => pdfRectToCss(r, pv));
        const top = pr.top + Math.min(...css.map((c) => c.top));
        const bottom = pr.top + Math.max(...css.map((c) => c.top + c.height));
        if (isVisible({ top, bottom, height: bottom - top }, { top: vp.top, bottom: vp.bottom, height: vp.height })) ids.push(b.id);
        cands.push({ id: b.id, top: top - vp.top, bottom: bottom - vp.top });
      }
    }
    onVisible(ids.slice(0, 100), dir.current);
    const atTop = el.scrollTop <= 1;
    const first = cands.filter((c) => c.bottom > 0).sort((x, y) => x.top - y.top)[0];
    const a = atTop && first ? { id: first.id } : pickAnchor(cands, 0, vp.height);
    if (a) setAnchorId(a.id);
  }, [pages, live, blocksByPage, onVisible, translatable]);

  useEffect(() => { updateLive(); }, [pages, model.blocks]); // eslint-disable-line react-hooks/exhaustive-deps
  // Re-run visibility/anchor whenever the live page set or loaded blocks change.
  useEffect(() => { deb.current.run(report); }, [report]);

  useEffect(() => {
    const el = pagesRef.current;
    return el ? attachUserIntent(el, origin.current) : undefined;
  }, []);

  useEffect(() => {
    const el = pagesRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setPaneWidth(el.clientWidth));
    ro.observe(el);
    setPaneWidth(el.clientWidth);
    return () => ro.disconnect();
  }, []);

  // A scale change (window resize, sidebar, mode) keeps the same relative scroll position.
  const relPos = useRef(0);
  useLayoutEffect(() => {
    const el = pagesRef.current;
    if (!el || el.scrollHeight <= el.clientHeight) return;
    origin.current.markProgrammatic(300);
    el.scrollTop = relPos.current * (el.scrollHeight - el.clientHeight);
  }, [scale]);

  const onScroll = () => {
    const el = pagesRef.current;
    if (!el) return;
    dir.current = el.scrollTop >= lastTop.current ? 'forward' : 'backward';
    lastTop.current = el.scrollTop;
    const cls = origin.current.classify();
    if (cls === 'user') {
      relPos.current = el.scrollHeight > el.clientHeight ? el.scrollTop / (el.scrollHeight - el.clientHeight) : 0;
      onScrolled();
    }
    updateLive();
    deb.current.run(report);
  };

  const scrollToBlockImpl = useCallback((blockId: string, ratio = 0) => {
    const el = pagesRef.current;
    if (!el) return false;
    for (const pm of pages) {
      const b = (blocksByPage.get(pm.index) ?? []).find((x) => x.id === blockId);
      if (!b || b.location.kind !== 'pdf' || !b.location.rects.length) continue;
      const node = el.querySelector<HTMLElement>(`[data-page="${pm.index}"]`);
      if (!node) return false;
      if (!pm.viewport) {
        // Bring the page in first; the caller retries after measurement.
        origin.current.markProgrammatic(300);
        el.scrollTop += node.getBoundingClientRect().top - el.getBoundingClientRect().top;
        setTimeout(() => scrollRetry.current?.(blockId, ratio), 120);
        return true;
      }
      const pvp = pm.viewport;
      const css = b.location.rects.map((r) => pdfRectToCss(r, pvp));
      const top = Math.min(...css.map((c) => c.top));
      const bottom = Math.max(...css.map((c) => c.top + c.height));
      origin.current.markProgrammatic(300);
      const pageTop = el.scrollTop + (node.getBoundingClientRect().top - el.getBoundingClientRect().top);
      el.scrollTop = Math.max(0, pageTop + top + (bottom - top) * ratio - el.clientHeight * 0.25);
      relPos.current = el.scrollHeight > el.clientHeight ? el.scrollTop / (el.scrollHeight - el.clientHeight) : 0;
      setAnchorId(blockId);
      return true;
    }
    return false;
  }, [pages, blocksByPage]);

  const scrollRetry = useRef<((id: string, r: number) => void) | null>(null);
  scrollRetry.current = (id, r) => { scrollToBlockImpl(id, r); };

  useImperativeHandle(ref, () => ({
    getAnchor() {
      const el = pagesRef.current;
      if (!el) return null;
      if (anchorId) return { blockId: anchorId, ratio: 0 };
      return null;
    },
    scrollToBlock: scrollToBlockImpl,
    anchorErrorPx(blockId) {
      const el = pagesRef.current;
      if (!el) return null;
      for (const pm of pages) {
        const b = (blocksByPage.get(pm.index) ?? []).find((x) => x.id === blockId);
        if (!b || b.location.kind !== 'pdf' || !pm.viewport || !b.location.rects.length) continue;
        const node = el.querySelector<HTMLElement>(`[data-page="${pm.index}"]`);
        if (!node) return null;
        const pv = pm.viewport;
        const css = b.location.rects.map((r) => pdfRectToCss(r, pv));
        const pr = node.getBoundingClientRect();
        const vp = el.getBoundingClientRect();
        const top = pr.top + Math.min(...css.map((c) => c.top));
        const bottom = pr.top + Math.max(...css.map((c) => c.top + c.height));
        const line = vp.top + vp.height * 0.25;
        const clamped = el.scrollTop <= 1 || el.scrollTop + el.clientHeight >= el.scrollHeight - 1;
        if (clamped && bottom > vp.top && top < vp.bottom) return 0;
        return line < top ? top - line : line > bottom ? line - bottom : 0;
      }
      return null;
    },
    scrollToSection(sectionId) {
      const s = model.sections.find((x) => x.id === sectionId);
      const node = s ? pagesRef.current?.querySelector<HTMLElement>(`[data-page="${Number(s.ordinal)}"]`) : null;
      const el = pagesRef.current;
      if (node && el) { origin.current.markProgrammatic(300); el.scrollTop += node.getBoundingClientRect().top - el.getBoundingClientRect().top; }
    },
  }), [anchorId, scrollToBlockImpl, model.sections, pages, blocksByPage]);

  if (error) return <div className="banner warn">PDF를 열 수 없습니다: {error}</div>;

  const selected = selectedId ? [...blocksByPage.values()].flat().find((b) => b.id === selectedId) ?? null : null;
  const hlId = highlightBlockId ?? selectedId;
  void syncScroll; // both pages share one scroller: nothing to synchronise
  return (
    <div className="pdf-split only">
      <div ref={pagesRef} className={`pdf-pages ${mode === 'original' ? 'single' : 'dual'}`} onScroll={onScroll} tabIndex={0} aria-label="PDF 페이지">
        {pages.map((pm) => {
          const blocks = blocksByPage.get(pm.index) ?? NO_BLOCKS;
          const isLive = live.has(pm.index);
          return (
            <div key={pm.index} className="pdf-row" style={{ gap: GUTTER, height: pm.height, marginBottom: GUTTER }}>
              <PdfPage
                doc={doc}
                meta={pm}
                scale={scale}
                live={isLive}
                blocks={blocks}
                sectionState={model.sections.find((s) => Number(s.ordinal) === pm.index)?.state}
                highlightBlockId={hlId}
                onPick={(b) => { setAnchorId(b.id); setSelectedId(b.id); }}
                onRendered={(c, items) => { canvases.current.set(pm.index, c); itemSizes.current.set(pm.index, items); setRendered((prev) => new Map(prev).set(pm.index, (prev.get(pm.index) ?? 0) + 1)); }}
              />
              {mode !== 'original' && (
                <TranslatedPage
                  meta={pm}
                  live={isLive}
                  blocks={blocks}
                  model={model}
                  source={canvases.current.get(pm.index) ?? null}
                  items={itemSizes.current.get(pm.index) ?? null}
                  version={rendered.get(pm.index) ?? 0}
                  targetLanguage={targetLanguage}
                  highlightBlockId={hlId}
                  onPick={(b) => { setAnchorId(b.id); setSelectedId(b.id); }}
                />
              )}
            </div>
          );
        })}
      </div>
      {selected && mode !== 'original' && (
        <aside className="pdf-pop" aria-label="선택한 문단의 번역">
          <header>
            <span className="faint">{selected.location.kind === 'pdf' ? `${selected.location.pageIndex + 1}쪽` : ''}</span>
            <span className="spacer" />
            <button className="btn small ghost" onClick={() => setSelectedId(null)} aria-label="닫기">닫기</button>
          </header>
          <TranslationCell detailed block={selected} display={model.display.get(selected.id)} job={model.jobs.get(selected.id)} preview={model.previews.get(selected.id)} autoActive={autoActive} targetLanguage={targetLanguage} actions={actions} />
        </aside>
      )}
    </div>
  );
});

function PdfPage({ doc, meta, scale, live, blocks, sectionState, highlightBlockId, onPick, onRendered }: {
  doc: PDFDocumentProxy | null; meta: PageMeta; scale: number; live: boolean; blocks: Block[]; sectionState?: string; highlightBlockId: string | null; onPick: (b: Block) => void; onRendered: (c: HTMLCanvasElement, items: ItemSize[]) => void;
}) {
  const onRenderedRef = useRef(onRendered);
  onRenderedRef.current = onRendered;
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
      const vp = page.getViewport({ scale: scale * dpr });
      const c = canvasRef.current;
      c.width = Math.floor(vp.width);
      c.height = Math.floor(vp.height);
      const ctx = c.getContext('2d');
      if (!ctx) return;
      task = page.render({ canvasContext: ctx, viewport: vp, canvas: c } as Parameters<PDFPageProxy['render']>[0]);
      const ok = await task.promise.then(() => true, () => false);
      if (cancelled || !textRef.current) return;
      // Text layer for native selection (original-mode text selection).
      const pdfjs = await import('pdfjs-dist');
      const tc = await page.getTextContent();
      // Same item order as extraction (itemRanges index into the `str` items).
      const sizes: ItemSize[] = [];
      for (const it of tc.items) if ('str' in it) sizes.push({ h: it.height, n: it.str.length });
      if (ok && !cancelled) onRenderedRef.current(c, sizes);
      const container = textRef.current;
      if (cancelled || !container) return; // unmounted while awaiting
      container.replaceChildren();
      const tl = new pdfjs.TextLayer({ textContentSource: tc, container, viewport: page.getViewport({ scale }) });
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
  }, [live, doc, meta.index, scale]);

  const hl = blocks.find((b) => b.id === highlightBlockId);
  const onClick = (e: React.MouseEvent<HTMLDivElement>) => {
    if (window.getSelection()?.toString()) return; // text selection, not a block pick
    const r = (e.currentTarget as HTMLDivElement).getBoundingClientRect();
    if (!meta.viewport) return;
    const p = cssPointToPdf(e.clientX - r.left, e.clientY - r.top, meta.viewport);
    const b = hitTest(blocks, p, 2);
    if (b) onPick(b);
  };
  return (
    <div className="pdf-page" data-page={meta.index} style={{ width: meta.width, height: meta.height, ['--scale-factor' as string]: scale }} onClick={onClick}>
      {live ? <canvas ref={canvasRef} aria-hidden="true" /> : null}
      <div ref={textRef} className="textLayer" />
      {hl && hl.location.kind === 'pdf' && meta.viewport && hl.location.rects.map((r, i) => {
        const c = pdfRectToCss(r, meta.viewport!);
        return <div key={i} className="pdf-hl" style={{ left: c.left, top: c.top, width: c.width, height: c.height }} />;
      })}
      {sectionState === 'failed' && <span className="badge danger page-badge">추출 실패</span>}
      {blocks.length > 0 && blocks.every((b) => b.translationPolicy !== 'auto') && <span className="badge warn page-badge">자동 번역 제한</span>}
    </div>
  );
}

/** The layout-preserving translated page: a copy of the page raster with each translated
 *  block drawn over its own region. Untranslated blocks keep showing the original. */
function TranslatedPage({ meta, live, blocks, model, source, items, version, targetLanguage, highlightBlockId, onPick }: {
  meta: PageMeta; live: boolean; blocks: Block[]; model: ReaderModel; source: HTMLCanvasElement | null; items: ItemSize[] | null; version: number; targetLanguage: string; highlightBlockId: string | null; onPick: (b: Block) => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [fit, setFit] = useState<Map<string, { bg: Rgb; width: number; height: number }>>(new Map());
  const vp = meta.viewport;
  const tblocks = useMemo(() => blocks.filter((b) => !b.roles.includes('running_header') && !b.roles.includes('running_footer') && b.sourceText.trim().length > 0), [blocks]);
  const boxes = useMemo(() => (vp ? resolveVerticalOverlaps(tblocks.map((b) => ({ b, id: b.id, box: blockBox(b, vp, items) })).filter((x): x is { b: Block; id: string; box: BlockBox } => !!x.box)) : []), [tblocks, vp, items]);
  const fonts = useMemo(() => harmonizeFontSizes(boxes), [boxes]);

  // Copy the original raster (cheaper than rendering the page twice). Pixels are read once
  // per render of the page, not on every React update.
  const raster = useRef<{ img: ImageData; k: number } | null>(null);
  const [rasterVersion, setRasterVersion] = useState(0);
  useEffect(() => {
    const c = canvasRef.current;
    if (!live || !c || !source || source.width === 0) return;
    c.width = source.width;
    c.height = source.height;
    const ctx = c.getContext('2d', { willReadFrequently: true });
    if (!ctx) return;
    ctx.drawImage(source, 0, 0);
    raster.current = { img: ctx.getImageData(0, 0, c.width, c.height), k: meta.width > 0 ? c.width / meta.width : 1 };
    setRasterVersion((v) => v + 1);
    return () => { raster.current = null; c.width = 0; c.height = 0; };
  }, [live, source, version, meta.width]);

  // Sample each block's background and grow its region into the empty space around it.
  useEffect(() => {
    const r = raster.current;
    if (!r || boxes.length === 0) return;
    const { img, k } = r;
    const at = (xCss: number, yCss: number): Rgb => {
      const x = Math.min(img.width - 1, Math.max(0, Math.round(xCss * k)));
      const y = Math.min(img.height - 1, Math.max(0, Math.round(yCss * k)));
      const i = (y * img.width + x) * 4;
      return [img.data[i]!, img.data[i + 1]!, img.data[i + 2]!];
    };
    const all = boxes.map((x) => x.box);
    const pageRight = Math.min(meta.width - 4, Math.max(...all.map((x) => x.left + x.width), 0));
    const next = new Map<string, { bg: Rgb; width: number; height: number }>();
    for (const { b, box } of boxes) {
      const pts: Rgb[] = [];
      for (const [fx, fy] of [[0, 0], [1, 0], [0, 1], [1, 1], [0.5, 0], [0.5, 1], [0, 0.5], [1, 0.5]] as const) pts.push(at(box.left - 2 + fx * (box.width + 4), box.top - 2 + fy * (box.height + 4)));
      const bg = pickBackground(pts);
      next.set(b.id, { bg, ...expandBox(img, k, box, bg, all, pageRight) });
    }
    setFit(next);
  }, [rasterVersion, boxes, meta.width]);

  return (
    <div className="pdf-page translated" style={{ width: meta.width, height: meta.height }} lang={targetLanguage} aria-label={`번역 페이지 ${meta.index + 1}`}>
      {live ? <canvas ref={canvasRef} aria-hidden="true" /> : null}
      {live && [...boxes].sort((x, y) => y.box.width * y.box.height - x.box.width * x.box.height).map(({ b, box }) => {
        const d = model.display.get(b.id);
        const job = model.jobs.get(b.id);
        const preview = model.previews.get(b.id);
        const text = d?.shown?.text ?? '';
        const busy = !!job && job.state !== 'FAILED';
        const f = fit.get(b.id);
        const slot = { left: box.left - 1, top: box.top - 1, width: box.width + 2, height: box.height + 2 };
        if (!text && !preview) {
          return (
            <button key={b.id} type="button" data-policy={b.translationPolicy} className={`tp-slot ${busy ? 'busy' : ''} ${job?.state === 'FAILED' ? 'failed' : ''} ${b.id === highlightBlockId ? 'sel' : ''}`} style={slot} onClick={() => onPick(b)} aria-label={busy ? '번역 중인 문단' : job?.state === 'FAILED' ? '번역에 실패한 문단' : '아직 번역되지 않은 문단'} />
          );
        }
        const fontSize = fonts.get(b.id) ?? box.fontSize;
        return (
          <FitText
            key={b.id}
            text={text || preview || ''}
            kind={`${b.kind}/${box.lines}`}
            cover={slot}
            maxWidth={(f?.width ?? box.width) + 2}
            maxHeight={(f?.height ?? box.height) + 2}
            baseSize={fontSize}
            // Keep the original line rhythm; single lines get a normal leading.
            lineHeight={box.pitch > 0 ? Math.max(1.05, Math.min(1.9, box.pitch / fontSize)) : 1.25}
            bg={f?.bg}
            stale={d?.status === 'stale'}
            manual={d?.status === 'manual'}
            streaming={!text && !!preview}
            selected={b.id === highlightBlockId}
            onClick={() => onPick(b)}
          />
        );
      })}
    </div>
  );
}

/** Translated text for one block. It always covers the original text region (`cover`) and
 *  may use the free space up to maxWidth/maxHeight; the font shrinks only if that is not enough. */
function FitText({ text, kind, cover, maxWidth, maxHeight, baseSize, lineHeight, bg, stale, manual, streaming, selected, onClick }: {
  text: string; kind: string; cover: { left: number; top: number; width: number; height: number }; maxWidth: number; maxHeight: number; baseSize: number; lineHeight: number; bg: Rgb | undefined; stale?: boolean; manual?: boolean; streaming?: boolean; selected: boolean; onClick: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  // When the region is too small for a readable translation (diagram labels, tight table
  // cells) the original stays visible instead; the translation is one click away.
  const [fallback, setFallback] = useState(false);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const fits = (px: number) => {
      el.style.fontSize = `${px}px`;
      return el.scrollHeight <= maxHeight + 1 && el.scrollWidth <= maxWidth + 1;
    };
    const refit = () => {
      const px = fitFontSize(baseSize, fits);
      el.style.fontSize = `${px}px`;
      const unreadable = px < baseSize * MIN_READABLE_RATIO || !fits(px);
      el.style.fontSize = `${px}px`;
      setFallback((prev) => (prev === unreadable ? prev : unreadable));
    };
    refit();
    // Line breaks can change after the first layout (late font activation for Hangul,
    // sub-pixel width settling): check again on the next frame and once fonts are ready.
    let alive = true;
    const again = () => { if (alive && (el.scrollHeight > maxHeight + 1 || el.scrollWidth > maxWidth + 1)) refit(); };
    const raf = requestAnimationFrame(again);
    const timer = window.setTimeout(again, 300);
    void document.fonts?.ready.then(again);
    return () => { alive = false; cancelAnimationFrame(raf); window.clearTimeout(timer); };
  }, [text, maxWidth, maxHeight, baseSize, lineHeight]);
  const color = bg ?? [255, 255, 255];
  return (
    <div
      ref={ref}
      className={`tp-text ${fallback ? 'fallback' : ''} ${stale ? 'stale' : ''} ${manual ? 'manual' : ''} ${streaming ? 'streaming' : ''} ${selected ? 'sel' : ''}`}
      style={{ left: cover.left, top: cover.top, width: maxWidth, minHeight: cover.height, maxHeight, lineHeight, background: fallback ? 'transparent' : `rgb(${color[0]},${color[1]},${color[2]})`, color: fallback ? 'transparent' : textColorFor(color) }}
      aria-label={fallback ? `번역: ${text}` : undefined}
      title={text}
      data-base={baseSize.toFixed(1)}
      data-cover={`${cover.left.toFixed(1)},${cover.top.toFixed(1)},${cover.width.toFixed(1)},${cover.height.toFixed(1)}`}
      data-kind={kind}
      data-max={`${maxWidth.toFixed(0)}x${maxHeight.toFixed(0)}`}
      onClick={(e) => { if (!window.getSelection()?.toString()) { e.stopPropagation(); onClick(); } }}
    >
      {text}
    </div>
  );
}
