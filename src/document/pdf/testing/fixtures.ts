/**
 * Test-only helpers: load the generated PDF fixtures with the pdfjs-dist legacy
 * build in Node (no worker thread: the worker module is installed as
 * `globalThis.pdfjsWorker`, which PDF.js uses as an in-thread "fake worker").
 *
 * Node built-ins are imported through a computed specifier so the app's
 * TypeScript config does not need @types/node.
 */
import type { PDFDocumentProxy } from 'pdfjs-dist';

interface NodeFs {
  readFileSync(path: URL | string): Uint8Array;
}

type PdfjsModule = typeof import('pdfjs-dist');

let pdfjsPromise: Promise<PdfjsModule> | null = null;

export async function loadPdfjsLegacy(): Promise<PdfjsModule> {
  pdfjsPromise ??= (async () => {
    const pdfjs = (await import('pdfjs-dist/legacy/build/pdf.mjs')) as unknown as PdfjsModule;
    const workerSpec = 'pdfjs-dist/legacy/build/pdf.worker.mjs';
    (globalThis as { pdfjsWorker?: unknown }).pdfjsWorker = await import(/* @vite-ignore */ workerSpec);
    return pdfjs;
  })();
  return pdfjsPromise;
}

const fixtureRoot = new URL('../../../../tests/fixtures/pdf/', import.meta.url);
const standardFonts = new URL('../../../../node_modules/pdfjs-dist/standard_fonts/', import.meta.url);

async function fs(): Promise<NodeFs> {
  const spec = 'node:' + 'fs';
  return (await import(/* @vite-ignore */ spec)) as NodeFs;
}

export async function readFixtureBytes(name: string): Promise<Uint8Array> {
  return new Uint8Array((await fs()).readFileSync(new URL(name, fixtureRoot)));
}

export async function readFixtureJson<T>(name: string): Promise<T> {
  const bytes = (await fs()).readFileSync(new URL(name, fixtureRoot));
  return JSON.parse(new TextDecoder().decode(bytes)) as T;
}

export async function openFixture(name: string): Promise<PDFDocumentProxy> {
  const pdfjs = await loadPdfjsLegacy();
  const data = await readFixtureBytes(name);
  const task = pdfjs.getDocument({
    data,
    standardFontDataUrl: decodeURIComponent(standardFonts.pathname),
    fontExtraProperties: true,
    useSystemFonts: false,
    enableXfa: false,
    stopAtErrors: false,
    verbosity: 0,
  });
  return task.promise;
}

export interface ExpectedBlock {
  kind: string;
  roles?: string[];
  policy?: string;
  text: string;
  rect: [number, number, number, number];
  lines: number;
  matchPrefix?: string;
  raw?: boolean;
  rotatedText?: boolean;
}

export interface ExpectedPage {
  pageIndex: number;
  viewBox: number[];
  rotation: number;
  likelyScan?: boolean;
  missingUnicode?: boolean;
  blocks: ExpectedBlock[];
}

export interface ExpectedFixture {
  fixture: string;
  pages: ExpectedPage[];
}
