/**
 * Worker-agnostic implementation of the extract protocol (testable without a
 * real Worker). extract.worker.ts only wires it to `self`.
 */
import { sha256Hex } from '../../domain/canonicalJson';
import { extractPage } from './extract';
import { applyRunningUpdates, detectRunningElements } from './running';
import type { ExtractDeps, ExtractRequest, ExtractWorkerRequest, ExtractWorkerResponse, PdfPageResult } from './types';

export interface WorkerCore {
  handle(msg: ExtractWorkerRequest): Promise<void>;
}

export function createWorkerCore(post: (msg: ExtractWorkerResponse) => void, deps: ExtractDeps = { sha256Hex }): WorkerCore {
  const cancelled = new Set<string>();

  async function runExtract(req: ExtractRequest): Promise<void> {
    const results: PdfPageResult[] = [];
    for (const page of req.pages) {
      if (cancelled.has(req.requestId)) break;
      try {
        const res = await extractPage(page, req.extractionId, deps);
        results.push(res);
        post({ type: 'result', requestId: req.requestId, pageIndex: res.pageIndex, blocks: res.blocks, diagnostics: res.diagnostics });
      } catch (e) {
        // A broken page never blocks the others (spec §4.2 "손상된 일부 페이지").
        post({ type: 'error', requestId: req.requestId, pageIndex: page.pageIndex, message: e instanceof Error ? e.message : String(e) });
      }
      // Yield so a 'cancel' message can be processed between pages.
      await new Promise<void>((r) => setTimeout(r, 0));
    }
    const wasCancelled = cancelled.has(req.requestId);
    if (!wasCancelled && req.detectRunning !== false && results.length > 0) {
      const updates = detectRunningElements(results);
      applyRunningUpdates(results, updates);
      if (updates.length) post({ type: 'running', requestId: req.requestId, updates });
    }
    cancelled.delete(req.requestId);
    post({ type: 'done', requestId: req.requestId, cancelled: wasCancelled });
  }

  return {
    async handle(msg) {
      if (!msg || typeof msg !== 'object') return;
      if (msg.type === 'cancel') {
        cancelled.add(msg.requestId);
        return;
      }
      if (msg.type === 'extract') {
        try {
          await runExtract(msg);
        } catch (e) {
          post({ type: 'error', requestId: msg.requestId, pageIndex: null, message: e instanceof Error ? e.message : String(e) });
          post({ type: 'done', requestId: msg.requestId, cancelled: false });
        }
      }
    },
  };
}
