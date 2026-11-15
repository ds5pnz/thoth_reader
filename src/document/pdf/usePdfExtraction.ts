// Drives progressive PDF extraction (spec §2.2): restore page and first pages first,
// then the rest at low priority, in small batches; each page is staged in ≤100-block /
// ≤1MiB batches and published atomically with extraction_finalize_section.
import { readPdfOutline } from './outline';
import { useEffect, useRef } from 'react';
import { call } from '../../ipc/client';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import type { ExtractionBeginResult } from '../../ipc/generated/ExtractionBeginResult';
import type { Block } from '../../ipc/generated/Block';
import { openSessionPdf, releaseSessionPdf } from './sessionPdf';
import { buildPageInput } from './pageInput';
import { PdfExtractClient } from './extractClient';
import { canonicalJson } from '../../domain/canonicalJson';
import type { PdfPageInput, RunningElementUpdate } from './types';

export const PDF_EXTRACTOR_VERSION = 'pdf-extractor/6';
const BATCH_PAGES = 6;
const MAX_BATCH_BLOCKS = 100;
const MAX_BATCH_BYTES = 900 * 1024;

async function sha256Hex(s: string): Promise<string> {
  const d = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(s));
  return [...new Uint8Array(d)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

export async function blocksChecksum(blocks: Block[]): Promise<string> {
  return sha256Hex(canonicalJson(blocks.map((b) => [b.id, b.sourceHash])));
}

function applyRunning(blocks: Block[], ups: RunningElementUpdate[]) {
  const byId = new Map(ups.map((u) => [u.blockId, u]));
  for (const b of blocks) {
    const u = byId.get(b.id);
    if (u) {
      if (!b.roles.includes(u.role)) b.roles.push(u.role);
      b.translationPolicy = 'preserve';
    }
  }
}

async function commitPage(session: ReaderSessionDto, token: string, sectionId: string, blocks: Block[]) {
  let batch: Block[] = [];
  let bytes = 0;
  let index = 0;
  const flush = async () => {
    if (!batch.length) return;
    await call('blocks_commit_batch', { sessionId: session.sessionId, extractionToken: token, sectionId, batchIndex: index++, blocks: batch });
    batch = [];
    bytes = 0;
  };
  for (const b of blocks) {
    const size = new TextEncoder().encode(JSON.stringify(b)).length;
    if (batch.length >= MAX_BATCH_BLOCKS || bytes + size > MAX_BATCH_BYTES) await flush();
    batch.push(b);
    bytes += size;
  }
  await flush();
  await call('extraction_finalize_section', { sessionId: session.sessionId, extractionToken: token, sectionId, expectedCount: blocks.length, checksum: await blocksChecksum(blocks), failed: false });
}

/** Page the reader is currently looking at, per session (set by the PDF view). */
const extractionFocus = new Map<string, number>();
export function setExtractionFocus(sessionId: string, pageIndex: number) {
  extractionFocus.set(sessionId, pageIndex);
}

export function usePdfExtraction(session: ReaderSessionDto | null, refreshSections: () => Promise<unknown>, onSession: (s: ReaderSessionDto) => void) {
  const started = useRef<string | null>(null);
  const latest = useRef({ session, refreshSections, onSession });
  latest.current = { session, refreshSections, onSession };
  const sessionId = session?.sessionId ?? null;
  useEffect(() => {
    const { session, refreshSections } = latest.current;
    if (!session || started.current === session.sessionId) return;
    started.current = session.sessionId;
    let cancelled = false;
    const client = new PdfExtractClient();
    void (async () => {
      const doc = await openSessionPdf(session);
      try {
        const meta = await doc.getMetadata().catch(() => null);
        const title = (meta?.info as { Title?: string } | undefined)?.Title ?? null;
        const begin = await call<ExtractionBeginResult>('extraction_begin', {
          sessionId: session.sessionId,
          extractorVersions: { pdfExtractor: PDF_EXTRACTOR_VERSION, pdfjs: '6.5.438' },
          pageCount: doc.numPages,
          title,
        });
        if (cancelled) return;
        // Merge into the live session: its generation may have moved while the PDF opened.
        latest.current.onSession({ ...(latest.current.session ?? session), extractionId: begin.extractionId });
        // The book's own table of contents, when it has one (the sidebar lists pages otherwise).
        void readPdfOutline(doc).then((toc) => {
          const cur = latest.current.session;
          if (cancelled || !cur || toc.length === 0) return;
          latest.current.onSession({ ...cur, metadata: { ...cur.metadata, toc } });
        });
        await refreshSections();
        const pending = begin.sections.filter((s) => s.state !== 'ready' && s.state !== 'failed').map((s) => ({ id: s.id, page: Number(s.ordinal) }));
        // Restore page first, then pages after it, then the rest.
        const restorePage = session.position?.location?.kind === 'pdf' ? session.position.location.pageIndex : 0;
        pending.sort((a, b) => {
          const ka = a.page >= restorePage ? a.page - restorePage : 1e6 + a.page;
          const kb = b.page >= restorePage ? b.page - restorePage : 1e6 + b.page;
          return ka - kb;
        });
        // Ramp batch size: the restore page is published alone so reflow shows it fast;
        // later batches are larger (running header detection needs ≥3 pages per batch).
        let batchNo = 0;
        let lastFocus = -1;
        while (pending.length > 0 && !cancelled) {
          // Follow the reader: when they jump elsewhere, extract around that page next
          // (forward first), so a random page does not wait for the whole book.
          const focus = extractionFocus.get(session.sessionId) ?? restorePage;
          const jumped = focus !== lastFocus;
          if (jumped) {
            lastFocus = focus;
            pending.sort((a, b) => {
              const ka = a.page >= focus ? a.page - focus : 1e6 + (focus - a.page);
              const kb = b.page >= focus ? b.page - focus : 1e6 + (focus - b.page);
              return ka - kb;
            });
          }
          const size = batchNo === 0 || jumped ? 1 : batchNo === 1 ? 3 : BATCH_PAGES;
          batchNo += 1;
          const chunk = pending.splice(0, size);
          const inputs: PdfPageInput[] = [];
          const results = new Map<number, Block[]>();
          const failed = new Set<number>();
          for (const p of chunk) {
            // One damaged page must not stop the rest of the book from being extracted.
            try {
              const page = await doc.getPage(p.page + 1);
              try {
                inputs.push(await buildPageInput(page));
              } finally {
                page.cleanup();
              }
            } catch {
              failed.add(p.page);
            }
          }
          let running: RunningElementUpdate[] = [];
          const job = client.extract(begin.extractionId, inputs, {
            onPage: (pi, blocks) => results.set(pi, blocks),
            onPageError: (pi) => { if (pi != null) failed.add(pi); },
            onRunning: (u) => { running = running.concat(u); },
          });
          await job.done;
          for (const p of chunk) {
            if (cancelled) break;
            const blocks = results.get(p.page);
            if (!blocks || failed.has(p.page)) {
              await call('extraction_finalize_section', { sessionId: session.sessionId, extractionToken: begin.extractionToken, sectionId: p.id, expectedCount: 0, checksum: '', failed: true }).catch(() => {});
              continue;
            }
            applyRunning(blocks, running);
            await commitPage(session, begin.extractionToken, p.id, blocks).catch(async () => {
              await call('extraction_finalize_section', { sessionId: session.sessionId, extractionToken: begin.extractionToken, sectionId: p.id, expectedCount: 0, checksum: '', failed: true }).catch(() => {});
            });
          }
          // Yield to keep the UI responsive between batches (low priority remainder).
          await new Promise((r) => setTimeout(r, batchNo <= 2 || jumped ? 0 : 30));
        }
      } finally {
        releaseSessionPdf(session);
      }
    })().catch(() => {});
    return () => { cancelled = true; client.dispose(); extractionFocus.delete(session.sessionId); };
  }, [sessionId]);
}
