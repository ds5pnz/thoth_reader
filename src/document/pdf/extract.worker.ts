/// <reference lib="webworker" />
/**
 * Dedicated module worker running the PDF layout extractor off the main thread.
 *
 *   new Worker(new URL('./extract.worker.ts', import.meta.url), { type: 'module' })
 *
 * Protocol (see types.ts):
 *   → { type: 'extract', requestId, extractionId, pages: PdfPageInput[], detectRunning? }
 *   ← { type: 'result', requestId, pageIndex, blocks, diagnostics }   (one per page, in order)
 *   ← { type: 'error', requestId, pageIndex | null, message }          (page failures do not stop the batch)
 *   ← { type: 'running', requestId, updates }                          (running header/footer roles, after all pages)
 *   ← { type: 'done', requestId, cancelled }
 *   → { type: 'cancel', requestId }                                    (checked between pages)
 */
import { createWorkerCore } from './extractWorkerCore';
import type { ExtractWorkerRequest, ExtractWorkerResponse } from './types';

declare const self: DedicatedWorkerGlobalScope;

const core = createWorkerCore((msg: ExtractWorkerResponse) => self.postMessage(msg));

self.addEventListener('message', (ev: MessageEvent<ExtractWorkerRequest>) => {
  void core.handle(ev.data);
});
