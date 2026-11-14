/**
 * Main-thread client for extract.worker.ts: request/response correlation,
 * per-page callbacks, cancellation and termination.
 */
import type { Block } from '../../ipc/generated/Block';
import type {
  ExtractWorkerRequest,
  ExtractWorkerResponse,
  PdfPageDiagnostics,
  PdfPageInput,
  RunningElementUpdate,
} from './types';

export interface WorkerLike {
  postMessage(msg: ExtractWorkerRequest): void;
  addEventListener(type: 'message', listener: (ev: MessageEvent<ExtractWorkerResponse>) => void): void;
  terminate(): void;
}

export interface ExtractCallbacks {
  onPage?: (pageIndex: number, blocks: Block[], diagnostics: PdfPageDiagnostics) => void;
  onPageError?: (pageIndex: number | null, message: string) => void;
  onRunning?: (updates: RunningElementUpdate[]) => void;
}

export interface ExtractJob {
  requestId: string;
  done: Promise<{ cancelled: boolean }>;
  cancel(): void;
}

/** Creates the bundled module worker (Vite emits it as a separate same-origin chunk). */
export function createExtractWorker(): WorkerLike {
  return new Worker(new URL('./extract.worker.ts', import.meta.url), { type: 'module', name: 'thoth-pdf-extract' }) as unknown as WorkerLike;
}

export class PdfExtractClient {
  #worker: WorkerLike;
  #seq = 0;
  #jobs = new Map<string, { cb: ExtractCallbacks; resolve: (v: { cancelled: boolean }) => void }>();

  constructor(worker: WorkerLike = createExtractWorker()) {
    this.#worker = worker;
    this.#worker.addEventListener('message', (ev) => this.#onMessage(ev.data));
  }

  extract(extractionId: string, pages: PdfPageInput[], cb: ExtractCallbacks = {}, detectRunning = true): ExtractJob {
    const requestId = `x${++this.#seq}`;
    const done = new Promise<{ cancelled: boolean }>((resolve) => this.#jobs.set(requestId, { cb, resolve }));
    this.#worker.postMessage({ type: 'extract', requestId, extractionId, pages, detectRunning });
    return {
      requestId,
      done,
      cancel: () => this.#worker.postMessage({ type: 'cancel', requestId }),
    };
  }

  /** Terminate the worker; pending jobs resolve as cancelled. */
  dispose(): void {
    this.#worker.terminate();
    for (const job of this.#jobs.values()) job.resolve({ cancelled: true });
    this.#jobs.clear();
  }

  #onMessage(msg: ExtractWorkerResponse): void {
    const job = this.#jobs.get(msg.requestId);
    if (!job) return; // late message for a disposed / unknown job
    switch (msg.type) {
      case 'result':
        job.cb.onPage?.(msg.pageIndex, msg.blocks, msg.diagnostics);
        break;
      case 'error':
        job.cb.onPageError?.(msg.pageIndex, msg.message);
        break;
      case 'running':
        job.cb.onRunning?.(msg.updates);
        break;
      case 'done':
        this.#jobs.delete(msg.requestId);
        job.resolve({ cancelled: msg.cancelled });
        break;
    }
  }
}
