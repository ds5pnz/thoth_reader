import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { ModalBackdrop } from '../../app/Modal';
import { call, errorMessage, mutate } from '../../ipc/client';
import { useReaderSession, trackClose } from './useReaderSession';
import { ReflowView, type ReflowHandle, type ReflowMode } from './ReflowView';
import { Sidebar } from './Sidebar';
import { HistoryDialog } from '../translation/HistoryDialog';
import type { CellActions } from '../translation/TranslationCell';
import { clearDrafts } from '../translation/TranslationCell';
import { NotesPanel, SelectionToolbar, useAnnotations, useHighlightPainter } from '../annotations/annotations';
import { findAll } from '../annotations/quote';
import { PdfThumbnails } from '../../document/pdf/Thumbnails';
import { TranslateExportDialog } from '../export/TranslateExportDialog';
import { formatEta } from '../export/range';
import type { ContentResolvers } from './SafeContentView';
import type { Block } from '../../ipc/generated/Block';
import type { ReaderAnchor } from '../../ipc/generated/ReaderAnchor';
import { useSettings } from '../../app/settings';
import { useToast } from '../../app/toast';
import { useShortcut } from '../../app/shortcuts';
import { flagOf } from '../../domain/flags';
import { describeActiveProfile, providerName, useActiveProfile } from '../settings/activeProfile';
import { IconExport, IconFullscreen, IconFullscreenExit, IconGear, IconLibrary, IconPause, IconPlay, IconSidebar } from './icons';
import { lazy, Suspense } from 'react';
import type { EpubOriginalHandle } from '../../document/epub/EpubOriginalView';
// epub.js (and its xmldom dependency) only loads when original EPUB mode is opened.
const EpubOriginalView = lazy(() => import('../../document/epub/EpubOriginalView').then((m) => ({ default: m.EpubOriginalView })));
import { PdfOriginalView, type PdfOriginalHandle } from '../../document/pdf/PdfOriginalView';
import { usePdfExtraction } from '../../document/pdf/usePdfExtraction';
import { setPasswordAsker } from '../../document/pdf/sessionPdf';
import './reader.css';

type PendingSwitch = { run: () => void } | null;

export function Reader({ documentId, onClose, onSettings }: { documentId: string; onClose: () => void; onSettings: () => void }) {
  const { model, sessionRef, loadSection, refreshSections, setGeneration, setBlockDisplay, resync, setSession, setBatch } = useReaderSession(documentId);
  const { settings, set } = useSettings();
  const toast = useToast();
  const [mode, setModeState] = useState<ReflowMode>(settings.readingMode ?? 'parallel');
  const [pdfView, setPdfView] = useState<'original' | 'reflow'>(settings.pdfView ?? 'original');
  const [paused, setPaused] = useState(false);
  const [historyFor, setHistoryFor] = useState<Block | null>(null);
  const [highlight, setHighlight] = useState<string | null>(null);
  const [blockedReason, setBlockedReason] = useState<string | null>(null);
  const [sidebarTab, setSidebarTabState] = useState<'toc' | 'search' | 'bookmarks' | 'notes' | 'pages'>('toc');
  // Sidebar visibility is a per-device convenience (not document data): localStorage.
  const [sidebarOpen, setSidebarOpen] = useState<boolean>(() => {
    try { return localStorage.getItem('thoth.sidebarOpen') !== '0'; } catch { return true; }
  });
  const toggleSidebar = useCallback(() => {
    // Keep the reading position while the content column changes width.
    switchViewRef.current(() => setSidebarOpen((v) => {
      try { localStorage.setItem('thoth.sidebarOpen', v ? '0' : '1'); } catch { /* storage unavailable */ }
      return !v;
    }));
  }, []);
  /** Choosing a tab (search shortcut, new bookmark) always reveals the sidebar. */
  const setSidebarTab = useCallback((t: 'toc' | 'search' | 'bookmarks' | 'notes' | 'pages') => {
    setSidebarTabState(t);
    setSidebarOpen(true);
    try { localStorage.setItem('thoth.sidebarOpen', '1'); } catch { /* storage unavailable */ }
  }, []);
  const switchViewRef = useRef<(apply: () => void) => void>((apply) => apply());
  const [pendingSwitch, setPendingSwitch] = useState<PendingSwitch>(null);
  const reflowRef = useRef<ReflowHandle>(null);
  const epubRef = useRef<EpubOriginalHandle>(null);
  const pdfRef = useRef<PdfOriginalHandle>(null);
  const dirty = useRef(new Map<string, () => Promise<boolean>>());
  const viewportSeq = useRef(0);
  const positionVersion = useRef(0);
  const saveTimer = useRef<number | null>(null);
  const restored = useRef(false);
  const blocksRef = useRef(model.blocks);
  blocksRef.current = model.blocks;
  const session = model.session;
  const isPdf = session?.format === 'pdf';
  const targetLanguage = useMemo(() => settings.defaultTargetLanguage ?? 'ko', [settings.defaultTargetLanguage]);
  const sourceLanguage = (session?.metadata?.sourceLanguage as string | undefined) ?? settings.defaultSourceLanguage ?? 'en';
  const showOriginalEpub = !isPdf && mode === 'original';
  const showPdfOriginal = isPdf && pdfView === 'original';
  const autoActive = (settings.autoTranslate ?? true) && !paused && mode !== 'original' && !!settings.activeProfileId;
  const activeProfile = useActiveProfile(settings.activeProfileId);

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

  /** Measure the anchor from the active view's DOM (25% line rule). */
  const measureAnchor = useCallback((): ReaderAnchor | null => {
    const s = sessionRef.current;
    if (!s?.extractionId) return null;
    let a: { blockId: string; ratio: number } | null = null;
    if (showPdfOriginal) a = pdfRef.current?.getAnchor() ?? null;
    else if (showOriginalEpub) a = epubRef.current?.getAnchor() ?? null;
    else a = reflowRef.current?.getAnchor() ?? null;
    if (!a) return null;
    // Keep the block's source location so restore can find the section (and, if the
    // extraction changes, fall back to CFI / PDF coordinates per spec §3.3).
    let location: ReaderAnchor['location'] = null;
    for (const bs of blocksRef.current.values()) {
      const hit = bs.find((b) => b.id === a!.blockId);
      if (hit) { location = hit.location; break; }
    }
    return { schemaVersion: 1, documentId: s.documentId, extractionId: s.extractionId, blockId: a.blockId, sourceOffsetU16: 0, withinBlockRatio: Math.min(1, Math.max(0, a.ratio)), location };
  }, [sessionRef, showPdfOriginal, showOriginalEpub]);

  // The reading anchor is state that only *user* scrolling changes. Mode switches,
  // re-layout and streaming translations restore from it and never re-measure it, so
  // repeated switches cannot drift (spec §3.4, T06).
  const userAnchor = useRef<ReaderAnchor | null>(null);
  const currentAnchor = useCallback((): ReaderAnchor | null => userAnchor.current ?? measureAnchor(), [measureAnchor]);

  // Style / glossary changes alter the cache key: refresh display state so stale cells are
  // marked and visible blocks are requested again with the new setting.
  const styleKey = `${settings.translationStyle ?? 'polite'}|${JSON.stringify(settings.glossary ?? [])}`;
  const lastStyleKey = useRef(styleKey);
  useEffect(() => {
    if (lastStyleKey.current === styleKey) return;
    lastStyleKey.current = styleKey;
    // Reloading display state re-renders the list, which re-reports the visible blocks.
    void resync();
  }, [styleKey, resync]);

  useEffect(() => {
    if (!import.meta.env.DEV) return;
    window.__thoth = {
      ...(window.__thoth ?? {}),
      anchor: () => currentAnchor(),
      session: () => sessionRef.current,
      anchorError: () => {
        const a = currentAnchor();
        if (!a?.blockId) return null;
        return reflowRef.current?.anchorErrorPx(a.blockId, a.withinBlockRatio) ?? pdfRef.current?.anchorErrorPx(a.blockId, a.withinBlockRatio) ?? epubRef.current?.anchorErrorPx(a.blockId, a.withinBlockRatio) ?? null;
      },
    };
  }, [currentAnchor]);

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

  const onScrolledSaveOnly = useCallback(() => {
    if (saveTimer.current) window.clearTimeout(saveTimer.current);
    saveTimer.current = window.setTimeout(() => void savePosition(), 500);
  }, [savePosition]);

  /** Called by views for user-initiated scrolling only. */
  const anchorTimer = useRef<number | null>(null);
  const onScrolled = useCallback(() => {
    if (anchorTimer.current) window.clearTimeout(anchorTimer.current);
    anchorTimer.current = window.setTimeout(() => { anchorTimer.current = null; const a = measureAnchor(); if (a) userAnchor.current = a; }, 160);
    if (saveTimer.current) window.clearTimeout(saveTimer.current);
    saveTimer.current = window.setTimeout(() => void savePosition(), 500);
  }, [savePosition, measureAnchor]);

  const lastAnchor = useRef<ReaderAnchor | null>(null);
  const onVisible = useCallback((ids: string[], direction: 'forward' | 'backward') => {
    const s = sessionRef.current;
    if (!s) return;
    const a = currentAnchor();
    if (a) lastAnchor.current = a;
    viewportSeq.current += 1;
    void call<{ blockedReason?: string }>('reader_set_viewport', { sessionId: s.sessionId, generation: s.generation, sequence: viewportSeq.current, visibleIds: autoActive ? ids : [], direction })
      .then((r) => setBlockedReason(r.blockedReason ?? null))
      .catch(() => {});
  }, [sessionRef, autoActive, currentAnchor]);

  // Restore saved position once the first sections exist: load the anchor's section,
  // then retry until its rows are rendered (React commits after loadSection resolves).
  useEffect(() => {
    if (restored.current || !session?.position || model.sections.length === 0) return;
    const a = session.position;
    restored.current = true;
    if (!a.blockId || a.extractionId !== session.extractionId) return;
    userAnchor.current = a;
    const loc = a.location;
    const want = loc?.kind === 'epub' ? loc.spineIndex : loc?.kind === 'pdf' ? loc.pageIndex : null;
    const target = want == null ? null : model.sections.find((sec) => {
      const l = sec.locator as { spineIndex?: number; pageIndex?: number };
      return Number(l.spineIndex ?? l.pageIndex ?? sec.ordinal) === want;
    });
    void (async () => {
      const candidates = target ? [target] : model.sections;
      for (const sec of candidates) {
        await loadSection(sec.id);
        for (let i = 0; i < 30; i++) {
          if (reflowRef.current?.scrollToBlock(a.blockId!, a.withinBlockRatio) || pdfRef.current?.scrollToBlock(a.blockId!, a.withinBlockRatio) || epubRef.current?.scrollToBlock(a.blockId!, a.withinBlockRatio)) return;
          await new Promise((r) => setTimeout(r, 100));
          if (!blocksRef.current.has(sec.id)) continue;
          if (!blocksRef.current.get(sec.id)!.some((b) => b.id === a.blockId)) break;
        }
      }
    })();
  }, [session, model.sections, loadSection]);

  /** Mode switch keeps the same source block (spec FR02). */
  const switchView = useCallback((apply: () => void) => {
    const run = () => {
      // A scroll that ended <160 ms ago has not been measured yet: do it now, before the
      // layout changes, or the switch would restore the position from before that scroll.
      if (anchorTimer.current) {
        window.clearTimeout(anchorTimer.current);
        anchorTimer.current = null;
        const m = measureAnchor();
        if (m) userAnchor.current = m;
      }
      const a = currentAnchor();
      apply();
      if (a?.blockId) {
        const id = a.blockId;
        const ratio = a.withinBlockRatio;
        userAnchor.current = a; // pin: the switch itself must not move the anchor
        const restore = () => reflowRef.current?.scrollToBlock(id, ratio) || pdfRef.current?.scrollToBlock(id, ratio) || epubRef.current?.scrollToBlock(id, ratio);
        // Retry until the new view has mounted and rendered the block, then once more
        // after row heights are re-measured (spec §3.4).
        let tries = 0;
        const attempt = () => {
          if (restore()) { setTimeout(restore, 350); return; }
          if (++tries < 40) setTimeout(attempt, 75);
        };
        setTimeout(attempt, 40);
      }
    };
    if (dirty.current.size > 0) setPendingSwitch({ run });
    else run();
  }, [currentAnchor, measureAnchor]);

  switchViewRef.current = switchView;
  useShortcut('mod+\\', toggleSidebar);

  const setMode = (m: ReflowMode) => switchView(() => {
    if (isPdf && pdfView === 'original' && m === 'interleaved') {
      toast({ kind: 'info', text: '문단 교차는 리플로우로 표시합니다' });
      setPdfView('reflow');
      void set('pdfView', 'reflow');
    }
    setModeState(m);
    void set('readingMode', m);
  });

  // Any unmount (route change, window reload) must close the session: cancel its jobs,
  // revoke the asset token and save the final position (spec §7.3).
  const closedRef = useRef(false);
  const anchorFnRef = useRef(currentAnchor);
  anchorFnRef.current = currentAnchor;
  const viewRef = useRef({ mode, pdfView });
  viewRef.current = { mode, pdfView };
  // Layout-effect cleanup runs before React detaches the DOM, so the anchor is still measurable.
  useLayoutEffect(() => () => {
    const s = sessionRef.current;
    if (closedRef.current || !s) return;
    closedRef.current = true;
    trackClose(s.documentId, call('document_close', { sessionId: s.sessionId, generation: s.generation, finalAnchor: anchorFnRef.current() ?? lastAnchor.current, view: viewRef.current }));
  }, [sessionRef]);

  const close = async () => {
    const doClose = async () => {
      const s = sessionRef.current;
      closedRef.current = true;
      if (s) {
        const p = call('document_close', { sessionId: s.sessionId, generation: s.generation, finalAnchor: currentAnchor() ?? lastAnchor.current, view: { mode, pdfView } });
        trackClose(s.documentId, p);
        await p.catch(() => {});
      }
      onClose();
    };
    if (dirty.current.size > 0) setPendingSwitch({ run: () => void doClose() });
    else await doClose();
  };

  const [bookmarksVersion, setBookmarksVersion] = useState(0);
  const [exportOpen, setExportOpen] = useState(false);
  // Full screen: nothing but the book. The toolbar comes back while the pointer is at the top.
  const [fullscreen, setFullscreenState] = useState(false);
  const [barShown, setBarShown] = useState(false);
  const setFullscreen = useCallback(async (on: boolean) => {
    try {
      const r = await call<{ fullscreen: boolean }>('window_fullscreen', { on });
      setFullscreenState(r.fullscreen);
      setBarShown(false);
      // The button just clicked would keep the toolbar open through :focus-within.
      (document.activeElement as HTMLElement | null)?.blur?.();
    } catch { /* window refused: stay as is */ }
  }, []);
  useEffect(() => {
    // Left by the system (green button, Mission Control): follow it.
    const sync = () => void call<{ fullscreen: boolean }>('window_fullscreen', { on: null }).then((r) => setFullscreenState(r.fullscreen)).catch(() => {});
    let t = 0;
    const onResize = () => { window.clearTimeout(t); t = window.setTimeout(sync, 400); };
    window.addEventListener('resize', onResize);
    return () => { window.clearTimeout(t); window.removeEventListener('resize', onResize); };
  }, []);
  useEffect(() => () => { void call('window_fullscreen', { on: false }).catch(() => {}); }, []);
  useShortcut('mod+shift+f', () => void setFullscreen(!fullscreen));
  useShortcut('escape', () => void setFullscreen(false), { enabled: fullscreen });
  // Highlights and notes
  const readerRef = useRef<HTMLDivElement>(null);
  const annotations = useAnnotations(session?.documentId, targetLanguage);
  const blocksByPdfPage = useMemo(() => {
    const m = new Map<number, Block[]>();
    for (const [, bs] of model.blocks) for (const b of bs) if (b.location.kind === 'pdf') (m.get(b.location.pageIndex) ?? m.set(b.location.pageIndex, []).get(b.location.pageIndex)!).push(b);
    for (const l of m.values()) l.sort((a, b) => a.ordinal - b.ordinal);
    return m;
  }, [model.blocks]);
  const pageBlocks = useCallback((page: number) => blocksByPdfPage.get(page) ?? [], [blocksByPdfPage]);
  // Search: every match in what is on screen is tinted while a query is active.
  const [findQuery, setFindQuery] = useState('');
  const findExtra = useMemo(() => ({
    name: 'thoth-find',
    ranges: () => {
      const el = readerRef.current;
      if (!el || !findQuery) return [];
      const out: Range[] = [];
      for (const n of el.querySelectorAll('.brow > .src, .tcell-text:not(.preview), .pdf-page:not(.translated) .textLayer, .tp-text')) {
        out.push(...findAll(n, findQuery, 100));
        if (out.length > 600) break;
      }
      return out;
    },
  }), [findQuery]);
  useHighlightPainter(readerRef, annotations.items, pageBlocks, session?.sessionId, findExtra);
  useShortcut('mod+d', () => {
    const s = sessionRef.current;
    const a = currentAnchor();
    if (!s || !a) return;
    void mutate('bookmark_put', { documentId: s.documentId, bookmarkId: null, anchor: a, label: new Date().toLocaleString('ko-KR') })
      .then(() => { toast({ kind: 'info', text: '북마크를 추가했습니다.' }); setSidebarTab('bookmarks'); setBookmarksVersion((v) => v + 1); })
      .catch((e) => toast({ kind: 'error', text: errorMessage(e) }));
  });
  useShortcut('mod+f', () => setSidebarTab('search'));

  const revealBlock = useCallback(async (blockId: string, sectionId?: string) => {
    if (sectionId) await loadSection(sectionId);
    // An explicit jump (search hit, bookmark, source reveal) becomes the new anchor.
    const s0 = sessionRef.current;
    if (s0?.extractionId) {
      let location: ReaderAnchor['location'] = null;
      for (const bs of blocksRef.current.values()) { const hit = bs.find((b) => b.id === blockId); if (hit) { location = hit.location; break; } }
      userAnchor.current = { schemaVersion: 1, documentId: s0.documentId, extractionId: s0.extractionId, blockId, sourceOffsetU16: 0, withinBlockRatio: 0, location };
      onScrolledSaveOnly();
    }
    setHighlight(blockId);
    setTimeout(() => {
      if (showPdfOriginal) pdfRef.current?.scrollToBlock(blockId, 0, true);
      else if (showOriginalEpub) epubRef.current?.scrollToBlock(blockId);
      else reflowRef.current?.scrollToBlock(blockId, 0, true);
    }, 30);
    setTimeout(() => setHighlight((h) => (h === blockId ? null : h)), 2500);
  }, [loadSection, showPdfOriginal, showOriginalEpub, sessionRef, onScrolledSaveOnly]);

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
    <div className={`reader ${fullscreen ? 'fs' : ''} ${fullscreen && barShown ? 'bar-shown' : ''}`} ref={readerRef}>
      {fullscreen && <div className="fs-top-strip" onMouseEnter={() => setBarShown(true)} aria-hidden="true" />}
      <SelectionToolbar
        root={readerRef}
        pageBlocks={pageBlocks}
        onCreate={(a) => void annotations.create(a)}
        onTranslate={async (text) => (await call<{ text: string }>('translate_selection', { sessionId: session.sessionId, text })).text}
        paragraphPair={(blockId) => {
          for (const [, bs] of model.blocks) {
            const b = bs.find((x) => x.id === blockId);
            if (b) return { source: b.sourceText, translation: model.display.get(blockId)?.shown?.text ?? '' };
          }
          return null;
        }}
      />
      <header className="toolbar" onMouseLeave={() => { if (fullscreen) setBarShown(false); }} onFocus={() => { if (fullscreen) setBarShown(true); }} onBlur={(e) => { if (fullscreen && !e.currentTarget.contains(e.relatedTarget as Node | null)) setBarShown(false); }}>
        <button className={`btn icon ${sidebarOpen ? 'active' : 'ghost'}`} onClick={toggleSidebar} aria-pressed={sidebarOpen} aria-controls="reader-sidebar" aria-label={sidebarOpen ? '사이드바 닫기' : '사이드바 열기'} title={sidebarOpen ? '사이드바 닫기 (⌘\\)' : '사이드바 열기 (⌘\\)'}>
          <IconSidebar />
        </button>
        <button className="btn icon ghost" onClick={() => void close()} aria-label="서재로 돌아가기" title="서재로 돌아가기"><IconLibrary /></button>
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
          <button aria-pressed={mode === 'translation'} onClick={() => setMode('translation')} title="번역문만 봅니다. 문단을 누르면 원문이 보입니다.">번역</button>
        </div>
        <span className="lang-pair" role="img" aria-label={`원문 ${sourceLanguage}에서 대상 언어 ${targetLanguage}로 번역`} title={`${sourceLanguage} → ${targetLanguage}`}>
          <span className="flag">{flagOf(sourceLanguage) ?? sourceLanguage}</span>
          <span className="arrow" aria-hidden="true">→</span>
          <span className="flag">{flagOf(targetLanguage) ?? targetLanguage}</span>
        </span>
        {mode !== 'original' && (
          <button
            className={`btn ghost small model-chip ${activeProfile && activeProfile.providerEnabled ? '' : 'attention'}`}
            onClick={onSettings}
            title={activeProfile ? `번역 모델: ${describeActiveProfile(activeProfile)}${activeProfile.providerEnabled ? '' : ' (Provider 비활성)'}` : '번역 모델이 설정되지 않았습니다'}
            aria-label={activeProfile ? `번역 모델 ${activeProfile.modelId}, ${providerName(activeProfile.providerLabel, activeProfile.providerKind)}` : '번역 모델 설정 필요'}
          >
            {activeProfile ? activeProfile.modelId : '모델 없음'}
          </button>
        )}
        <button className={`btn ghost tx-open ${model.batch?.state === 'running' ? 'running' : 'icon'}`} onClick={() => setExportOpen(true)} title="범위 번역 · 내보내기" aria-label="범위 번역 · 내보내기">
          <IconExport />
          {model.batch?.state === 'running' && <span>{`${model.batch.total > 0 ? Math.round(((model.batch.done + model.batch.failed + model.batch.skipped) / model.batch.total) * 100) : 0}% · ${formatEta(model.batch.etaSeconds)}`}</span>}
        </button>
        <button className={`btn icon ${fullscreen ? 'active' : 'ghost'}`} onClick={() => void setFullscreen(!fullscreen)} aria-pressed={fullscreen} aria-label={fullscreen ? '전체 화면 끝내기' : '전체 화면'} title={fullscreen ? '전체 화면 끝내기 (Esc)' : '전체 화면 (⇧⌘F)'}>
          {fullscreen ? <IconFullscreenExit /> : <IconFullscreen />}
        </button>
        <button className={`btn icon ${paused ? 'active' : 'ghost'}`} onClick={() => void togglePause()} aria-pressed={paused} aria-label={paused ? '자동 번역 재개' : '자동 번역 일시정지'} title={paused ? '자동 번역 재개' : '자동 번역 일시정지'}>
          {paused ? <IconPlay /> : <IconPause />}
        </button>
        {paused && <button className="btn small ghost" onClick={() => void cancelRunning()}>진행 중 요청 취소</button>}
        <button className={`btn icon ghost ${settings.activeProfileId ? '' : 'attention'}`} onClick={onSettings} aria-label={settings.activeProfileId ? '번역 설정' : '번역 설정 필요'} title={settings.activeProfileId ? '번역 설정 (⌘,)' : '번역 설정이 필요합니다 (⌘,)'}>
          <IconGear />
        </button>
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
        {sidebarOpen && !fullscreen && <Sidebar
          tab={sidebarTab}
          bookmarksVersion={bookmarksVersion}
          onFindQuery={setFindQuery}
          pagesPanel={isPdf ? (
            <PdfThumbnails
              session={session}
              currentPage={() => {
                if (pdfRef.current) return pdfRef.current.currentPage();
                const l = currentAnchor()?.location;
                return l?.kind === 'pdf' ? l.pageIndex : 0;
              }}
              onPage={(i) => {
                if (pdfRef.current) { pdfRef.current.scrollToPage(i); return; }
                const sec = model.sections.find((x) => Number(x.ordinal) === i);
                if (sec) { userAnchor.current = null; void loadSection(sec.id); reflowRef.current?.scrollToSection(sec.id); }
              }}
            />
          ) : undefined}
          notes={<NotesPanel items={annotations.items} onJump={(blockId, sectionId) => void revealBlock(blockId, sectionId)} onUpdate={(a, patch) => void annotations.update(a, patch)} onRemove={(a) => void annotations.remove(a)} />}
          onTab={setSidebarTab}
          session={session}
          model={model}
          onJump={(blockId, sectionId) => void revealBlock(blockId, sectionId)}
          onSection={(sectionId) => { userAnchor.current = null; void loadSection(sectionId); reflowRef.current?.scrollToSection(sectionId); pdfRef.current?.scrollToSection(sectionId); epubRef.current?.scrollToSection(sectionId); }}
          currentAnchor={currentAnchor}
        />}
        <main className="content">
          {showPdfOriginal ? (
            <PdfOriginalView
              ref={pdfRef}
              session={session}
              model={model}
              loadSection={loadSection}
              mode={mode === 'original' ? 'original' : mode === 'translation' ? 'translation' : 'parallel'}
              onVisible={onVisible}
              onScrolled={onScrolled}
              actions={actions}
              autoActive={autoActive}
              targetLanguage={targetLanguage}
              syncScroll={settings.syncScroll ?? true}
              highlightBlockId={highlight}
            />
          ) : showOriginalEpub ? (
            <Suspense fallback={<div className="muted" style={{ padding: 24 }}>원본 보기를 불러오는 중…</div>}>
              <EpubOriginalView ref={epubRef} session={session} model={model} loadSection={loadSection} onScrolled={onScrolled} openExternal={resolvers.openExternal} annotations={annotations.items} />
            </Suspense>
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
              pdfSession={isPdf ? session : null}
            />
          )}
        </main>
      </div>
      {historyFor && (
        <HistoryDialog sessionId={session.sessionId} block={historyFor} targetLanguage={targetLanguage} onClose={() => setHistoryFor(null)} onApplied={() => void call('reader_get_snapshot', { sessionId: session.sessionId, afterSequence: 0, blockIds: [historyFor.id] }).then((r) => { const d = (r as { display: Parameters<typeof setBlockDisplay>[0][] }).display[0]; if (d) setBlockDisplay(d); })} />
      )}
      {exportOpen && (
        <TranslateExportDialog
          session={session}
          sections={model.sections}
          batch={model.batch}
          onBatch={setBatch}
          currentPage={(() => { if (pdfRef.current) return pdfRef.current.currentPage(); const l = currentAnchor()?.location; return l?.kind === 'pdf' ? l.pageIndex : l?.kind === 'epub' ? l.spineIndex : 0; })()}
          onClose={() => setExportOpen(false)}
        />
      )}
      {pwPrompt && <PasswordDialog reason={pwPrompt.reason} onDone={(v) => { pwPrompt.resolve(v); setPwPrompt(null); }} />}
      {pendingSwitch && (
        <ModalBackdrop role="alertdialog" aria-labelledby="unsaved-title" onClose={() => setPendingSwitch(null)}>
          <div className="modal" style={{ width: 420 }}>
            <header><h2 id="unsaved-title">저장하지 않은 수정이 있습니다</h2></header>
            <div className="body"><p>편집 중인 번역을 저장할까요?</p></div>
            <footer>
              <button className="btn" onClick={() => setPendingSwitch(null)}>계속 편집</button>
              <button className="btn danger" onClick={() => { dirty.current.clear(); clearDrafts(); const p = pendingSwitch; setPendingSwitch(null); p.run(); }}>폐기</button>
              <button className="btn primary" onClick={async () => {
                const flushes = [...dirty.current.values()];
                const results = await Promise.all(flushes.map((f) => f()));
                if (results.every(Boolean)) { dirty.current.clear(); clearDrafts(); const p = pendingSwitch; setPendingSwitch(null); p.run(); }
              }}>저장</button>
            </footer>
          </div>
        </ModalBackdrop>
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
    <ModalBackdrop role="dialog" aria-labelledby="pw-title" onClose={() => onDone(null)}>
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
    </ModalBackdrop>
  );
}
