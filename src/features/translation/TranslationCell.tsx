import { memo, useEffect, useRef, useState } from 'react';
import type { Block } from '../../ipc/generated/Block';
import type { BlockDisplay } from '../../ipc/generated/BlockDisplay';
import type { JobView } from '../reader/useReaderSession';
import { renderMarked, stripInlineTags } from './marked';

export interface CellActions {
  retranslate: (blockId: string) => void;
  translateNow: (blockId: string) => void;
  cancel: (jobId: string) => void;
  saveEdit: (block: Block, text: string, expectedHeadVersion: number) => Promise<boolean>;
  openHistory: (block: Block) => void;
  revealSource: (block: Block) => void;
  setExcluded: (block: Block, excluded: boolean, prefVersion: number) => void;
  applyCandidate: (block: Block) => void;
  setDirty: (blockId: string, dirty: boolean, flush?: () => Promise<boolean>) => void;
}

const MAX_EDIT_BYTES = 128 * 1024;

/**
 * Status badge. In the reading flow (`detailed` = false) only states that need the reader's
 * attention are shown, so translated text reads like a plain document; the inspect popover
 * shows every state.
 */
function statusOf(d: BlockDisplay | undefined, job: JobView | undefined, autoActive: boolean, detailed: boolean): { label: string; cls: string } | null {
  if (job) {
    if (job.state === 'FAILED') return { label: '실패', cls: 'danger' };
    if (job.state === 'RETRY_WAIT') return { label: '재시도 대기', cls: 'warn' };
    if (!detailed) return null;
    if (job.state === 'QUEUED') return { label: '대기', cls: '' };
    return { label: '번역 중', cls: 'accent' };
  }
  if (!d || !detailed) return null;
  if (d.status === 'excluded') return { label: '번역 제외', cls: '' };
  if (d.status === 'manual') return { label: '수동 수정', cls: 'ok' };
  if (d.status === 'stale') return { label: '이전 설정의 번역', cls: 'warn' };
  if (d.status === 'none') return autoActive ? { label: '대기', cls: '' } : null;
  return null;
}

// Unsaved edits live outside the row: rows are virtualized and unmount when scrolled away,
// and an edit in progress must survive that (and keep the unsaved-changes guard armed).
const drafts = new Map<string, string>();
export function clearDrafts() { drafts.clear(); }

export const TranslationCell = memo(function TranslationCell(props: {
  block: Block;
  display: BlockDisplay | undefined;
  job: JobView | undefined;
  preview: string | undefined;
  autoActive: boolean;
  targetLanguage: string;
  actions: CellActions;
  /** Show every status badge (inspect popover). Default: quiet, document-like. */
  detailed?: boolean;
}) {
  const { block, display, job, preview, autoActive, targetLanguage, actions, detailed = false } = props;
  const draftKey = `${block.id}|${targetLanguage}`;
  const [editing, setEditingState] = useState(() => drafts.has(draftKey));
  const [draft, setDraftState] = useState(() => drafts.get(draftKey) ?? '');
  const setDraft = (v: string) => { drafts.set(draftKey, v); setDraftState(v); };
  const setEditing = (on: boolean) => { if (!on) drafts.delete(draftKey); setEditingState(on); };
  const [saving, setSaving] = useState(false);
  const [conflict, setConflict] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  const taRef = useRef<HTMLTextAreaElement>(null);
  const shownText = display?.shown?.text ?? '';
  const status = statusOf(display, job, autoActive, detailed);
  const tooLong = new TextEncoder().encode(draft).length > MAX_EDIT_BYTES;
  const dirty = editing && draft !== shownText;

  const save = async (): Promise<boolean> => {
    if (tooLong || saving) return false;
    setSaving(true);
    try {
      const ok = await actions.saveEdit(block, draft, display?.headVersion ?? 0);
      if (ok) { setEditing(false); setConflict(false); }
      else setConflict(true);
      return ok;
    } finally {
      setSaving(false);
    }
  };

  useEffect(() => {
    actions.setDirty(block.id, dirty, dirty ? save : undefined);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dirty, draft]);
  // Scrolled away with an unsaved draft: stay registered so leaving the view still asks.
  useEffect(() => () => { if (!drafts.has(draftKey)) actions.setDirty(block.id, false); }, [actions, block.id, draftKey]);

  const startEdit = () => {
    setDraft(shownText || preview || '');
    setEditing(true);
    setMenuOpen(false);
    setTimeout(() => taRef.current?.focus(), 0);
  };

  if (editing) {
    return (
      <div className="tcell editing" lang={targetLanguage}>
        <textarea
          ref={taRef}
          className="input"
          value={draft}
          rows={Math.min(16, Math.max(3, Math.ceil(draft.length / 60)))}
          aria-label="번역 수정"
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); void save(); }
            else if (e.key === 'Escape') { e.preventDefault(); setEditing(false); setConflict(false); }
            e.stopPropagation();
          }}
        />
        <div className="tcell-edit-bar">
          {conflict && <span className="badge warn">다른 변경이 먼저 저장되었습니다. 입력 내용은 유지됩니다.</span>}
          {tooLong && <span className="badge danger">128KiB를 넘었습니다</span>}
          <span className="spacer" />
          <button className="btn small" onClick={() => { setEditing(false); setConflict(false); }}>취소 (Esc)</button>
          <button className="btn small primary" onClick={() => void save()} disabled={saving || tooLong}>{saving ? '저장 중…' : '저장 (⌘↵)'}</button>
        </div>
      </div>
    );
  }

  const running = job && (job.state === 'RUNNING' || job.state === 'STREAMING' || job.state === 'QUEUED');
  return (
    <div className={`tcell ${display?.status ?? 'none'} ${detailed ? 'detailed' : 'quiet'}`} lang={targetLanguage} aria-busy={running ? true : undefined}>
      <div className="tcell-head">
        {status && <span className={`badge ${status.cls}`}>{status.label}</span>}
        {display?.shown?.warning && (
          <span className="badge warn on-hover" title={display.shown.warning.startsWith('script:') ? '번역에 다른 문자 체계가 섞였습니다. 재번역하거나 직접 수정해 주세요.' : '번역 결과의 언어가 대상 언어와 달라 보입니다.'}>확인 필요</span>
        )}
        {display?.hasCandidate && <button className="btn small ghost on-hover" onClick={() => actions.applyCandidate(block)} title="수동 고정본 대신 새 번역을 적용">새 번역 적용</button>}
        <span className="spacer" />
        <div className="tcell-menu">
          <button className="btn small ghost" aria-haspopup="menu" aria-expanded={menuOpen} aria-label="문단 메뉴" onClick={() => setMenuOpen((v) => !v)}>⋯</button>
          {menuOpen && (
            <ul role="menu" className="menu" onMouseLeave={() => setMenuOpen(false)}>
              <li role="menuitem"><button onClick={() => { setMenuOpen(false); actions.retranslate(block.id); }}>재번역</button></li>
              <li role="menuitem"><button onClick={startEdit}>수정</button></li>
              <li role="menuitem"><button onClick={() => { setMenuOpen(false); actions.openHistory(block); }}>이력 보기</button></li>
              <li role="menuitem"><button onClick={() => { setMenuOpen(false); actions.revealSource(block); }}>원본 위치로 이동</button></li>
              <li role="menuitem">
                <button onClick={() => { setMenuOpen(false); actions.setExcluded(block, display?.userOverride !== 'preserve', display?.preferenceVersion ?? 0); }}>
                  {display?.userOverride === 'preserve' ? '번역 제외 해제' : '번역 제외'}
                </button>
              </li>
            </ul>
          )}
        </div>
      </div>
      {running && preview != null && (detailed || !shownText) ? (
        <div className="tcell-text preview">
          {/* Popover: the existing translation stays and the new result is previewed below it.
              Reading flow: the existing text simply stays until the new one is committed. */}
          {detailed && shownText && <p className="tcell-prev">{shownText}</p>}
          <p className="tcell-stream" aria-live="off">{stripInlineTags(preview)}</p>
        </div>
      ) : shownText ? (
        <div className="tcell-text" onDoubleClick={startEdit}>{renderMarked(shownText, display?.shown?.marks)}</div>
      ) : display?.status === 'excluded' ? null : (
        <div className="tcell-empty faint">
          {job?.state === 'FAILED' ? (
            <>
              <span>{job.errorCode === 'AUTH_FAILED' ? '인증을 확인해 주세요.' : job.errorCode === 'INVALID_OUTPUT' ? '모델 응답 형식이 올바르지 않습니다.' : '번역에 실패했습니다.'}</span>
              <button className="btn small" onClick={() => actions.translateNow(block.id)} title={job.sentUnknown ? '원문이 이미 전송되었을 수 있어 새 요청이 발생할 수 있습니다' : undefined}>다시 시도</button>
            </>
          ) : running ? (detailed ? '번역 중…' : null) : !autoActive && display?.status === 'none' ? (
            <button className="btn small ghost" onClick={() => actions.translateNow(block.id)}>번역</button>
          ) : null}
        </div>
      )}
      {running && job && <button className="btn small ghost cancel on-hover" onClick={() => actions.cancel(job.jobId)}>취소</button>}
    </div>
  );
});
