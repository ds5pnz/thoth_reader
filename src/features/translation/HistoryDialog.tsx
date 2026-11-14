import { useEffect, useState } from 'react';
import { ModalBackdrop } from '../../app/Modal';
import { call, errorMessage, mutate } from '../../ipc/client';
import type { Block } from '../../ipc/generated/Block';
import type { HistoryPage } from '../../ipc/generated/HistoryPage';
import type { RevisionRow } from '../../ipc/generated/RevisionRow';

const ORIGIN: Record<string, string> = { generated: '모델 번역', manual: '수동 수정', restore: '이력 복원' };

export function HistoryDialog({ sessionId, block, targetLanguage, onClose, onApplied }: {
  sessionId: string;
  block: Block;
  targetLanguage: string;
  onClose: () => void;
  onApplied: () => void;
}) {
  const [revs, setRevs] = useState<RevisionRow[]>([]);
  const [head, setHead] = useState<HistoryPage['head'] | null>(null);
  const [cursor, setCursor] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const load = async (from: string | null) => {
    try {
      const p = await call<HistoryPage>('translation_history', { sessionId, blockId: block.id, targetLanguage, cursor: from });
      setRevs((prev) => (from ? [...prev, ...p.revisions] : p.revisions));
      setHead(p.head);
      setCursor(p.nextCursor);
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  useEffect(() => { void load(null); }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const apply = async (r: RevisionRow) => {
    if (!head) return;
    try {
      await mutate('translation_apply_revision', { sessionId, revisionId: r.id, expectedHeadVersion: head.version });
      onApplied();
      onClose();
    } catch (e) {
      setErr(errorMessage(e));
      void load(null);
    }
  };

  return (
    <ModalBackdrop role="dialog" aria-labelledby="hist-title" onClose={onClose}>
      <div className="modal">
        <header>
          <h2 id="hist-title">번역 이력</h2>
          <button className="btn ghost small" onClick={onClose} aria-label="닫기">닫기</button>
        </header>
        <div className="body">
          <blockquote className="hist-source">{block.sourceText.slice(0, 400)}{block.sourceText.length > 400 ? '…' : ''}</blockquote>
          {err && <p className="badge danger">{err}</p>}
          {revs.length === 0 && <p className="muted">아직 저장된 번역이 없습니다.</p>}
          <ol className="hist-list">
            {revs.map((r) => {
              const isManualHead = head?.manualRevisionId === r.id;
              const isGenHead = head?.generatedRevisionId === r.id;
              return (
                <li key={r.id} className={isManualHead || isGenHead ? 'current' : ''}>
                  <div className="hist-meta">
                    <span className="badge">{ORIGIN[r.origin] ?? r.origin}</span>
                    {isManualHead && <span className="badge ok">고정됨</span>}
                    {isGenHead && !head?.manualRevisionId && <span className="badge accent">표시 중</span>}
                    {isGenHead && head?.manualRevisionId && <span className="badge">후보</span>}
                    <span className="faint">{new Date(Number(r.createdAt)).toLocaleString('ko-KR')}</span>
                    <span className="spacer" />
                    {!isManualHead && !(isGenHead && !head?.manualRevisionId) && (
                      <button className="btn small" onClick={() => void apply(r)}>{r.origin === 'generated' ? '새 번역 적용' : '이 버전 복원'}</button>
                    )}
                  </div>
                  <p className="hist-text" lang={targetLanguage}>{r.text}</p>
                </li>
              );
            })}
          </ol>
          {cursor && <button className="btn" onClick={() => void load(cursor)}>더 보기</button>}
        </div>
      </div>
    </ModalBackdrop>
  );
}
