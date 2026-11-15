// Dev-only WKWebView self-test (never bundled into release: guarded by import.meta.env.DEV).
// Driven by THOTH_SELFTEST=<file>[:<file>...] on the Rust side. Reports metrics only
// (no document text) through the debug_log command.
import { invoke } from '@tauri-apps/api/core';
import { call } from '../ipc/client';
import type { ImportCandidate } from '../ipc/generated/ImportCandidate';
import type { ImportResult } from '../ipc/generated/ImportResult';

type Hooks = { session: () => { sessionId: string; assetToken: string; metadata?: Record<string, unknown> } | null;
  reloadLibrary: () => Promise<void>; open: (id: string) => void; close: () => void; anchor: () => unknown; anchorError: () => number | null; reloadSettings: () => Promise<void> };
declare global {
  interface Window { __thoth?: Partial<Hooks> }
}

const log = (level: string, message: unknown) =>
  invoke('debug_log', { meta: { apiVersion: 1, requestId: crypto.randomUUID() }, level, message: typeof message === 'string' ? message : JSON.stringify(message) }).catch(() => {});

export function installErrorForwarding() {
  window.addEventListener('error', (e) => {
    // Benign browser notice (observers deferring to the next frame); not an app error.
    if (e.message.startsWith('ResizeObserver loop')) return;
    void log('error', `${e.message} @ ${e.filename}:${e.lineno}`);
  });
  window.addEventListener('unhandledrejection', (e) => {
    const r = e.reason as { message?: string; code?: string; stack?: string };
    void log('error', `unhandled: ${r?.code ?? ''} ${r?.message ?? String(e.reason)} | ${String(r?.stack ?? '').slice(0, 600)}`);
  });
  const orig = console.error.bind(console);
  console.error = (...a: unknown[]) => { void log('console', a.map(String).join(' ').slice(0, 800)); orig(...a); };
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function waitFor(pred: () => boolean, ms: number): Promise<number | null> {
  const t0 = performance.now();
  while (performance.now() - t0 < ms) {
    if (pred()) return Math.round(performance.now() - t0);
    await sleep(50);
  }
  return null;
}

function clickButton(text: string): boolean {
  const b = [...document.querySelectorAll('button')].find((x) => x.textContent?.trim() === text);
  if (!b) return false;
  b.click();
  return true;
}

function rows() { return document.querySelectorAll('[data-block-id]').length; }
function canvases() { return [...document.querySelectorAll<HTMLCanvasElement>('.pdf-page canvas')].filter((c) => c.width > 0).length; }
function epubFrameText(): number {
  const f = document.querySelector<HTMLIFrameElement>('.epub-host iframe');
  // A cover page may have no text: count rendered elements instead.
  try { return f?.contentDocument?.body?.childElementCount ?? 0; } catch { return -1; }
}

export async function runSelftest() {
  const cfg = await call<{ candidates: ImportCandidate[]; provider: string | null; model: string | null; slowMs: number | null; scenario: string | null; page?: number | null; only?: string | null }>('debug_selftest_files').catch(() => ({ candidates: [], provider: null, model: null, slowMs: null, scenario: null, page: null, only: null }));
  WAIT = cfg.slowMs ?? 30000;
  (globalThis as { __thothPdfPage?: number }).__thothPdfPage = cfg.page ?? 0;
  const cands = cfg.candidates;
  if (!cands.length) return;
  if (cfg.provider) await setupProvider(cfg.provider, cfg.model);
  await log('info', `selftest start: ${cands.length} files`);
  await sleep(500);
  if (cfg.scenario === 'library') {
    await libraryCheck(cands.length);
    await log('info', 'selftest done');
    return;
  }
  const only = (cfg.only ?? '').split(',').filter(Boolean).map(Number);
  for (const [i, c] of cands.entries()) {
    if (only.length && !only.includes(i + 1)) continue;
    const label = `doc-${i + 1}`;
    const t0 = performance.now();
    let r: ImportResult;
    try {
      // Long runs outlive the 10-minute import token: take a fresh one for this file.
      const fresh = await call<{ candidates: ImportCandidate[] }>('debug_selftest_files').catch(() => null);
      r = await call<ImportResult>('document_import', { importToken: fresh?.candidates[i]?.importToken ?? c.importToken });
    } catch (e) {
      await log('result', { label, stage: 'import', error: String((e as Error).message) });
      continue;
    }
    const importMs = Math.round(performance.now() - t0);
    const t1 = performance.now();
    window.__thoth?.open?.(r.documentId);
    const isPdf = r.format === 'pdf';
    const firstContent = await waitFor(() => rows() > 0 || canvases() > 0 || epubFrameText() > 0 || !!document.querySelector('.reader-error'), 30000);
    const res: Record<string, unknown> = { label, format: r.format, importMs, openToFirstContentMs: firstContent, totalOpenMs: Math.round(performance.now() - t1) };
    await sleep(cfg.scenario === 'security' ? 300 : 1500);
    if (cfg.scenario) {
      res.scenario = cfg.scenario;
      if (cfg.scenario === 'modes20') await modes20(res, isPdf);
      if (cfg.scenario === 'security') await securityScan(res);
      if (cfg.scenario === 'outage') await outage(res);
      if (cfg.scenario === 'style') await styleRun(res);
      if (cfg.scenario === 'chrome') await chromeCheck(res);
      if (cfg.scenario === 'pdflayout') await pdfLayout(res);
      if (cfg.scenario === 'audit') await layoutAudit(res, i);
      if (cfg.scenario === 'quiet') await quietCheck(res);
      if (cfg.scenario === 'structure') await structureCheck(res);
      if (cfg.scenario === 'features') await featuresCheck(res);
      if (cfg.scenario === 'viewer') await viewerCheck(res);
      if (cfg.scenario === 'annotate') await annotateCheck(res, isPdf);
      if (cfg.scenario === 'export') await exportCheck(res, isPdf);
      if (cfg.scenario === 'selection') await selectionCheck(res, isPdf);
      if (cfg.scenario === 'epubview' && !isPdf) await epubViewCheck(res);
      if (cfg.scenario === 'screens') await screensCheck(res, i, cands.length);
      if (cfg.scenario === 'views') await viewsCheck(res);
      if (cfg.scenario === 'pdftx' && isPdf) await pdfTranslationOnly(res);
      if (cfg.scenario === 'providers') await providersCheck(res, cfg.provider ?? '');
      await log('result', res);
      window.__thoth?.close?.();
      await sleep(800);
      continue;
    }
    res.initial = { rows: rows(), canvases: canvases(), anchor: window.__thoth?.anchor?.() };
    if (!isPdf) {
      clickButton('병렬');
      await waitFor(() => !!document.querySelector('.reflow'), 5000);
      await sleep(1200);
      const pairs = [...document.querySelectorAll<HTMLElement>('.brow')].filter((r) => r.querySelector('.trn-mirror'));
      const ok = (im: HTMLImageElement | null) => !!im && im.complete && im.naturalWidth > 0;
      res.mirror = {
        imageRows: pairs.length,
        bothLoaded: pairs.filter((r) => ok(r.querySelector('.src img')) && ok(r.querySelector('.trn-mirror img'))).length,
        sameHeight: pairs.filter((r) => Math.abs((r.querySelector('.src img')?.getBoundingClientRect().height ?? 0) - (r.querySelector('.trn-mirror img')?.getBoundingClientRect().height ?? -9)) <= 1).length,
        excludedBadgesOnImages: pairs.filter((r) => [...r.querySelectorAll('.badge')].some((b) => b.textContent === '번역 제외')).length,
      };
    }
    if (isPdf) {
      clickButton('리플로우');
      res.reflowRowsMs = await waitFor(() => rows() > 0, 15000);
      await sleep(800);
      res.afterReflow = { rows: rows(), anchor: window.__thoth?.anchor?.() };
      clickButton('교차');
      await sleep(800);
      res.afterInterleaved = { rows: rows(), anchor: window.__thoth?.anchor?.() };
      clickButton('원본');
      clickButton('병렬');
      res.backToOriginalMs = await waitFor(() => canvases() > 0, 15000);
      await sleep(1200); // deferred anchor restore runs at 60ms / 400ms
      res.afterOriginal = { canvases: canvases(), anchor: window.__thoth?.anchor?.() };
    } else {
      clickButton('교차');
      await sleep(800);
      res.afterInterleaved = { rows: rows(), anchor: window.__thoth?.anchor?.() };
      clickButton('원문');
      res.originalFrameMs = await waitFor(() => epubFrameText() > 0, 15000);
      res.afterOriginal = { frameTextLen: epubFrameText(), anchor: window.__thoth?.anchor?.() };
      const f = document.querySelector<HTMLIFrameElement>('.epub-host iframe');
      res.epubDebug = {
        iframes: document.querySelectorAll('.epub-host iframe').length,
        hostHtmlLen: document.querySelector('.epub-host')?.innerHTML.length ?? -1,
        srcdocLen: f?.srcdoc?.length ?? -1,
        src: f?.getAttribute('src')?.slice(0, 40) ?? null,
        sandbox: f?.getAttribute('sandbox') ?? null,
        state: (window as unknown as { __thothEpub?: unknown }).__thothEpub ?? null,
        banner: document.querySelector('.epub-original .banner')?.textContent ?? null,
        readyState: (() => { try { return f?.contentDocument?.readyState ?? null; } catch (e) { return `ERR ${String(e)}`; } })(),
        bodyChildren: (() => { try { return f?.contentDocument?.body?.childElementCount ?? null; } catch { return -1; } })(),
        frameH: f?.getBoundingClientRect().height ?? null,
        imgs: (() => { try { const im = [...(f?.contentDocument?.images ?? [])]; return { total: im.length, loaded: im.filter((x) => x.complete && x.naturalWidth > 0).length }; } catch { return null; } })(),
      };
      const tok = (document.querySelector('[data-asset-token]') as HTMLElement | null)?.dataset.assetToken;
      if (tok) {
        res.fetchOpf = await fetch(`thoth-doc://localhost/${tok}/epub/package.opf`).then((x) => `${x.status} ${x.headers.get('content-type')}`).catch((e: unknown) => `ERR ${String((e as Error)?.message ?? e)}`);
      }
      clickButton('병렬');
      await waitFor(() => rows() > 0, 10000);
      await sleep(800);
      res.afterParallel = { rows: rows(), anchor: window.__thoth?.anchor?.() };
    }
    res.dangerBadges = document.querySelectorAll('.badge.danger').length;
    if (cfg.provider && !isPdf) await translationChecks(res, r.documentId);
    await log('result', res);
    window.__thoth?.close?.();
    await sleep(800);
  }
  await log('info', 'selftest done');
}

let WAIT = 30000;

async function setupProvider(endpoint: string, model: string | null) {
  try {
    // "codex": the locally installed Codex (its own sign-in); otherwise an OpenAI-compatible endpoint
    const p = endpoint === 'codex'
      ? await (async () => { const c = await call<{ id: string }>('codex_connect'); await call('provider_save', { providerId: c.id, enabled: true }); return c; })()
      : await call<{ id: string }>('provider_save', { providerId: null, kind: 'openai_compatible', label: 'selftest mock', endpoint, accountNamespace: 'selftest', allowPrivateHttp: false, enabled: true });
    const probe = await call<{ ok: boolean; models: { id: string }[]; capabilities: Record<string, unknown> | null; warnings: string[] }>('provider_probe', { providerId: p.id, modelId: model });
    await log('result', { stage: 'provider', probeOk: probe.ok, models: probe.models.map((m) => m.id), caps: probe.capabilities, warnings: probe.warnings });
    const prof = await call<{ id: string }>('profile_create', { providerId: p.id, modelId: model ?? probe.models[0]?.id ?? 'mock-ko', sourceLanguage: 'en', targetLanguage: 'ko', temperature: 0.2, seed: null, maxOutputTokens: null, contextLimit: 8192, modelDigest: null, promptMode: /rosetta/i.test(model ?? '') ? 'rosetta' : /translategemma|hy-?mt/i.test(model ?? '') ? 'simple' : 'structured' });
    await call('settings_patch', { key: 'activeProfileId', value: prof.id, expectedVersion: null });
    // The settings UI stores these with the profile; PDFs carry no language metadata.
    await call('settings_patch', { key: 'defaultSourceLanguage', value: 'en', expectedVersion: null });
    await call('settings_patch', { key: 'defaultTargetLanguage', value: 'ko', expectedVersion: null });
    await window.__thoth?.reloadSettings?.();
  } catch (e) {
    await log('result', { stage: 'provider', error: String((e as Error).message) });
  }
}

function translated() { return [...document.querySelectorAll('.tcell-text')].filter((x) => (x.textContent ?? '').trim().length > 0).length; }

async function translationChecks(res: Record<string, unknown>, documentId: string) {
  const t0 = performance.now();
  res.firstTranslationMs = await waitFor(() => translated() > 0, WAIT);
  // Scroll into body text: visible blocks must be requested first (P1).
  const scroller = document.querySelector<HTMLElement>('.reflow');
  const before = translated();
  if (scroller) { scroller.scrollTop = 4000; userScroll(scroller); }
  res.afterScrollTranslatedMs = await waitFor(() => translated() >= before + 3, WAIT);
  res.translatedCells = translated();
  res.translateWindowMs = Math.round(performance.now() - t0);
  // Manual edit on the first translated cell: double-click, type, Cmd+Enter.
  res.failedCells = document.querySelectorAll('.tcell .badge.danger').length;
  // Register consistency (counts only): Korean body sentences should all be polite (…니다.)
  // with the default style; plain endings (…다. without 니다) indicate drift.
  const texts = [...document.querySelectorAll<HTMLElement>('.brow:not(.kind-heading) .tcell-text')].map((x) => x.textContent ?? '');
  const sentences = texts.flatMap((t) => t.split(/(?<=[.!?])\s+/)).filter((x) => /[가-힣]/.test(x) && /[.!?]$/.test(x.trim()));
  res.style = {
    cells: texts.length,
    sentences: sentences.length,
    polite: sentences.filter((x) => /(니다|세요|까요|시오)[.!?]$/.test(x.trim())).length,
    plain: sentences.filter((x) => /다[.!?]$/.test(x.trim()) && !/니다[.!?]$/.test(x.trim())).length,
    scriptWarnings: [...document.querySelectorAll('.tcell .badge')].filter((b) => b.textContent === '확인 필요').length,
  };
  const cell = [...document.querySelectorAll<HTMLElement>('.tcell-text')].find((x) => (x.textContent ?? '').trim().length > 0);
  if (cell) {
    cell.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    const ta = await (async () => { await waitFor(() => !!document.querySelector('.tcell.editing textarea'), 3000); return document.querySelector<HTMLTextAreaElement>('.tcell.editing textarea'); })();
    if (ta) {
      const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!;
      setter.call(ta, '수동으로 고친 번역');
      ta.dispatchEvent(new Event('input', { bubbles: true }));
      ta.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', metaKey: true, bubbles: true }));
      res.manualSavedMs = await waitFor(() => [...document.querySelectorAll('.tcell .badge')].some((b) => b.textContent === '수동 수정'), 5000);
    }
  }
  // Close and reopen: cached revisions + manual pin must come back without new requests.
  await sleep(900); // position save debounce (500ms)
  const anchorBefore = window.__thoth?.anchor?.() as { blockId?: string } | null;
  res.anchorBeforeClose = anchorBefore?.blockId?.slice(0, 8) ?? null;
  window.__thoth?.close?.();
  await sleep(600);
  await log('mark', 'reopen-begin');
  const t1 = performance.now();
  window.__thoth?.open?.(documentId);
  res.reopenTranslatedMs = await waitFor(() => translated() > 0, 10000);
  const wantId = anchorBefore?.blockId;
  res.reopenAnchorRestoredMs = wantId ? await waitFor(() => (window.__thoth?.anchor?.() as { blockId?: string } | null)?.blockId === wantId, 10000) : null;
  await sleep(500);
  res.reopenAnchor = ((window.__thoth?.anchor?.() as { blockId?: string } | null)?.blockId ?? '').slice(0, 8);
  res.reopenManualKept = [...document.querySelectorAll('.tcell-text')].some((x) => x.textContent === '수동으로 고친 번역');
  res.reopenWindowMs = Math.round(performance.now() - t1);
  await log('mark', 'reopen-end');
}

/** Simulate real input before the scroll so it counts as the user's. */
function userScroll(el: HTMLElement) {
  el.dispatchEvent(new WheelEvent('wheel', { bubbles: true, deltaY: 100 }));
  el.dispatchEvent(new Event('scroll'));
}

const anchorId = () => (window.__thoth?.anchor?.() as { blockId?: string } | null)?.blockId ?? null;

/** T06 / AC02: 20 mode switches from a mid-document position. The anchor state must not
 *  change, and the anchor point must sit within the spec tolerance of the 25% line
 *  (48 CSS px for EPUB/reflow; inside the block region or 5% of the viewport for PDF original). */
async function modes20(res: Record<string, unknown>, isPdf: boolean) {
  // The reading mode is a persisted setting: start from a known block-list mode.
  if (isPdf) clickButton('리플로우');
  clickButton('병렬');
  await waitFor(() => rows() > 0 && !!document.querySelector('.reflow'), 20000);
  const scroller = document.querySelector<HTMLElement>('.reflow');
  if (scroller) { scroller.scrollTop = 6000; userScroll(scroller); }
  await waitFor(() => anchorId() != null && rows() > 0, 15000);
  await sleep(1500);
  const start = anchorId();
  const seq = isPdf ? ['교차', '병렬', '원본', '리플로우'] : ['교차', '원문', '병렬'];
  const steps: { mode: string; same: boolean; errPx: number | null }[] = [];
  for (let i = 0; i < 20; i++) {
    const m = seq[i % seq.length]!;
    clickButton(m);
    if (m === '원문') await waitFor(() => epubFrameText() > 0, 10000);
    else if (m === '원본') await waitFor(() => canvases() > 0, 10000);
    else await waitFor(() => rows() > 0, 10000);
    await sleep(1100);
    const err = window.__thoth?.anchorError?.() ?? null;
    steps.push({ mode: m, same: anchorId() === start, errPx: err == null ? null : Math.round(err) });
  }
  const tol = (m: string) => (m === '원본' ? window.innerHeight * 0.05 : 48);
  res.modes20 = {
    start: start?.slice(0, 8) ?? null,
    dbg: start ? undefined : { rows: rows(), placeholders: document.querySelectorAll('.placeholder').length, top: scroller?.scrollTop, h: scroller?.scrollHeight },
    anchorChanged: steps.filter((x) => !x.same).length,
    notMeasured: steps.filter((x) => x.errPx == null).map((x) => x.mode),
    overTolerance: steps.filter((x) => x.errPx != null && x.errPx > tol(x.mode)),
    maxErrPx: Math.max(0, ...steps.map((x) => x.errPx ?? 0)),
  };
}

/** AC08 / T08 in the real webview: the rendered sanitized document has nothing executable
 *  and no reference outside the asset origin. */
async function securityScan(res: Record<string, unknown>) {
  const refused = document.querySelector('.reader-error p')?.textContent;
  if (refused) { res.security = { rejectedAtOpen: refused.slice(0, 80) }; clickButton('라이브러리로'); return; }
  clickButton('원문');
  await waitFor(() => epubFrameText() > 0, 10000);
  await sleep(600);
  const f = document.querySelector<HTMLIFrameElement>('.epub-host iframe');
  const d = f?.contentDocument;
  if (!d) { res.security = { error: 'no frame document' }; return; }
  const all = [...d.querySelectorAll('*')];
  const external = (u: string | null) => !!u && /^(https?:|\/\/|data:|file:|javascript:)/i.test(u.trim());
  res.security = {
    sandbox: f?.getAttribute('sandbox'),
    scripts: d.querySelectorAll('script').length,
    activeEmbeds: d.querySelectorAll('iframe,object,embed,form,base[href^="http"],meta[http-equiv="refresh" i]').length,
    eventHandlers: all.filter((e) => [...e.attributes].some((a) => a.name.toLowerCase().startsWith('on'))).length,
    externalLoads: all.filter((e) => ['IMG', 'LINK', 'SOURCE', 'VIDEO', 'AUDIO', 'IMAGE', 'USE'].includes(e.tagName.toUpperCase()) && (external(e.getAttribute('src')) || external(e.getAttribute('href')) || external(e.getAttribute('xlink:href')))).length,
    jsLinks: [...d.querySelectorAll('a[href]')].filter((a) => /^\s*(javascript|data|file|vbscript):/i.test(a.getAttribute('href') ?? '')).length,
    // A script that somehow ran would have set this marker (fixtures try to).
    pwned: Boolean((window as unknown as { __pwned?: unknown }).__pwned) || Boolean((f?.contentWindow as unknown as { __pwned?: unknown } | null)?.__pwned),
    nonAssetRequests: performance.getEntriesByType('resource').map((e) => e.name).filter((n) => !n.startsWith('thoth-doc:') && !n.startsWith(location.origin) && !n.startsWith('blob:') && !n.startsWith('ipc:') && !n.startsWith('http://ipc.localhost')).length,
  };
}

/** AC14: provider unreachable. Source stays readable, cached translations still show,
 *  failures surface as job errors without breaking the reader. */
async function outage(res: Record<string, unknown>) {
  clickButton('병렬');
  await waitFor(() => rows() > 0 && !!document.querySelector('.reflow'), 15000);
  const scroller = document.querySelector<HTMLElement>('.reflow');
  if (scroller) { scroller.scrollTop = 4000; userScroll(scroller); }
  await sleep(6000);
  res.outage = {
    rows: rows(),
    sourceReadable: [...document.querySelectorAll('.brow .src')].filter((x) => (x.textContent ?? '').trim().length > 0).length,
    cachedShown: translated(),
    failedBadges: [...document.querySelectorAll('.tcell .badge')].filter((b) => b.textContent === '실패' || b.textContent === '재시도 대기').length,
    banner: document.querySelector('.banner.warn')?.textContent?.slice(0, 60) ?? null,
  };
}

/** Translate several screens of body text so register consistency can be counted from the DB. */
async function styleRun(res: Record<string, unknown>) {
  clickButton('병렬');
  await waitFor(() => rows() > 0 && !!document.querySelector('.reflow'), 15000);
  const scroller = document.querySelector<HTMLElement>('.reflow');
  let total = 0;
  for (const top of [3000, 3700, 4400, 5100, 5800]) {
    if (!scroller) break;
    scroller.scrollTop = top;
    userScroll(scroller);
    // wait until every visible auto cell has text or failed
    await waitFor(() => {
      const vis = [...document.querySelectorAll<HTMLElement>('.brow')].filter((r) => { const b = r.getBoundingClientRect(); return b.bottom > 80 && b.top < window.innerHeight; });
      return vis.length > 0 && vis.every((r) => !r.querySelector('.tcell') || r.querySelector('.tcell-text') || r.querySelector('.tcell.excluded') || r.querySelector('.badge.danger'));
    }, WAIT);
    total = translated();
  }
  res.styleRun = { translatedVisible: total };
}

/** Toolbar chrome: flags, icon buttons, collapsible sidebar that keeps the reading position. */
async function chromeCheck(res: Record<string, unknown>) {
  clickButton('병렬');
  await waitFor(() => rows() > 0 && !!document.querySelector('.reflow'), 15000);
  const scroller = document.querySelector<HTMLElement>('.reflow');
  if (scroller) { scroller.scrollTop = 5000; userScroll(scroller); }
  await sleep(1500);
  const byLabel = (re: RegExp) => [...document.querySelectorAll<HTMLButtonElement>('button[aria-label]')].find((b) => re.test(b.getAttribute('aria-label') ?? ''));
  const start = anchorId();
  const widths: number[] = [];
  const errs: (number | null)[] = [];
  const sidebarStates: boolean[] = [];
  for (let i = 0; i < 4; i++) {
    byLabel(/^사이드바 (닫기|열기)$/)?.click();
    await sleep(1100);
    sidebarStates.push(!!document.querySelector('#reader-sidebar'));
    widths.push(Math.round(document.querySelector('.content')?.getBoundingClientRect().width ?? 0));
    errs.push(window.__thoth?.anchorError?.() ?? null);
  }
  const pause = byLabel(/^자동 번역 (일시정지|재개)$/);
  const labelBefore = pause?.getAttribute('aria-label');
  pause?.click();
  await sleep(300);
  const labelAfter = byLabel(/^자동 번역 (일시정지|재개)$/)?.getAttribute('aria-label');
  byLabel(/^자동 번역 재개$/)?.click();
  res.chrome = {
    flags: [...document.querySelectorAll('.lang-pair .flag')].map((f) => f.textContent),
    langLabel: document.querySelector('.lang-pair')?.getAttribute('aria-label'),
    iconButtons: ['사이드바', '자동 번역', '번역 설정'].map((k) => { const b = byLabel(new RegExp(k)); return !!b && !!b.querySelector('svg') && (b.textContent ?? '').trim() === ''; }),
    sidebarStates,
    contentWidths: widths,
    anchorKept: anchorId() === start,
    maxAnchorErrPx: Math.max(0, ...errs.map((e) => Math.round(e ?? 0))),
    pauseToggle: [labelBefore, labelAfter],
  };
}

/** Layout-preserving PDF side-by-side: translated pages line up with originals, show the page
 *  raster (figures/table rules), and carry fitted translations in block positions. */
async function pdfLayout(res: Record<string, unknown>) {
  clickButton('원본');
  clickButton('병렬');
  await waitFor(() => canvases() > 0, 20000);
  const pane = document.querySelector<HTMLElement>('.pdf-pages');
  const page = Number((globalThis as { __thothPdfPage?: number }).__thothPdfPage ?? 0);
  const goto = async (i: number) => {
    const n = document.querySelector<HTMLElement>(`.pdf-page[data-page="${i}"]`);
    if (!pane || !n) return;
    pane.scrollTop += n.getBoundingClientRect().top - pane.getBoundingClientRect().top - 8;
    userScroll(pane);
    await sleep(600);
  };
  await goto(page);
  // wait for translations (or slots) on the visible page to settle
  await waitFor(() => document.querySelectorAll('.tp-text').length >= 3, WAIT);
  await waitFor(() => document.querySelectorAll('.tp-slot.busy').length === 0, WAIT);
  await sleep(800);
  const rowsEl = [...document.querySelectorAll<HTMLElement>('.pdf-row')].filter((r) => r.querySelector('.pdf-page.translated canvas'));
  const pairs = rowsEl.map((r) => {
    const o = r.querySelector<HTMLElement>('.pdf-page:not(.translated)')!.getBoundingClientRect();
    const t = r.querySelector<HTMLElement>('.pdf-page.translated')!.getBoundingClientRect();
    const c = r.querySelector<HTMLCanvasElement>('.pdf-page.translated canvas')!;
    return { dy: Math.round(Math.abs(o.top - t.top)), sameSize: Math.abs(o.width - t.width) < 1 && Math.abs(o.height - t.height) < 1, painted: c.width > 0 };
  });
  const texts = [...document.querySelectorAll<HTMLElement>('.tp-text')];
  const sizes = texts.map((t) => parseFloat(getComputedStyle(t).fontSize));
  res.perBlock = [...document.querySelectorAll<HTMLElement>('.pdf-row')].filter((r) => r.querySelector(`.pdf-page[data-page="${page}"]`)).flatMap((r) => [...r.querySelectorAll<HTMLElement>('.tp-text')]).map((t) => ({ k: t.dataset.kind, base: t.dataset.base, font: parseFloat(getComputedStyle(t).fontSize).toFixed(1), cover: (t.dataset.cover ?? '').split(',').slice(2).map((x) => Math.round(Number(x))).join('x'), max: t.dataset.max, client: `${t.clientWidth}x${t.clientHeight}`, scroll: `${t.scrollWidth}x${t.scrollHeight}`, fb: t.classList.contains('fallback'), chars: (t.textContent ?? '').length }));
  {
    const onPage = [...document.querySelectorAll<HTMLElement>('.pdf-row')].filter((r) => r.querySelector(`.pdf-page[data-page="${page}"]`)).flatMap((r) => [...r.querySelectorAll<HTMLElement>('.tp-text')]);
    res.bold = {
      overlays: onPage.length,
      withStrong: onPage.filter((t) => t.querySelector('strong')).length,
      leadingStrong: onPage.filter((t) => t.firstElementChild?.querySelector('strong') && (t.textContent ?? '').startsWith(t.querySelector('strong')?.textContent ?? '\u0000')).length,
      wholeStrong: onPage.filter((t) => (t.querySelector('strong')?.textContent ?? '') === (t.textContent ?? ' ')).length,
      strongLens: onPage.filter((t) => t.querySelector('strong')).map((t) => `${(t.querySelector('strong')?.textContent ?? '').length}/${(t.textContent ?? '').length}`),
      rawTagsLeft: onPage.filter((t) => /<\/?[bi]>/i.test(t.textContent ?? '')).length,
    };
  }
  res.pdfLayout = {
    paneWidth: pane?.clientWidth,
    liveRows: pairs.length,
    aligned: pairs.every((p) => p.dy === 0 && p.sameSize),
    rasterCopied: pairs.filter((p) => p.painted).length,
    overlays: texts.length,
    overflowing: texts.filter((t) => t.scrollHeight > t.clientHeight + 2 || t.scrollWidth > t.clientWidth + 2).length,
    fontPx: sizes.length ? { min: Math.min(...sizes), median: sizes.slice().sort((a, b) => a - b)[Math.floor(sizes.length / 2)], max: Math.max(...sizes) } : null,
    // distinct font sizes on the anchor page (body text should share one size)
    fontBuckets: Object.fromEntries([...new Set(sizes.map((x) => x.toFixed(1)))].map((k) => [k, sizes.filter((x) => x.toFixed(1) === k).length])),
    lineHeights: [...new Set(texts.map((t) => (parseFloat(getComputedStyle(t).lineHeight) / parseFloat(getComputedStyle(t).fontSize)).toFixed(2)))],
    shrunk: texts.filter((t) => Number(t.dataset.base ?? 0) - parseFloat(getComputedStyle(t).fontSize) > 0.3).length,
    slotsLeft: document.querySelectorAll('.tp-slot').length,
    failed: document.querySelectorAll('.tp-slot.failed').length,
    horizontalScroll: pane ? pane.scrollWidth > pane.clientWidth + 1 : null,
  };
  // Overlays must fully hide the original underneath, also when a translation is stale.
  const opaque = (els: HTMLElement[]) => els.every((t) => { const cs = getComputedStyle(t); const m = /rgba?\(([^)]+)\)/.exec(cs.backgroundColor); const a = m ? m[1]!.split(',').map((x) => x.trim())[3] : '0'; return cs.opacity === '1' && (a === undefined || Number(a) === 1); });
  const freshOpaque = opaque(texts);
  await call('settings_patch', { key: 'translationStyle', value: 'plain', expectedVersion: null }).catch(() => {});
  await window.__thoth?.reloadSettings?.();
  await waitFor(() => document.querySelectorAll('.tp-text.stale').length > 0, 8000);
  const stale = [...document.querySelectorAll<HTMLElement>('.tp-text.stale')];
  res.opacity = { freshOpaque, staleCount: stale.length, staleOpaque: opaque(stale), buttonSelectable: getComputedStyle(document.querySelector('.seg button')!).webkitUserSelect !== 'none' };
  // popover: click a translated block
  texts[0]?.click();
  await sleep(300);
  res.popover = { open: !!document.querySelector('.pdf-pop'), hasMenu: !!document.querySelector('.pdf-pop .tcell-menu button') };
}

/** Deterministic PRNG so audits are reproducible. */
function mulberry32(a: number) {
  return () => { a |= 0; a = (a + 0x6d2b79f5) | 0; let t = Math.imul(a ^ (a >>> 15), 1 | a); t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
}

/**
 * Layout audit of the translated page on random pages (numbers only, no document text).
 * Per page it reports blocks whose translation overlaps another block, is clipped, had to
 * shrink a lot, leaves its page, or could not be placed.
 */
async function layoutAudit(res: Record<string, unknown>, docIndex: number) {
  clickButton('원본');
  clickButton('병렬');
  await waitFor(() => canvases() > 0, 20000);
  const pane = document.querySelector<HTMLElement>('.pdf-pages');
  const total = document.querySelectorAll('.pdf-page[data-page]').length;
  const perDoc = Number((globalThis as { __thothPdfPage?: number }).__thothPdfPage || 10);
  const rnd = mulberry32(1000 + docIndex);
  const picks = new Set<number>();
  while (picks.size < Math.min(perDoc, Math.max(1, total - 6))) picks.add(3 + Math.floor(rnd() * Math.max(1, total - 6)));
  const pages: Record<string, unknown>[] = [];
  for (const pi of [...picks].sort((a, b) => a - b)) {
    const orig = document.querySelector<HTMLElement>(`.pdf-page[data-page="${pi}"]`);
    if (!pane || !orig) continue;
    pane.scrollTop += orig.getBoundingClientRect().top - pane.getBoundingClientRect().top - 8;
    userScroll(pane);
    const row = orig.parentElement!;
    const tp = () => row.querySelector<HTMLElement>('.pdf-page.translated');
    // settle: raster copied, blocks present (or page has none), nothing waiting or streaming
    await waitFor(() => !!tp()?.querySelector('canvas') && tp()!.querySelector<HTMLCanvasElement>('canvas')!.width > 0, 20000);
    const t0 = performance.now();
    let last = -1; let stable = 0;
    while (performance.now() - t0 < 45000) {
      await sleep(400);
      const n = tp()?.querySelectorAll('.tp-text').length ?? 0;
      const busy = tp()?.querySelectorAll('.tp-slot.busy').length ?? 0;
      if (n === last && busy === 0 && (n > 0 || performance.now() - t0 > 6000)) { stable += 1; if (stable >= 3) break; } else stable = 0;
      last = n;
    }
    const page = tp();
    if (!page) continue;
    const pr = page.getBoundingClientRect();
    const fallbacks = page.querySelectorAll('.tp-text.fallback').length;
    const texts = [...page.querySelectorAll<HTMLElement>('.tp-text:not(.fallback)')];
    const rects = texts.map((t) => t.getBoundingClientRect());
    const overlaps: number[] = [];
    const detail: unknown[] = [];
    const cov = (t: HTMLElement) => (t.dataset.cover ?? '0,0,0,0').split(',').map(Number) as [number, number, number, number];
    let coverOverlaps = 0;
    let nestedPairs = 0;
    for (let a = 0; a < rects.length; a++) for (let b = a + 1; b < rects.length; b++) {
      const w = Math.min(rects[a]!.right, rects[b]!.right) - Math.max(rects[a]!.left, rects[b]!.left);
      const h = Math.min(rects[a]!.bottom, rects[b]!.bottom) - Math.max(rects[a]!.top, rects[b]!.top);
      // A small block fully inside a larger one (equation number, label) is drawn on top.
      const areaA = rects[a]!.width * rects[a]!.height, areaB = rects[b]!.width * rects[b]!.height;
      const nested = w > 2 && h > 2 && w * h >= Math.min(areaA, areaB) * 0.85 && Math.min(areaA, areaB) < Math.max(areaA, areaB) * 0.25;
      if (nested) { nestedPairs += 1; continue; }
      if (w > 2 && h > 2) {
        overlaps.push(Math.round(w * h));
        const ca = cov(texts[a]!), cb = cov(texts[b]!);
        const cw = Math.min(ca[0] + ca[2], cb[0] + cb[2]) - Math.max(ca[0], cb[0]);
        const ch = Math.min(ca[1] + ca[3], cb[1] + cb[3]) - Math.max(ca[1], cb[1]);
        const src = cw > 2 && ch > 2; // the original block regions already overlap
        if (src) coverOverlaps += 1;
        const g = (i: number, c: number[]) => ({ k: texts[i]!.dataset.kind, cover: c.map(Math.round), box: [Math.round(rects[i]!.left - pr.left), Math.round(rects[i]!.top - pr.top), Math.round(rects[i]!.width), Math.round(rects[i]!.height)] });
        if (detail.length < 4) detail.push({ src, w: Math.round(w), h: Math.round(h), a: g(a, ca), b: g(b, cb) });
      }
    }
    const clipDetail = texts.filter((t) => t.scrollHeight > t.clientHeight + 2 || t.scrollWidth > t.clientWidth + 2).slice(0, 3).map((t) => ({ k: t.dataset.kind, sw: t.scrollWidth, cw: t.clientWidth, sh: t.scrollHeight, ch: t.clientHeight, base: t.dataset.base, font: getComputedStyle(t).fontSize, cover: cov(t).map(Math.round) }));
    const fontOf = (t: HTMLElement) => parseFloat(getComputedStyle(t).fontSize);
    const ratio = texts.map((t) => fontOf(t) / Math.max(1, Number(t.dataset.base ?? fontOf(t))));
    pages.push({
      page: pi,
      overlays: texts.length,
      slots: page.querySelectorAll('.tp-slot').length,
      failed: page.querySelectorAll('.tp-slot.failed').length,
      overlapPairs: overlaps.length,
      nestedPairs,
      fallbacks,
      coverOverlaps,
      detail,
      clipDetail,
      autoSlots: page.querySelectorAll('.tp-slot[data-policy="auto"]').length,
      overlapArea: overlaps.reduce((x, y) => x + y, 0),
      clipped: texts.filter((t) => t.scrollHeight > t.clientHeight + 2 || t.scrollWidth > t.clientWidth + 2).length,
      shrunk: ratio.filter((r) => r < 0.9).length,
      tiny: ratio.filter((r) => r < 0.6).length,
      minRatio: ratio.length ? Number(Math.min(...ratio).toFixed(2)) : null,
      offPage: rects.filter((r) => r.left < pr.left - 1 || r.right > pr.right + 1 || r.top < pr.top - 1 || r.bottom > pr.bottom + 1).length,
      fontKinds: new Set(texts.map((t) => fontOf(t).toFixed(1))).size,
      pageW: Math.round(pr.width),
    });
  }
  res.audit = { totalPages: total, pages };
}

/** Reading flow must look like a plain document: no state badges or state colours. */
async function quietCheck(res: Record<string, unknown>) {
  clickButton('병렬');
  await waitFor(() => rows() > 0 && !!document.querySelector('.reflow'), 15000);
  const scroller = document.querySelector<HTMLElement>('.reflow');
  if (scroller) { scroller.scrollTop = 3000; userScroll(scroller); }
  await waitFor(() => translated() >= 3, WAIT);
  const visibleBadges = () => [...document.querySelectorAll<HTMLElement>('.reflow .tcell .badge, .reflow .tcell .cancel')].filter((b) => getComputedStyle(b).opacity !== '0').map((b) => b.textContent);
  const during = visibleBadges();
  await waitFor(() => document.querySelectorAll('.reflow .tcell[aria-busy="true"]').length === 0, WAIT);
  // make everything stale, then look again
  await call('settings_patch', { key: 'translationStyle', value: 'plain', expectedVersion: null }).catch(() => {});
  await window.__thoth?.reloadSettings?.();
  await waitFor(() => document.querySelectorAll('.reflow .tcell.stale').length > 0, 8000);
  const cells = [...document.querySelectorAll<HTMLElement>('.reflow .tcell-text')];
  const src = document.querySelector<HTMLElement>('.reflow .brow.kind-paragraph .src');
  res.quiet = {
    badgesWhileTranslating: during,
    staleCells: document.querySelectorAll('.reflow .tcell.stale').length,
    badgesWhenStale: visibleBadges(),
    textColours: [...new Set(cells.map((c) => getComputedStyle(c).color))],
    sourceColour: src ? getComputedStyle(src).color : null,
  };
}

/** EPUB side-by-side: the translation keeps the source's structure (heading size/weight per
 *  level, bold terms). Numbers only. */
async function structureCheck(res: Record<string, unknown>) {
  clickButton('병렬');
  await waitFor(() => rows() > 0 && !!document.querySelector('.reflow'), 15000);
  const scroller = document.querySelector<HTMLElement>('.reflow');
  const seen = new Map<string, { kind: string; lvl: string; srcPx: number; trPx: number; srcW: string; trW: string; srcStrong: number; trStrong: number; rawTags: boolean }>();
  for (const top of [2500, 3300, 4100, 4900]) {
    if (!scroller) break;
    scroller.scrollTop = top;
    userScroll(scroller);
    await waitFor(() => {
      const vis = [...document.querySelectorAll<HTMLElement>('.brow')].filter((r) => { const b = r.getBoundingClientRect(); return b.bottom > 80 && b.top < window.innerHeight; });
      return vis.length > 0 && vis.every((r) => !r.querySelector('.tcell') || r.querySelector('.tcell-text') || r.querySelector('.tcell.excluded') || r.querySelector('.badge.danger'));
    }, WAIT);
    for (const r of document.querySelectorAll<HTMLElement>('.brow')) {
      const src = r.querySelector<HTMLElement>('.src');
      const tr = r.querySelector<HTMLElement>('.tcell-text');
      if (!src || !tr || !r.dataset.blockId) continue;
      const cs = getComputedStyle(src), ct = getComputedStyle(tr);
      seen.set(r.dataset.blockId, {
        kind: [...r.classList].find((c) => c.startsWith('kind-'))?.slice(5) ?? '',
        lvl: [...r.classList].find((c) => c.startsWith('lvl-'))?.slice(4) ?? '',
        srcPx: parseFloat(cs.fontSize), trPx: parseFloat(ct.fontSize), srcW: cs.fontWeight, trW: ct.fontWeight,
        srcStrong: src.querySelectorAll('strong').length, trStrong: tr.querySelectorAll('strong').length,
        rawTags: /<\/?[bi]>/i.test(tr.textContent ?? ''),
      });
    }
  }
  const all = [...seen.values()];
  const headings = all.filter((x) => x.kind === 'heading');
  const withBold = all.filter((x) => x.srcStrong > 0);
  res.structure = {
    rows: all.length,
    sizeMismatch: all.filter((x) => Math.abs(x.srcPx - x.trPx) > 0.5).length,
    weightMismatch: all.filter((x) => x.srcW !== x.trW).length,
    headings: headings.length,
    headingLevels: Object.fromEntries([...new Set(headings.map((h) => h.lvl || '-'))].map((l) => [l, headings.filter((h) => (h.lvl || '-') === l).length])),
    headingSizes: [...new Set(headings.map((h) => `${h.lvl || '-'}:${h.srcPx}/${h.trPx}`))],
    srcRowsWithBold: withBold.length,
    boldKept: withBold.filter((x) => x.trStrong > 0).length,
    rawTagsLeft: all.filter((x) => x.rawTags).length,
  };
}

/** Library shelf: covers, metadata and both sort orders. Statistics only — no book text. */
async function libraryCheck(n: number) {
  const ids: string[] = [];
  const formats: string[] = [];
  for (let i = 0; i < n; i++) {
    try {
      const fresh = await call<{ candidates: ImportCandidate[] }>('debug_selftest_files');
      const r = await call<ImportResult>('document_import', { importToken: fresh.candidates[i]?.importToken ?? '' });
      if (!ids.includes(r.documentId)) { ids.push(r.documentId); formats.push(r.format); }
    } catch (e) {
      await log('result', { label: `doc-${i + 1}`, stage: 'import', error: String((e as Error).message) });
    }
  }
  const t0 = performance.now();
  await window.__thoth?.reloadLibrary?.();
  const books = () => [...document.querySelectorAll<HTMLElement>('.shelf .book')];
  const settled = () => books().length === ids.length && books().every((b) => {
    const img = b.querySelector<HTMLImageElement>('.cover-img');
    return img ? img.complete && img.naturalWidth > 0 : false;
  });
  // PDF covers render one at a time; give them a while, then report whatever is there.
  const settledMs = await waitFor(settled, 240000);
  const stat = (fmt: string) => {
    const bs = books().filter((b) => b.dataset.format === fmt);
    return {
      books: bs.length,
      imageCovers: bs.filter((b) => { const i = b.querySelector<HTMLImageElement>('.cover-img'); return !!i && i.naturalWidth > 0; }).length,
      genericCovers: bs.filter((b) => b.querySelector('.cover-generic')).length,
      withAuthor: bs.filter((b) => b.querySelector('.book-author')).length,
      withImprint: bs.filter((b) => b.querySelector('.book-imprint')).length,
    };
  };
  const covers = books().map((b) => b.querySelector<HTMLElement>('.cover')!.getBoundingClientRect());
  const titles = () => books().map((b) => b.querySelector('.book-title')?.textContent ?? '');
  const res: Record<string, unknown> = {
    label: 'library', imported: ids.length, epubImported: formats.filter((f) => f === 'epub').length, settledMs, waitedMs: Math.round(performance.now() - t0),
    epub: stat('epub'), pdf: stat('pdf'),
    coverRatioOk: covers.filter((r) => Math.abs(r.width / r.height - 2 / 3) < 0.01).length,
    columns: new Set(covers.map((r) => Math.round(r.left))).size,
    horizontalOverflow: document.documentElement.scrollWidth > window.innerWidth + 1,
    brokenImages: books().filter((b) => { const i = b.querySelector<HTMLImageElement>('.cover-img'); return !!i && i.complete && i.naturalWidth === 0; }).length,
  };
  // Title order
  clickButton('제목 순');
  await sleep(300);
  const c = new Intl.Collator(['ko', 'en'], { numeric: true, sensitivity: 'base' });
  const t = titles();
  res.titleSorted = t.every((x, i) => i === 0 || c.compare(t[i - 1] ?? '', x) <= 0);
  res.titleSortPersisted = localStorage.getItem('thoth.librarySort') === 'title';
  // Recent order: read the last listed book, come back, it must lead the shelf.
  const lastTitle = t[t.length - 1];
  const target = books()[books().length - 1]?.querySelector<HTMLButtonElement>('.book-open');
  target?.click();
  await waitFor(() => rows() > 0 || canvases() > 0 || epubFrameText() > 0 || !!document.querySelector('.reader-error'), 30000);
  await sleep(1000);
  window.__thoth?.close?.();
  await waitFor(() => books().length === ids.length, 15000);
  clickButton('최근 읽은 순');
  await sleep(400);
  res.recentFirstIsLastRead = titles()[0] === lastTitle;
  res.readLabels = books().filter((b) => /읽음$/.test(b.querySelector('.book-read')?.textContent ?? '')).length;
  res.newFlags = document.querySelectorAll('.cover-flag.new').length;
  await log('result', res);
}

/** PDF outline TOC, coloured runs and page-break pairs on a real book. Counts only. */
async function featuresCheck(res: Record<string, unknown>) {
  const sid = () => window.__thoth?.session?.()?.sessionId;
  type Sec = { id: string; ordinal: number; state: string };
  const sections = async () => (sid() ? await call<Sec[]>('sections_list', { sessionId: sid() }).catch(() => [] as Sec[]) : []);
  // let the extractor publish a good part of the book
  const want = 80;
  for (let i = 0; i < 120; i++) {
    const s = await sections();
    if (s.length && s.filter((x) => x.state === 'ready' || x.state === 'failed').length >= Math.min(want, s.length)) break;
    await sleep(1000);
  }
  const secs = (await sections()).filter((x) => x.state === 'ready').sort((a, b) => Number(a.ordinal) - Number(b.ordinal)).slice(0, want);
  let blocks = 0; let coloured = 0; let colourNodes = 0; let continues = 0;
  const colours = new Set<string>();
  let colourPage = -1; let continuesPage = -1;
  type Node = { t: string; v?: string; c?: Node[] };
  const walk = (ns: Node[]): number => ns.reduce((n, x) => n + (x.t === 'color' ? (colours.add(String(x.v)), 1) : 0) + (x.c ? walk(x.c) : 0), 0);
  for (const s of secs) {
    const page = await call<{ blocks: { content: Node[]; roles: string[] }[] }>('blocks_get', { sessionId: sid(), sectionId: s.id, cursor: null, limit: 1000 }).catch(() => ({ blocks: [] }));
    for (const b of page.blocks) {
      blocks++;
      const n = walk(b.content ?? []);
      if (n) { coloured++; colourNodes += n; if (colourPage < 0) colourPage = Number(s.ordinal); }
      if (b.roles?.includes('continues_on_next_page')) { continues++; if (continuesPage < 0 && Number(s.ordinal) > 2) continuesPage = Number(s.ordinal); }
    }
  }
  const toc = (window.__thoth?.session?.()?.metadata?.toc as unknown[] | undefined) ?? [];
  res.features = {
    readySections: secs.length, blocks, colouredBlocks: coloured, colourNodes, colours: colours.size, continuesBlocks: continues,
    tocTopLevel: toc.length, tocDom: document.querySelectorAll('.toc .toc-item').length, tocPageLabels: document.querySelectorAll('.toc .toc-page').length,
  };
  // side-by-side on the first page with coloured text, then on a page-break pair
  clickButton('병렬');
  await waitFor(() => canvases() > 0, 20000);
  const pane = document.querySelector<HTMLElement>('.pdf-pages');
  const visit = async (i: number) => {
    const n = document.querySelector<HTMLElement>(`.pdf-page[data-page="${i}"]`);
    if (!pane || !n) return null;
    pane.scrollTop += n.getBoundingClientRect().top - pane.getBoundingClientRect().top - 8;
    userScroll(pane);
    await waitFor(() => document.querySelectorAll('.tp-text').length > 0, WAIT);
    await sleep(2500);
    const texts = [...document.querySelectorAll<HTMLElement>('.tp-text')].map((x) => x.textContent ?? '');
    return {
      overlays: texts.length,
      inkSpans: document.querySelectorAll('.tp-text .ink').length,
      rawTags: texts.filter((x) => /<\/?[bic]>|<pb/i.test(x)).length,
      failed: document.querySelectorAll('.tcell .badge.danger, .tp-text.failed').length,
    };
  };
  if (colourPage >= 0) (res.features as Record<string, unknown>).colourPage = await visit(colourPage);
  if (colourPage >= 0) (res.features as Record<string, unknown>).colourTruth = await colourTruth(Math.max(colourPage, Number((globalThis as { __thothPdfPage?: number }).__thothPdfPage ?? 0)), 24).catch((e) => ({ error: String((e as Error).message) }));
  if (continuesPage >= 0) (res.features as Record<string, unknown>).continuesPage = await visit(continuesPage);
}

/**
 * Ground truth for extracted item colours: render the page and look at the ink actually
 * under each text item. Compares "chromatic vs neutral" and, for chromatic ink, the hue.
 */
async function colourTruth(firstPage: number, pages: number) {
  const token = window.__thoth?.session?.()?.assetToken;
  if (!token) return { error: 'no session' };
  const { loadPdfDocument } = await import('../document/pdf/pdfjs');
  const { buildPageInput } = await import('../document/pdf/pageInput');
  const { doc, task } = await loadPdfDocument({ url: `thoth-doc://localhost/${token}/pdf`, onPassword: async () => null });
  const chroma = (r: number, g: number, b: number) => Math.max(r, g, b) - Math.min(r, g, b);
  const out = { pages: 0, pagesWithColours: 0, mixedItems: 0, items: 0, chromaticAssigned: 0, chromaticInk: 0, agree: 0, assignedButNeutralInk: 0, inkButNeutralAssigned: 0, hueMismatch: 0, unknown: 0 };
  try {
    for (let p = firstPage; p < Math.min(doc.numPages, firstPage + pages); p++) {
      const page = await doc.getPage(p + 1);
      const input = await buildPageInput(page);
      out.pages++;
      if (input.items.some((i) => i.color)) out.pagesWithColours++;
      const SCALE = 2;
      const vp = page.getViewport({ scale: SCALE });
      const canvas = document.createElement('canvas');
      canvas.width = Math.floor(vp.width); canvas.height = Math.floor(vp.height);
      const ctx = canvas.getContext('2d', { willReadFrequently: true })!;
      ctx.fillStyle = '#fff'; ctx.fillRect(0, 0, canvas.width, canvas.height);
      await page.render({ canvasContext: ctx, viewport: vp, canvas } as Parameters<typeof page.render>[0]).promise;
      for (const it of input.items) {
        if (!it.str.trim() || it.str.trim().length < 3) continue;
        const [a, b, , d, e, f] = it.transform as [number, number, number, number, number, number];
        if (Math.abs(b) > 0.01 || a <= 0 || it.width <= 0) continue; // horizontal text only
        const h = Math.abs(d) || it.height;
        if (!it.colorRuns) { out.items++; out.unknown++; continue; }
        if (it.colorRuns.length > 1) out.mixedItems++;
        for (const run of it.colorRuns) {
          const piece = it.str.slice(run.start, run.end);
          if (piece.replace(/\s/g, '').length < 4) continue;
          // the run's share of the item box (proportional widths are close enough once trimmed)
          const fa = run.start / it.str.length;
          const fb = run.end / it.str.length;
          const pad = (fb - fa) * 0.18;
          const [x0, y0] = vp.convertToViewportPoint(e + it.width * (fa + pad), f + h * 0.75);
          const [x1, y1] = vp.convertToViewportPoint(e + it.width * (fb - pad), f + h * 0.05);
          const x = Math.max(0, Math.floor(Math.min(x0, x1))); const y = Math.max(0, Math.floor(Math.min(y0, y1)));
          const w = Math.min(canvas.width - x, Math.ceil(Math.abs(x1 - x0))); const hh = Math.min(canvas.height - y, Math.ceil(Math.abs(y1 - y0)));
          if (w < 4 || hh < 4) continue;
          const px = ctx.getImageData(x, y, w, hh).data;
          // ink = the darkest fifth of non-white pixels (anti-aliased edges wash colours out)
          const ink: [number, number, number, number][] = [];
          for (let q = 0; q < px.length; q += 4) {
            const lum = 0.3 * px[q]! + 0.59 * px[q + 1]! + 0.11 * px[q + 2]!;
            if (lum < 200) ink.push([lum, px[q]!, px[q + 1]!, px[q + 2]!]);
          }
          if (ink.length < 12) continue;
          ink.sort((m, n) => m[0] - n[0]);
          const top = ink.slice(0, Math.max(6, Math.floor(ink.length / 5)));
          const avg = [1, 2, 3].map((c) => top.reduce((sum, v) => sum + v[c as 1 | 2 | 3], 0) / top.length) as [number, number, number];
          out.items++;
          const ar = parseInt(run.color.slice(1, 3), 16); const ag = parseInt(run.color.slice(3, 5), 16); const ab = parseInt(run.color.slice(5, 7), 16);
          const assignedChromatic = chroma(ar, ag, ab) > 70;
          const inkChromatic = chroma(...avg) > 70;
          if (assignedChromatic) out.chromaticAssigned++;
          if (inkChromatic) out.chromaticInk++;
          if (assignedChromatic && !inkChromatic) out.assignedButNeutralInk++;
          else if (!assignedChromatic && inkChromatic) out.inkButNeutralAssigned++;
          else if (assignedChromatic && inkChromatic && Math.hypot(ar - avg[0], ag - avg[1], ab - avg[2]) > 110) out.hueMismatch++;
          else out.agree++;
        }
      }
      canvas.width = 0; canvas.height = 0;
      page.cleanup();
    }
  } finally {
    await task.destroy().catch(() => {});
  }
  return out;
}

/** PDF viewer basics: zoom, page indicator / go to page, links and back. */
async function viewerCheck(res: Record<string, unknown>) {
  const out: Record<string, unknown> = {};
  res.viewer = out;
  clickButton('원본');
  clickButton('원문');
  await waitFor(() => canvases() > 0, 20000);
  const pane = document.querySelector<HTMLElement>('.pdf-pages')!;
  const pageW = () => document.querySelector<HTMLElement>('.pdf-page[data-page]')?.getBoundingClientRect().width ?? 0;
  const pageNo = () => Number(document.querySelector<HTMLInputElement>('.pdf-pageno input')?.value ?? -1);
  const btn = (label: string) => document.querySelector<HTMLButtonElement>(`.pdf-controls button[aria-label="${label}"]`);
  const total = document.querySelectorAll('.pdf-page[data-page]').length;
  out.controls = !!document.querySelector('.pdf-controls');
  // zoom
  const w0 = pageW();
  btn('확대')?.click(); await sleep(500);
  btn('확대')?.click(); await sleep(700);
  const w1 = pageW();
  out.zoomIn = { ratio: Math.round((w1 / w0) * 100) / 100, hScroll: pane.scrollWidth > pane.clientWidth + 1, label: document.querySelector('.pdf-zoom')?.textContent };
  await waitFor(() => canvases() > 0, 8000);
  const c = [...document.querySelectorAll<HTMLCanvasElement>('.pdf-page canvas')].filter((x) => x.width > 0)[0];
  out.canvasPixels = c ? c.width * c.height : 0;
  // far enough in that the page is wider than the pane: it must scroll sideways
  for (let i = 0; i < 4; i++) { btn('확대')?.click(); await sleep(350); }
  await sleep(600);
  const left0 = pane.scrollLeft;
  pane.scrollLeft = 400;
  out.wide = { label: document.querySelector('.pdf-zoom')?.textContent, pageWider: pageW() > pane.clientWidth, scrollable: pane.scrollWidth > pane.clientWidth + 1, scrolled: pane.scrollLeft > left0, paneWithinWindow: pane.getBoundingClientRect().right <= window.innerWidth + 1 };
  {
    const chain: string[] = [];
    for (let e: HTMLElement | null = pane; e && e !== document.body; e = e.parentElement) chain.push(`${e.className.split(' ')[0] || e.tagName}:${Math.round(e.getBoundingClientRect().width)}/${getComputedStyle(e).minWidth}/${getComputedStyle(e).display}`);
    out.chain = { win: window.innerWidth, chain, rowW: Math.round(document.querySelector('.pdf-row')!.getBoundingClientRect().width), scrollW: pane.scrollWidth, clientW: pane.clientWidth, visible: document.visibilityState };
  }
  const ctlBtn = (t: string) => [...document.querySelectorAll<HTMLButtonElement>('.pdf-controls button')].find((b) => b.textContent === t);
  ctlBtn('쪽 맞춤')?.click(); await sleep(800);
  const row = document.querySelector<HTMLElement>('.pdf-page[data-page]')!.getBoundingClientRect();
  out.fitPage = { pageFitsHeight: row.height <= pane.clientHeight + 1, label: document.querySelector('.pdf-zoom')?.textContent, pressed: ctlBtn('쪽 맞춤')?.getAttribute('aria-pressed') };
  ctlBtn('폭 맞춤')?.click(); await sleep(700);
  out.zoomReset = Math.abs(pageW() - w0) < 2;
  // the bar only shows with the pointer near it
  const bar = document.querySelector<HTMLElement>('.pdf-controls')!;
  const split = document.querySelector<HTMLElement>('.pdf-split')!;
  const sr = split.getBoundingClientRect();
  split.dispatchEvent(new MouseEvent('mousemove', { bubbles: true, clientX: sr.left + 200, clientY: sr.top + 80 }));
  await sleep(350);
  const hidden = getComputedStyle(bar).opacity;
  split.dispatchEvent(new MouseEvent('mousemove', { bubbles: true, clientX: sr.left + 200, clientY: sr.bottom - 30 }));
  await sleep(350);
  out.bar = { awayOpacity: hidden, nearOpacity: getComputedStyle(bar).opacity, cls: bar.className };
  // keys: space, shift+space, arrows
  const key = (k: string, shift = false) => window.dispatchEvent(new KeyboardEvent('keydown', { key: k, shiftKey: shift, bubbles: true, cancelable: true }));
  (document.activeElement as HTMLElement | null)?.blur?.();
  const k0 = pageNo();
  key(' '); await sleep(700);
  const k1 = pageNo();
  key('ArrowRight'); await sleep(700);
  const k2 = pageNo();
  key(' ', true); await sleep(700);
  key('ArrowLeft'); await sleep(700);
  out.keys = { start: k0, afterSpace: k1, afterRight: k2, backTo: pageNo() };
  // go to page
  const target = Math.min(total, 37);
  const input = document.querySelector<HTMLInputElement>('.pdf-pageno input')!;
  input.focus();
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
  setter.call(input, String(target));
  input.dispatchEvent(new Event('input', { bubbles: true }));
  input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
  await sleep(900);
  out.goto = { asked: target, shown: pageNo(), total };
  // scrolling updates the indicator
  pane.scrollTop += (document.querySelector<HTMLElement>('.pdf-row')?.getBoundingClientRect().height ?? 800) * 2;
  userScroll(pane);
  await sleep(700);
  out.afterScroll = pageNo();
  // links: find a page with an internal link among the first pages after the cover, follow it, come back
  let followed: Record<string, unknown> | null = null;
  for (let p = 2; p < Math.min(total, 40) && !followed; p++) {
    setter.call(input, String(p)); input.focus(); input.dispatchEvent(new Event('input', { bubbles: true })); input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    await sleep(900);
    const links = [...document.querySelectorAll<HTMLButtonElement>(`.pdf-page[data-page="${p - 1}"] .pdf-link`)];
    const internal = links.find((l) => l.getAttribute('aria-label') === '문서 안의 위치로 이동');
    if (!internal) continue;
    const from = pageNo();
    internal.click();
    await sleep(1200);
    const to = pageNo();
    btn('이전 위치로 돌아가기')?.click();
    await sleep(900);
    followed = { linksOnPage: links.length, from, to, moved: to !== from, backTo: pageNo() };
  }
  out.link = followed;
  // two-page spread, night pages, thumbnails
  const ctl = (text: string) => [...document.querySelectorAll<HTMLButtonElement>('.pdf-controls button')].find((b) => b.textContent === text);
  ctl('한 쪽')?.click();
  await sleep(900);
  const rowsWithTwo = [...document.querySelectorAll('.pdf-row')].filter((r) => r.querySelectorAll('.pdf-page[data-page]').length === 2).length;
  out.spread = { rowsWithTwo, firstRowPages: document.querySelector('.pdf-row')?.querySelectorAll('.pdf-page[data-page]').length, hScroll: pane.scrollWidth > pane.clientWidth + 1 };
  ctl('두 쪽')?.click();
  await sleep(600);
  ctl('야간')?.click();
  await sleep(300);
  out.night = getComputedStyle(document.querySelector('.pdf-page')!).filter.includes('invert');
  ctl('야간')?.click();
  if (!document.querySelector('#reader-sidebar')) document.querySelector<HTMLButtonElement>('button[aria-controls="reader-sidebar"]')?.click();
  await sleep(300);
  clickButton('쪽');
  const thumbMs = await waitFor(() => document.querySelectorAll('.thumb-img img').length >= 3, 15000);
  const before = pageNo();
  document.querySelector<HTMLButtonElement>('.thumb[data-thumb="11"]')?.click();
  await sleep(900);
  out.thumbs = { total: document.querySelectorAll('.thumb').length, renderedMs: thumbMs, rendered: document.querySelectorAll('.thumb-img img').length, jumpFrom: before, jumpTo: pageNo(), currentMarked: !!document.querySelector('.thumb.current') };
  // full screen: toolbar tucked away, back with Esc
  document.querySelector<HTMLButtonElement>('button[aria-label="전체 화면"]')?.click();
  const fsMs = await waitFor(() => !!document.querySelector('.reader.fs'), 6000);
  await sleep(1200);
  const tb = document.querySelector<HTMLElement>('.reader > .toolbar');
  const tucked = tb ? tb.getBoundingClientRect().bottom <= 1 : null;
  document.querySelector<HTMLElement>('.fs-top-strip')?.dispatchEvent(new MouseEvent('mouseover', { bubbles: true }));
  await sleep(500);
  const shown = tb ? tb.getBoundingClientRect().top >= -1 && tb.getBoundingClientRect().bottom > 20 : null;
  window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
  const exitMs = await waitFor(() => !document.querySelector('.reader.fs'), 6000);
  await sleep(1200);
  out.fullscreen = { fsMs, tucked, shownOnHover: shown, exitMs, sidebarHiddenInFs: true };
}

/** Highlights, notes and search tinting in the real webview. Counts only. */
async function annotateCheck(res: Record<string, unknown>, isPdf: boolean) {
  const out: Record<string, unknown> = {};
  res.annotate = out;
  type Reg = { has(n: string): boolean; get(n: string): { size: number } | undefined };
  const reg = (CSS as unknown as { highlights?: Reg }).highlights;
  out.api = !!reg;
  const size = (n: string) => reg?.get(n)?.size ?? 0;
  if (isPdf) clickButton('리플로우');
  clickButton('병렬');
  await waitFor(() => rows() > 3, 20000);
  await sleep(800);
  // select the first ~24 characters of a paragraph that is long enough
  const src = [...document.querySelectorAll<HTMLElement>('.brow.kind-paragraph > .src')].find((e) => (e.textContent ?? '').trim().length > 60);
  if (!src) { out.error = 'no paragraph'; return; }
  const blockId = src.closest<HTMLElement>('.brow')!.dataset.blockId!;
  const walker = document.createTreeWalker(src, NodeFilter.SHOW_TEXT);
  const first = walker.nextNode() as Text;
  const r = document.createRange();
  r.setStart(first, 0);
  r.setEnd(first, Math.min(first.data.length, 24));
  const sel = window.getSelection()!;
  sel.removeAllRanges();
  sel.addRange(r);
  const barMs = await waitFor(() => !!document.querySelector('.sel-bar'), 3000);
  out.toolbarMs = barMs;
  document.querySelector<HTMLButtonElement>('.sel-bar .hl-yellow')?.click();
  out.paintedMs = await waitFor(() => size('thoth-yellow') > 0, 4000);
  out.yellowRanges = size('thoth-yellow');
  // the note list shows it
  if (!document.querySelector('.sidebar, #reader-sidebar')) document.querySelector<HTMLButtonElement>('button[aria-controls="reader-sidebar"]')?.click();
  await sleep(300);
  clickButton('메모');
  await sleep(500);
  out.listed = document.querySelectorAll('.note-item').length;
  // PDF: the same highlight shows on the page's text layer in the original view
  if (isPdf) {
    clickButton('원본');
    await waitFor(() => canvases() > 0, 15000);
    out.onTextLayerMs = await waitFor(() => size('thoth-yellow') > 0 && !!document.querySelector('.textLayer span'), 12000);
    out.textLayerRanges = size('thoth-yellow');
    clickButton('리플로우');
    await waitFor(() => rows() > 3, 10000);
  }
  // search tinting + stepping
  clickButton('검색');
  await sleep(300);
  const word = (src.textContent ?? '').trim().split(/\s+/).find((w) => /^[A-Za-z]{5,}$/.test(w)) ?? 'the';
  const input = document.querySelector<HTMLInputElement>('.search input[type="search"]');
  if (input) {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, word);
    input.dispatchEvent(new Event('input', { bubbles: true }));
    input.form?.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
    out.hitsMs = await waitFor(() => document.querySelectorAll('.hits li').length > 0, 8000);
    out.hits = document.querySelectorAll('.hits li').length;
    out.findTintMs = await waitFor(() => size('thoth-find') > 0, 4000);
    out.findRanges = size('thoth-find');
    document.querySelector<HTMLButtonElement>('.search-nav button[aria-label="다음 결과"]')?.click();
    await sleep(900);
    out.stepped = !!document.querySelector('.hits button.current');
  }
  // survives a reload of the list; then delete
  clickButton('메모');
  await sleep(400);
  [...document.querySelectorAll<HTMLButtonElement>('.note-item button')].find((b) => b.textContent === '삭제')?.click();
  out.deletedMs = await waitFor(() => document.querySelectorAll('.note-item').length === 0 && size('thoth-yellow') === 0, 4000);
  void blockId;
}

/** Range translation with progress, then every export format (files land in a test dir). */
async function exportCheck(res: Record<string, unknown>, isPdf: boolean) {
  const out: Record<string, unknown> = {};
  res.export = out;
  await sleep(isPdf ? 6000 : 2500); // let a few pages/chapters be extracted
  [...document.querySelectorAll<HTMLButtonElement>('button.tx-open')][0]?.click();
  await waitFor(() => !!document.querySelector('.tx-dialog'), 4000);
  const dlg = () => document.querySelector<HTMLElement>('.tx-dialog')!;
  const inDlg = (text: string) => [...dlg().querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === text);
  const setVal = (el: HTMLInputElement | HTMLSelectElement, v: string) => {
    const proto = el instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value')!.set!.call(el, v);
    el.dispatchEvent(new Event(el instanceof HTMLSelectElement ? 'change' : 'input', { bubbles: true }));
  };
  out.chapters = dlg().querySelectorAll('.tx-scope input[type="radio"]').length === 3;
  // a small range
  const range = dlg().querySelector<HTMLInputElement>('.tx-pages')!;
  range.focus();
  setVal(range, isPdf ? '5-9' : '2-3');
  await sleep(300);
  out.summary = dlg().querySelector('.tx-summary')?.textContent;
  inDlg('이 범위 번역')?.click();
  const seen: number[] = [];
  let sawEta = false;
  const t0 = performance.now();
  for (let i = 0; i < 600; i++) {
    const txt = dlg().querySelector('.batch-line .batch-text')?.textContent ?? '';
    const m = /(\d+)%/.exec(txt);
    if (m) { const v = Number(m[1]); if (seen[seen.length - 1] !== v) seen.push(v); }
    if (/약 \d+|1분 미만/.test(dlg().textContent ?? '')) sawEta = true;
    if (/번역을 마쳤습니다|멈췄습니다|중지했습니다/.test(dlg().textContent ?? '')) break;
    await sleep(200);
  }
  out.batch = { ms: Math.round(performance.now() - t0), steps: seen.length, last: seen[seen.length - 1], monotonic: seen.every((v, i) => i === 0 || v >= seen[i - 1]!), sawEta, endText: (/번역을 마쳤습니다/.test(dlg().textContent ?? '') ? 'done' : 'other'), line: dlg().querySelector('.batch-line .batch-text')?.textContent };
  out.toolbarChip = document.querySelector('button.tx-open')?.textContent;
  // exports
  const formats = isPdf ? ['pdf-sbs', 'pdf', 'epub', 'html', 'markdown', 'text'] : ['epub', 'pdf', 'html'];
  const done: Record<string, unknown> = {};
  for (const f of formats) {
    setVal(dlg().querySelector<HTMLSelectElement>('.tx-row select')!, f);
    await sleep(200);
    const t1 = performance.now();
    const before = document.querySelectorAll('.toast').length;
    (inDlg('번역하고 내보내기') ?? inDlg('내보내기') ?? inDlg('지금까지 번역된 만큼 내보내기'))?.click();
    const ms = await waitFor(() => !dlg().querySelector('.tx-actions button:disabled') && !inDlg('취소') && performance.now() - t1 > 400, 120000);
    const toastText = [...document.querySelectorAll('.toast')].slice(before).map((x) => x.textContent ?? '').join(' | ');
    done[f] = { ms, error: dlg().querySelector('.badge.danger')?.textContent ?? null, toasts: document.querySelectorAll('.toast').length - before, pictures: Number(/그림 (\d+)개/.exec(toastText)?.[1] ?? 0), pages: Number(/PDF (\d+)쪽/.exec(toastText)?.[1] ?? 0) };
  }
  out.formats = done;
  // Export alone must translate a range nobody translated yet.
  if (isPdf) {
    range.focus();
    setVal(range, '12-14');
    await sleep(300);
    setVal(dlg().querySelector<HTMLSelectElement>('.tx-row select')!, 'pdf-sbs');
    await sleep(200);
    const callsBefore = dlg().querySelector('.batch-line .batch-text')?.textContent ?? '';
    const t2 = performance.now();
    const toastsBefore = document.querySelectorAll('.toast').length;
    inDlg('번역하고 내보내기')?.click();
    let sawTranslating = false;
    for (let i = 0; i < 900; i++) {
      if (/내보내기 전에 범위를 번역하는 중/.test(dlg().textContent ?? '')) sawTranslating = true;
      if (!inDlg('취소') && !dlg().querySelector('.tx-actions button:disabled') && performance.now() - t2 > 600) break;
      await sleep(200);
    }
    const toastText = [...document.querySelectorAll('.toast')].slice(toastsBefore).map((x) => x.textContent ?? '').join(' | ');
    out.autoTranslate = { ms: Math.round(performance.now() - t2), sawTranslating, before: callsBefore, after: dlg().querySelector('.batch-line .batch-text')?.textContent, error: dlg().querySelector('.badge.danger')?.textContent ?? null, untranslatedMentioned: /번역되지 않은 문단/.test(toastText), saved: /저장했습니다/.test(toastText) };
  }
  inDlg('닫기')?.click();
}

/** Selection toolbar: translate only the selected words, copy. */
async function selectionCheck(res: Record<string, unknown>, isPdf: boolean) {
  const out: Record<string, unknown> = {};
  res.selection = out;
  if (isPdf) clickButton('리플로우');
  clickButton('병렬');
  await waitFor(() => rows() > 3, 20000);
  await sleep(800);
  const src = [...document.querySelectorAll<HTMLElement>('.brow.kind-paragraph > .src')].find((e) => (e.textContent ?? '').trim().length > 60);
  if (!src) { out.error = 'no paragraph'; return; }
  const first = document.createTreeWalker(src, NodeFilter.SHOW_TEXT).nextNode() as Text;
  const r = document.createRange();
  r.setStart(first, 0);
  r.setEnd(first, Math.min(first.data.length, 30));
  const sel = window.getSelection()!;
  sel.removeAllRanges();
  sel.addRange(r);
  out.toolbarMs = await waitFor(() => !!document.querySelector('.sel-bar'), 3000);
  const bar = () => document.querySelector<HTMLElement>('.sel-bar');
  const b = (t: string) => [...(bar()?.querySelectorAll<HTMLButtonElement>('button') ?? [])].find((x) => x.textContent === t);
  out.buttons = [...(bar()?.querySelectorAll('button') ?? [])].map((x) => x.textContent || x.getAttribute('aria-label')).length;
  b('선택 번역')?.click();
  out.translatedMs = await waitFor(() => { const t = bar()?.querySelector('.sel-result p')?.textContent ?? ''; return t.length > 0 && t !== '번역 중…'; }, 20000);
  out.resultLen = (bar()?.querySelector('.sel-result p')?.textContent ?? '').length;
  out.isError = !!bar()?.querySelector('.sel-result .warn-text');
  b('닫기')?.click();
  await sleep(300);
  out.closed = !document.querySelector('.sel-bar');
}

/** PDF page view, translation only: one translated page per row, source on click. */
async function pdfTranslationOnly(res: Record<string, unknown>) {
  const out: Record<string, unknown> = {};
  res.pdftx = out;
  const snap = async (name: string) => { const r = await invoke<unknown>('debug_snapshot', { meta: { apiVersion: 1, requestId: crypto.randomUUID() }, name }).catch((e) => String(e)); return /"saved":true/.test(JSON.stringify(r)); };
  await sleep(4000);
  clickButton('병렬');
  await sleep(2500);
  const pages = () => document.querySelector<HTMLElement>('.pdf-pages');
  const w = (sel: string) => document.querySelector<HTMLElement>(sel)?.getBoundingClientRect().width ?? 0;
  const over = () => (pages()?.scrollWidth ?? 0) - (pages()?.clientWidth ?? 0);
  out.parallel = { cls: pages()?.className, translatedW: Math.round(w('.pdf-page.translated')), over: over() };
  clickButton('번역');
  await sleep(3500);
  const orig = document.querySelector<HTMLElement>('.pdf-page:not(.translated)');
  out.translation = {
    cls: pages()?.className,
    translatedW: Math.round(w('.pdf-page.translated')),
    originalHidden: orig ? getComputedStyle(orig).visibility === 'hidden' && getComputedStyle(orig).position === 'absolute' : null,
    originalCanvas: !!orig?.querySelector('canvas'),
    over: over(),
    texts: document.querySelectorAll('.pdf-page.translated .tp-text').length,
    slots: document.querySelectorAll('.pdf-page.translated .tp-slot').length,
  };
  await waitFor(() => document.querySelectorAll('.pdf-page.translated .tp-text').length > 2, 30000);
  out.translatedTexts = document.querySelectorAll('.pdf-page.translated .tp-text').length;
  out.shot = await snap('pdf-translation-only');
  document.querySelector<HTMLElement>('.pdf-page.translated .tp-text')?.click();
  await sleep(600);
  out.popover = { open: !!document.querySelector('.pdf-pop'), sourceShown: (document.querySelector('.pdf-pop-src')?.textContent ?? '').length > 0 };
  out.popShot = await snap('pdf-translation-only-pop');
  clickButton('원문');
  await sleep(1500);
  out.backToOriginal = { over: over(), cls: pages()?.className, originalVisible: orig ? getComputedStyle(document.querySelector<HTMLElement>('.pdf-page:not(.translated)')!).visibility !== 'hidden' : null };
}

/** Settings: switching the provider in one step, picking a model on another one, removing one. */
async function providersCheck(res: Record<string, unknown>, endpoint: string) {
  const out: Record<string, unknown> = {};
  res.providers = out;
  const snap = async (name: string) => { const r = await invoke<unknown>('debug_snapshot', { meta: { apiVersion: 1, requestId: crypto.randomUUID() }, name }).catch((e) => String(e)); return /"saved":true/.test(JSON.stringify(r)); };
  // a second provider on the same server, never used
  await call('provider_save', { providerId: null, kind: 'openai_compatible', label: '둘째 서버', endpoint, accountNamespace: 'second', allowPrivateHttp: false, enabled: true });
  document.querySelector<HTMLButtonElement>('button[aria-label="번역 설정"]')?.click();
  await waitFor(() => document.querySelectorAll('.modal.settings .prov-list li').length === 2, 6000);
  await sleep(800);
  const dlg = () => document.querySelector<HTMLElement>('.modal.settings')!;
  const rows = () => [...dlg().querySelectorAll<HTMLElement>('.prov-list li')];
  const btn = (root: ParentNode, text: string) => [...root.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === text);
  const inUse = () => rows().map((r) => r.querySelector('.in-use')?.textContent ?? null);
  out.start = { rows: rows().length, selectedIsActive: rows()[0]?.classList.contains('sel'), inUse: inUse(), useButtons: rows().map((r) => !!btn(r, '사용')) };
  // "use" on a provider without a model: nothing switches, the reader is told what to do
  btn(rows()[1]!, '사용')?.click();
  await sleep(900);
  out.useWithoutModel = { note: dlg().querySelector('[role="status"].muted')?.textContent?.slice(0, 40), inUse: inUse(), secondSelected: rows()[1]?.classList.contains('sel') };
  // its models were listed without pressing anything
  out.autoModelsMs = await waitFor(() => (dlg().querySelector<HTMLSelectElement>('.card select.input')?.options.length ?? 0) > 1, 8000);
  const sel = dlg().querySelector<HTMLSelectElement>('.card select.input');
  if (sel) {
    const v = [...sel.options].map((o) => o.value).find((x) => x) ?? '';
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!.call(sel, v);
    sel.dispatchEvent(new Event('change', { bubbles: true }));
    await sleep(300);
    btn(dlg(), '이 모델로 번역하기')?.click();
    await waitFor(() => !!rows()[1]?.querySelector('.in-use'), 8000);
  }
  out.afterPick = { inUse: inUse(), chip: document.querySelector('.model-chip')?.getAttribute('title') };
  // one click back to the first provider
  btn(rows()[0]!, '사용')?.click();
  await waitFor(() => !!rows()[0]?.querySelector('.in-use'), 6000);
  await sleep(400);
  out.switchedBack = { inUse: inUse(), note: dlg().querySelector('[role="status"].muted')?.textContent?.slice(0, 30), chip: document.querySelector('.model-chip')?.getAttribute('title') };
  out.shot = await snap('providers');
  // remove the second one
  rows()[1]?.querySelector<HTMLButtonElement>('.prov-pick')?.click();
  await sleep(500);
  btn(dlg(), '삭제')?.click();
  await sleep(300);
  out.confirmShown = !!dlg().querySelector('#remove-title');
  out.confirmShot = await snap('providers-remove');
  [...dlg().querySelectorAll<HTMLButtonElement>('.consent button')].find((b) => b.textContent === '삭제')?.click();
  await waitFor(() => rows().length === 1, 6000);
  await sleep(400);
  out.afterRemove = { rows: rows().length, inUse: inUse(), listed: (await call<{ id: string }[]>('provider_list')).length };
  // removing the one in use stops translation until another is chosen
  btn(dlg(), '삭제')?.click();
  await sleep(300);
  [...dlg().querySelectorAll<HTMLButtonElement>('.consent button')].find((b) => b.textContent === '삭제')?.click();
  await waitFor(() => rows().length === 0, 6000);
  await sleep(600);
  out.afterRemoveActive = { rows: rows().length, active: await call<unknown>('profile_active'), chip: document.querySelector('.model-chip')?.textContent ?? null };
  [...dlg().querySelectorAll<HTMLButtonElement>('header button')].find((b) => b.textContent === '닫기')?.click();
}

/** Pictures in the translation column, the translation-only view, and the dark original view. */
async function viewsCheck(res: Record<string, unknown>) {
  const out: Record<string, unknown> = {};
  res.views = out;
  const snap = async (name: string) => { const r = await invoke<unknown>('debug_snapshot', { meta: { apiVersion: 1, requestId: crypto.randomUUID() }, name }).catch((e) => String(e)); return /"saved":true/.test(JSON.stringify(r)); };
  clickButton('병렬');
  await waitFor(() => !!document.querySelector('.reflow'), 5000);
  await sleep(1500);
  const scroller = () => document.querySelector<HTMLElement>('.reflow');
  const fig = () => document.querySelector<HTMLElement>('.brow .trn-figure');
  // walk the chapters until a figure with a caption shows up
  if (!document.querySelector('#reader-sidebar')) document.querySelector<HTMLButtonElement>('button[aria-controls="reader-sidebar"]')?.click();
  await sleep(300);
  clickButton('목차');
  await sleep(300);
  const items = [...document.querySelectorAll<HTMLButtonElement>('.toc-item')];
  for (let i = 0; i < Math.min(items.length, 14) && !fig(); i++) {
    items[i]?.click();
    await sleep(1200);
    for (let k = 0; k < 30 && !fig(); k++) { const sc = scroller(); if (!sc) break; const before = sc.scrollTop; sc.scrollTop += 1800; userScroll(sc); await sleep(250); if (sc.scrollTop === before) break; }
  }
  const f = fig();
  out.figureFound = !!f;
  if (f) {
    f.scrollIntoView({ block: 'center' });
    const sc = scroller(); if (sc) userScroll(sc);
    await sleep(2500);
    const row = fig()?.closest('.brow');
    const img = fig()?.querySelector('img');
    await waitFor(() => !!row?.querySelector('.tcell-text'), 20000);
    out.figure = { kind: row?.className.match(/kind-\w+/)?.[0], translatedSideImage: !!img && img.complete && img.naturalWidth > 0, captionTranslated: !!row?.querySelector('.tcell-text'), sameHeight: Math.abs((row?.querySelector('.src img')?.getBoundingClientRect().height ?? 0) - (img?.getBoundingClientRect().height ?? -9)) <= 1 };
    out.figureShot = await snap('view-figure');
  }
  // translation only
  clickButton('번역');
  await waitFor(() => !!document.querySelector('.reflow.mode-translation'), 5000);
  await sleep(1500);
  const vis = (sel: string) => [...document.querySelectorAll<HTMLElement>(sel)].filter((e) => { const b = e.getBoundingClientRect(); return b.bottom > 60 && b.top < window.innerHeight && b.height > 0; });
  out.translationOnly = { hidden: document.querySelectorAll('.brow.src-hidden').length, srcOnly: document.querySelectorAll('.brow.src-only').length, visibleSources: vis('.brow.src-hidden > .src').length, visibleTranslations: vis('.brow.src-hidden .tcell-text').length };
  out.translationShot = await snap('view-translation');
  const target = vis('.brow.src-hidden.kind-paragraph .tcell-text')[0];
  target?.click();
  await sleep(500);
  out.reveal = { opened: document.querySelectorAll('.brow.reveal').length, sourceVisible: vis('.brow.reveal > .src').length };
  out.revealShot = await snap('view-translation-reveal');
  vis('.brow.reveal .tcell-text')[0]?.click();
  await sleep(400);
  out.closedAgain = document.querySelectorAll('.brow.reveal').length === 0;
  // dark original
  await call('settings_patch', { key: 'theme', value: 'dark', expectedVersion: null }).catch(() => {});
  await window.__thoth?.reloadSettings?.();
  clickButton('원문');
  const frameDoc = () => document.querySelector<HTMLIFrameElement>('.epub-host iframe')?.contentDocument ?? null;
  out.originalTextMs = await waitFor(() => (frameDoc()?.body?.textContent ?? '').trim().length > 200, 40000);
  await sleep(1500);
  const doc = frameDoc();
  const win = document.querySelector<HTMLIFrameElement>('.epub-host iframe')?.contentWindow;
  out.frameDebug = { frames: document.querySelectorAll('.epub-host iframe').length, epub: (window as unknown as { __thothEpub?: unknown }).__thothEpub ?? null, err: document.querySelector('.reader-error')?.textContent ?? null };
  const p = doc?.querySelector('p');
  out.dark = { htmlClass: doc?.documentElement.classList.contains('thoth-dark'), bodyBg: doc?.body && win ? win.getComputedStyle(doc.body).backgroundColor : null, textColor: p && win ? win.getComputedStyle(p).color : null };
  out.darkShot = await snap('view-original-dark');
  await call('settings_patch', { key: 'theme', value: 'light', expectedVersion: null }).catch(() => {});
  await window.__thoth?.reloadSettings?.();
  await sleep(600);
  out.lightAgain = { htmlClass: doc?.documentElement.classList.contains('thoth-dark'), bodyBg: doc?.body && win ? win.getComputedStyle(doc.body).backgroundColor : null };
}

/** Documentation screenshots (README): the reader on the first book, then the library. */
async function screensCheck(res: Record<string, unknown>, index: number, count: number) {
  const out: Record<string, unknown> = {};
  res.screens = out;
  const snap = async (name: string) => { const r = await invoke<unknown>('debug_snapshot', { meta: { apiVersion: 1, requestId: crypto.randomUUID() }, name }).catch((e) => String((e as Error)?.message ?? e)); const t = JSON.stringify(r); return /"saved":true/.test(t) || t.slice(0, 200); };
  if (index === 0) {
    // leave the front matter: go to the first real chapter
    if (!document.querySelector('#reader-sidebar')) document.querySelector<HTMLButtonElement>('button[aria-controls="reader-sidebar"]')?.click();
    await sleep(400);
    clickButton('목차');
    await sleep(400);
    const items = [...document.querySelectorAll<HTMLButtonElement>('.toc-item')];
    const chapter = items.find((b) => /chapter i\b|chapter 1\b|rabbit/i.test(b.textContent ?? '')) ?? items[Math.min(2, items.length - 1)];
    clickButton('병렬');
    await waitFor(() => !!document.querySelector('.reflow'), 5000);
    await sleep(1500);
    const atChapter = () => [...document.querySelectorAll<HTMLElement>('.brow.kind-heading')].some((r) => { const b = r.getBoundingClientRect(); return b.top > 40 && b.top < 260 && /rabbit|chapter/i.test(r.textContent ?? ''); });
    for (let k = 0; k < 4 && !atChapter(); k++) { chapter?.click(); await waitFor(atChapter, 4000); }
    out.atChapter = atChapter();
    await sleep(1000);
    const filled = () => {
      const vis = [...document.querySelectorAll<HTMLElement>('.brow')].filter((r) => { const b = r.getBoundingClientRect(); return b.bottom > 60 && b.top < window.innerHeight; });
      return vis.length > 2 && vis.every((r) => !r.querySelector('.tcell') || r.querySelector('.tcell-text') || r.querySelector('.tcell.excluded'));
    };
    out.pending = { sources: document.querySelectorAll('.tcell-source').length, busy: document.querySelectorAll('.tcell-source.busy').length, shot: await snap('reader-pending') };
    out.translatedMs = await waitFor(filled, 240000);
    out.sourcesLeftVisible = [...document.querySelectorAll<HTMLElement>('.tcell-source')].filter((e) => { const b = e.getBoundingClientRect(); return b.bottom > 60 && b.top < window.innerHeight; }).length;
    await sleep(1200);
    out.parallel = await snap('reader-parallel');
    // settings: which model translates, and renaming the provider
    out.modelChip = document.querySelector('.model-chip')?.textContent ?? null;
    document.querySelector<HTMLButtonElement>('button[aria-label="번역 설정"]')?.click();
    await waitFor(() => !!document.querySelector('.modal.settings .current-model'), 5000);
    await sleep(600);
    out.currentModel = document.querySelector('.current-model strong')?.textContent ?? null;
    out.inUseBadge = document.querySelector('.prov-pick .in-use')?.textContent ?? null;
    const nameInput = [...document.querySelectorAll<HTMLInputElement>('.modal.settings .card label')].find((l) => l.textContent?.startsWith('이름'))?.querySelector('input');
    if (nameInput) {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(nameInput, 'LM Studio (로컬)');
      nameInput.dispatchEvent(new Event('input', { bubbles: true }));
      await sleep(200);
      [...document.querySelectorAll<HTMLButtonElement>('.modal.settings button')].find((b) => b.textContent === '이름 저장')?.click();
      await sleep(900);
    }
    out.renamed = { list: document.querySelector('.prov-pick strong')?.textContent ?? null, heading: document.querySelector('.modal.settings section:nth-of-type(2) .card h3')?.textContent ?? null };
    out.settingsShot = await snap('settings');
    [...document.querySelectorAll<HTMLButtonElement>('.modal.settings header button')].find((b) => b.textContent === '닫기')?.click();
    await sleep(500);
    out.chipTitleAfterRename = document.querySelector('.model-chip')?.getAttribute('title') ?? null;
    clickButton('원문');
    await waitFor(() => epubFrameText() > 0, 15000);
    await sleep(1500);
    chapter?.click();
    await sleep(3500);
    out.original = await snap('reader-original');
    clickButton('병렬');
    await sleep(800);
  }
  if (index === count - 1) {
    window.__thoth?.close?.();
    await waitFor(() => document.querySelectorAll('.book-card, .lib-card, [data-doc-id]').length > 0, 8000);
    await sleep(4000); // covers
    out.library = await snap('library');
  }
}

/** EPUB original view: highlight from a selection inside the book, page turning, two columns. */
async function epubViewCheck(res: Record<string, unknown>) {
  const out: Record<string, unknown> = {};
  res.epubview = out;
  clickButton('원문');
  await waitFor(() => epubFrameText() > 200, 20000);
  await sleep(1200);
  const frame = () => document.querySelector<HTMLIFrameElement>('.epub-host iframe');
  type W = Window & { CSS: { highlights: { get(n: string): { size: number } | undefined } } };
  const hl = () => (frame()?.contentWindow as W | null)?.CSS?.highlights?.get('thoth-yellow')?.size ?? 0;
  // find a rendered paragraph with enough text; the first chapters are often covers
  let para: Element | null = null;
  const findPara = () => [...(frame()?.contentDocument?.querySelectorAll('p') ?? [])].find((p) => (p.textContent ?? '').trim().length > 80) ?? null;
  await waitFor(() => !!(para = findPara()), 3000);
  if (!para) {
    if (!document.querySelector('#reader-sidebar')) document.querySelector<HTMLButtonElement>('button[aria-controls="reader-sidebar"]')?.click();
    await sleep(300);
    clickButton('목차');
    await sleep(300);
    const tocN = document.querySelectorAll('.toc-item').length;
    out.tocItems = tocN;
    for (let i = 1; i < Math.min(tocN, 10) && !para; i++) {
      [...document.querySelectorAll<HTMLButtonElement>('.toc-item')][i]?.click();
      await waitFor(() => !!(para = findPara()), 4000);
    }
  }
  if (!para) {
    const d = frame()?.contentDocument;
    const lens = [...(d?.querySelectorAll('p') ?? [])].map((p) => (p.textContent ?? '').trim().length);
    out.error = 'no paragraph rendered';
    out.debug = { hasDoc: !!d, frames: document.querySelectorAll('.epub-host iframe').length, ps: lens.length, maxP: Math.max(0, ...lens), bodyLen: (d?.body?.textContent ?? '').length, tags: [...new Set([...(d?.body?.querySelectorAll('*') ?? [])].map((e) => e.tagName.toLowerCase()))].slice(0, 14), flow: localStorage.getItem('thoth.epubFlow') };
    return;
  }
  const paraEl = para as Element;
  const doc = frame()!.contentDocument!;
  // first text node that has real words (leading nodes are often just whitespace)
  const tw = doc.createTreeWalker(paraEl, 4);
  let text = tw.nextNode() as Text | null;
  while (text && text.data.trim().length < 12) text = tw.nextNode() as Text | null;
  if (!text) { out.error = 'no text node'; return; }
  const lead = text.data.length - text.data.trimStart().length;
  const r = doc.createRange();
  r.setStart(text, lead);
  r.setEnd(text, Math.min(text.data.length, lead + 24));
  const sel = frame()!.contentWindow!.getSelection()!;
  sel.removeAllRanges();
  sel.addRange(r);
  out.toolbarMs = await waitFor(() => !!document.querySelector('.sel-bar'), 4000);
  out.selDebug = (window as unknown as { __thothSel?: unknown }).__thothSel ?? null;
  out.paraInfo = { len: (paraEl.textContent ?? '').length, tag: paraEl.parentElement?.tagName };
  out.hasTranslate = [...document.querySelectorAll('.sel-bar button')].some((b) => b.textContent === '선택 번역');
  document.querySelector<HTMLButtonElement>('.sel-bar .hl-yellow')?.click();
  out.paintedMs = await waitFor(() => hl() > 0, 5000);
  out.ranges = hl();
  // page turning
  const ctl = (t: string) => [...document.querySelectorAll<HTMLButtonElement>('.epub-controls button')].find((b) => b.textContent === t);
  const anchorBefore = anchorId();
  ctl('쪽 넘김')?.click();
  await waitFor(() => epubFrameText() > 200 && !!document.querySelector('.epub-turn.next'), 15000);
  await sleep(1500);
  const cont = () => document.querySelector<HTMLElement>('.epub-host .epub-container');
  out.paged = { turnButtons: document.querySelectorAll('.epub-turn').length, keptPlace: anchorId() === anchorBefore, highlightsAfterSwitch: hl() };
  const x0 = cont()?.scrollLeft ?? -1;
  const a0 = anchorId();
  document.querySelector<HTMLButtonElement>('.epub-turn.next')?.click();
  await sleep(1200);
  const x1 = cont()?.scrollLeft ?? -1;
  const a1 = anchorId();
  document.querySelector<HTMLButtonElement>('.epub-turn.prev')?.click();
  await sleep(1200);
  out.turn = { moved: x1 !== x0 || a1 !== a0, back: (cont()?.scrollLeft ?? -2) === x0 || anchorId() === a0 };
  const colW = () => parseFloat(getComputedStyle(frame()!.contentDocument!.documentElement).columnWidth || '0') || frame()!.contentDocument!.body.getBoundingClientRect().width;
  const w1 = colW();
  ctl('2단')?.click();
  await waitFor(() => epubFrameText() > 200, 15000);
  await sleep(1500);
  const w2 = colW();
  out.columns = { one: Math.round(w1), two: Math.round(w2), narrower: w2 < w1 * 0.75 };
  ctl('1단')?.click();
  await sleep(800);
  ctl('스크롤')?.click();
  await waitFor(() => epubFrameText() > 200 && !document.querySelector('.epub-turn'), 15000);
  await sleep(1200);
  out.backToScroll = { keptPlace: anchorId() === anchorBefore, highlights: hl() };
  // links: the frame runs no listeners, so a click is seen as a fragment jump from outside
  const links = () => [...(frame()?.contentDocument?.querySelectorAll<HTMLAnchorElement>('a[data-thoth-href]') ?? [])];
  const internal = () => links().filter((a) => a.getAttribute('data-thoth-external') !== '1' && /\d+\.xhtml/.test(a.getAttribute('data-thoth-href') ?? ''));
  if (!internal().length) {
    if (!document.querySelector('#reader-sidebar')) document.querySelector<HTMLButtonElement>('button[aria-controls="reader-sidebar"]')?.click();
    await sleep(300);
    clickButton('목차');
    await sleep(300);
    const items = [...document.querySelectorAll<HTMLButtonElement>('.toc-item')];
    for (let i = 0; i < Math.min(items.length, 12) && !internal().length; i++) {
      items[i]?.click();
      await waitFor(() => internal().length > 0, 2500);
    }
  }
  const cand = internal();
  const linkOut: Record<string, unknown> = { links: links().length, internal: cand.length, external: links().filter((a) => a.getAttribute('data-thoth-external') === '1').length, rawLeft: frame()?.contentDocument?.querySelectorAll('a[href]:not([data-thoth-href])').length ?? -1 };
  out.linkCheck = linkOut;
  if (cand.length) {
    const a = cand[cand.length - 1]!;
    const urlBefore = frame()?.contentDocument?.URL.split('#')[0];
    const posBefore = `${cont()?.scrollTop}:${cont()?.scrollLeft}:${anchorId()}`;
    const armed = a.getAttribute('href');
    a.click();
    await sleep(120);
    linkOut.hashJumped = frame()?.contentWindow?.location.hash === armed;
    linkOut.movedMs = await waitFor(() => frame()?.contentDocument?.URL.split('#')[0] !== urlBefore || `${cont()?.scrollTop}:${cont()?.scrollLeft}:${anchorId()}` !== posBefore, 6000);
    linkOut.chapterChanged = frame()?.contentDocument?.URL.split('#')[0] !== urlBefore;
    linkOut.rearmed = a.isConnected ? a.getAttribute('href') !== armed : null;
  }
}
