// Highlights and notes: creation from a text selection, painting with the CSS Custom
// Highlight API (no DOM changes, so virtualized rows and the PDF text layer are untouched),
// and the sidebar list.
import { useCallback, useEffect, useRef, useState, type RefObject } from 'react';
import { call, errorMessage, mutate } from '../../ipc/client';
import type { AnnotationRow } from '../../ipc/generated/AnnotationRow';
import type { Block } from '../../ipc/generated/Block';
import { useToast } from '../../app/toast';
import { countOccurrences, findQuoteRange, ink, inkOf, inkPosition } from './quote';

export const HIGHLIGHT_COLORS = ['yellow', 'green', 'blue', 'pink'] as const;
export type HighlightColor = (typeof HIGHLIGHT_COLORS)[number];
const COLOR_LABEL: Record<HighlightColor, string> = { yellow: '노랑', green: '초록', blue: '파랑', pink: '분홍' };

export interface NewAnnotation { blockId: string; side: 'source' | 'translation'; quote: string; occurrence: number; color: HighlightColor; note: string }

export function useAnnotations(documentId: string | undefined, targetLanguage: string) {
  const [items, setItems] = useState<AnnotationRow[]>([]);
  const toast = useToast();
  const reload = useCallback(async () => {
    if (!documentId) return;
    setItems(await call<AnnotationRow[]>('annotation_list', { documentId }).catch(() => []));
  }, [documentId]);
  useEffect(() => { void reload(); }, [reload]);
  const create = useCallback(async (a: NewAnnotation) => {
    if (!documentId) return;
    try {
      await mutate('annotation_put', { documentId, annotationId: null, blockId: a.blockId, side: a.side, targetLanguage: a.side === 'translation' ? targetLanguage : null, quote: a.quote, occurrence: a.occurrence, color: a.color, note: a.note });
      await reload();
    } catch (e) {
      toast({ kind: 'error', text: errorMessage(e) });
    }
  }, [documentId, targetLanguage, reload, toast]);
  const update = useCallback(async (a: AnnotationRow, patch: { color?: string; note?: string }) => {
    if (!documentId) return;
    try {
      await mutate('annotation_put', { documentId, annotationId: a.id, blockId: a.blockId ?? '00000000-0000-0000-0000-000000000000', side: a.side, targetLanguage: a.targetLanguage, quote: a.quote, occurrence: a.occurrence, color: patch.color ?? a.color, note: patch.note ?? a.note });
      await reload();
    } catch (e) {
      toast({ kind: 'error', text: errorMessage(e) });
    }
  }, [documentId, reload, toast]);
  const remove = useCallback(async (a: AnnotationRow) => {
    if (!documentId) return;
    try {
      await mutate('annotation_delete', { documentId, annotationId: a.id });
      await reload();
    } catch (e) {
      toast({ kind: 'error', text: errorMessage(e) });
    }
  }, [documentId, reload, toast]);
  return { items, create, update, remove, reload };
}

type HighlightRegistry = { set(name: string, h: unknown): void; delete(name: string): void };
const registry = (): HighlightRegistry | null => (typeof CSS !== 'undefined' && 'highlights' in CSS ? (CSS as unknown as { highlights: HighlightRegistry }).highlights : null);
const HighlightCtor = (): (new (...r: Range[]) => unknown) | null => (globalThis as { Highlight?: new (...r: Range[]) => unknown }).Highlight ?? null;

const sel = (id: string) => (typeof CSS !== 'undefined' && CSS.escape ? CSS.escape(id) : id);
const pageOf = (a: AnnotationRow): number | null => {
  const l = a.location as { kind?: string; pageIndex?: number } | null;
  return l?.kind === 'pdf' && typeof l.pageIndex === 'number' ? l.pageIndex : null;
};

/** Occurrences of `q` in the blocks that precede `blockId` on its page. */
function occurrencesBefore(blocks: readonly Block[], blockId: string, q: string): number {
  let n = 0;
  for (const b of blocks) {
    if (b.id === blockId) break;
    n += countOccurrences(ink(b.sourceText), q);
  }
  return n;
}

/** Keeps the registered highlight ranges in step with what is currently rendered. */
export function useHighlightPainter(root: RefObject<HTMLElement | null>, items: readonly AnnotationRow[], pageBlocks: (page: number) => readonly Block[], mountKey?: unknown, extra?: { name: string; ranges: () => Range[] }) {
  const latest = useRef({ items, pageBlocks, extra });
  latest.current = { items, pageBlocks, extra };
  useEffect(() => {
    const el = root.current;
    const reg = registry();
    const H = HighlightCtor();
    if (!el || !reg || !H) return;
    let timer = 0;
    const paint = () => {
      const byColor = new Map<string, Range[]>();
      const add = (color: string, r: Range | null) => { if (r) (byColor.get(color) ?? byColor.set(color, []).get(color)!).push(r); };
      for (const a of latest.current.items) {
        if (!a.blockId) continue;
        const id = sel(a.blockId);
        if (a.side === 'source') {
          const src = el.querySelector(`.brow[data-block-id="${id}"] > .src`);
          if (src) add(a.color, findQuoteRange(src, a.quote, a.occurrence));
          const page = pageOf(a);
          const layer = page == null ? null : el.querySelector(`.pdf-page[data-page="${page}"]:not(.translated) .textLayer`);
          if (layer && layer.childNodes.length) add(a.color, findQuoteRange(layer, a.quote, a.occurrence, occurrencesBefore(latest.current.pageBlocks(page!), a.blockId, ink(a.quote))));
        } else {
          for (const node of el.querySelectorAll(`.brow[data-block-id="${id}"] .tcell-text:not(.preview), .tp-text[data-block-id="${id}"]`)) add(a.color, findQuoteRange(node, a.quote, a.occurrence));
        }
      }
      for (const c of HIGHLIGHT_COLORS) {
        const rs = byColor.get(c);
        if (rs?.length) reg.set(`thoth-${c}`, new H(...rs)); else reg.delete(`thoth-${c}`);
      }
      const x = latest.current.extra;
      if (x) {
        const rs = x.ranges();
        if (rs.length) reg.set(x.name, new H(...rs)); else reg.delete(x.name);
      }
    };
    const schedule = () => { window.clearTimeout(timer); timer = window.setTimeout(paint, 180); };
    const mo = new MutationObserver(schedule);
    mo.observe(el, { subtree: true, childList: true, characterData: true });
    el.addEventListener('scroll', schedule, { capture: true, passive: true });
    schedule();
    return () => {
      window.clearTimeout(timer);
      mo.disconnect();
      el.removeEventListener('scroll', schedule, { capture: true });
      for (const c of HIGHLIGHT_COLORS) reg.delete(`thoth-${c}`);
      if (latest.current.extra) reg.delete(latest.current.extra.name);
    };
  }, [root, mountKey]);
  // repaint when the list changes
  useEffect(() => {
    const el = root.current;
    if (!el) return;
    el.dispatchEvent(new Event('scroll'));
  }, [items, root, extra, mountKey]);
}

export interface SelectionTarget { blockId: string; side: 'source' | 'translation'; quote: string; occurrence: number; rect: DOMRect }

/** A selection made somewhere this document cannot see (the EPUB iframe). */
export const EXTERNAL_SELECTION = 'thoth:selection';
export interface ExternalSelection { target: SelectionTarget | null; clear: () => void }

/** What the current text selection refers to, if it lies inside one paragraph. */
export function selectionTarget(root: HTMLElement, pageBlocks: (page: number) => readonly Block[]): SelectionTarget | null {
  const s = window.getSelection();
  if (!s || s.isCollapsed || s.rangeCount === 0) return null;
  const range = s.getRangeAt(0);
  const quote = s.toString().replace(/\s+/gu, ' ').trim();
  const q = ink(quote);
  if (q.length < 1 || quote.length > 4000) return null;
  const startEl = (range.startContainer.nodeType === 1 ? range.startContainer : range.startContainer.parentElement) as Element | null;
  if (!startEl || !root.contains(startEl)) return null;
  const rect = range.getBoundingClientRect();
  const within = (container: Element | null) => !!container && container.contains(range.endContainer);
  const layer = startEl.closest('.pdf-page:not(.translated) .textLayer');
  if (layer) {
    if (!within(layer)) return null;
    const page = Number((layer.closest('.pdf-page') as HTMLElement | null)?.dataset.page ?? -1);
    const blocks = pageBlocks(page);
    const pageIndex = countOccurrences(inkOf(layer).text, q, inkPosition(layer, range.startContainer, range.startOffset));
    let seen = 0;
    for (const b of blocks) {
      const n = countOccurrences(ink(b.sourceText), q);
      if (pageIndex < seen + n) return { blockId: b.id, side: 'source', quote, occurrence: pageIndex - seen, rect };
      seen += n;
    }
    // The text layer and the extracted paragraphs disagree on order: take the first paragraph that has it.
    const any = blocks.find((b) => ink(b.sourceText).includes(q));
    return any ? { blockId: any.id, side: 'source', quote, occurrence: 0, rect } : null;
  }
  const row = startEl.closest<HTMLElement>('.brow[data-block-id]');
  const overlay = startEl.closest<HTMLElement>('.tp-text[data-block-id]');
  const src = startEl.closest('.src');
  const trn = startEl.closest('.tcell-text:not(.preview)') ?? overlay;
  const container = src ?? trn;
  const blockId = row?.dataset.blockId ?? overlay?.dataset.blockId;
  if (!container || !blockId || !within(container)) return null;
  const occurrence = countOccurrences(inkOf(container).text, q, inkPosition(container, range.startContainer, range.startOffset));
  return { blockId, side: src ? 'source' : 'translation', quote, occurrence, rect };
}

/** Floating bar over a selection: highlight in a colour, optionally with a note. */
/** Put text on the clipboard (async API first; a hidden textarea where that is refused). */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.setAttribute('readonly', '');
    ta.style.cssText = 'position:fixed;left:-9999px;top:0;opacity:0';
    document.body.appendChild(ta);
    ta.select();
    let ok = false;
    try { ok = document.execCommand('copy'); } catch { ok = false; }
    ta.remove();
    return ok;
  }
}

export function SelectionToolbar({ root, pageBlocks, onCreate, onTranslate, paragraphPair }: {
  root: RefObject<HTMLElement | null>;
  pageBlocks: (page: number) => readonly Block[];
  onCreate: (a: NewAnnotation) => void;
  /** Translate just the selected words. */
  onTranslate?: (text: string) => Promise<string>;
  /** Source and translation of the paragraph the selection is in, for "copy both". */
  paragraphPair?: (blockId: string) => { source: string; translation: string } | null;
}) {
  const toast = useToast();
  const [result, setResult] = useState<{ state: 'busy' | 'done' | 'error'; text: string } | null>(null);
  const [target, setTarget] = useState<SelectionTarget | null>(null);
  const [noting, setNoting] = useState(false);
  const [note, setNote] = useState('');
  const barRef = useRef<HTMLDivElement>(null);
  const keep = useRef(false);
  const external = useRef<ExternalSelection | null>(null);
  keep.current = result !== null;
  const pb = useRef(pageBlocks);
  pb.current = pageBlocks;
  useEffect(() => {
    let timer = 0;
    const update = () => {
      window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        if (barRef.current?.contains(document.activeElement)) return; // typing a note
        if (keep.current) return; // showing a translation of the (now cleared) selection
        const el = root.current;
        const own = el ? selectionTarget(el, pb.current) : null;
        if (!own && external.current) return; // the iframe's selection is still there
        external.current = null;
        setTarget(own);
        setNoting(false);
        setNote('');
        setResult(null);
      }, 180);
    };
    const onExternal = (e: Event) => {
      const d = (e as CustomEvent<ExternalSelection>).detail;
      if (barRef.current?.contains(document.activeElement) || keep.current) return;
      external.current = d.target ? d : null;
      setTarget(d.target);
      setNoting(false);
      setNote('');
    };
    document.addEventListener('selectionchange', update);
    window.addEventListener(EXTERNAL_SELECTION, onExternal);
    return () => { window.clearTimeout(timer); document.removeEventListener('selectionchange', update); window.removeEventListener(EXTERNAL_SELECTION, onExternal); };
  }, [root]);
  if (!target) return null;
  const close = () => { setResult(null); keep.current = false; external.current?.clear(); external.current = null; window.getSelection()?.removeAllRanges(); setTarget(null); };
  const copied = (ok: boolean) => toast({ kind: ok ? 'info' : 'error', text: ok ? '복사했습니다.' : '복사하지 못했습니다.' });
  const pair = paragraphPair?.(target.blockId) ?? null;
  const translate = async () => {
    if (!onTranslate) return;
    setResult({ state: 'busy', text: '' });
    try { setResult({ state: 'done', text: await onTranslate(target.quote) }); } catch (e) { setResult({ state: 'error', text: errorMessage(e) }); }
  };
  const make = (color: HighlightColor, withNote = '') => { onCreate({ blockId: target.blockId, side: target.side, quote: target.quote, occurrence: target.occurrence, color, note: withNote }); close(); };
  const top = Math.max(8, target.rect.top - 44);
  const left = Math.min(window.innerWidth - 300, Math.max(8, target.rect.left + target.rect.width / 2 - 140));
  return (
    <div ref={barRef} className={`sel-bar ${result ? 'with-result' : ''}`} role="toolbar" aria-label="선택한 글" style={{ top: result ? Math.max(8, target.rect.top - 44 - 8) : top, left }} onMouseDown={(e) => { if ((e.target as HTMLElement).tagName !== 'INPUT') e.preventDefault(); }}>
      {result ? (
        <div className="sel-result" aria-live="polite">
          <p className={result.state === 'error' ? 'warn-text' : undefined}>{result.state === 'busy' ? '번역 중…' : result.text}</p>
          <div className="note-actions">
            <span className="spacer" />
            {result.state === 'done' && <button className="btn small" onClick={() => void copyText(result.text).then(copied)}>복사</button>}
            <button className="btn small" onClick={close}>닫기</button>
          </div>
        </div>
      ) : noting ? (
        <form onSubmit={(e) => { e.preventDefault(); make('yellow', note.trim()); }}>
          <input className="input" autoFocus placeholder="메모" aria-label="메모 내용" value={note} maxLength={2000} onChange={(e) => setNote(e.target.value)} onKeyDown={(e) => { if (e.key === 'Escape') setNoting(false); e.stopPropagation(); }} />
          <button className="btn small primary" type="submit">저장</button>
        </form>
      ) : (
        <>
          {HIGHLIGHT_COLORS.map((c) => <button key={c} className={`hl-dot hl-${c}`} onClick={() => make(c)} aria-label={`${COLOR_LABEL[c]} 형광펜`} title={`${COLOR_LABEL[c]} 형광펜`} />)}
          <button className="btn small ghost" onClick={() => setNoting(true)}>메모</button>
          <span className="sel-sep" aria-hidden="true" />
          <button className="btn small ghost" onClick={() => void copyText(target.quote).then(copied)}>복사</button>
          {pair && pair.translation && <button className="btn small ghost" title="이 문단의 원문과 번역을 함께 복사" onClick={() => void copyText(`${pair.source}\n\n${pair.translation}`).then(copied)}>문단 원문+번역</button>}
          {onTranslate && target.side === 'source' && <button className="btn small ghost" onClick={() => void translate()}>선택 번역</button>}
        </>
      )}
    </div>
  );
}

/** Sidebar list of highlights and notes. */
export function NotesPanel({ items, onJump, onUpdate, onRemove }: {
  items: readonly AnnotationRow[];
  onJump: (blockId: string, sectionId?: string) => void;
  onUpdate: (a: AnnotationRow, patch: { color?: string; note?: string }) => void;
  onRemove: (a: AnnotationRow) => void;
}) {
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState('');
  if (items.length === 0) return <p className="muted side-empty">글을 선택하면 형광펜과 메모를 남길 수 있습니다.</p>;
  return (
    <ul className="notes" role="list">
      {items.map((a) => (
        <li key={a.id} className="note-item">
          <button className="note-quote" disabled={!a.blockId} onClick={() => a.blockId && onJump(a.blockId, a.sectionId ?? undefined)} title={a.blockId ? '이 위치로 이동' : '이 문단은 현재 판본에 없습니다'}>
            <span className={`hl-dot hl-${a.color}`} aria-hidden="true" />
            <span className="note-text">{a.quote}</span>
          </button>
          {editing === a.id ? (
            <form className="note-edit" onSubmit={(e) => { e.preventDefault(); onUpdate(a, { note: draft.trim() }); setEditing(null); }}>
              <textarea className="input" rows={3} autoFocus value={draft} maxLength={4000} aria-label="메모 내용" onChange={(e) => setDraft(e.target.value)} onKeyDown={(e) => { if (e.key === 'Escape') setEditing(null); e.stopPropagation(); }} />
              <div className="note-actions">
                <button type="button" className="btn small" onClick={() => setEditing(null)}>취소</button>
                <button type="submit" className="btn small primary">저장</button>
              </div>
            </form>
          ) : (
            <>
              {a.note && <p className="note-body">{a.note}</p>}
              <div className="note-actions">
                <span className="faint">{a.side === 'translation' ? '번역문' : '원문'}{!a.blockId ? ' · 위치 없음' : ''}</span>
                <span className="spacer" />
                {HIGHLIGHT_COLORS.filter((c) => c !== a.color).map((c) => <button key={c} className={`hl-dot small hl-${c}`} onClick={() => onUpdate(a, { color: c })} aria-label={`${COLOR_LABEL[c]}(으)로 바꾸기`} />)}
                <button className="btn small ghost" onClick={() => { setDraft(a.note); setEditing(a.id); }}>{a.note ? '메모 수정' : '메모 추가'}</button>
                <button className="btn small ghost danger" onClick={() => onRemove(a)}>삭제</button>
              </div>
            </>
          )}
        </li>
      ))}
    </ul>
  );
}
