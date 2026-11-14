// Dev-only WKWebView self-test (never bundled into release: guarded by import.meta.env.DEV).
// Driven by THOTH_SELFTEST=<file>[:<file>...] on the Rust side. Reports metrics only
// (no document text) through the debug_log command.
import { invoke } from '@tauri-apps/api/core';
import { call } from '../ipc/client';
import type { ImportCandidate } from '../ipc/generated/ImportCandidate';
import type { ImportResult } from '../ipc/generated/ImportResult';

type Hooks = { open: (id: string) => void; close: () => void; anchor: () => unknown };
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
  const cands = await call<ImportCandidate[]>('debug_selftest_files').catch(() => []);
  if (!cands.length) return;
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
    await log('result', res);
    window.__thoth?.close?.();
    await sleep(800);
  }
  await log('info', 'selftest done');
}
