import type { Block } from '../../ipc/generated/Block';

/** The subset of a PDF.js `TextItem` the extractor needs (structured-clone friendly). */
export interface PdfTextItemLike {
  str: string;
  /** 'ltr' | 'rtl' | 'ttb' */
  dir: string;
  /** [a, b, c, d, e, f] in PDF user space (the page's coordinate system, unscaled). */
  transform: number[];
  /** Advance along the baseline, user space units. */
  width: number;
  height: number;
  fontName: string;
  hasEOL: boolean;
  /** Fill colour the item was painted with (`#rrggbb`), when it could be determined. */
  color?: string | null;
  /** Colour per stretch of `str` (UTF-16 offsets): one item often mixes colours, because
   *  text extraction merges neighbouring glyph runs whatever their colour. */
  colorRuns?: { start: number; end: number; color: string }[];
}

/** PDF.js `TextStyle` (+ optional real font facts resolved from `commonObjs`). */
export interface PdfTextStyleLike {
  fontFamily: string;
  ascent?: number | null;
  descent?: number | null;
  vertical?: boolean;
  /** Real PostScript font name when known (e.g. "ABCDEF+Times-Bold"). */
  fontName?: string | null;
  isMonospace?: boolean | null;
}

/** Image placement found in the operator list, in PDF user space. */
export interface PdfImageRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** Optional PDF.js struct tree (tagged PDF). Only used as a diagnostic hint today. */
export interface PdfStructNodeLike {
  role?: string;
  children?: (PdfStructNodeLike | { type: string; id?: string })[];
}

export interface PdfPageInput {
  pageIndex: number;
  /** PDF.js `page.view` / viewport.viewBox: [x0, y0, x1, y1] in user space (crop ∩ media). */
  viewBox: number[];
  /** Page /Rotate normalised to 0 | 90 | 180 | 270. */
  rotation: number;
  items: PdfTextItemLike[];
  styles: Record<string, PdfTextStyleLike>;
  /** Number of image painting operators on the page (scan detection). */
  imageCount: number;
  /** Text painted with render mode 3 (invisible) — the signature of an OCR layer over a scan. */
  invisibleText?: boolean;
  /** Image placements, when the caller could compute them. */
  imageRects?: PdfImageRect[];
  structTree?: PdfStructNodeLike | null;
}

export type QualityReason =
  | 'unresolved_columns'
  | 'missing_unicode'
  | 'overlapping_text'
  | 'likely_scan'
  | 'rotated_text'
  | 'low_text_density';

export interface PdfPageDiagnostics {
  pageIndex: number;
  /** Page-level reasons (subset of QualityReason). */
  reasons: QualityReason[];
  textItemCount: number;
  /** Items dropped because they lie completely outside the view box. */
  clippedItemCount: number;
  /** Characters that look like unmapped glyphs (U+FFFD, C0/C1 controls, PUA, empty-with-width). */
  missingUnicodeChars: number;
  totalChars: number;
  imageCount: number;
  /** Fraction of the page area covered by images (0..1), when image rects are known. */
  imageCoverage: number;
  columnCount: number;
  /** Text orientation (degrees, user space) of the dominant text group. */
  dominantTextAngle: number;
  blockCount: number;
  translatableBlockCount: number;
  tagged: boolean;
}

export interface PdfPageResult {
  pageIndex: number;
  blocks: Block[];
  diagnostics: PdfPageDiagnostics;
}

export interface ExtractDeps {
  sha256Hex: (text: string) => Promise<string>;
}

// ---------------------------------------------------------------------------
// Worker protocol
// ---------------------------------------------------------------------------

export interface ExtractRequest {
  type: 'extract';
  requestId: string;
  extractionId: string;
  pages: PdfPageInput[];
  /**
   * Run the cross-page running header/footer pass over this batch after all
   * pages are extracted (default true). The caller can also run
   * `detectRunningElements` itself over results accumulated across batches.
   */
  detectRunning?: boolean;
}

export interface CancelRequest {
  type: 'cancel';
  requestId: string;
}

export type ExtractWorkerRequest = ExtractRequest | CancelRequest;

export interface ExtractPageResultMessage {
  type: 'result';
  requestId: string;
  pageIndex: number;
  blocks: Block[];
  diagnostics: PdfPageDiagnostics;
}

export interface RunningElementUpdate {
  pageIndex: number;
  blockId: string;
  role: 'running_header' | 'running_footer';
  translationPolicy: 'preserve';
}

export interface ExtractRunningMessage {
  type: 'running';
  requestId: string;
  updates: RunningElementUpdate[];
}

export interface ExtractDoneMessage {
  type: 'done';
  requestId: string;
  cancelled: boolean;
}

export interface ExtractErrorMessage {
  type: 'error';
  requestId: string;
  /** Page that failed, or null when the whole request failed. Other pages still complete. */
  pageIndex: number | null;
  message: string;
}

export type ExtractWorkerResponse =
  | ExtractPageResultMessage
  | ExtractRunningMessage
  | ExtractDoneMessage
  | ExtractErrorMessage;
