import { describe, expect, it } from 'vitest';
import { PdfExtractClient, type WorkerLike } from './extractClient';
import { createWorkerCore } from './extractWorkerCore';
import { filler, item, pageInput, para } from './testing/synthetic';
import type { ExtractWorkerRequest, ExtractWorkerResponse, PdfPageInput } from './types';

function pages(n: number): PdfPageInput[] {
  return Array.from({ length: n }, (_, i) =>
    pageInput([item('Running Title', 72, 760, 9, 'H'), ...para([filler(i, 80) + '.'], 72, 700), item(String(i + 1), 300, 30, 9, 'H')], { pageIndex: i }),
  );
}

describe('extract worker protocol', () => {
  it('streams one result per page, then running updates, then done', async () => {
    const out: ExtractWorkerResponse[] = [];
    const core = createWorkerCore((m) => out.push(m));
    await core.handle({ type: 'extract', requestId: 'r1', extractionId: 'ext', pages: pages(3) });
    expect(out.map((m) => m.type)).toEqual(['result', 'result', 'result', 'running', 'done']);
    const results = out.filter((m) => m.type === 'result');
    expect(results.map((m) => m.pageIndex)).toEqual([0, 1, 2]);
    const running = out.find((m) => m.type === 'running');
    expect(running?.type === 'running' && running.updates.every((u) => u.role === 'running_header' || u.role === 'running_footer')).toBe(true);
    expect(out.at(-1)).toEqual({ type: 'done', requestId: 'r1', cancelled: false });
  });

  it('reports a broken page and continues with the others', async () => {
    const out: ExtractWorkerResponse[] = [];
    const core = createWorkerCore((m) => out.push(m));
    const bad = { ...pages(1)[0]!, pageIndex: 1, viewBox: null as unknown as number[] };
    await core.handle({ type: 'extract', requestId: 'r2', extractionId: 'ext', pages: [pages(1)[0]!, bad, { ...pages(1)[0]!, pageIndex: 2 }], detectRunning: false });
    expect(out.map((m) => m.type)).toEqual(['result', 'error', 'result', 'done']);
    expect(out[1]).toMatchObject({ type: 'error', requestId: 'r2', pageIndex: 1 });
  });

  it('honours cancellation between pages', async () => {
    const out: ExtractWorkerResponse[] = [];
    const core = createWorkerCore((m) => out.push(m));
    const run = core.handle({ type: 'extract', requestId: 'r3', extractionId: 'ext', pages: pages(20) });
    await core.handle({ type: 'cancel', requestId: 'r3' });
    await run;
    expect(out.filter((m) => m.type === 'result').length).toBeLessThan(20);
    expect(out.at(-1)).toEqual({ type: 'done', requestId: 'r3', cancelled: true });
  });

  it('client correlates responses through a Worker-like transport', async () => {
    const listeners: ((ev: MessageEvent<ExtractWorkerResponse>) => void)[] = [];
    const core = createWorkerCore((m) => listeners.forEach((l) => l({ data: structuredClone(m) } as MessageEvent<ExtractWorkerResponse>)));
    const fake: WorkerLike = {
      postMessage: (msg: ExtractWorkerRequest) => void core.handle(structuredClone(msg)),
      addEventListener: (_t, l) => listeners.push(l),
      terminate: () => undefined,
    };
    const client = new PdfExtractClient(fake);
    const seen: number[] = [];
    let updates = 0;
    const job = client.extract('ext', pages(3), { onPage: (pi, blocks) => seen.push(pi + blocks.length * 0), onRunning: (u) => (updates = u.length) });
    expect(await job.done).toEqual({ cancelled: false });
    expect(seen).toEqual([0, 1, 2]);
    expect(updates).toBeGreaterThan(0);
    client.dispose();
  });
});
