// Parallel / interleaved / source-only reflow list for EPUB and PDF reflow (spec §1.1, §3.4).
// Virtualized (~150 rows kept), visibility via IntersectionObserver with 150ms debounce.
import { forwardRef, useCallback, useEffect, useImperativeHandle, useMemo, useRef } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import type { Block } from '../../ipc/generated/Block';
import type { SectionRow } from '../../ipc/generated/SectionRow';
import type { ReaderModel } from './useReaderSession';
import { SafeContentView, type ContentResolvers } from './SafeContentView';
import { TranslationCell, type CellActions } from '../translation/TranslationCell';
import { Debouncer, isVisible, pickAnchor, scrollTopForAnchor, ScrollOrigin, attachUserIntent, VISIBLE_DEBOUNCE_MS } from '../../domain/visibility';

export type ReflowMode = 'parallel' | 'interleaved' | 'original';

type Row =
  | { kind: 'block'; key: string; block: Block; sectionId: string }
  | { kind: 'placeholder'; key: string; section: SectionRow }
  | { kind: 'section-break'; key: string; section: SectionRow };

export interface ReflowHandle {
  getAnchor: () => { blockId: string; ratio: number } | null;
  scrollToBlock: (blockId: string, ratio?: number, highlight?: boolean) => boolean;
  scrollToSection: (sectionId: string) => void;
  /** Distance (CSS px) between the anchor point and the 25% reading line; 0 when the
   *  scroll range is clamped and the block is on screen; null when not rendered. */
  anchorErrorPx: (blockId: string, ratio: number) => number | null;
}

function isLeaf(b: Block, parents: Set<string>) {
  return !parents.has(b.id);
}

export const ReflowView = forwardRef<ReflowHandle, {
  model: ReaderModel;
  mode: ReflowMode;
  loadSection: (id: string) => void;
  onVisible: (ids: string[], direction: 'forward' | 'backward') => void;
  onScrolled: () => void;
  resolvers: ContentResolvers;
  actions: CellActions;
  autoActive: boolean;
  targetLanguage: string;
  sourceLanguage: string;
  highlightBlockId: string | null;
}>(function ReflowView(props, ref) {
  const { model, mode, loadSection, onVisible, onScrolled, resolvers, actions, autoActive, targetLanguage, sourceLanguage, highlightBlockId } = props;
  const scrollRef = useRef<HTMLDivElement>(null);
  const origin = useRef(new ScrollOrigin());
  const lastTop = useRef(0);
  const direction = useRef<'forward' | 'backward'>('forward');

  const rows: Row[] = useMemo(() => {
    const out: Row[] = [];
    for (const s of model.sections) {
      if (s.state === 'failed') {
        out.push({ kind: 'section-break', key: `f-${s.id}`, section: s });
        continue;
      }
      const bs = model.blocks.get(s.id);
      if (!bs) {
        if (s.state === 'ready' && s.blockCount > 0) out.push({ kind: 'placeholder', key: `p-${s.id}`, section: s });
        else if (s.state !== 'ready') out.push({ kind: 'placeholder', key: `p-${s.id}`, section: s });
        continue;
      }
      const parents = new Set(bs.map((b) => b.parentId).filter((x): x is string => !!x));
      for (const b of bs) if (isLeaf(b, parents) && !b.roles.includes('running_header') && !b.roles.includes('running_footer')) out.push({ kind: 'block', key: b.id, block: b, sectionId: s.id });
    }
    return out;
  }, [model.sections, model.blocks]);

  const indexById = useMemo(() => {
    const m = new Map<string, number>();
    rows.forEach((r, i) => { if (r.kind === 'block') m.set(r.block.id, i); });
    return m;
  }, [rows]);

  const virt = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: (i) => {
      const r = rows[i];
      if (!r) return 80;
      if (r.kind === 'placeholder') return Math.max(120, Math.min(4000, Number(r.section.blockCount) * 90));
      if (r.kind === 'section-break') return 60;
      return Math.max(48, Math.ceil(r.block.sourceText.length / 70) * 26 * (mode === 'interleaved' ? 2 : 1) + 24);
    },
    overscan: 8,
    getItemKey: (i) => rows[i]?.key ?? i,
  });

  // Lazy-load sections whose placeholders are rendered.
  const items = virt.getVirtualItems();
  useEffect(() => {
    for (const it of items) {
      const r = rows[it.index];
      if (r?.kind === 'placeholder' && r.section.state === 'ready') loadSection(r.section.id);
    }
  }, [items, rows, loadSection]);

  // Visibility reporting: compute from the rendered rows (viewport ∩ row boxes).
  const debouncer = useRef(new Debouncer(VISIBLE_DEBOUNCE_MS));
  const reportVisible = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    const vp = el.getBoundingClientRect();
    const ids: string[] = [];
    el.querySelectorAll<HTMLElement>('[data-block-id]').forEach((node) => {
      const r = node.getBoundingClientRect();
      if (isVisible({ top: r.top, bottom: r.bottom, height: r.height }, { top: vp.top, bottom: vp.bottom, height: vp.height })) ids.push(node.dataset.blockId!);
    });
    onVisible(ids.slice(0, 100), direction.current);
  }, [onVisible]);

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const io = new IntersectionObserver(() => debouncer.current.run(reportVisible), { root: el, threshold: [0, 0.1, 0.5, 1] });
    el.querySelectorAll('[data-block-id]').forEach((n) => io.observe(n));
    debouncer.current.run(reportVisible);
    return () => io.disconnect();
  }, [items, reportVisible, mode]);

  // Scroll anchoring (spec §3.4): remember the block under the reading line and its
  // on-screen offset; when rows resize without user input (translations streaming in,
  // images/fonts loading, sections measured) shift scrollTop so that block stays put.
  const sticky = useRef<{ id: string; offset: number } | null>(null);
  const alignToken = useRef(0);
  const lastUserScroll = useRef(0);
  const captureSticky = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    const vp = el.getBoundingClientRect();
    const cands: { id: string; top: number; bottom: number }[] = [];
    el.querySelectorAll<HTMLElement>('[data-block-id]').forEach((n) => {
      const r = n.getBoundingClientRect();
      cands.push({ id: n.dataset.blockId!, top: r.top - vp.top, bottom: r.bottom - vp.top });
    });
    const a = pickAnchor(cands, 0, vp.height);
    const c = a && cands.find((x) => x.id === a.id);
    if (a && c) sticky.current = { id: a.id, offset: c.top };
  }, []);
  const innerRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = scrollRef.current;
    const inner = innerRef.current;
    if (!el || !inner) return;
    let raf = 0;
    const ro = new ResizeObserver(() => {
      // Adjust in the next frame: changing scroll inside the RO callback re-triggers it.
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(() => {
        const st = sticky.current;
        if (!st || performance.now() - lastUserScroll.current < 250) return;
        const node = el.querySelector<HTMLElement>(`[data-block-id="${CSS.escape(st.id)}"]`);
        if (!node) return;
        const delta = node.getBoundingClientRect().top - el.getBoundingClientRect().top - st.offset;
        if (Math.abs(delta) > 1) {
          origin.current.markProgrammatic(150);
          el.scrollTop += delta;
        }
      });
    });
    ro.observe(inner);
    inner.querySelectorAll('.row-wrap').forEach((r) => ro.observe(r));
    return () => { cancelAnimationFrame(raf); ro.disconnect(); };
  }, [items]);

  useEffect(() => {
    const el = scrollRef.current;
    return el ? attachUserIntent(el, origin.current) : undefined;
  }, []);

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    direction.current = el.scrollTop >= lastTop.current ? 'forward' : 'backward';
    lastTop.current = el.scrollTop;
    if (origin.current.classify() === 'user') {
      lastUserScroll.current = performance.now();
      onScrolled();
      debouncer.current.run(() => { captureSticky(); reportVisible(); });
    } else {
      debouncer.current.run(reportVisible);
    }
  };

  useImperativeHandle(ref, () => ({
    getAnchor() {
      const el = scrollRef.current;
      if (!el) return null;
      const vp = el.getBoundingClientRect();
      const cands: { id: string; top: number; bottom: number }[] = [];
      el.querySelectorAll<HTMLElement>('[data-block-id]').forEach((n) => {
        const r = n.querySelector<HTMLElement>('.src')?.getBoundingClientRect() ?? n.getBoundingClientRect();
        cands.push({ id: n.dataset.blockId!, top: r.top - vp.top, bottom: r.bottom - vp.top });
      });
      // At the very top the 25% line cannot be honoured after a clamped restore; the
      // reading position is then the first visible block.
      if (el.scrollTop <= 1) {
        const first = cands.filter((c) => c.bottom > 0).sort((x, y) => x.top - y.top)[0];
        if (first) return { blockId: first.id, ratio: 0 };
      }
      const a = pickAnchor(cands, 0, vp.height);
      return a ? { blockId: a.id, ratio: a.ratio } : null;
    },
    scrollToBlock(blockId, ratio = 0, _highlight = false) {
      const idx = indexById.get(blockId);
      if (idx == null) return false;
      const token = ++alignToken.current;
      const startedAt = performance.now();
      // The virtual list's own scrollToIndex keeps re-aligning the row to the top for a
      // while; so it is used only to get the row rendered, and then this loop asserts the
      // 25%-line position until measurements settle (or the user scrolls).
      const step = (n: number) => {
        const el = scrollRef.current;
        if (!el || token !== alignToken.current || lastUserScroll.current > startedAt) return;
        const node = el.querySelector<HTMLElement>(`[data-block-id="${CSS.escape(blockId)}"]`);
        if (!node) {
          origin.current.markProgrammatic(400);
          virt.scrollToIndex(idx, { align: 'start' });
        } else {
          const src = node.querySelector<HTMLElement>('.src') ?? node;
          // Rows are positioned with transform: translateY, which offsetTop ignores; use
          // layout rects relative to the scroller instead.
          const sr = src.getBoundingClientRect();
          const er = el.getBoundingClientRect();
          const target = scrollTopForAnchor(el.scrollTop + (sr.top - er.top), sr.height, ratio, el.clientHeight);
          if (Math.abs(target - el.scrollTop) > 1) {
            origin.current.markProgrammatic(200);
            el.scrollTop = target;
          }
          sticky.current = { id: blockId, offset: node.getBoundingClientRect().top - er.top - (target - el.scrollTop) };
        }
        if (n < 10) setTimeout(() => step(n + 1), n < 3 ? 40 : 90);
      };
      step(0);
      return true;
    },
    anchorErrorPx(blockId, ratio) {
      const el = scrollRef.current;
      const node = el?.querySelector<HTMLElement>(`[data-block-id="${CSS.escape(blockId)}"]`);
      if (!el || !node) return null;
      const vp = el.getBoundingClientRect();
      const r = (node.querySelector<HTMLElement>('.src') ?? node).getBoundingClientRect();
      const err = Math.abs(r.top + r.height * ratio - (vp.top + vp.height * 0.25));
      const clamped = el.scrollTop <= 1 || el.scrollTop + el.clientHeight >= el.scrollHeight - 1;
      return clamped && r.bottom > vp.top && r.top < vp.bottom ? 0 : err;
    },
    scrollToSection(sectionId) {
      const idx = rows.findIndex((r) => (r.kind === 'block' && r.sectionId === sectionId) || (r.kind !== 'block' && r.section.id === sectionId));
      if (idx >= 0) {
        origin.current.markProgrammatic(400);
        virt.scrollToIndex(idx, { align: 'start' });
      }
    },
  }), [indexById, rows, virt]);

  return (
    <div ref={scrollRef} className={`reflow mode-${mode}`} onScroll={onScroll} tabIndex={0} aria-label="본문">
      <div ref={innerRef} style={{ height: virt.getTotalSize(), position: 'relative' }}>
        {items.map((it) => {
          const r = rows[it.index]!;
          return (
            <div key={it.key} data-index={it.index} ref={virt.measureElement} className="row-wrap" style={{ position: 'absolute', top: 0, left: 0, right: 0, transform: `translateY(${it.start}px)` }}>
              {r.kind === 'placeholder' ? (
                <div className="placeholder faint">{r.section.state === 'ready' ? '불러오는 중…' : r.section.state === 'failed' ? '이 부분을 추출하지 못했습니다' : '색인 중…'}</div>
              ) : r.kind === 'section-break' ? (
                <div className="placeholder warn-text">이 페이지는 추출에 실패했습니다. 원본 보기에서 읽을 수 있습니다.</div>
              ) : (
                <BlockRow
                  block={r.block}
                  mode={mode}
                  model={model}
                  resolvers={resolvers}
                  actions={actions}
                  autoActive={autoActive}
                  targetLanguage={targetLanguage}
                  sourceLanguage={sourceLanguage}
                  highlighted={highlightBlockId === r.block.id}
                />
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
});

function BlockRow({ block, mode, model, resolvers, actions, autoActive, targetLanguage, sourceLanguage, highlighted }: {
  block: Block; mode: ReflowMode; model: ReaderModel; resolvers: ContentResolvers; actions: CellActions; autoActive: boolean; targetLanguage: string; sourceLanguage: string; highlighted: boolean;
}) {
  const Tag = block.kind === 'heading' ? 'h3' : block.kind === 'code' ? 'pre' : block.kind === 'quote' ? 'blockquote' : 'div';
  const restricted = block.quality.level === 'low' || block.translationPolicy !== 'auto';
  const srcId = `src-${block.id}`;
  const imageOnly = isImageOnly(block);
  return (
    <div className={`brow kind-${block.kind} ${highlighted ? 'hl' : ''}`} data-block-id={block.id}>
      <Tag className="src" id={srcId} lang={sourceLanguage}>
        <SafeContentView nodes={block.content.length ? block.content : [{ t: 'text', v: block.sourceText }]} resolvers={resolvers} />
        {restricted && block.quality.reasons.length > 0 && <span className="badge warn qual" title={block.quality.reasons.join(', ')}>제한</span>}
      </Tag>
      {mode === 'parallel' && imageOnly && (
        // Side-by-side: images have nothing to translate, so the same image is mirrored
        // in the translation column to keep both sides aligned. Decorative duplicate.
        <div className="trn trn-mirror" aria-hidden="true">
          <SafeContentView nodes={block.content} resolvers={resolvers} />
        </div>
      )}
      {mode !== 'original' && !imageOnly && (
        <div className="trn" aria-describedby={srcId}>
          <TranslationCell
            block={block}
            display={model.display.get(block.id)}
            job={model.jobs.get(block.id)}
            preview={model.previews.get(block.id)}
            autoActive={autoActive}
            targetLanguage={targetLanguage}
            actions={actions}
          />
        </div>
      )}
    </div>
  );
}

/** A block that shows only images/formula images (no text to translate). */
export function isImageOnly(block: Pick<Block, 'content' | 'sourceText'>): boolean {
  if (block.content.length === 0) return false;
  if (/\p{L}/u.test(block.sourceText)) return false;
  let images = 0;
  for (const n of block.content) {
    if (n.t === 'image_ref' || n.t === 'math_ref') images += 1;
    else if (n.t === 'break' || (n.t === 'text' && n.v.trim() === '')) continue;
    else return false;
  }
  return images > 0;
}
