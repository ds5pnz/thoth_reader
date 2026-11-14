// Dev-only WKWebView self-test (never bundled into release: guarded by import.meta.env.DEV).
// Driven by THOTH_SELFTEST=<file>[:<file>...] on the Rust side. Reports metrics only
// (no document text) through the debug_log command.
import { invoke } from '@tauri-apps/api/core';
import { call } from '../ipc/client';
import type { ImportCandidate } from '../ipc/generated/ImportCandidate';
import type { ImportResult } from '../ipc/generated/ImportResult';

type Hooks = { open: (id: string) => void; close: () => void; anchor: () => unknown; reloadSettings: () => Promise<void> };
declare global {
  interface Window { __thoth?: Partial<Hooks> }
}

const log = (level: string, message: unknown) =>
  invoke('debug_log', { meta: { apiVersion: 1, requestId: crypto.randomUUID() }, level, message: typeof message === 'string' ? message : JSON.stringify(message) }).catch(() => {});

export function installErrorForwarding() {
  window.addEventListener('error', (e) => void log('error', `${e.message} @ ${e.filename}:${e.lineno}`));
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
  const cfg = await call<{ candidates: ImportCandidate[]; provider: string | null; model: string | null; slowMs: number | null }>('debug_selftest_files').catch(() => ({ candidates: [], provider: null, model: null, slowMs: null }));
  WAIT = cfg.slowMs ?? 30000;
  const cands = cfg.candidates;
  if (!cands.length) return;
  if (cfg.provider) await setupProvider(cfg.provider, cfg.model);
  await log('info', `selftest start: ${cands.length} files`);
  await sleep(500);
  for (const [i, c] of cands.entries()) {
    const label = `doc-${i + 1}`;
    const t0 = performance.now();
    let r: ImportResult;
    try {
      r = await call<ImportResult>('document_import', { importToken: c.importToken });
    } catch (e) {
      await log('result', { label, stage: 'import', error: String((e as Error).message) });
      continue;
    }
    const importMs = Math.round(performance.now() - t0);
    const t1 = performance.now();
    window.__thoth?.open?.(r.documentId);
    const isPdf = r.format === 'pdf';
    const firstContent = await waitFor(() => rows() > 0 || canvases() > 0, 30000);
    const res: Record<string, unknown> = { label, format: r.format, importMs, openToFirstContentMs: firstContent, totalOpenMs: Math.round(performance.now() - t1) };
    await sleep(1500);
    res.initial = { rows: rows(), canvases: canvases(), anchor: window.__thoth?.anchor?.() };
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
    const p = await call<{ id: string }>('provider_save', { providerId: null, kind: 'openai_compatible', label: 'selftest mock', endpoint, accountNamespace: 'selftest', allowPrivateHttp: false, enabled: true });
    const probe = await call<{ ok: boolean; models: { id: string }[]; capabilities: Record<string, unknown> | null; warnings: string[] }>('provider_probe', { providerId: p.id });
    await log('result', { stage: 'provider', probeOk: probe.ok, models: probe.models.map((m) => m.id), caps: probe.capabilities, warnings: probe.warnings });
    const prof = await call<{ id: string }>('profile_create', { providerId: p.id, modelId: model ?? probe.models[0]?.id ?? 'mock-ko', sourceLanguage: 'en', targetLanguage: 'ko', temperature: 0.2, seed: null, maxOutputTokens: null, contextLimit: 8192, modelDigest: null });
    await call('settings_patch', { key: 'activeProfileId', value: prof.id, expectedVersion: null });
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
  if (scroller) { scroller.scrollTop = 4000; scroller.dispatchEvent(new Event('scroll')); }
  res.afterScrollTranslatedMs = await waitFor(() => translated() >= before + 3, WAIT);
  res.translatedCells = translated();
  res.translateWindowMs = Math.round(performance.now() - t0);
  // Manual edit on the first translated cell: double-click, type, Cmd+Enter.
  res.samples = [...document.querySelectorAll<HTMLElement>('.brow')].filter((r) => r.querySelector('.tcell-text')).slice(0, 2).map((r) => ({ src: (r.querySelector('.src')?.textContent ?? '').slice(0, 90), ko: (r.querySelector('.tcell-text')?.textContent ?? '').slice(0, 90) }));
  res.failedCells = document.querySelectorAll('.tcell .badge.danger').length;
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
  await sleep(1500);
  res.reopenAnchor = ((window.__thoth?.anchor?.() as { blockId?: string } | null)?.blockId ?? '').slice(0, 8);
  res.reopenManualKept = [...document.querySelectorAll('.tcell-text')].some((x) => x.textContent === '수동으로 고친 번역');
  res.reopenWindowMs = Math.round(performance.now() - t1);
  await log('mark', 'reopen-end');
}
