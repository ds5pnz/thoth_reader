// Original (source-only) EPUB mode: epub.js renders the *sanitized* package served by the
// thoth-doc protocol. Scripts are never allowed; the iframe is sandboxed without allow-scripts.
import { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState } from 'react';
import ePub, { type Book, type Rendition } from 'epubjs';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import type { ReaderModel } from '../../features/reader/useReaderSession';
import type { Block } from '../../ipc/generated/Block';
import { blockForCfi, cfiStart } from './cfi';
import type { AnnotationRow } from '../../ipc/generated/AnnotationRow';
import { EXTERNAL_SELECTION, HIGHLIGHT_COLORS, type ExternalSelection } from '../../features/annotations/annotations';
import { countOccurrences, findQuoteInRange, ink, inkBefore, inkOfRange } from '../../features/annotations/quote';
import { useShortcut } from '../../app/shortcuts';

type Flow = 'scrolled' | 'paged';
const FLOW_KEY = 'thoth.epubFlow';
const COLUMNS_KEY = 'thoth.epubColumns';
const HIGHLIGHT_CSS: Record<string, string> = { yellow: 'rgba(255, 214, 10, .45)', green: 'rgba(64, 200, 110, .38)', blue: 'rgba(70, 150, 255, .34)', pink: 'rgba(255, 105, 160, .36)' };
const pref = (k: string, d: string) => { try { return localStorage.getItem(k) ?? d; } catch { return d; } };
const savePref = (k: string, v: string) => { try { localStorage.setItem(k, v); } catch { /* private mode */ } };

export interface EpubOriginalHandle {
  getAnchor: () => { blockId: string; ratio: number } | null;
  scrollToBlock: (blockId: string, ratio?: number) => boolean;
  anchorErrorPx: (blockId: string, ratio: number) => number | null;
  scrollToSection: (sectionId: string) => void;
}

/** Rewrite src/href attributes that point into the sanitized package ("../r/…", "../x/…")
 *  to absolute asset URLs. Only attribute values are touched. */
export function absolutizeResourceRefs(xhtml: string, assetBase: string): string {
  return xhtml.replace(/(\s(?:src|href|xlink:href)=")\.\.\/(r|x)\/([^"#?]+)/g, (_m, attr: string, dir: string, rest: string) => `${attr}${assetBase}${dir}/${rest}`);
}

const LINK_PREFIX = '#thoth-l-';
let linkSeq = 0;

/** Turn every link in a rendered chapter into a same-document fragment jump. The frame runs
 *  no listeners, but a fragment jump changes its location, and that can be seen from outside:
 *  the real target stays in `data-thoth-href` and the click is acted on by the poll. */
export function armLinks(doc: Document): number {
  let n = 0;
  for (const a of doc.querySelectorAll<Element>('a[href]:not([data-thoth-href])')) {
    a.setAttribute('data-thoth-href', a.getAttribute('href') ?? '');
    a.setAttribute('href', `${LINK_PREFIX}${++linkSeq}`);
    a.removeAttribute('target'); // epub.js marks absolute links _blank; popups are not allowed
    n++;
  }
  return n;
}

/** The link whose fragment the frame just jumped to, re-armed so a second click is seen too. */
export function takeClickedLink(doc: Document, hash: string): { href: string; external: boolean } | null {
  if (!hash.startsWith(LINK_PREFIX)) return null;
  const a = [...doc.querySelectorAll<Element>('a[data-thoth-href]')].find((x) => x.getAttribute('href') === hash);
  if (!a) return null;
  a.setAttribute('href', `${LINK_PREFIX}${++linkSeq}`);
  return { href: a.getAttribute('data-thoth-href') ?? '', external: a.getAttribute('data-thoth-external') === '1' };
}

/** "…/12.xhtml#note3" (a link into the sanitized package) → spine index and fragment. */
export function parseSpineLink(href: string): { index: number; fragment: string | null } | null {
  const m = /(?:^|\/)(\d+)\.xhtml(?:#(.*))?$/.exec(href);
  return m ? { index: Number(m[1]), fragment: m[2] || null } : null;
}

export function packageUrl(session: ReaderSessionDto) {
  return `thoth-doc://localhost/${session.assetToken}/epub/package.opf`;
}

export const EpubOriginalView = forwardRef<EpubOriginalHandle, {
  session: ReaderSessionDto;
  model: ReaderModel;
  loadSection: (id: string) => void;
  onScrolled: () => void;
  openExternal: (url: string) => void;
  annotations?: readonly AnnotationRow[];
}>(function EpubOriginalView({ session, model, loadSection, onScrolled, openExternal, annotations = [] }, ref) {
  const host = useRef<HTMLDivElement>(null);
  const rendition = useRef<Rendition | null>(null);
  const book = useRef<Book | null>(null);
  const currentCfi = useRef<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Reading layout: continuous scroll, or pages turned left/right (optionally two columns).
  const [flow, setFlowState] = useState<Flow>(() => (pref(FLOW_KEY, 'scrolled') === 'paged' ? 'paged' : 'scrolled'));
  const [columns, setColumnsState] = useState<1 | 2>(() => (pref(COLUMNS_KEY, '1') === '2' ? 2 : 1));
  const flowRef = useRef(flow);
  flowRef.current = flow;
  const [renderTick, setRenderTick] = useState(0);
  const [near, setNear] = useState(false);
  const annotationsRef = useRef(annotations);
  annotationsRef.current = annotations;
  const allBlocks = useRef<Block[]>([]);
  allBlocks.current = [...model.blocks.values()].flat();
  const currentIndex = useRef<number | null>(null);
  const pendingDisplay = useRef<Promise<unknown> | null>(null);
  const programmaticUntil = useRef(0);
  // The rendition outlives renders: read sections through a ref, never the mount-time list.
  const sectionsRef = useRef(model.sections);
  sectionsRef.current = model.sections;
  useEffect(() => {
    // Sections can arrive after the first relocation (book opened straight into this view).
    const sec = model.sections.find((s) => Number((s.locator as { spineIndex?: number }).spineIndex) === currentIndex.current);
    if (sec) loadSection(sec.id);
  }, [model.sections, loadSection]);
  const onScrolledRef = useRef(onScrolled);
  onScrolledRef.current = onScrolled;

  useEffect(() => {
    if (!host.current) return;
    const b = ePub(packageUrl(session), { openAs: 'opf' });
    book.current = b;
    const r = b.renderTo(host.current, {
      width: '100%',
      height: '100%',
      flow: flow === 'paged' ? 'paginated' : 'scrolled-doc',
      spread: flow === 'paged' && columns === 2 ? 'always' : 'none',
      minSpreadWidth: 0,
      allowScriptedContent: false,
      allowPopups: false,
      // WKWebView does not load sandboxed srcdoc documents reliably; blob URLs do load.
      method: 'blobUrl',
    } as Parameters<Book['renderTo']>[1]);
    rendition.current = r;
    // The document CSP keeps base-uri 'none', so epub.js' injected <base> is ignored.
    // Make resource references absolute (attributes only; text nodes/CFIs untouched).
    const assetBase = `thoth-doc://localhost/${session.assetToken}/epub/`;
    (b.spine as unknown as { hooks: { serialize: { register: (fn: (out: string, section: { output: string }) => void) => void } } }).hooks.serialize.register((_out, section) => {
      section.output = absolutizeResourceRefs(section.output, assetBase);
    });
    r.themes.default({
      body: { 'font-size': 'var(--reader-size, 17px)', 'line-height': 'var(--reader-line, 1.6)' },
      ...Object.fromEntries(HIGHLIGHT_COLORS.map((c) => [`::highlight(thoth-${c})`, { 'background-color': HIGHLIGHT_CSS[c]! }])),
    });
    r.on('rendered', () => setRenderTick((t) => t + 1));
    r.on('relocated', (loc: { start: { cfi: string; index: number } }) => {
      currentCfi.current = loc.start.cfi;
      const sec = sectionsRef.current.find((s) => Number((s.locator as { spineIndex?: number }).spineIndex) === loc.start.index);
      currentIndex.current = loc.start.index;
      if (sec) loadSection(sec.id);
      // Programmatic display() also relocates; only user scrolling moves the anchor.
      if (performance.now() > programmaticUntil.current) onScrolledRef.current();
    });
    // Links: see the link poll below (listeners inside the frame never run).
    r.hooks.content.register((contents: { document: Document }) => { armLinks(contents.document); });
    // Changing the layout re-creates the rendition: come back to where the reader was.
    const resume = currentCfi.current;
    const start = session.position?.blockId ? allBlocks.current.find((x) => x.id === session.position?.blockId) : undefined;
    b.opened.catch((e: unknown) => console.error('epub open failed', String((e as Error)?.message ?? e)));
    if (import.meta.env.DEV) {
      const dbg: Record<string, unknown> = { stage: 'created' };
      (window as unknown as { __thothEpub?: unknown }).__thothEpub = dbg;
      b.opened.then(() => { dbg.stage = 'opened'; dbg.spine = (b.spine as unknown as { length: number }).length; }).catch((e: unknown) => { dbg.openError = String((e as Error)?.message ?? e); });
      r.on('rendered', () => { dbg.rendered = ((dbg.rendered as number) ?? 0) + 1; });
      r.on('displayError', (e: unknown) => { dbg.displayError = String(e); });
    }
    // The first display() relocates too; it is not the user scrolling.
    programmaticUntil.current = performance.now() + 1500;
    pendingDisplay.current = r.display(resume ?? (start ? cfiStart(start) ?? undefined : undefined)).catch((e: unknown) => { console.error('epub display failed', String((e as Error)?.message ?? e)); setError(String(e)); });
    return () => {
      rendition.current = null;
      book.current = null;
      // epub.js throws if destroyed while a display() is still queued; let it settle first.
      const settle = pendingDisplay.current ?? Promise.resolve();
      void settle.catch(() => {}).then(() => { try { r.destroy(); b.destroy(); } catch { /* already torn down */ } });
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.assetToken, flow, columns]);

  /** DOM range of a block in the rendered chapter, or null (not rendered / a point location). */
  const blockRange = (b: Block): Range | null => {
    const r = rendition.current;
    if (!r || b.location.kind !== 'epub' || !b.location.cfiRange.includes(',')) return null;
    try {
      return (r as unknown as { getRange: (c: string) => Range | null }).getRange(b.location.cfiRange);
    } catch {
      return null;
    }
  };
  const spineBlocks = () => {
    const idx = currentIndex.current;
    return allBlocks.current.filter((b) => b.location.kind === 'epub' && b.location.spineIndex === idx).sort((x, y) => x.ordinal - y.ordinal);
  };

  // A selection inside the book's iframe → the shared selection toolbar.
  const reportSelection = useRef<(doc: Document, win: Window) => void>(() => {});
  reportSelection.current = (doc, win) => {
    const send = (detail: ExternalSelection) => window.dispatchEvent(new CustomEvent(EXTERNAL_SELECTION, { detail }));
    const sel = win.getSelection();
    const clear = () => { try { win.getSelection()?.removeAllRanges(); } catch { /* frame gone */ } };
    if (!sel || sel.isCollapsed || sel.rangeCount === 0) { send({ target: null, clear }); return; }
    const range = sel.getRangeAt(0);
    const quote = sel.toString().replace(/\s+/gu, ' ').trim();
    const q = ink(quote);
    if (!q || quote.length > 4000) { send({ target: null, clear }); return; }
    // The paragraph that contains the whole selection.
    let hit: { b: Block; br: Range } | null = null;
    for (const b of spineBlocks()) {
      const br = blockRange(b);
      if (!br || br.startContainer.ownerDocument !== doc) continue;
      if (br.compareBoundaryPoints(Range.START_TO_START, range) <= 0 && br.compareBoundaryPoints(Range.END_TO_END, range) >= 0) hit = { b, br };
      else if (hit && br.compareBoundaryPoints(Range.START_TO_START, range) > 0) break;
    }
    const frame = host.current?.querySelector<HTMLIFrameElement>('iframe');
    if (import.meta.env.DEV) (window as unknown as { __thothSel?: unknown }).__thothSel = { blocks: spineBlocks().length, ranged: spineBlocks().filter((b) => !!blockRange(b)).length, hit: !!hit, frame: !!frame, idx: currentIndex.current };
    if (!hit || !frame) { send({ target: null, clear }); return; }
    const occurrence = countOccurrences(inkOfRange(hit.br).text, q, inkBefore(hit.br, range.startContainer, range.startOffset));
    const r = range.getBoundingClientRect();
    const fr = frame.getBoundingClientRect();
    send({ target: { blockId: hit.b.id, side: 'source', quote, occurrence, rect: new DOMRect(fr.left + r.left, fr.top + r.top, r.width, r.height) }, clear });
  };

  // The book's iframe is sandboxed without scripts, and WebKit then runs no event listeners
  // in it — not even ones attached from here. So the frame is observed from outside:
  // its selection is polled, and keyboard focus is handed back to this view so page keys
  // and shortcuts keep working after a click in the text.
  const rootRef = useRef<HTMLDivElement>(null);
  const lastHash = useRef('');
  const followLink = useRef<(l: { href: string; external: boolean }) => void>(() => {});
  followLink.current = (l) => {
    if (l.external) { if (/^https?:\/\//i.test(l.href)) openExternal(l.href); return; }
    const to = parseSpineLink(l.href);
    const item = to && (book.current?.spine as unknown as { get: (i: number) => { href: string } | null } | undefined)?.get(to.index);
    if (!to || !item) return;
    // A link is the reader moving: let the relocation update the reading position.
    void rendition.current?.display(to.fragment ? `${item.href}#${to.fragment}` : item.href).catch(() => {});
  };
  useEffect(() => {
    let last = '';
    const t = window.setInterval(() => {
      const frame = host.current?.querySelector<HTMLIFrameElement>('iframe');
      const win = frame?.contentWindow;
      const doc = frame?.contentDocument;
      if (!frame || !win || !doc) return;
      if (document.activeElement === frame) rootRef.current?.focus({ preventScroll: true });
      armLinks(doc);
      const hash = win.location.hash;
      if (hash !== lastHash.current) {
        lastHash.current = hash;
        const link = takeClickedLink(doc, hash);
        if (link) followLink.current(link);
      }
      const sel = win.getSelection();
      const sig = sel && !sel.isCollapsed && sel.rangeCount ? `${sel.toString().length}:${sel.anchorOffset}:${sel.focusOffset}:${sel.toString().slice(0, 24)}` : '';
      if (sig === last) return;
      last = sig;
      reportSelection.current(doc, win);
    }, 150);
    return () => window.clearInterval(t);
  }, []);
  // ⌘C copies what is selected in the book (focus lives out here, see above).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.key.toLowerCase() !== 'c' || e.shiftKey || e.altKey) return;
      if (window.getSelection()?.toString()) return; // something out here is selected: normal copy
      const text = host.current?.querySelector<HTMLIFrameElement>('iframe')?.contentWindow?.getSelection()?.toString();
      if (!text) return;
      e.preventDefault();
      void navigator.clipboard.writeText(text).catch(() => {});
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  // Paint highlights inside the book's iframe (its own highlight registry).
  useEffect(() => {
    const frame = host.current?.querySelector<HTMLIFrameElement>('iframe');
    const win = frame?.contentWindow as (Window & { CSS?: { highlights?: { set(n: string, h: unknown): void; delete(n: string): void } }; Highlight?: new (...r: Range[]) => unknown }) | null | undefined;
    const reg = win?.CSS?.highlights;
    const H = win?.Highlight;
    if (!reg || !H) return;
    const byId = new Map(allBlocks.current.map((b) => [b.id, b] as const));
    const byColor = new Map<string, Range[]>();
    for (const a of annotations) {
      if (a.side !== 'source' || !a.blockId) continue;
      const b = byId.get(a.blockId);
      if (!b || b.location.kind !== 'epub' || b.location.spineIndex !== currentIndex.current) continue;
      const br = blockRange(b);
      const found = br ? findQuoteInRange(br, a.quote, a.occurrence) : null;
      if (found) (byColor.get(a.color) ?? byColor.set(a.color, []).get(a.color)!).push(found);
    }
    for (const c of HIGHLIGHT_COLORS) {
      const rs = byColor.get(c);
      try { if (rs?.length) reg.set(`thoth-${c}`, new H(...rs)); else reg.delete(`thoth-${c}`); } catch { /* frame replaced */ }
    }
  }, [annotations, renderTick, model.blocks]); // eslint-disable-line react-hooks/exhaustive-deps

  const setFlow = useCallback((f: Flow) => { savePref(FLOW_KEY, f); setFlowState(f); }, []);
  const setColumns = useCallback((n: 1 | 2) => { savePref(COLUMNS_KEY, String(n)); setColumnsState(n); }, []);
  const turn = useCallback((dir: 1 | -1) => { if (flowRef.current === 'paged') void (dir > 0 ? rendition.current?.next() : rendition.current?.prev()); }, []);
  useShortcut('arrowright', () => turn(1), { enabled: flow === 'paged' });
  useShortcut('arrowleft', () => turn(-1), { enabled: flow === 'paged' });
  useShortcut('pagedown', () => turn(1), { enabled: flow === 'paged' });
  useShortcut('pageup', () => turn(-1), { enabled: flow === 'paged' });

  /** Viewport rect of a block inside the epub.js iframe (via its canonical CFI range). */
  const blockRect = (b: Block): { top: number; height: number } | null => {
    const r = rendition.current;
    const frame = host.current?.querySelector<HTMLIFrameElement>('iframe');
    if (!r || !frame || b.location.kind !== 'epub') return null;
    try {
      const cfi = b.location.cfiRange.includes(',') ? b.location.cfiRange : cfiStart(b);
      const range = cfi ? (r as unknown as { getRange: (c: string) => Range | null }).getRange(cfi) : null;
      if (!range) return null;
      let rect = range.getBoundingClientRect();
      if (rect.height === 0 && rect.width === 0) {
        const n = range.startContainer.nodeType === 1 ? (range.startContainer as Element) : range.startContainer.parentElement;
        if (!n) return null;
        rect = n.getBoundingClientRect();
      }
      const fr = frame.getBoundingClientRect();
      return { top: fr.top + rect.top, height: rect.height };
    } catch {
      return null;
    }
  };
  const container = () => host.current?.querySelector<HTMLElement>('.epub-container') ?? null;

  useImperativeHandle(ref, () => ({
    getAnchor() {
      const c = container();
      const idx = currentIndex.current;
      // Pages have no reading line: the anchor is the paragraph the page starts in.
      if (c && idx != null && flowRef.current !== 'paged') {
        // Blocks of the displayed spine item are in document order, so their tops are
        // monotonic: binary-search the block under the 25% reading line.
        const list = allBlocks.current.filter((b) => b.location.kind === 'epub' && b.location.spineIndex === idx).sort((x, y) => x.ordinal - y.ordinal);
        const cr = c.getBoundingClientRect();
        const line = cr.top + cr.height * 0.25;
        let lo = 0, hi = list.length - 1, best = -1;
        let bestRect: { top: number; height: number } | null = null;
        while (lo <= hi) {
          const mid = (lo + hi) >> 1;
          const rect = blockRect(list[mid]!);
          if (!rect) { lo = mid + 1; continue; }
          if (rect.top <= line) { best = mid; bestRect = rect; lo = mid + 1; } else hi = mid - 1;
        }
        if (best >= 0 && bestRect) {
          const ratio = bestRect.height > 0 ? Math.min(1, Math.max(0, (line - bestRect.top) / bestRect.height)) : 0;
          return { blockId: list[best]!.id, ratio };
        }
        if (list[0]) return { blockId: list[0].id, ratio: 0 };
      }
      const cfi = currentCfi.current;
      if (!cfi) return null;
      const b = blockForCfi(allBlocks.current, cfi);
      return b ? { blockId: b.id, ratio: 0 } : null;
    },
    scrollToBlock(blockId, ratio = 0) {
      const b = allBlocks.current.find((x) => x.id === blockId);
      const cfi = b ? cfiStart(b) : null;
      if (!b || !cfi || !rendition.current) return false;
      programmaticUntil.current = performance.now() + 1500;
      pendingDisplay.current = rendition.current.display(cfi).then(() => {
        if (!rendition.current) return;
        // display() puts the CFI at the top; move it to the 25% reading line.
        const align = () => {
          if (flowRef.current === 'paged') return; // display() already turned to its page
          const c = container();
          const rect = blockRect(b);
          if (!c || !rect) return;
          const cr = c.getBoundingClientRect();
          programmaticUntil.current = performance.now() + 800;
          c.scrollTop += rect.top + rect.height * ratio - (cr.top + cr.height * 0.25);
        };
        align();
        setTimeout(align, 250); // after images/fonts settle
      }).catch(() => {});
      return true;
    },
    anchorErrorPx(blockId, ratio) {
      const b = allBlocks.current.find((x) => x.id === blockId);
      const c = container();
      const rect = b ? blockRect(b) : null;
      if (!c || !rect) return null;
      const cr = c.getBoundingClientRect();
      if (flowRef.current === 'paged') {
        // on the page shown = no error
        const frame = host.current?.querySelector<HTMLIFrameElement>('iframe');
        const range = b ? blockRange(b) : null;
        if (!frame || !range) return null;
        const fr = frame.getBoundingClientRect();
        const x = fr.left + range.getBoundingClientRect().left;
        return x >= cr.left - 1 && x <= cr.right + 1 ? 0 : Math.abs(x - cr.left);
      }
      const err = Math.abs(rect.top + rect.height * ratio - (cr.top + cr.height * 0.25));
      const clamped = c.scrollTop <= 1 || c.scrollTop + c.clientHeight >= c.scrollHeight - 1;
      return clamped && rect.top + rect.height > cr.top && rect.top < cr.bottom ? 0 : err;
    },
    scrollToSection(sectionId) {
      const s = model.sections.find((x) => x.id === sectionId);
      const idx = Number((s?.locator as { spineIndex?: number } | undefined)?.spineIndex);
      if (Number.isFinite(idx)) void rendition.current?.display(idx);
    },
  }), [model.sections]);

  return (
    <div ref={rootRef} tabIndex={-1} className="epub-original" data-asset-token={import.meta.env.DEV ? session.assetToken : undefined} onMouseMove={(e) => { const r = e.currentTarget.getBoundingClientRect(); const n = r.bottom - e.clientY < 110; setNear((v) => (v === n ? v : n)); }} onMouseLeave={() => setNear(false)}>
      {error && <p className="banner warn">원본 보기를 표시할 수 없습니다. 병렬 보기에서 읽을 수 있습니다.</p>}
      <div ref={host} className={`epub-host ${flow}`} />
      {flow === 'paged' && (
        <>
          <button className="epub-turn prev" onClick={() => turn(-1)} aria-label="이전 쪽" title="이전 쪽 (←)">‹</button>
          <button className="epub-turn next" onClick={() => turn(1)} aria-label="다음 쪽" title="다음 쪽 (→)">›</button>
        </>
      )}
      {/* The iframe swallows mouse moves: this strip is what notices the pointer at the bottom. */}
      <div className="epub-hover-strip" onMouseEnter={() => setNear(true)} onMouseLeave={(e) => { if (!(e.relatedTarget instanceof Node) || !e.currentTarget.parentElement?.querySelector('.epub-controls')?.contains(e.relatedTarget)) setNear(false); }} aria-hidden="true" />
      <div className={`pdf-controls epub-controls ${near ? 'near' : ''}`} role="toolbar" aria-label="EPUB 보기 도구" onMouseLeave={() => setNear(false)}>
        <div className="seg" role="group" aria-label="넘김 방식">
          <button aria-pressed={flow === 'scrolled'} onClick={() => setFlow('scrolled')}>스크롤</button>
          <button aria-pressed={flow === 'paged'} onClick={() => setFlow('paged')}>쪽 넘김</button>
        </div>
        {flow === 'paged' && (
          <div className="seg" role="group" aria-label="단 수">
            <button aria-pressed={columns === 1} onClick={() => setColumns(1)}>1단</button>
            <button aria-pressed={columns === 2} onClick={() => setColumns(2)}>2단</button>
          </div>
        )}
      </div>
    </div>
  );
});
