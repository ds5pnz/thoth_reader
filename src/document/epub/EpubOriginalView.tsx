// Original (source-only) EPUB mode: epub.js renders the *sanitized* package served by the
// thoth-doc protocol. Scripts are never allowed; the iframe is sandboxed without allow-scripts.
import { forwardRef, useEffect, useImperativeHandle, useRef, useState } from 'react';
import ePub, { type Book, type Rendition } from 'epubjs';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import type { ReaderModel } from '../../features/reader/useReaderSession';
import type { Block } from '../../ipc/generated/Block';
import { blockForCfi, cfiStart } from './cfi';

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

export function packageUrl(session: ReaderSessionDto) {
  return `thoth-doc://localhost/${session.assetToken}/epub/package.opf`;
}

export const EpubOriginalView = forwardRef<EpubOriginalHandle, {
  session: ReaderSessionDto;
  model: ReaderModel;
  loadSection: (id: string) => void;
  onScrolled: () => void;
  openExternal: (url: string) => void;
}>(function EpubOriginalView({ session, model, loadSection, onScrolled, openExternal }, ref) {
  const host = useRef<HTMLDivElement>(null);
  const rendition = useRef<Rendition | null>(null);
  const book = useRef<Book | null>(null);
  const currentCfi = useRef<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const allBlocks = useRef<Block[]>([]);
  allBlocks.current = [...model.blocks.values()].flat();
  const currentIndex = useRef<number | null>(null);
  const pendingDisplay = useRef<Promise<unknown> | null>(null);
  const programmaticUntil = useRef(0);
  const onScrolledRef = useRef(onScrolled);
  onScrolledRef.current = onScrolled;

  useEffect(() => {
    if (!host.current) return;
    const b = ePub(packageUrl(session), { openAs: 'opf' });
    book.current = b;
    const r = b.renderTo(host.current, {
      width: '100%',
      height: '100%',
      flow: 'scrolled-doc',
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
    r.themes.default({ body: { 'font-size': 'var(--reader-size, 17px)', 'line-height': 'var(--reader-line, 1.6)' } });
    r.on('relocated', (loc: { start: { cfi: string; index: number } }) => {
      currentCfi.current = loc.start.cfi;
      const sec = model.sections.find((s) => Number((s.locator as { spineIndex?: number }).spineIndex) === loc.start.index);
      currentIndex.current = loc.start.index;
      if (sec) loadSection(sec.id);
      // Programmatic display() also relocates; only user scrolling moves the anchor.
      if (performance.now() > programmaticUntil.current) onScrolledRef.current();
    });
    // Links inside the document never navigate the app; external http(s) go through Rust.
    r.hooks.content.register((contents: { document: Document }) => {
      contents.document.addEventListener('click', (e) => {
        const a = (e.target as HTMLElement | null)?.closest?.('a[href]') as HTMLAnchorElement | null;
        if (!a) return;
        if (a.getAttribute('data-thoth-external') === '1') {
          e.preventDefault();
          openExternal(a.getAttribute('href') ?? '');
        }
      }, true);
    });
    const start = session.position?.blockId ? allBlocks.current.find((x) => x.id === session.position?.blockId) : undefined;
    b.opened.catch((e: unknown) => console.error('epub open failed', String((e as Error)?.message ?? e)));
    if (import.meta.env.DEV) {
      const dbg: Record<string, unknown> = { stage: 'created' };
      (window as unknown as { __thothEpub?: unknown }).__thothEpub = dbg;
      b.opened.then(() => { dbg.stage = 'opened'; dbg.spine = (b.spine as unknown as { length: number }).length; }).catch((e: unknown) => { dbg.openError = String((e as Error)?.message ?? e); });
      r.on('rendered', () => { dbg.rendered = ((dbg.rendered as number) ?? 0) + 1; });
      r.on('displayError', (e: unknown) => { dbg.displayError = String(e); });
    }
    pendingDisplay.current = r.display(start ? cfiStart(start) ?? undefined : undefined).catch((e: unknown) => { console.error('epub display failed', String((e as Error)?.message ?? e)); setError(String(e)); });
    return () => {
      rendition.current = null;
      book.current = null;
      // epub.js throws if destroyed while a display() is still queued; let it settle first.
      const settle = pendingDisplay.current ?? Promise.resolve();
      void settle.catch(() => {}).then(() => { try { r.destroy(); b.destroy(); } catch { /* already torn down */ } });
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.assetToken]);

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
      if (c && idx != null) {
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
    <div className="epub-original" data-asset-token={import.meta.env.DEV ? session.assetToken : undefined}>
      {error && <p className="banner warn">원본 보기를 표시할 수 없습니다. 병렬 보기에서 읽을 수 있습니다.</p>}
      <div ref={host} className="epub-host" />
    </div>
  );
});
