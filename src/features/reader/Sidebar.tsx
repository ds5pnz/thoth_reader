import { useEffect, useRef, useState } from 'react';
import { call, errorMessage, mutate } from '../../ipc/client';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import type { SearchPage } from '../../ipc/generated/SearchPage';
import type { SearchHit } from '../../ipc/generated/SearchHit';
import type { BookmarkRow } from '../../ipc/generated/BookmarkRow';
import type { ReaderAnchor } from '../../ipc/generated/ReaderAnchor';
import type { ReaderModel } from './useReaderSession';

type TocEntry = { label: string; spineIndex: number; fragment?: string | null; children?: TocEntry[] };

export function Sidebar({ tab, onTab, session, model, onJump, onSection, currentAnchor, bookmarksVersion = 0 }: {
  bookmarksVersion?: number;
  tab: 'toc' | 'search' | 'bookmarks';
  onTab: (t: 'toc' | 'search' | 'bookmarks') => void;
  session: ReaderSessionDto;
  model: ReaderModel;
  onJump: (blockId: string, sectionId?: string) => void;
  onSection: (sectionId: string) => void;
  currentAnchor: () => ReaderAnchor | null;
}) {
  return (
    <aside className="sidebar" id="reader-sidebar" aria-label="탐색">
      <div className="seg side-tabs" role="tablist">
        <button role="tab" aria-selected={tab === 'toc'} aria-pressed={tab === 'toc'} onClick={() => onTab('toc')}>목차</button>
        <button role="tab" aria-selected={tab === 'search'} aria-pressed={tab === 'search'} onClick={() => onTab('search')}>검색</button>
        <button role="tab" aria-selected={tab === 'bookmarks'} aria-pressed={tab === 'bookmarks'} onClick={() => onTab('bookmarks')}>북마크</button>
      </div>
      <div className="side-body">
        {tab === 'toc' && <Toc session={session} model={model} onSection={onSection} />}
        {tab === 'search' && <Search session={session} onJump={onJump} />}
        {tab === 'bookmarks' && <Bookmarks session={session} onJump={onJump} currentAnchor={currentAnchor} version={bookmarksVersion} />}
      </div>
    </aside>
  );
}

function Toc({ session, model, onSection }: { session: ReaderSessionDto; model: ReaderModel; onSection: (id: string) => void }) {
  const toc = (session.metadata?.toc as TocEntry[] | undefined) ?? (model.session?.metadata?.toc as TocEntry[] | undefined) ?? [];
  const bySpine = new Map<number, string>();
  for (const s of model.sections) {
    const loc = s.locator as { spineIndex?: number; pageIndex?: number };
    bySpine.set(Number(loc.spineIndex ?? loc.pageIndex ?? s.ordinal), s.id);
  }
  const render = (entries: TocEntry[], depth: number) => (
    <ul className="toc" role={depth === 0 ? 'tree' : 'group'}>
      {entries.map((e, i) => (
        <li key={i} role="treeitem" aria-level={depth + 1}>
          <button className="toc-item" style={{ paddingLeft: 8 + depth * 12 }} onClick={() => { const id = bySpine.get(e.spineIndex); if (id) onSection(id); }}>{e.label}</button>
          {e.children && e.children.length > 0 && render(e.children, depth + 1)}
        </li>
      ))}
    </ul>
  );
  if (toc.length) return render(toc, 0);
  // PDF or no TOC: list pages/sections.
  return (
    <ul className="toc">
      {model.sections.slice(0, 2000).map((s) => (
        <li key={s.id}>
          <button className="toc-item" onClick={() => onSection(s.id)}>
            {session.format === 'pdf' ? `${Number(s.ordinal) + 1}쪽` : `섹션 ${Number(s.ordinal) + 1}`}
            {s.state === 'failed' && <span className="badge danger">실패</span>}
            {s.state !== 'ready' && s.state !== 'failed' && <span className="faint"> 색인 중</span>}
          </button>
        </li>
      ))}
    </ul>
  );
}

function highlightSnippet(h: SearchHit) {
  const u = h.snippet;
  // offsets are UTF-16, matching JS string indices.
  return <>{u.slice(0, h.snippetMatchStartU16)}<mark>{u.slice(h.snippetMatchStartU16, h.snippetMatchEndU16)}</mark>{u.slice(h.snippetMatchEndU16)}</>;
}

function Search({ session, onJump }: { session: ReaderSessionDto; onJump: (blockId: string, sectionId?: string) => void }) {
  const [q, setQ] = useState('');
  const [scope, setScope] = useState<'all' | 'source' | 'translation'>('all');
  const [page, setPage] = useState<SearchPage | null>(null);
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  useEffect(() => { inputRef.current?.focus(); }, []);
  const run = async (cursor: number | null) => {
    if (!q.trim()) return;
    setBusy(true);
    setErr(null);
    try {
      const p = await call<SearchPage>('search_query', { sessionId: session.sessionId, query: q, scope, cursor });
      setPage(p);
      setHits((prev) => (cursor ? [...prev, ...p.hits] : p.hits));
    } catch (e) { setErr(errorMessage(e)); }
    finally { setBusy(false); }
  };
  return (
    <div className="search">
      <form onSubmit={(e) => { e.preventDefault(); void run(null); }}>
        <input ref={inputRef} className="input" type="search" value={q} onChange={(e) => setQ(e.target.value)} placeholder="원문·번역 검색 (⌘F)" aria-label="검색어" />
        <select className="input" value={scope} onChange={(e) => setScope(e.target.value as typeof scope)} aria-label="검색 범위">
          <option value="all">원문+번역</option><option value="source">원문</option><option value="translation">저장된 번역</option>
        </select>
      </form>
      {page && (
        <p className="faint search-scope">
          {page.method === 'substring' ? '1~2글자: 이 문서 안 부분 문자열 검색' : '3글자 이상: 색인 검색'} · 색인 범위 {Number(page.indexedSections)}/{Number(page.totalSections)} 섹션
        </p>
      )}
      {busy && <p className="faint">검색 중…</p>}
      {err && <p className="badge danger">{err}</p>}
      <ul className="hits">
        {hits.map((h, i) => (
          <li key={`${h.blockId}-${h.scope}-${i}`}>
            <button onClick={() => onJump(h.blockId, h.sectionId)}>
              <span className="badge">{h.scope === 'source' ? '원문' : '번역'}</span> {highlightSnippet(h)}
            </button>
          </li>
        ))}
      </ul>
      {page?.nextCursor != null && <button className="btn small" onClick={() => void run(page.nextCursor!)}>더 보기</button>}
      {page && hits.length === 0 && <p className="muted">결과가 없습니다.</p>}
    </div>
  );
}

function Bookmarks({ session, onJump, currentAnchor, version }: { session: ReaderSessionDto; onJump: (blockId: string) => void; currentAnchor: () => ReaderAnchor | null; version: number }) {
  const [items, setItems] = useState<BookmarkRow[]>([]);
  const load = async () => { setItems(await call<BookmarkRow[]>('bookmark_list', { documentId: session.documentId }).catch(() => [])); };
  useEffect(() => { void load(); }, [version]); // eslint-disable-line react-hooks/exhaustive-deps
  const add = async () => {
    const a = currentAnchor();
    if (!a) return;
    await mutate('bookmark_put', { documentId: session.documentId, bookmarkId: null, anchor: a, label: new Date().toLocaleString('ko-KR') });
    await load();
  };
  const rename = async (b: BookmarkRow, label: string) => {
    await mutate('bookmark_put', { documentId: session.documentId, bookmarkId: b.id, anchor: b.anchor, label });
    await load();
  };
  const del = async (b: BookmarkRow) => { await mutate('bookmark_delete', { documentId: session.documentId, bookmarkId: b.id }); await load(); };
  return (
    <div className="bookmarks">
      <button className="btn small" onClick={() => void add()}>현재 위치 북마크 (⌘D)</button>
      <ul>
        {items.map((b) => (
          <li key={b.id}>
            <button className="bm-jump" onClick={() => b.anchor.blockId && onJump(b.anchor.blockId)}>{b.label}</button>
            <button className="btn small ghost" onClick={() => { const l = prompt('북마크 이름', b.label); if (l != null) void rename(b, l); }} aria-label="이름 바꾸기">✎</button>
            <button className="btn small ghost" onClick={() => void del(b)} aria-label="삭제">✕</button>
          </li>
        ))}
      </ul>
    </div>
  );
}
