import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { call, errorMessage, mutate } from '../../ipc/client';
import { useReaderSession } from './useReaderSession';
import { ReflowView, type ReflowHandle, type ReflowMode } from './ReflowView';
import { Sidebar } from './Sidebar';
import { HistoryDialog } from '../translation/HistoryDialog';
import type { CellActions } from '../translation/TranslationCell';
import type { ContentResolvers } from './SafeContentView';
import type { Block } from '../../ipc/generated/Block';
import type { ReaderAnchor } from '../../ipc/generated/ReaderAnchor';
import { useSettings } from '../../app/settings';
import { useToast } from '../../app/toast';
import { useShortcut } from '../../app/shortcuts';
import { EpubOriginalView, type EpubOriginalHandle } from '../../document/epub/EpubOriginalView';
import { PdfOriginalView, type PdfOriginalHandle } from '../../document/pdf/PdfOriginalView';
import { usePdfExtraction } from '../../document/pdf/usePdfExtraction';
import { setPasswordAsker } from '../../document/pdf/sessionPdf';
import './reader.css';

type PendingSwitch = { run: () => void } | null;

export function Reader({ documentId, onClose, onSettings }: { documentId: string; onClose: () => void; onSettings: () => void }) {
  const { model, sessionRef, loadSection, refreshSections, setGeneration, setBlockDisplay, resync, setSession } = useReaderSession(documentId);
  const { settings, set } = useSettings();
  const toast = useToast();
  const [mode, setModeState] = useState<ReflowMode>(settings.readingMode ?? 'parallel');
  const [pdfView, setPdfView] = useState<'original' | 'reflow'>(settings.pdfView ?? 'original');
  const [paused, setPaused] = useState(false);
  const [historyFor, setHistoryFor] = useState<Block | null>(null);
  const [highlight, setHighlight] = useState<string | null>(null);
  const [blockedReason, setBlockedReason] = useState<string | null>(null);
  const [sidebarTab, setSidebarTab] = useState<'toc' | 'search' | 'bookmarks'>('toc');
  const [pendingSwitch, setPendingSwitch] = useState<PendingSwitch>(null);
  const reflowRef = useRef<ReflowHandle>(null);
  const epubRef = useRef<EpubOriginalHandle>(null);
  const pdfRef = useRef<PdfOriginalHandle>(null);
  const dirty = useRef(new Map<string, () => Promise<boolean>>());
  const viewportSeq = useRef(0);
  const positionVersion = useRef(0);
  const saveTimer = useRef<number | null>(null);
  const restored = useRef(false);
  const session = model.session;
  const isPdf = session?.format === 'pdf';
  const targetLanguage = useMemo(() => settings.defaultTargetLanguage ?? 'ko', [settings.defaultTargetLanguage]);
  const sourceLanguage = (session?.metadata?.sourceLanguage as string | undefined) ?? settings.defaultSourceLanguage ?? 'en';
  const showOriginalEpub = !isPdf && mode === 'original';
  const showPdfOriginal = isPdf && pdfView === 'original';
  const autoActive = (settings.autoTranslate ?? true) && !paused && mode !== 'original' && !!settings.activeProfileId;

  usePdfExtraction(isPdf ? session : null, refreshSections, (s) => setSession(s));
  const [pwPrompt, setPwPrompt] = useState<{ reason: string; resolve: (v: string | null) => void } | null>(null);
  useEffect(() => {
    // PDF passwords are session-only: never persisted or sent to Rust.
    setPasswordAsker((reason) => new Promise((resolve) => setPwPrompt({ reason, resolve })));
    return () => setPasswordAsker(async () => null);
  }, []);

  useEffect(() => { if (session) positionVersion.current = Number(session.positionVersion); }, [session?.sessionId]); // eslint-disable-line react-hooks/exhaustive-deps

  // Keep backend reading mode in sync (original mode stops auto translation & prefetch).
  useEffect(() => {
    if (!session) return;
    void call('reader_set_mode', { sessionId: session.sessionId, mode: showPdfOriginal && mode === 'interleaved' ? 'parallel' : mode }).catch(() => {});
  }, [session?.sessionId, mode, showPdfOriginal]); // eslint-disable-line react-hooks/exhaustive-deps

  // Profile / language switch from settings => new generation (old responses never applied).
  const lastProfile = useRef<string | null | undefined>(undefined);
  useEffect(() => {
    if (!session) return;
    const key = `${settings.activeProfileId ?? ''}|${targetLanguage}`;
    if (lastProfile.current === undefined) { lastProfile.current = key; return; }
    if (lastProfile.current === key) return;
    lastProfile.current = key;
    void call<{ generation: number }>('reader_switch_profile', { sessionId: session.sessionId, profileId: settings.activeProfileId ?? null, targetLanguage, sourceLanguage: null })
      .then((r) => { setGeneration(r.generation); void resync(); })
      .catch((e) => toast({ kind: 'error', text: errorMessage(e) }));
  }, [settings.activeProfileId, targetLanguage, session?.sessionId]); // eslint-disable-line react-hooks/exhaustive-deps

  const currentAnchor = useCallback((): ReaderAnchor | null => {
    const s = sessionRef.current;
    if (!s?.extractionId) return null;
    let a: { blockId: string; ratio: number } | null = null;
    if (showPdfOriginal) a = pdfRef.current?.getAnchor() ?? null;
    else if (showOriginalEpub) a = epubRef.current?.getAnchor() ?? null;
    else a = reflowRef.current?.getAnchor() ?? null;
    if (!a) return null;
    return { schemaVersion: 1, documentId: s.documentId, extractionId: s.extractionId, blockId: a.blockId, sourceOffsetU16: 0, withinBlockRatio: Math.min(1, Math.max(0, a.ratio)), location: null };
  }, [sessionRef, showPdfOriginal, showOriginalEpub]);

  const savePosition = useCallback(async () => {
    const s = sessionRef.current;
    const anchor = currentAnchor();
    if (!s || !anchor) return;
    try {
      const r = await call<{ version: number }>('reader_save_position', { sessionId: s.sessionId, anchor, view: { mode, pdfView }, expectedVersion: positionVersion.current });
      positionVersion.current = r.version;
    } catch (e: unknown) {
      const cur = (e as { error?: { details?: { currentVersion?: number } } })?.error?.details?.currentVersion;
      if (typeof cur === 'number') positionVersion.current = cur;
    }
  }, [sessionRef, currentAnchor, mode, pdfView]);

  const onScrolled = useCallback(() => {
    if (saveTimer.current) window.clearTimeout(saveTimer.current);
    saveTimer.current = window.setTimeout(() => void savePosition(), 500);
  }, [savePosition]);

  const onVisible = useCallback((ids: string[], direction: 'forward' | 'backward') => {
    const s = sessionRef.current;
    if (!s) return;
    viewportSeq.current += 1;
    void call<{ blockedReason?: string }>('reader_set_viewport', { sessionId: s.sessionId, generation: s.generation, sequence: viewportSeq.current, visibleIds: autoActive ? ids : [], direction })
      .then((r) => setBlockedReason(r.blockedReason ?? null))
      .catch(() => {});
  }, [sessionRef, autoActive]);

  // Restore saved position once the first sections exist.
  useEffect(() => {
    if (restored.current || !session?.position || model.sections.length === 0) return;
    const a = session.position;
    if (!a.blockId || a.extractionId !== session.extractionId) { restored.current = true; return; }
    void (async () => {
      for (const sec of model.sections) {
        await loadSection(sec.id);
        if (reflowRef.current?.scrollToBlock(a.blockId!, a.withinBlockRatio) || pdfRef.current?.scrollToBlock(a.blockId!, a.withinBlockRatio) || epubRef.current?.scrollToBlock(a.blockId!)) break;
      }
      restored.current = true;
    })();
  }, [session, model.sections, loadSection]);

  /** Mode switch keeps the same source block (spec FR02). */
  const switchView = useCallback((apply: () => void) => {
    const run = () => {
      const a = currentAnchor();
      apply();
      if (a?.blockId) {
        const id = a.blockId;
        const ratio = a.withinBlockRatio;
        setTimeout(() => {
          reflowRef.current?.scrollToBlock(id, ratio) || pdfRef.current?.scrollToBlock(id, ratio) || epubRef.current?.scrollToBlock(id);
        }, 60);
      }
    };
    if (dirty.current.size > 0) setPendingSwitch({ run });
    else run();
  }, [currentAnchor]);

  const setMode = (m: ReflowMode) => switchView(() => {
    if (isPdf && pdfView === 'original' && m === 'interleaved') {
      toast({ kind: 'info', text: '문단 교차는 리플로우로 표시합니다' });
      setPdfView('reflow');
      void set('pdfView', 'reflow');
    }
    setModeState(m);
    void set('readingMode', m);
  });

  const close = async () => {
    const doClose = async () => {
      const s = sessionRef.current;
      if (s) await call('document_close', { sessionId: s.sessionId, generation: s.generation, finalAnchor: currentAnchor(), view: { mode, pdfView } }).catch(() => {});
      onClose();
    };
    if (dirty.current.size > 0) setPendingSwitch({ run: () => void doClose() });
    else await doClose();
  };

  useShortcut('mod+d', () => {
    const s = sessionRef.current;
    const a = currentAnchor();
    if (!s || !a) return;
    void mutate('bookmark_put', { documentId: s.documentId, bookmarkId: null, anchor: a, label: new Date().toLocaleString('ko-KR') })
      .then(() => { toast({ kind: 'info', text: '북마크를 추가했습니다.' }); setSidebarTab('bookmarks'); })
      .catch((e) => toast({ kind: 'error', text: errorMessage(e) }));
  });
  useShortcut('mod+f', () => setSidebarTab('search'));

  const revealBlock = useCallback(async (blockId: string, sectionId?: string) => {
    if (sectionId) await loadSection(sectionId);
    setHighlight(blockId);
    setTimeout(() => {
      if (showPdfOriginal) pdfRef.current?.scrollToBlock(blockId, 0, true);
      else if (showOriginalEpub) epubRef.current?.scrollToBlock(blockId);
      else reflowRef.current?.scrollToBlock(blockId, 0, true);
    }, 30);
    setTimeout(() => setHighlight((h) => (h === blockId ? null : h)), 2500);
  }, [loadSection, showPdfOriginal, showOriginalEpub]);

  const actions: CellActions = useMemo(() => ({
    retranslate: (blockId) => {
      const s = sessionRef.current;
      if (!s) return;
      void mutate('translation_request', { sessionId: s.sessionId, blockId, force: true }).catch((e) => toast({ kind: 'error', text: errorMessage(e) }));
    },
    translateNow: (blockId) => {
      const s = sessionRef.current;
      if (!s) return;
      void mutate<{ kind: string; display?: unknown }>('translation_request', { sessionId: s.sessionId, blockId, force: false })
        .catch((e) => toast({ kind: 'error', text: errorMessage(e) }));
    },
    cancel: (jobId) => {
      const s = sessionRef.current;
      if (!s) return;
      void call('translation_cancel', { sessionId: s.sessionId, jobId }).catch(() => {});
    },
    saveEdit: async (block, text, expectedHeadVersion) => {
      const s = sessionRef.current;
      if (!s) return false;
      try {
        await mutate('translation_save_edit', { sessionId: s.sessionId, blockId: block.id, targetLanguage, text, expectedHeadVersion });
        toast({ kind: 'info', text: '수정본을 저장했습니다.' }); // only after the DB commit
        return true;
      } catch (e) {
        toast({ kind: 'error', text: errorMessage(e) });
        return false;
      }
    },
    openHistory: (block) => setHistoryFor(block),
    revealSource: (block) => {
      if (isPdf) {
        if (pdfView !== 'original') { setPdfView('original'); }
        setTimeout(() => pdfRef.current?.scrollToBlock(block.id, 0, true), 80);
      } else {
        void revealBlock(block.id, block.sectionId);
      }
    },
    setExcluded: (block, excluded, prefVersion) => {
      const s = sessionRef.current;
      if (!s) return;
      void mutate('block_set_preference', { sessionId: s.sessionId, blockId: block.id, translationOverride: excluded ? 'preserve' : null, expectedVersion: prefVersion })
        .catch((e) => toast({ kind: 'error', text: errorMessage(e) }));
    },
    applyCandidate: (block) => setHistoryFor(block),
    setDirty: (blockId, isDirty, flush) => {
      if (isDirty && flush) dirty.current.set(blockId, flush);
      else dirty.current.delete(blockId);
    },
  }), [sessionRef, targetLanguage, toast, isPdf, pdfView, revealBlock]);

  const resolvers: ContentResolvers = useMemo(() => ({
    resourceUrl: (rid) => {
      const s = sessionRef.current;
      const res = (s?.metadata?.resources as Record<string, string> | undefined)?.[rid];
      return s && res ? `thoth-doc://localhost/${s.assetToken}/epub/${res}` : null;
    },
    openExternal: (url) => { void call('open_external', { url }).catch((e) => toast({ kind: 'error', text: errorMessage(e) })); },
    openInternal: (href) => {
      const m = /^(\d+)\.xhtml/.exec(href.replace(/^.*\//, ''));
      if (!m) return;
      const sec = model.sections.find((s) => Number((s.locator as { spineIndex?: number }).spineIndex) === Number(m[1]));
      if (sec) { void loadSection(sec.id); reflowRef.current?.scrollToSection(sec.id); }
    },
  }), [sessionRef, toast, model.sections, loadSection]);

  const togglePause = async () => {
    const s = sessionRef.current;
    if (!s) return;
    const next = !paused;
    setPaused(next);
    await call('translation_pause', { sessionId: s.sessionId, paused: next, cancelRunning: false }).catch(() => {});
  };
  const cancelRunning = async () => {
    const s = sessionRef.current;
    if (!s) return;
    await call('translation_pause', { sessionId: s.sessionId, paused: true, cancelRunning: true }).catch(() => {});
    setPaused(true);
  };

  if (model.error) {
    return (
      <div className="reader-error">
        <p>{model.error}</p>
        <button className="btn" onClick={onClose}>라이브러리로</button>
      </div>
    );
  }
  if (!session) return <div className="reader-loading muted">문서를 여는 중…</div>;

  const restrictions = (session.metadata?.restrictions as string[] | undefined) ?? [];
  const title = (session.metadata?.title as string) || (session.metadata?.pdfTitle as string) || session.title;
  return (
    <div className="reader">
      <header className="toolbar">
        <button className="btn ghost" onClick={() => void close()} aria-label="라이브러리로 돌아가기">← 라이브러리</button>
        <h1 className="doc-title" title={title}>{title}</h1>
        {restrictions.length > 0 && <span className="badge warn" title={restrictions.join(', ')}>제한된 문서</span>}
        {model.indexProgress && model.indexProgress.ready + model.indexProgress.failed < model.indexProgress.total && (
          <span className="badge">색인 {model.indexProgress.ready}/{model.indexProgress.total}</span>
        )}
        <span className="spacer" />
        {isPdf && (
          <div className="seg" role="group" aria-label="PDF 보기">
            <button aria-pressed={pdfView === 'original'} onClick={() => switchView(() => { setPdfView('original'); void set('pdfView', 'original'); if (mode === 'interleaved') setModeState('parallel'); })}>원본</button>
            <button aria-pressed={pdfView === 'reflow'} onClick={() => switchView(() => { setPdfView('reflow'); void set('pdfView', 'reflow'); })}>리플로우</button>
          </div>
        )}
        <div className="seg" role="group" aria-label="읽기 모드">
          <button aria-pressed={mode === 'parallel'} onClick={() => setMode('parallel')}>병렬</button>
          <button aria-pressed={mode === 'interleaved'} onClick={() => setMode('interleaved')}>교차</button>
          <button aria-pressed={mode === 'original'} onClick={() => setMode('original')}>원문</button>
        </div>
        <span className="lang-pair" title="원문 → 대상 언어">{sourceLanguage} → {targetLanguage}</span>
        <button className={`btn small ${paused ? '' : 'ghost'}`} onClick={() => void togglePause()} aria-pressed={paused} title="자동 번역 일시정지">{paused ? '자동 번역 재개' : '일시정지'}</button>
        {paused && <button className="btn small ghost" onClick={() => void cancelRunning()}>진행 중 요청 취소</button>}
        <button className="btn small ghost" onClick={onSettings}>{settings.activeProfileId ? '번역 설정' : '번역 설정 필요'}</button>
      </header>
      {blockedReason === 'source_language_required' && (
        <SourceLanguagePrompt onPick={(lang) => {
          const s = sessionRef.current;
          if (!s) return;
          void call<{ generation: number }>('reader_switch_profile', { sessionId: s.sessionId, profileId: null, targetLanguage: null, sourceLanguage: lang })
            .then((r) => { setGeneration(r.generation); setBlockedReason(null); setSession({ ...s, generation: r.generation, metadata: { ...s.metadata, sourceLanguage: lang } }); });
        }} />
      )}
      {blockedReason && blockedReason !== 'source_language_required' && autoActive && (
        <div className="banner warn">
          {blockedReason === 'no_profile' ? '번역 모델이 설정되지 않았습니다. 원문과 저장된 번역은 계속 읽을 수 있습니다.' :
            blockedReason === 'AUTH_FAILED' ? '인증에 실패해 Provider를 중지했습니다. 설정에서 키를 확인해 주세요.' :
              blockedReason === 'PROVIDER_UNAVAILABLE' ? 'Provider가 비활성 상태입니다. 원문과 저장된 번역은 계속 읽을 수 있습니다.' : null}
        </div>
      )}
      <div className="reader-body">
        <Sidebar
          tab={sidebarTab}
          onTab={setSidebarTab}
          session={session}
          model={model}
          onJump={(blockId, sectionId) => void revealBlock(blockId, sectionId)}
          onSection={(sectionId) => { void loadSection(sectionId); reflowRef.current?.scrollToSection(sectionId); pdfRef.current?.scrollToSection(sectionId); epubRef.current?.scrollToSection(sectionId); }}
          currentAnchor={currentAnchor}
        />
        <main className="content">
          {showPdfOriginal ? (
            <PdfOriginalView
              ref={pdfRef}
              session={session}
              model={model}
              loadSection={loadSection}
              mode={mode === 'original' ? 'original' : 'parallel'}
              onVisible={onVisible}
              onScrolled={onScrolled}
              actions={actions}
              autoActive={autoActive}
              targetLanguage={targetLanguage}
              syncScroll={settings.syncScroll ?? true}
              highlightBlockId={highlight}
            />
          ) : showOriginalEpub ? (
            <EpubOriginalView ref={epubRef} session={session} model={model} loadSection={loadSection} onScrolled={onScrolled} openExternal={resolvers.openExternal} />
          ) : (
            <ReflowView
              ref={reflowRef}
              model={model}
              mode={mode}
              loadSection={loadSection}
              onVisible={onVisible}
              onScrolled={onScrolled}
              resolvers={resolvers}
              actions={actions}
              autoActive={autoActive}
              targetLanguage={targetLanguage}
              sourceLanguage={sourceLanguage}
              highlightBlockId={highlight}
            />
          )}
        </main>
      </div>
      {historyFor && (
        <HistoryDialog sessionId={session.sessionId} block={historyFor} targetLanguage={targetLanguage} onClose={() => setHistoryFor(null)} onApplied={() => void call('reader_get_snapshot', { sessionId: session.sessionId, afterSequence: 0, blockIds: [historyFor.id] }).then((r) => { const d = (r as { display: Parameters<typeof setBlockDisplay>[0][] }).display[0]; if (d) setBlockDisplay(d); })} />
      )}
      {pwPrompt && <PasswordDialog reason={pwPrompt.reason} onDone={(v) => { pwPrompt.resolve(v); setPwPrompt(null); }} />}
      {pendingSwitch && (
        <div className="modal-backdrop" role="alertdialog" aria-modal="true" aria-labelledby="unsaved-title">
          <div className="modal" style={{ width: 420 }}>
            <header><h2 id="unsaved-title">저장하지 않은 수정이 있습니다</h2></header>
            <div className="body"><p>편집 중인 번역을 저장할까요?</p></div>
            <footer>
              <button className="btn" onClick={() => setPendingSwitch(null)}>계속 편집</button>
              <button className="btn danger" onClick={() => { dirty.current.clear(); const p = pendingSwitch; setPendingSwitch(null); p.run(); }}>폐기</button>
              <button className="btn primary" onClick={async () => {
                const flushes = [...dirty.current.values()];
                const results = await Promise.all(flushes.map((f) => f()));
                if (results.every(Boolean)) { dirty.current.clear(); const p = pendingSwitch; setPendingSwitch(null); p.run(); }
              }}>저장</button>
            </footer>
          </div>
        </div>
      )}
    </div>
  );
}

function SourceLanguagePrompt({ onPick }: { onPick: (lang: string) => void }) {
  const [v, setV] = useState('en');
  return (
    <div className="banner warn" role="alert">
      원문 언어를 확인할 수 없습니다. 잘못된 언어로 요청하지 않도록 원문 언어를 지정해 주세요.
      <select className="input" value={v} onChange={(e) => setV(e.target.value)} aria-label="원문 언어">
        <option value="en">영어 (en)</option><option value="ja">일본어 (ja)</option><option value="zh">중국어 (zh)</option>
        <option value="de">독일어 (de)</option><option value="fr">프랑스어 (fr)</option><option value="es">스페인어 (es)</option><option value="ko">한국어 (ko)</option>
      </select>
      <button className="btn small primary" onClick={() => onPick(v)}>적용</button>
    </div>
  );
}

function PasswordDialog({ reason, onDone }: { reason: string; onDone: (v: string | null) => void }) {
  const [v, setV] = useState('');
  return (
    <div className="modal-backdrop" role="dialog" aria-modal="true" aria-labelledby="pw-title">
      <form className="modal" style={{ width: 380 }} onSubmit={(e) => { e.preventDefault(); onDone(v); }}>
        <header><h2 id="pw-title">PDF 암호</h2></header>
        <div className="body">
          <p>{reason === 'incorrect_password' ? '암호가 올바르지 않습니다. 다시 입력해 주세요.' : '이 PDF는 암호로 보호되어 있습니다. 암호는 이번 세션에서만 사용하며 저장하지 않습니다.'}</p>
          <input className="input" type="password" autoFocus value={v} onChange={(e) => setV(e.target.value)} aria-label="PDF 암호" style={{ width: '100%' }} />
        </div>
        <footer>
          <button type="button" className="btn" onClick={() => onDone(null)}>취소</button>
          <button type="submit" className="btn primary">열기</button>
        </footer>
      </form>
    </div>
  );
}
