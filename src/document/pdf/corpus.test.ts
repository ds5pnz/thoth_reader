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
import { analyzeLayout } from './layout';
import { detectRunningElements, applyRunningUpdates } from './running';
import { sha256Hex } from '../../domain/canonicalJson';
import type { PdfPageResult } from './types';

type Env = { THOTH_CORPUS_ONLY?: string; THOTH_CORPUS?: string; THOTH_CORPUS_PAGES?: string; THOTH_CORPUS_REPORT?: string; THOTH_CORPUS_DEBUG?: string };
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
    const debug: string[] = [];
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
        const splitReasons: Record<string, number> = {};
        let listPages = 0;
        let failedPages = 0;
        const pageErrors: string[] = [];
        for (const p of pages) {
          try {
            const page = await doc.getPage(p + 1);
            const input = await buildPageInput(page);
            results.push(await extractPage(input, 'corpus', { sha256Hex }));
            // Which rule cut a paragraph that reads on (no sentence end before, lower case after).
            // Index, contents and word-list pages are short lines by nature: not a reflow defect.
            const pageLayout = analyzeLayout(input);
            const lens = pageLayout.groups.flatMap((g) => g.lines.map((l) => l.text.trim().length)).sort((x, y) => x - y);
            const listLike = lens.length > 0 && (lens[Math.floor(lens.length / 2)] ?? 0) < 40;
            if (listLike) listPages += 1;
            if (env.THOTH_CORPUS_DEBUG === 'long') {
              for (const g of pageLayout.groups) for (const pp of g.paragraphs) {
                const ls = pp.units.flatMap((u) => u.lines);
                const chars = ls.reduce((n, l) => n + l.text.length, 0);
                if (chars <= 1500 || pp.units[0]!.kind === 'table') continue;
                const ds: string[] = [];
                for (let k = 1; k < ls.length; k++) ds.push(((ls[k - 1]!.base - ls[k]!.base) / ls[k]!.size).toFixed(2));
                debug.push(JSON.stringify({ page: p + 1, lines: ls.length, chars, mono: ls.filter((l) => l.mono).length, lead: Number(((pp.leading ?? 0) / ls[0]!.size).toFixed(2)), x0: ls.slice(0, 30).map((l) => Math.round(l.x0)).join(' '), ds: ds.slice(0, 30).join(' ') }));
              }
            }
            for (const g of listLike ? [] : pageLayout.groups) {
              for (let i = 1; i < g.paragraphs.length; i++) {
                const a = g.paragraphs[i - 1]!;
                const b = g.paragraphs[i]!;
                const la = a.units.at(-1)!.lines.at(-1)!;
                const lb = b.units[0]!.lines[0]!;
                if (a.units[0]!.kind === 'table' || b.units[0]!.kind === 'table') continue;
                if (lb.text.trim().length < 5 || (la.text.trim().length < 25 && la.base - lb.base > 2 * la.size)) continue;
                if (!/^\p{Ll}/u.test(lb.text.trim()) || /[.!?:;…。]["'”’»)\]]*$/.test(la.text.trim())) continue;
                const why = a.flow === b.flow ? (b.startReason ?? '?') : 'new_flow';
                if (env.THOTH_CORPUS_DEBUG === why) {
                  const fl = b.flow.units.filter((u) => u.kind === 'line').map((u) => u.lines[0]!);
                  const ds: number[] = [];
                  for (let k = 1; k < fl.length; k++) ds.push(Number(((fl[k - 1]!.base - fl[k]!.base) / fl[k - 1]!.size).toFixed(2)));
                  const fx = (f: typeof a.flow) => ({ col: f.column, span: f.spanning, unres: f.unresolved, n: f.units.length, x: [Math.round(Math.min(...f.units.map((u) => u.x0))), Math.round(Math.max(...f.units.map((u) => u.x1)))], y: [Math.round(Math.max(...f.units.map((u) => u.top))), Math.round(Math.min(...f.units.map((u) => u.bottom)))] });
                  debug.push(JSON.stringify({ page: p + 1, d: Number(((la.base - lb.base) / la.size).toFixed(2)), sizes: [la.size, lb.size].map((v) => Number(v.toFixed(1))), x: [la.x0, la.x1, lb.x0, lb.x1].map((v) => Math.round(v)), len: [la.text.length, lb.text.length], bold: [la.bold, lb.bold], mono: [la.mono, lb.mono], A: fx(a.flow), B: fx(b.flow), flows: g.flows.length, lead: Number(((b.leading ?? 0) / la.size).toFixed(2)), flowDeltas: ds.slice(0, 12).join(' ') }));
                }
                splitReasons[why] = (splitReasons[why] ?? 0) + 1;
              }
            }
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
        // Reflow defects, counted without keeping any text: a paragraph that stops
        // mid-sentence and is followed by one starting in lower case is almost always one
        // paragraph cut in two.
        let paras = 0;
        let midSplits = 0;
        let shortParas = 0;
        // The opposite defect: paragraphs glued together show up as very long ones.
        let longParas = 0;
        const paraLens: number[] = [];
        let lowerStarts = 0;
        const splitPages: Record<number, number> = {};
        for (const r of results) {
          const body = r.blocks.filter((b) => !b.roles.some((x) => x.startsWith('running_') || x === 'page_number' || x === 'footnote'));
          for (let i = 0; i < body.length; i++) {
            const b = body[i]!;
            if (b.kind !== 'paragraph') continue;
            const t = b.sourceText.trim();
            if (!t) continue;
            paras += 1;
            if (t.length < 25) shortParas += 1;
            if (t.length > 1500) longParas += 1;
            paraLens.push(t.length);
            if (/^\p{Ll}/u.test(t)) lowerStarts += 1;
            const prev = body[i - 1];
            if (prev && prev.kind === 'paragraph' && /^\p{Ll}/u.test(t) && !/[.!?:;…。]["'”’»)\]]*$/.test(prev.sourceText.trim())) {
              midSplits += 1;
              splitPages[r.diagnostics.pageIndex] = (splitPages[r.diagnostics.pageIndex] ?? 0) + 1;
            }
          }
        }
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
          paras,
          midSplits,
          midSplitRate: paras ? Number((midSplits / paras).toFixed(4)) : 0,
          shortParas,
          longParas,
          paraLenP95: paraLens.sort((x, y) => x - y)[Math.floor(paraLens.length * 0.95)] ?? 0,
          paraLenMax: paraLens.at(-1) ?? 0,
          lowerStarts,
          splitReasons,
          bodySplits: Object.values(splitReasons).reduce((x, y) => x + y, 0),
          listPages,
          worstSplitPages: Object.entries(splitPages).sort((a, b) => b[1] - a[1]).slice(0, 5).map(([p, n]) => `${Number(p) + 1}:${n}`),
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
    fs.writeFileSync(out, JSON.stringify({ generatedAt: new Date().toISOString(), maxPagesPerDoc: MAX_PAGES, documents: report, ...(debug.length ? { debug } : {}) }, null, 2));
  }, 60 * 60 * 1000);
});
