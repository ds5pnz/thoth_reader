/**
 * Safe PDF.js loader (spec §4.2, §8.4).
 *
 *  - bundled worker of the same pdfjs-dist version (`GlobalWorkerOptions.workerSrc`),
 *  - no remote CMaps / fonts / wasm: bundled assets via a custom BinaryDataFactory,
 *  - no XFA, no scripting (`enableScripting` is never set; PDF actions are inert),
 *  - `isEvalSupported: false` (ignored by pdfjs-dist ≥5, which removed eval-based
 *    font compilation; kept so a downgrade cannot silently re-enable it),
 *  - `stopAtErrors: false`: damaged pages degrade instead of failing the document,
 *  - range loading tuned for the `thoth-doc` custom protocol,
 *  - password via a session-only callback; the password is never stored.
 */
import type { PDFDocumentLoadingTask, PDFDocumentProxy, PDFPageProxy } from 'pdfjs-dist';
import type { DocumentInitParameters } from 'pdfjs-dist/types/src/display/api';

export type PasswordReason = 'need_password' | 'incorrect_password';

export interface PdfAssetResolver {
  workerSrc: string;
  resolve(kind: 'cMapUrl' | 'standardFontDataUrl' | 'wasmUrl', filename: string): string | undefined;
}

export interface LoadPdfOptions {
  /** e.g. `thoth-doc://localhost/<sessionToken>/<documentId>`; must support HTTP Range. */
  url: string;
  /**
   * Called when the PDF is encrypted. Resolve with the password, or null to
   * abort. Called again with 'incorrect_password' after a wrong attempt. The
   * password only lives for the duration of this call.
   */
  onPassword?: (reason: PasswordReason) => Promise<string | null>;
  onProgress?: (loaded: number, total: number) => void;
  /**
   * Keep real font names / monospace flags on `page.commonObjs` (needed for
   * bold / code detection in pageInput.ts). Costs some memory. Default true.
   */
  fontExtraProperties?: boolean;
  /** Override bundled asset resolution (tests). */
  assets?: PdfAssetResolver;
}

export class PdfPasswordCancelledError extends Error {
  constructor() {
    super('PDF password entry cancelled');
    this.name = 'PdfPasswordCancelledError';
  }
}

/** Sentinel base URLs: PDF.js requires non-null, slash-terminated values; the factory ignores them. */
const SENTINEL = {
  cMapUrl: 'thoth-bundled:/pdfjs/cmaps/',
  standardFontDataUrl: 'thoth-bundled:/pdfjs/standard_fonts/',
  wasmUrl: 'thoth-bundled:/pdfjs/wasm/',
} as const;

/** BinaryDataFactory that only serves bundled files (shape expected by PDF.js `BinaryDataFactory`). */
export function makeBundledBinaryDataFactory(assets: PdfAssetResolver) {
  return class BundledBinaryDataFactory {
    // PDF.js constructs the factory with the (sentinel) base URLs; they are not used.
    constructor(_opts?: unknown) {
      void _opts;
    }
    async fetch({ kind, filename }: { kind: string; filename: string }): Promise<Uint8Array> {
      if (kind !== 'cMapUrl' && kind !== 'standardFontDataUrl' && kind !== 'wasmUrl') {
        throw new Error(`unsupported PDF.js data kind: ${kind}`);
      }
      if (!/^[A-Za-z0-9._-]+$/.test(filename)) throw new Error('invalid PDF.js data file name');
      const url = assets.resolve(kind, filename);
      if (!url) throw new Error(`PDF.js data file not bundled: ${kind}/${filename}`);
      const res = await fetch(url, { credentials: 'same-origin' });
      if (!res.ok) throw new Error(`failed to load bundled PDF.js data ${filename}: ${res.status}`);
      return new Uint8Array(await res.arrayBuffer());
    }
  };
}

/** Pure: the `getDocument` parameters used by the app (unit-tested). */
export function buildDocumentParams(url: string, assets: PdfAssetResolver, opts: Pick<LoadPdfOptions, 'fontExtraProperties'> = {}): DocumentInitParameters {
  const params: DocumentInitParameters & { isEvalSupported?: boolean; enableScripting?: boolean } = {
    url,
    // Range loading through the custom protocol.
    disableAutoFetch: true,
    disableStream: false,
    disableRange: false,
    rangeChunkSize: 65536,
    withCredentials: false,
    // Bundled data only.
    cMapUrl: SENTINEL.cMapUrl,
    cMapPacked: true,
    standardFontDataUrl: SENTINEL.standardFontDataUrl,
    wasmUrl: SENTINEL.wasmUrl,
    useWorkerFetch: false,
    useSystemFonts: false,
    BinaryDataFactory: makeBundledBinaryDataFactory(assets),
    // Hardening.
    isEvalSupported: false,
    enableXfa: false,
    stopAtErrors: false,
    fontExtraProperties: opts.fontExtraProperties ?? true,
    // Images larger than 64 MPx are not decoded (memory bound, spec §10.3).
    maxImageSize: 64 * 1024 * 1024,
    pdfBug: false,
  };
  return params;
}

let workerConfigured = false;

/** Open a PDF from a URL with the hardened configuration. */
export async function loadPdfDocument(opts: LoadPdfOptions): Promise<{ doc: PDFDocumentProxy; task: PDFDocumentLoadingTask }> {
  const pdfjs = await import('pdfjs-dist');
  const assets = opts.assets ?? (await defaultAssets());
  if (!workerConfigured || pdfjs.GlobalWorkerOptions.workerSrc !== assets.workerSrc) {
    pdfjs.GlobalWorkerOptions.workerSrc = assets.workerSrc;
    workerConfigured = true;
  }
  const task = pdfjs.getDocument(buildDocumentParams(opts.url, assets, opts));
  if (opts.onProgress) {
    const cb = opts.onProgress;
    task.onProgress = ({ loaded, total }: { loaded: number; total: number }) => cb(loaded, total);
  }
  const onPassword = opts.onPassword;
  if (onPassword) {
    task.onPassword = (update: (pw: string | Error) => void, code: number) => {
      const reason: PasswordReason = code === pdfjs.PasswordResponses.INCORRECT_PASSWORD ? 'incorrect_password' : 'need_password';
      onPassword(reason).then(
        (pw) => update(pw === null ? new PdfPasswordCancelledError() : pw),
        (err: unknown) => update(err instanceof Error ? err : new PdfPasswordCancelledError()),
      );
    };
  }
  const doc = await task.promise;
  return { doc, task };
}

async function defaultAssets(): Promise<PdfAssetResolver> {
  const m = await import('./pdfjsAssets');
  return { workerSrc: m.PDFJS_WORKER_URL, resolve: m.bundledAssetUrl };
}

// ---------------------------------------------------------------------------
// Links (data only; opened by the app on an explicit user click)
// ---------------------------------------------------------------------------

/** Returns the URL if it is an absolute http(s) URL without credentials, else null. */
export function safeLinkUrl(url: unknown): string | null {
  if (typeof url !== 'string' || url.length === 0 || url.length > 2048) return null;
  let u: URL;
  try {
    u = new URL(url);
  } catch {
    return null;
  }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') return null;
  if (u.username || u.password) return null;
  return u.href;
}

export interface PdfLink {
  /** Annotation rect in PDF user space [x1, y1, x2, y2]. */
  rect: [number, number, number, number];
  /** Validated external URL, or null for internal destinations. */
  url: string | null;
  /** Internal destination (named or explicit), passed through as data. */
  dest: unknown;
}

interface AnnotationLike {
  subtype?: unknown;
  url?: unknown;
  dest?: unknown;
  rect?: unknown;
}

/** Link annotations of a page as inert data. JavaScript / launch / file actions are dropped. */
export async function getPageLinks(page: PDFPageProxy): Promise<PdfLink[]> {
  const anns = (await page.getAnnotations({ intent: 'display' })) as AnnotationLike[];
  const out: PdfLink[] = [];
  for (const a of anns) {
    if (a.subtype !== 'Link' || !Array.isArray(a.rect) || a.rect.length !== 4) continue;
    const url = safeLinkUrl(a.url);
    const dest = a.dest ?? null;
    if (!url && dest === null) continue;
    out.push({ rect: a.rect.map(Number) as [number, number, number, number], url, dest });
  }
  return out;
}
