// "Translate a range / export": pick pages, chapters or the whole book, translate what is
// missing in the background (progress + time estimate), then export.
import { useEffect, useMemo, useRef, useState } from 'react';
import { call, errorMessage } from '../../ipc/client';
import type { BatchStatus } from '../../ipc/generated/BatchStatus';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import type { SectionRow } from '../../ipc/generated/SectionRow';
import { ModalBackdrop } from '../../app/Modal';
import { useToast } from '../../app/toast';
import { chaptersOf, etaClock, formatEta, parsePageRanges, sectionsInRanges, type TocLike } from './range';
import { exportSideBySide, exportTypeset, type ExportProgress } from './pdfExport';

type Scope = 'all' | 'pages' | 'chapters';
type Format = 'pdf-sbs' | 'pdf' | 'epub' | 'html' | 'markdown' | 'text';

export function BatchLine({ batch }: { batch: BatchStatus }) {
  const settled = batch.done + batch.failed + batch.skipped;
  const pct = batch.total > 0 ? Math.min(100, Math.round((settled / batch.total) * 100)) : 0;
  const clock = etaClock(batch.etaSeconds);
  return (
    <div className="batch-line">
      <div className="batch-bar" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct} aria-label="번역 진행률"><span style={{ width: `${pct}%` }} /></div>
      <p className="batch-text">
        <strong>{pct}%</strong> · 문단 {settled.toLocaleString('ko-KR')} / {batch.total.toLocaleString('ko-KR')}
        {batch.failed > 0 && <span className="warn-text"> · 실패 {batch.failed}</span>}
        {batch.sectionsWaiting > 0 && <span> · 본문 추출 대기 {batch.sectionsWaiting}</span>}
      </p>
      {batch.state === 'running' && (
        <p className="batch-text muted">남은 시간 {formatEta(batch.etaSeconds)}{clock ? ` (${clock}쯤 완료)` : ''}{batch.sectionsWaiting > 0 ? ' — 아직 추출되지 않은 부분은 추정치입니다' : ''}</p>
      )}
    </div>
  );
}

export function TranslateExportDialog({ session, sections, batch, onBatch, currentPage, onClose }: {
  session: ReaderSessionDto;
  sections: readonly SectionRow[];
  batch: BatchStatus | null;
  onBatch: (b: BatchStatus | null) => void;
  /** 0-based current page/section, used as the default page range. */
  currentPage: number;
  onClose: () => void;
}) {
  const isPdf = session.format === 'pdf';
  const toast = useToast();
  const chapters = useMemo(() => chaptersOf(((session.metadata?.toc as TocLike[] | undefined) ?? []), sections.length), [session.metadata, sections.length]);
  const [scope, setScope] = useState<Scope>('all');
  const [pages, setPages] = useState(`${currentPage + 1}-${Math.min(sections.length, currentPage + 10)}`);
  const [picked, setPicked] = useState<Set<number>>(new Set());
  const [format, setFormat] = useState<Format>(isPdf ? 'pdf-sbs' : 'epub');
  const [bilingual, setBilingual] = useState(false);
  const [busy, setBusy] = useState<ExportProgress | 'saving' | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const cancelExport = useRef(false);

  // pick up a run that is already going
  useEffect(() => {
    void call<{ status: BatchStatus | null }>('translation_batch_status', { sessionId: session.sessionId }).then((r) => { if (r.status) onBatch(r.status); }).catch(() => {});
  }, [session.sessionId]); // eslint-disable-line react-hooks/exhaustive-deps

  const ranges = useMemo(() => {
    if (scope === 'all') return [{ from: 0, to: sections.length - 1 }];
    if (scope === 'pages') return parsePageRanges(pages, sections.length);
    return chapters.filter((_, i) => picked.has(i)).map((c) => ({ from: c.from, to: c.to }));
  }, [scope, pages, picked, chapters, sections.length]);
  const sectionIds = useMemo(() => (ranges ? sectionsInRanges(sections, ranges) : []), [sections, ranges]);
  const chosen = useMemo(() => { const ids = new Set(sectionIds); return sections.filter((s) => ids.has(s.id)); }, [sections, sectionIds]);
  const notReady = chosen.filter((s) => s.state !== 'ready' && s.state !== 'failed').length;
  const unit = isPdf ? '쪽' : '장';
  const running = batch?.state === 'running';

  const start = async () => {
    setErr(null);
    try {
      onBatch(await call<BatchStatus>('translation_batch_start', { sessionId: session.sessionId, sectionIds }));
    } catch (e) { setErr(errorMessage(e)); }
  };
  const stop = () => void call('translation_batch_cancel', { sessionId: session.sessionId }).catch(() => {});

  const doExport = async () => {
    setErr(null);
    cancelExport.current = false;
    try {
      if (format === 'pdf-sbs' || format === 'pdf') {
        setBusy({ done: 0, total: chosen.length });
        const opts = { onProgress: (p: ExportProgress) => setBusy(p), cancelled: () => cancelExport.current, bilingual };
        const r = format === 'pdf-sbs' ? await exportSideBySide(session, chosen, opts) : await exportTypeset(session, chosen, opts);
        if (r.saved) toast({ kind: 'info', text: `PDF ${r.pages}쪽을 저장했습니다.${r.untranslated > 0 ? ` 번역되지 않은 문단 ${r.untranslated}개는 원문 그대로입니다.` : ''}` });
      } else {
        setBusy('saving');
        const r = await call<{ saved: boolean; untranslated?: number }>('export_translation', { sessionId: session.sessionId, sectionIds, format, bilingual });
        if (r.saved) toast({ kind: 'info', text: `저장했습니다.${(r.untranslated ?? 0) > 0 ? ` 번역되지 않은 문단 ${r.untranslated}개는 원문으로 들어갔습니다.` : ''}` });
      }
    } catch (e) { setErr(errorMessage(e)); }
    finally { setBusy(null); }
  };

  const exporting = busy !== null;
  return (
    <ModalBackdrop role="dialog" aria-labelledby="tx-title" onClose={exporting ? undefined : onClose}>
      <div className="modal tx-dialog">
        <header><h2 id="tx-title">범위 번역 · 내보내기</h2></header>
        <div className="body">
          <fieldset className="tx-scope" disabled={exporting}>
            <legend>범위</legend>
            <label><input type="radio" name="scope" checked={scope === 'all'} onChange={() => setScope('all')} /> 책 전체 <span className="faint">({sections.length}{unit})</span></label>
            <label>
              <input type="radio" name="scope" checked={scope === 'pages'} onChange={() => setScope('pages')} /> {isPdf ? '쪽 범위' : '장 번호 범위'}
              <input className="input tx-pages" value={pages} onFocus={() => setScope('pages')} onChange={(e) => setPages(e.target.value)} placeholder="예: 10-40, 55" aria-label="범위" aria-invalid={scope === 'pages' && !ranges} />
            </label>
            {chapters.length > 0 && (
              <>
                <label><input type="radio" name="scope" checked={scope === 'chapters'} onChange={() => setScope('chapters')} /> 챕터 선택</label>
                {scope === 'chapters' && (
                  <ul className="tx-chapters">
                    {chapters.map((c, i) => (
                      <li key={i}>
                        <label>
                          <input type="checkbox" checked={picked.has(i)} onChange={() => setPicked((prev) => { const n = new Set(prev); if (n.has(i)) n.delete(i); else n.add(i); return n; })} />
                          <span className="tx-ch-label">{c.label}</span>
                          <span className="faint">{isPdf ? `${c.from + 1}–${c.to + 1}쪽` : `${c.to - c.from + 1}장`}</span>
                        </label>
                      </li>
                    ))}
                  </ul>
                )}
              </>
            )}
          </fieldset>
          <p className="muted tx-summary">
            {scope === 'pages' && !ranges ? '범위를 "10-40, 55"처럼 입력해 주세요.' : `선택: ${chosen.length}${unit}`}
            {notReady > 0 && ` · 본문 추출 중 ${notReady}${unit}`}
          </p>

          <section className="tx-block">
            <h3>번역</h3>
            <p className="muted">선택한 범위에서 아직 번역되지 않은 문단을 차례로 번역합니다. 창을 닫아도 계속되고, 책을 닫으면 멈춥니다.</p>
            {batch && <BatchLine batch={batch} />}
            {batch && batch.state !== 'running' && (
              <p className="batch-text muted">
                {batch.state === 'done' ? '번역을 마쳤습니다.' : batch.state === 'cancelled' ? '중지했습니다.' : batch.message ?? '멈췄습니다.'}
                {batch.skipped > 0 && ` 코드·그림처럼 번역하지 않는 문단 ${batch.skipped}개는 건너뛰었습니다.`}
              </p>
            )}
            <div className="tx-actions">
              {running ? <button className="btn" onClick={stop}>중지</button> : <button className="btn primary" onClick={() => void start()} disabled={sectionIds.length === 0 || exporting}>이 범위 번역</button>}
            </div>
          </section>

          <section className="tx-block">
            <h3>내보내기</h3>
            <div className="tx-row">
              <select className="input" value={format} onChange={(e) => setFormat(e.target.value as Format)} disabled={exporting} aria-label="형식">
                {isPdf && <option value="pdf-sbs">PDF — 원문과 번역 나란히</option>}
                <option value="epub">EPUB — 번역된 전자책</option>
                <option value="pdf">PDF — 번역문 조판</option>
                <option value="html">HTML</option>
                <option value="markdown">Markdown</option>
                <option value="text">텍스트</option>
              </select>
              {format !== 'pdf-sbs' && <label className="tx-check"><input type="checkbox" checked={bilingual} onChange={(e) => setBilingual(e.target.checked)} disabled={exporting} /> 원문 함께</label>}
            </div>
            <p className="faint tx-note">
              {format === 'pdf-sbs' ? '쪽마다 왼쪽에 원문, 오른쪽에 번역을 놓습니다. 번역되지 않은 문단은 원문 그대로 남습니다. 글자는 이미지로 저장됩니다.' :
                format === 'epub' ? (isPdf ? '번역문을 전자책으로 만듭니다. 글자는 그대로 살아 있고, PDF 원본의 그림은 포함되지 않습니다.' : '번역문을 전자책으로 만듭니다. 글자는 그대로 살아 있고 책의 그림도 포함됩니다.') :
                format === 'pdf' ? '번역문을 A4로 조판합니다. 그림은 포함되지 않고, 글자는 이미지로 저장됩니다.' :
                  '글만 내보냅니다. 번역되지 않은 문단은 원문으로 들어갑니다.'}
            </p>
            {busy && busy !== 'saving' && (
              <div className="batch-line">
                <div className="batch-bar" role="progressbar" aria-valuemin={0} aria-valuemax={busy.total} aria-valuenow={busy.done} aria-label="내보내기 진행률"><span style={{ width: `${busy.total ? (busy.done / busy.total) * 100 : 0}%` }} /></div>
                <p className="batch-text">{busy.done} / {busy.total}{unit}</p>
              </div>
            )}
            <div className="tx-actions">
              {exporting && busy !== 'saving' ? <button className="btn" onClick={() => { cancelExport.current = true; }}>취소</button> : <button className="btn primary" onClick={() => void doExport()} disabled={sectionIds.length === 0 || exporting}>{running ? '지금까지 번역된 만큼 내보내기' : '내보내기'}</button>}
            </div>
          </section>
          {err && <p className="badge danger">{err}</p>}
        </div>
        <footer><button className="btn" onClick={onClose} disabled={exporting}>닫기</button></footer>
      </div>
    </ModalBackdrop>
  );
}
