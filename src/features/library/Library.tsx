import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ModalBackdrop } from '../../app/Modal';
import { listen } from '@tauri-apps/api/event';
import { call, errorMessage, mutate } from '../../ipc/client';
import type { DocumentRow } from '../../ipc/generated/DocumentRow';
import type { LibraryPage } from '../../ipc/generated/LibraryPage';
import type { ImportCandidate } from '../../ipc/generated/ImportCandidate';
import type { ImportResult } from '../../ipc/generated/ImportResult';
import { useShortcut } from '../../app/shortcuts';
import { useToast } from '../../app/toast';
import { IconGear } from '../reader/icons';
import {
  awaitingEpubMeta, coverHue, coverUrl, displayAuthors, displayImprint, displayTitle, needsPdfCover, queuePdfCover, relativeDay, sortDocuments, type SortKey,
} from './covers';
import './library.css';

const STATE_LABEL: Record<string, string> = {
  importing: '가져오는 중',
  partial: '일부 제한',
  failed: '열 수 없음',
};
const SORT_KEY = 'thoth.librarySort';

type Pending = { token: string; name: string; status: 'importing' | 'done' | 'error'; message?: string };

function readSort(): SortKey {
  try {
    return localStorage.getItem(SORT_KEY) === 'title' ? 'title' : 'recent';
  } catch {
    return 'recent';
  }
}

async function fetchAll(): Promise<DocumentRow[]> {
  const out: DocumentRow[] = [];
  let cursor: string | null = null;
  for (let i = 0; i < 200; i++) {
    const page: LibraryPage = await call<LibraryPage>('library_list', { cursor, limit: 100 });
    out.push(...page.documents);
    cursor = page.nextCursor;
    if (!cursor) break;
  }
  return out;
}

function Cover({ doc }: { doc: DocumentRow }) {
  const url = coverUrl(doc);
  const [broken, setBroken] = useState<string | null>(null);
  const title = displayTitle(doc);
  if (url && broken !== url) {
    return <img className="cover-img" src={url} alt="" loading="lazy" decoding="async" draggable={false} onError={() => setBroken(url)} />;
  }
  // Generic cover: the title set on a tinted board, like a plain cloth binding.
  return (
    <div className="cover-generic" style={{ '--hue': coverHue(title) } as React.CSSProperties} aria-hidden>
      <span className="cover-generic-title">{title}</span>
      <span className="cover-generic-author">{displayAuthors(doc)}</span>
    </div>
  );
}

const IconTrash = () => (
  <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
    <path d="M2.8 4.2h10.4M6.2 4.2V2.8h3.6v1.4M4.2 4.2l.6 8.6a1 1 0 0 0 1 .9h4.4a1 1 0 0 0 1-.9l.6-8.6M6.6 6.8v4.2M9.4 6.8v4.2" />
  </svg>
);

export function Library({ onOpen, onSettings }: { onOpen: (id: string) => void; onSettings: () => void }) {
  const [docs, setDocs] = useState<DocumentRow[] | null>(null);
  const [sort, setSort] = useState<SortKey>(readSort);
  const [pending, setPending] = useState<Pending[]>([]);
  const [confirmDelete, setConfirmDelete] = useState<DocumentRow | null>(null);
  const [dragging, setDragging] = useState(false);
  const toast = useToast();
  const polls = useRef(0);
  const patches = useRef(new Map<string, Record<string, unknown>>());

  const load = useCallback(async () => {
    try {
      const rows = await fetchAll();
      // A reload that started before a PDF cover was stored must not undo that row.
      setDocs(rows.map((d) => (patches.current.has(d.id) && d.metadata?.libraryMeta == null ? { ...d, metadata: { ...d.metadata, ...patches.current.get(d.id) } } : d)));
    } catch (e) {
      setDocs((d) => d ?? []);
      toast({ kind: 'error', text: errorMessage(e) });
    }
  }, [toast]);

  useEffect(() => { void load(); }, [load]);
  useEffect(() => {
    if (import.meta.env.DEV) window.__thoth = { ...(window.__thoth ?? {}), reloadLibrary: load };
  }, [load]);

  const patchDoc = useCallback((id: string, patch: Record<string, unknown>) => {
    patches.current.set(id, patch);
    setDocs((prev) => prev && prev.map((d) => (d.id === id ? { ...d, metadata: { ...d.metadata, ...patch } } : d)));
  }, []);

  // PDF covers are rendered here once; EPUB covers arrive from Rust shortly after import.
  useEffect(() => {
    if (!docs) return;
    for (const d of docs) if (needsPdfCover(d)) queuePdfCover(d.id, patchDoc);
    if (docs.some(awaitingEpubMeta) && polls.current < 40) {
      const t = window.setTimeout(() => { polls.current += 1; void load(); }, 1500);
      return () => window.clearTimeout(t);
    }
  }, [docs, load, patchDoc]);

  const chooseSort = (k: SortKey) => {
    setSort(k);
    try { localStorage.setItem(SORT_KEY, k); } catch { /* private mode */ }
  };

  const importCandidates = useCallback(async (cands: ImportCandidate[]) => {
    if (!cands.length) return;
    setPending((p) => [...p, ...cands.map((c) => ({ token: c.importToken, name: c.fileName, status: 'importing' as const }))]);
    for (const c of cands) {
      try {
        const r = await mutate<ImportResult>('document_import', { importToken: c.importToken });
        setPending((p) => p.map((x) => (x.token === c.importToken ? { ...x, status: 'done', message: r.deduplicated ? '이미 서재에 있는 책입니다' : undefined } : x)));
      } catch (e) {
        setPending((p) => p.map((x) => (x.token === c.importToken ? { ...x, status: 'error', message: errorMessage(e) } : x)));
      }
    }
    polls.current = 0;
    await load();
    window.setTimeout(() => setPending((p) => p.filter((x) => x.status === 'importing' || x.status === 'error')), 3000);
  }, [load]);

  const pick = useCallback(async () => {
    try {
      const cands = await call<ImportCandidate[]>('import_pick');
      await importCandidates(cands);
    } catch (e) {
      toast({ kind: 'error', text: errorMessage(e) });
    }
  }, [importCandidates, toast]);

  useShortcut('mod+o', () => void pick());

  useEffect(() => {
    // Rust registers dropped paths and sends back only tokens + file names.
    const un1 = listen<ImportCandidate[]>('import-candidates', (e) => { setDragging(false); void importCandidates(e.payload); });
    const un2 = listen('tauri://drag-enter', () => setDragging(true));
    const un3 = listen('tauri://drag-leave', () => setDragging(false));
    return () => { void un1.then((f) => f()); void un2.then((f) => f()); void un3.then((f) => f()); };
  }, [importCandidates]);

  const doDelete = async (d: DocumentRow) => {
    setConfirmDelete(null);
    try {
      await mutate('document_delete', { documentId: d.id, deleteHistory: true });
      toast({ kind: 'info', text: `“${displayTitle(d)}”을(를) 삭제했습니다. 외부 원본 파일은 그대로 있습니다.` });
      await load();
    } catch (e) {
      toast({ kind: 'error', text: errorMessage(e) });
    }
  };

  const sorted = useMemo(() => sortDocuments(docs ?? [], sort), [docs, sort]);

  return (
    <div className={`library ${dragging ? 'dragging' : ''}`}>
      <header className="lib-header">
        <h1>서재</h1>
        {sorted.length > 0 && <span className="lib-count">{sorted.length}권</span>}
        <div className="spacer" />
        <div className="seg" role="group" aria-label="정렬">
          <button aria-pressed={sort === 'recent'} onClick={() => chooseSort('recent')}>최근 읽은 순</button>
          <button aria-pressed={sort === 'title'} onClick={() => chooseSort('title')}>제목 순</button>
        </div>
        <button className="btn primary" onClick={() => void pick()} title="가져오기 (⌘O)">책 가져오기</button>
        <button className="btn icon" onClick={onSettings} title="설정 (⌘,)" aria-label="설정"><IconGear /></button>
      </header>
      {pending.length > 0 && (
        <ul className="pending" aria-label="가져오기 진행">
          {pending.map((p) => (
            <li key={p.token} className={p.status}>
              <span>{p.name}</span>
              <span className="muted">{p.status === 'importing' ? '가져오는 중…' : p.status === 'done' ? (p.message ?? '완료') : p.message}</span>
            </li>
          ))}
        </ul>
      )}
      {docs !== null && sorted.length === 0 ? (
        <div className="empty">
          <p>PDF 또는 EPUB 파일을 끌어다 놓거나 ⌘O로 가져오세요.</p>
          <p className="faint">원본은 앱 관리 사본으로 보관되며 외부 파일은 변경되지 않습니다.</p>
        </div>
      ) : (
        <ul className="shelf" role="list">
          {sorted.map((d) => {
            const title = displayTitle(d);
            const authors = displayAuthors(d);
            const imprint = displayImprint(d);
            const restrictions = (d.metadata?.restrictions as string[] | undefined) ?? [];
            const blocked = d.state === 'failed' || d.state === 'importing';
            const progress = typeof d.metadata?.progress === 'number' && d.lastOpenedAt != null ? Math.min(1, Math.max(0, d.metadata.progress)) : null;
            const stateLabel = STATE_LABEL[d.state];
            const pages = typeof d.metadata?.pageCount === 'number' ? `${d.metadata.pageCount}쪽` : '';
            const detail = [d.format.toUpperCase(), pages, `${(d.byteSize / 1024 / 1024).toFixed(1)} MB`].filter(Boolean).join(' · ');
            return (
              <li key={d.id} className={`book ${blocked ? 'blocked' : ''}`} data-format={d.format}>
                <button className="book-open" onClick={() => onOpen(d.id)} disabled={blocked} title={[title, authors, imprint].filter(Boolean).join('\n')}>
                  <span className="cover">
                    <Cover doc={d} />
                    {stateLabel && <span className={`cover-flag ${d.state === 'failed' ? 'danger' : ''}`}>{stateLabel}</span>}
                    {d.lastOpenedAt == null && !stateLabel && <span className="cover-flag new">새 책</span>}
                    {progress != null && <span className="cover-progress" aria-hidden="true"><span style={{ width: `${Math.max(2, progress * 100)}%` }} /></span>}
                  </span>
                  <span className="book-title">{title}</span>
                  {authors && <span className="book-author">{authors}</span>}
                  {imprint && <span className="book-imprint">{imprint}</span>}
                  <span className="book-detail">
                    {detail}
                    {restrictions.length > 0 && <span className="badge warn" title={restrictions.join(', ')}>제한 {restrictions.length}</span>}
                  </span>
                  <span className="book-read">{d.lastOpenedAt != null ? `${relativeDay(Number(d.lastOpenedAt))} 읽음${progress != null ? ` · ${Math.round(progress * 100)}%` : ''}` : '아직 읽지 않음'}</span>
                </button>
                <button className="book-delete" onClick={() => setConfirmDelete(d)} aria-label={`${title} 삭제`} title="삭제"><IconTrash /></button>
              </li>
            );
          })}
        </ul>
      )}
      {confirmDelete && (
        <ModalBackdrop role="dialog" aria-labelledby="del-title" onClose={() => setConfirmDelete(null)}>
          <div className="modal" style={{ width: 440 }}>
            <header><h2 id="del-title">책을 삭제할까요?</h2></header>
            <div className="body">
              <p>“{displayTitle(confirmDelete)}”의 앱 관리 사본, 모든 번역과 수정 이력, 북마크와 위치가 삭제됩니다. 되돌릴 수 없습니다.</p>
              <p className="muted">가져올 때 선택한 외부 원본 파일은 삭제하지 않습니다.</p>
            </div>
            <footer>
              <button className="btn" onClick={() => setConfirmDelete(null)} autoFocus>취소</button>
              <button className="btn primary" onClick={() => void doDelete(confirmDelete)}>삭제</button>
            </footer>
          </div>
        </ModalBackdrop>
      )}
    </div>
  );
}
