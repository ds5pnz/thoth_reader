import { useCallback, useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { call, errorMessage, mutate } from '../../ipc/client';
import type { DocumentRow } from '../../ipc/generated/DocumentRow';
import type { LibraryPage } from '../../ipc/generated/LibraryPage';
import type { ImportCandidate } from '../../ipc/generated/ImportCandidate';
import type { ImportResult } from '../../ipc/generated/ImportResult';
import { useShortcut } from '../../app/shortcuts';
import { useToast } from '../../app/toast';
import './library.css';

const STATE_LABEL: Record<string, string> = {
  importing: '가져오는 중',
  indexing: '색인 중',
  ready: '준비됨',
  partial: '일부 제한',
  failed: '실패',
};

type Pending = { token: string; name: string; status: 'importing' | 'done' | 'error'; message?: string };

export function Library({ onOpen, onSettings }: { onOpen: (id: string) => void; onSettings: () => void }) {
  const [docs, setDocs] = useState<DocumentRow[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [pending, setPending] = useState<Pending[]>([]);
  const [confirmDelete, setConfirmDelete] = useState<DocumentRow | null>(null);
  const [dragging, setDragging] = useState(false);
  const toast = useToast();

  const load = useCallback(async (append = false, from: string | null = null) => {
    try {
      const page = await call<LibraryPage>('library_list', { cursor: append ? from : null, limit: 50 });
      setDocs((prev) => (append ? [...prev, ...page.documents] : page.documents));
      setCursor(page.nextCursor);
    } catch (e) {
      toast({ kind: 'error', text: errorMessage(e) });
    }
  }, [toast]);

  useEffect(() => { void load(); }, [load]);

  const importCandidates = useCallback(async (cands: ImportCandidate[]) => {
    if (!cands.length) return;
    setPending((p) => [...p, ...cands.map((c) => ({ token: c.importToken, name: c.fileName, status: 'importing' as const }))]);
    for (const c of cands) {
      try {
        const r = await mutate<ImportResult>('document_import', { importToken: c.importToken });
        setPending((p) => p.map((x) => (x.token === c.importToken ? { ...x, status: 'done', message: r.deduplicated ? '이미 있는 문서와 연결했습니다' : undefined } : x)));
      } catch (e) {
        setPending((p) => p.map((x) => (x.token === c.importToken ? { ...x, status: 'error', message: errorMessage(e) } : x)));
      }
    }
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
      toast({ kind: 'info', text: `“${d.title}”을(를) 삭제했습니다. 외부 원본 파일은 그대로 있습니다.` });
      await load();
    } catch (e) {
      toast({ kind: 'error', text: errorMessage(e) });
    }
  };

  return (
    <div className={`library ${dragging ? 'dragging' : ''}`}>
      <header className="lib-header">
        <h1>Thoth Reader</h1>
        <div className="spacer" />
        <button className="btn primary" onClick={() => void pick()} title="열기 (⌘O)">문서 가져오기</button>
        <button className="btn" onClick={onSettings} title="설정 (⌘,)">설정</button>
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
      {docs.length === 0 ? (
        <div className="empty">
          <p>PDF 또는 EPUB 파일을 끌어다 놓거나 ⌘O로 가져오세요.</p>
          <p className="faint">원본은 앱 관리 사본으로 보관되며 외부 파일은 변경되지 않습니다.</p>
        </div>
      ) : (
        <ul className="doc-list" role="list">
          {docs.map((d) => {
            const restrictions = (d.metadata?.restrictions as string[] | undefined) ?? [];
            return (
              <li key={d.id} className="doc">
                <button className="doc-open" onClick={() => onOpen(d.id)} disabled={d.state === 'failed' || d.state === 'importing'}>
                  <span className={`fmt ${d.format}`}>{d.format.toUpperCase()}</span>
                  <span className="title">{(d.metadata?.title as string) || (d.metadata?.pdfTitle as string) || d.title}</span>
                  <span className="meta">
                    <span className={`badge ${d.state === 'failed' ? 'danger' : d.state === 'partial' ? 'warn' : d.state === 'ready' ? 'ok' : ''}`}>{STATE_LABEL[d.state] ?? d.state}</span>
                    {restrictions.length > 0 && <span className="badge warn" title={restrictions.join(', ')}>제한 {restrictions.length}</span>}
                    <span className="faint">{(d.byteSize / 1024 / 1024).toFixed(1)} MiB</span>
                    {d.lastOpenedAt && <span className="faint">최근 {new Date(Number(d.lastOpenedAt)).toLocaleDateString('ko-KR')}</span>}
                  </span>
                </button>
                <button className="btn ghost small danger" onClick={() => setConfirmDelete(d)} aria-label={`${d.title} 삭제`}>삭제</button>
              </li>
            );
          })}
        </ul>
      )}
      {cursor && <button className="btn more" onClick={() => void load(true, cursor)}>더 보기</button>}
      {confirmDelete && (
        <div className="modal-backdrop" role="dialog" aria-modal="true" aria-labelledby="del-title">
          <div className="modal" style={{ width: 440 }}>
            <header><h2 id="del-title">문서를 삭제할까요?</h2></header>
            <div className="body">
              <p>“{confirmDelete.title}”의 앱 관리 사본, 모든 번역과 수정 이력, 북마크와 위치가 삭제됩니다. 되돌릴 수 없습니다.</p>
              <p className="muted">가져올 때 선택한 외부 원본 파일은 삭제하지 않습니다.</p>
            </div>
            <footer>
              <button className="btn" onClick={() => setConfirmDelete(null)} autoFocus>취소</button>
              <button className="btn primary" onClick={() => void doDelete(confirmDelete)}>삭제</button>
            </footer>
          </div>
        </div>
      )}
    </div>
  );
}
