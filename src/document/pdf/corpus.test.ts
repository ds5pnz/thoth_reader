// @vitest-environment node
/**
 * Local corpus harness (not a fixture test). Runs only when THOTH_CORPUS points at a folder
 * of the user's own PDFs. Documents are read in place and never copied into the repo; the
 * report contains statistics only (no source text).
 *
 *   THOTH_CORPUS=/path/to/books npx vitest run src/document/pdf/corpus.test.ts
 */
import { describe, it } from 'vitest';
import { loadPdfjsLegacy } from './testing/fixtures';
import { buildPageInput } from './pageInput';
import { extractPage } from './extract';
import { detectRunningElements, applyRunningUpdates } from './running';
import { sha256Hex } from '../../domain/canonicalJson';
import type { PdfPageResult } from './types';

type Env = { THOTH_CORPUS_ONLY?: string; THOTH_CORPUS?: string; THOTH_CORPUS_PAGES?: string; THOTH_CORPUS_REPORT?: string };
const env = ((globalThis as { process?: { env: Env } }).process?.env ?? {}) as Env;
const ROOT = env.THOTH_CORPUS;
const MAX_PAGES = Number(env.THOTH_CORPUS_PAGES ?? 40);

interface NodeFs {
  readdirSync(p: string): string[];
  readFileSync(p: string): Uint8Array;
  writeFileSync(p: string, d: string): void;
  mkdirSync(p: string, o: { recursive: boolean }): void;
  statSync(p: string): { size: number };
}

async function nodeFs(): Promise<NodeFs> {
  return (await import(/* @vite-ignore */ 'node:' + 'fs')) as NodeFs;
}

/** Spread the sample across the document: first pages + evenly spaced pages. */
export function samplePages(n: number, max: number): number[] {
  if (n <= max) return Array.from({ length: n }, (_, i) => i);
  const head = Math.min(8, max);
  const out = new Set<number>(Array.from({ length: head }, (_, i) => i));
  const rest = max - head;
  for (let k = 0; k < rest; k++) out.add(Math.floor(head + ((n - head) * k) / rest));
  return [...out].sort((a, b) => a - b);
}

describe.skipIf(!ROOT)('local PDF corpus', () => {
  it('extracts a page sample of every PDF and writes a stats-only report', async () => {
    const fs = await nodeFs();
    const pdfjs = await loadPdfjsLegacy();
    const files = fs.readdirSync(ROOT!).filter((f) => f.toLowerCase().endsWith('.pdf')).sort();
    const only = env.THOTH_CORPUS_ONLY ? Number(env.THOTH_CORPUS_ONLY) : null;
    const report: Record<string, unknown>[] = [];
    let idx = 0;
    for (const f of files) {
      idx += 1;
      if (only != null && idx !== only) continue;
      const label = `pdf-${String(idx).padStart(2, '0')}`; // file names are not written to the report
      const t0 = performance.now();
      const row: Record<string, unknown> = { label, bytes: fs.statSync(`${ROOT}/${f}`).size };
      try {
        const buf = fs.readFileSync(`${ROOT}/${f}`);
        const data = new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
        const task = pdfjs.getDocument({ data, fontExtraProperties: true, useSystemFonts: false, enableXfa: false, stopAtErrors: false, verbosity: 0 });
        const doc = await task.promise;
        const pages = samplePages(doc.numPages, MAX_PAGES);
        const results: PdfPageResult[] = [];
        let failedPages = 0;
        const pageErrors: string[] = [];
        for (const p of pages) {
          try {
            const page = await doc.getPage(p + 1);
            const input = await buildPageInput(page);
            results.push(await extractPage(input, 'corpus', { sha256Hex }));
            page.cleanup();
          } catch (pe) {
            failedPages += 1;
            if (pageErrors.length < 3) pageErrors.push(`${(pe as Error).name}: ${String((pe as Error).message).slice(0, 160)}`);
          }
        }
        applyRunningUpdates(results, detectRunningElements(results));
        const kinds: Record<string, number> = {};
        const levels: Record<string, number> = {};
        const policies: Record<string, number> = {};
        const reasons: Record<string, number> = {};
        let chars = 0;
        let autoChars = 0;
        let scanPages = 0;
        let emptyPages = 0;
        for (const r of results) {
          if (r.diagnostics.reasons?.includes('likely_scan')) scanPages += 1;
          if (r.blocks.length === 0) emptyPages += 1;
          for (const rs of r.diagnostics.reasons ?? []) reasons[rs] = (reasons[rs] ?? 0) + 1;
          for (const b of r.blocks) {
            kinds[b.kind] = (kinds[b.kind] ?? 0) + 1;
            levels[b.quality.level] = (levels[b.quality.level] ?? 0) + 1;
            policies[b.translationPolicy] = (policies[b.translationPolicy] ?? 0) + 1;
            const isBody = !b.roles.some((x) => x.startsWith('running_') || x === 'page_number');
            if (isBody) {
              chars += b.sourceText.length;
              if (b.translationPolicy === 'auto') autoChars += b.sourceText.length;
            }
          }
        }
        const ms = performance.now() - t0;
        Object.assign(row, {
          pagesTotal: doc.numPages,
          pagesSampled: pages.length,
          failedPages,
          pageErrors,
          scanPages,
          emptyPages,
          blocks: results.reduce((a, r) => a + r.blocks.length, 0),
          kinds,
          levels,
          policies,
          pageReasons: reasons,
          autoCoverage: chars ? Number((autoChars / chars).toFixed(3)) : 0,
          msPerPage: Number((ms / Math.max(1, pages.length)).toFixed(1)),
        });
        await task.destroy();
      } catch (e) {
        row.error = `${(e as Error).name}: ${String((e as Error).message).slice(0, 160)}`;
      }
      report.push(row);
      // eslint-disable-next-line no-console
      console.log(JSON.stringify(row));
    }
    const out = env.THOTH_CORPUS_REPORT ?? 'reports/corpus-pdf.json';
    fs.mkdirSync(out.replace(/\/[^/]*$/, ''), { recursive: true });
    fs.writeFileSync(out, JSON.stringify({ generatedAt: new Date().toISOString(), maxPagesPerDoc: MAX_PAGES, documents: report }, null, 2));
  }, 60 * 60 * 1000);
});
