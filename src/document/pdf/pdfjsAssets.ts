/**
 * Bundled PDF.js assets, resolved through Vite so nothing is ever fetched from
 * a remote origin (spec §4.2): the worker script, Adobe CMaps, the standard
 * (Foxit/Liberation) font files and the image-decoder wasm modules.
 *
 * Vite fingerprints asset file names, so a "base directory URL" cannot be
 * handed to PDF.js. Instead every file is imported with `?url` (eager glob →
 * map of file name → emitted URL) and served to PDF.js through a custom
 * `BinaryDataFactory` (main thread fetch, `useWorkerFetch: false`).
 *
 * Not bundled on purpose: `quickjs-eval.*` (PDF scripting sandbox — scripting is
 * never enabled), the `*_nowasm_fallback.js` decoders (would need dynamic
 * `import()` from a directory URL) and the CMYK ICC profile (PDF.js loads it with
 * a synchronous XHR from `iccUrl`; without it CMYK falls back to the built-in
 * conversion).
 */
import workerUrl from 'pdfjs-dist/build/pdf.worker.mjs?url&no-inline';

type UrlMap = Record<string, string>;

const cmapGlob = import.meta.glob('../../../node_modules/pdfjs-dist/cmaps/*.bcmap', { query: '?url&no-inline', import: 'default', eager: true }) as UrlMap;
const fontGlob = import.meta.glob('../../../node_modules/pdfjs-dist/standard_fonts/*.{pfb,ttf}', { query: '?url&no-inline', import: 'default', eager: true }) as UrlMap;
const wasmGlob = import.meta.glob('../../../node_modules/pdfjs-dist/wasm/{openjpeg,jbig2}.wasm', { query: '?url&no-inline', import: 'default', eager: true }) as UrlMap;

function byFileName(glob: UrlMap): Map<string, string> {
  const m = new Map<string, string>();
  for (const [path, url] of Object.entries(glob)) {
    const name = path.slice(path.lastIndexOf('/') + 1);
    m.set(name, url);
  }
  return m;
}

export type BinaryKind = 'cMapUrl' | 'standardFontDataUrl' | 'wasmUrl';

export const PDFJS_WORKER_URL: string = workerUrl;

const tables: Record<BinaryKind, Map<string, string>> = {
  cMapUrl: byFileName(cmapGlob),
  standardFontDataUrl: byFileName(fontGlob),
  wasmUrl: byFileName(wasmGlob),
};

/** URL of a bundled PDF.js data file, or undefined if it is not bundled. */
export function bundledAssetUrl(kind: BinaryKind, filename: string): string | undefined {
  return tables[kind].get(filename);
}

export function bundledAssetCounts(): Record<BinaryKind, number> {
  return { cMapUrl: tables.cMapUrl.size, standardFontDataUrl: tables.standardFontDataUrl.size, wasmUrl: tables.wasmUrl.size };
}
