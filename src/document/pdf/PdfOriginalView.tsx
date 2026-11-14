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
import { blockBox, fitFontSize, pickBackground, textColorFor, type Rgb } from './translatedPage';

export interface PdfOriginalHandle {
  getAnchor: () => { blockId: string; ratio: number } | null;
  scrollToBlock: (blockId: string, ratio?: number, highlight?: boolean) => boolean;
  scrollToSection: (sectionId: string) => void;
  /** 0 when the 25% line is inside the block's region (spec §10.4), else the distance. */
  anchorErrorPx: (blockId: string, ratio: number) => number | null;
}

const MAX_LIVE_CANVASES = 5;
const MAX_SCALE = 1.8;
const GUTTER = 16;

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
  }, [pages, doc, sectionByPage, loadSection]);

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
          const blocks = blocksByPage.get(pm.index) ?? [];
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
                onRendered={(c) => { canvases.current.set(pm.index, c); setRendered((prev) => new Map(prev).set(pm.index, (prev.get(pm.index) ?? 0) + 1)); }}
              />
              {mode !== 'original' && (
                <TranslatedPage
                  meta={pm}
                  live={isLive}
                  blocks={blocks.filter(translatable)}
                  model={model}
                  source={canvases.current.get(pm.index) ?? null}
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
          <TranslationCell block={selected} display={model.display.get(selected.id)} job={model.jobs.get(selected.id)} preview={model.previews.get(selected.id)} autoActive={autoActive} targetLanguage={targetLanguage} actions={actions} />
        </aside>
      )}
    </div>
  );
});

function PdfPage({ doc, meta, scale, live, blocks, sectionState, highlightBlockId, onPick, onRendered }: {
  doc: PDFDocumentProxy | null; meta: PageMeta; scale: number; live: boolean; blocks: Block[]; sectionState?: string; highlightBlockId: string | null; onPick: (b: Block) => void; onRendered: (c: HTMLCanvasElement) => void;
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
      if (ok) onRenderedRef.current(c);
      // Text layer for native selection (original-mode text selection).
      const pdfjs = await import('pdfjs-dist');
      const tc = await page.getTextContent();
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
function TranslatedPage({ meta, live, blocks, model, source, version, targetLanguage, highlightBlockId, onPick }: {
  meta: PageMeta; live: boolean; blocks: Block[]; model: ReaderModel; source: HTMLCanvasElement | null; version: number; targetLanguage: string; highlightBlockId: string | null; onPick: (b: Block) => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [colors, setColors] = useState<Map<string, Rgb>>(new Map());
  const vp = meta.viewport;
  const boxes = useMemo(() => (vp ? blocks.map((b) => ({ b, box: blockBox(b, vp) })).filter((x): x is { b: Block; box: NonNullable<ReturnType<typeof blockBox>> } => !!x.box) : []), [blocks, vp]);

  // Copy the original raster (cheaper than rendering the page twice) and sample each
  // block's background so the overlay blends into coloured boxes and table cells.
  useEffect(() => {
    const c = canvasRef.current;
    if (!live || !c || !source || source.width === 0) return;
    c.width = source.width;
    c.height = source.height;
    const ctx = c.getContext('2d', { willReadFrequently: true });
    if (!ctx) return;
    ctx.drawImage(source, 0, 0);
    const k = meta.width > 0 ? c.width / meta.width : 1;
    const next = new Map<string, Rgb>();
    for (const { b, box } of boxes) {
      const pts: Rgb[] = [];
      for (const [fx, fy] of [[0, 0], [1, 0], [0, 1], [1, 1], [0.5, 0], [0.5, 1], [0, 0.5], [1, 0.5]] as const) {
        const x = Math.min(c.width - 1, Math.max(0, Math.round((box.left - 2 + fx * (box.width + 4)) * k)));
        const y = Math.min(c.height - 1, Math.max(0, Math.round((box.top - 2 + fy * (box.height + 4)) * k)));
        const d = ctx.getImageData(x, y, 1, 1).data;
        pts.push([d[0]!, d[1]!, d[2]!]);
      }
      next.set(b.id, pickBackground(pts));
    }
    setColors(next);
    return () => { c.width = 0; c.height = 0; };
  }, [live, source, version, boxes, meta.width]);

  return (
    <div className="pdf-page translated" style={{ width: meta.width, height: meta.height }} lang={targetLanguage} aria-label={`번역 페이지 ${meta.index + 1}`}>
      {live ? <canvas ref={canvasRef} aria-hidden="true" /> : null}
      {live && boxes.map(({ b, box }) => {
        const d = model.display.get(b.id);
        const job = model.jobs.get(b.id);
        const preview = model.previews.get(b.id);
        const text = d?.shown?.text ?? '';
        const busy = !!job && job.state !== 'FAILED';
        const bg = colors.get(b.id);
        const style = { left: box.left - 1, top: box.top - 1, width: box.width + 2, height: box.height + 2 };
        if (!text && !preview) {
          return (
            <button key={b.id} type="button" className={`tp-slot ${busy ? 'busy' : ''} ${job?.state === 'FAILED' ? 'failed' : ''} ${b.id === highlightBlockId ? 'sel' : ''}`} style={style} onClick={() => onPick(b)} aria-label={busy ? '번역 중인 문단' : job?.state === 'FAILED' ? '번역에 실패한 문단' : '아직 번역되지 않은 문단'} />
          );
        }
        return (
          <FitText
            key={b.id}
            text={text || preview || ''}
            box={style}
            baseSize={box.fontSize}
            bg={bg}
            stale={d?.status === 'stale'}
            manual={d?.status === 'manual'}
            streaming={!text && !!preview}
            selected={b.id === highlightBlockId}
            centered={false}
            onClick={() => onPick(b)}
          />
        );
      })}
    </div>
  );
}

function FitText({ text, box, baseSize, bg, stale, manual, streaming, selected, centered, onClick }: {
  text: string; box: { left: number; top: number; width: number; height: number }; baseSize: number; bg: Rgb | undefined; stale?: boolean; manual?: boolean; streaming?: boolean; selected: boolean; centered: boolean; onClick: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.fontSize = `${fitFontSize(baseSize, (px) => { el.style.fontSize = `${px}px`; return el.scrollHeight <= box.height + 1 && el.scrollWidth <= box.width + 1; })}px`;
  }, [text, box.width, box.height, baseSize]);
  const color = bg ?? [255, 255, 255];
  return (
    <div
      ref={ref}
      className={`tp-text ${stale ? 'stale' : ''} ${manual ? 'manual' : ''} ${streaming ? 'streaming' : ''} ${selected ? 'sel' : ''}`}
      style={{ ...box, background: `rgb(${color[0]},${color[1]},${color[2]})`, color: textColorFor(color), textAlign: centered ? 'center' : undefined }}
      title={text}
      onClick={(e) => { if (!window.getSelection()?.toString()) { e.stopPropagation(); onClick(); } }}
    >
      {text}
    </div>
  );
}
