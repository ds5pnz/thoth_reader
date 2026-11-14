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
  scrollToBlock: (blockId: string) => boolean;
  scrollToSection: (sectionId: string) => void;
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
    } as Parameters<Book['renderTo']>[1]);
    rendition.current = r;
    r.themes.default({ body: { 'font-size': 'var(--reader-size, 17px)', 'line-height': 'var(--reader-line, 1.6)' } });
    r.on('relocated', (loc: { start: { cfi: string; index: number } }) => {
      currentCfi.current = loc.start.cfi;
      const sec = model.sections.find((s) => Number((s.locator as { spineIndex?: number }).spineIndex) === loc.start.index);
      if (sec) loadSection(sec.id);
      onScrolledRef.current();
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
    r.display(start ? cfiStart(start) ?? undefined : undefined).catch((e: unknown) => setError(String(e)));
    return () => {
      r.destroy();
      b.destroy();
      rendition.current = null;
      book.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.assetToken]);

  useImperativeHandle(ref, () => ({
    getAnchor() {
      const cfi = currentCfi.current;
      if (!cfi) return null;
      const b = blockForCfi(allBlocks.current, cfi);
      return b ? { blockId: b.id, ratio: 0 } : null;
    },
    scrollToBlock(blockId) {
      const b = allBlocks.current.find((x) => x.id === blockId);
      const cfi = b ? cfiStart(b) : null;
      if (!cfi || !rendition.current) return false;
      void rendition.current.display(cfi);
      return true;
    },
    scrollToSection(sectionId) {
      const s = model.sections.find((x) => x.id === sectionId);
      const idx = Number((s?.locator as { spineIndex?: number } | undefined)?.spineIndex);
      if (Number.isFinite(idx)) void rendition.current?.display(idx);
    },
  }), [model.sections]);

  return (
    <div className="epub-original">
      {error && <p className="banner warn">원본 보기를 표시할 수 없습니다. 병렬 보기에서 읽을 수 있습니다.</p>}
      <div ref={host} className="epub-host" />
    </div>
  );
});
