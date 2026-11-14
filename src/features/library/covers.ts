// Library covers. EPUB covers are extracted by Rust at import; PDF covers are page 1 rendered
// here once (no reader session) and stored as a small JPEG next to the library database.
import { call } from '../../ipc/client';
import type { DocumentRow } from '../../ipc/generated/DocumentRow';
import type { PDFPageProxy } from 'pdfjs-dist';

export const LIBRARY_META_VERSION = 1;
const THUMB_WIDTH = 420;

export function coverUrl(d: DocumentRow): string | null {
  if (d.metadata?.hasCover !== true) return null;
  return `thoth-doc://localhost/cover/${d.id}?v=${Number(d.metadata?.coverVersion ?? 0)}`;
}

export function needsPdfCover(d: DocumentRow): boolean {
  return d.format === 'pdf' && d.state !== 'importing' && d.metadata?.libraryMeta !== LIBRARY_META_VERSION;
}

export function awaitingEpubMeta(d: DocumentRow): boolean {
  return d.format === 'epub' && d.state !== 'failed' && d.metadata?.libraryMeta !== LIBRARY_META_VERSION;
}

/** "A; B" / "A and B" style author strings from PDF Info → separate names. */
export function splitAuthors(raw: unknown): string[] {
  if (typeof raw !== 'string') return [];
  return raw.split(/\s*(?:;|&|\band\b)\s*/i).map((s) => s.trim()).filter(Boolean).slice(0, 8);
}

function blobToBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onerror = () => reject(r.error);
    r.onload = () => resolve(String(r.result).replace(/^data:[^,]*,/, ''));
    r.readAsDataURL(blob);
  });
}

/** Render page 1 as the cover and read Title/Author. Always records the attempt. */
async function makePdfCover(documentId: string): Promise<Record<string, unknown>> {
  const { token } = await call<{ token: string }>('library_thumb_begin', { documentId });
  let imageBase64: string | null = null;
  let title: string | null = null;
  let authors: string[] = [];
  let destroy: (() => Promise<void>) | null = null;
  try {
    const { loadPdfDocument } = await import('../../document/pdf/pdfjs');
    // Encrypted PDFs get a generic cover: the library never asks for a password.
    const { doc, task } = await loadPdfDocument({ url: `thoth-doc://localhost/${token}/pdf`, onPassword: async () => null, fontExtraProperties: false });
    destroy = () => task.destroy();
    const info = (await doc.getMetadata().catch(() => null))?.info as { Title?: unknown; Author?: unknown } | undefined;
    if (typeof info?.Title === 'string' && info.Title.trim()) title = info.Title.trim();
    authors = splitAuthors(info?.Author);
    const page = await doc.getPage(1);
    const base = page.getViewport({ scale: 1 });
    const vp = page.getViewport({ scale: THUMB_WIDTH / base.width });
    const canvas = document.createElement('canvas');
    canvas.width = Math.floor(vp.width);
    canvas.height = Math.floor(vp.height);
    const ctx = canvas.getContext('2d');
    if (ctx) {
      ctx.fillStyle = '#fff';
      ctx.fillRect(0, 0, canvas.width, canvas.height);
      await page.render({ canvasContext: ctx, viewport: vp, canvas } as Parameters<PDFPageProxy['render']>[0]).promise;
      const blob = await new Promise<Blob | null>((res) => canvas.toBlob(res, 'image/jpeg', 0.85));
      if (blob) imageBase64 = await blobToBase64(blob);
    }
    canvas.width = 0;
    canvas.height = 0;
  } catch {
    // damaged / encrypted: keep the generic cover
  } finally {
    await destroy?.().catch(() => {});
  }
  const r = await call<{ hasCover: boolean }>('library_cover_set', { documentId, token, imageBase64, title, authors });
  const patch: Record<string, unknown> = { hasCover: r.hasCover, coverVersion: Date.now(), libraryMeta: LIBRARY_META_VERSION, authors };
  if (title) patch.pdfTitle = title;
  return patch;
}

const queued = new Set<string>();
let chain: Promise<void> = Promise.resolve();

/** One PDF at a time; `onDone` receives the metadata patch to apply to the listed row. */
export function queuePdfCover(documentId: string, onDone: (id: string, patch: Record<string, unknown>) => void): void {
  if (queued.has(documentId)) return;
  queued.add(documentId);
  chain = chain.then(async () => {
    try {
      onDone(documentId, await makePdfCover(documentId));
    } catch {
      queued.delete(documentId); // e.g. read-only database: try again on a later visit
    }
  });
}

// ---- display helpers

export function displayTitle(d: DocumentRow): string {
  return (d.metadata?.title as string) || (d.metadata?.pdfTitle as string) || d.title;
}

export function displayAuthors(d: DocumentRow): string {
  const a = d.metadata?.authors;
  return Array.isArray(a) ? a.filter((x): x is string => typeof x === 'string' && !!x).join(', ') : '';
}

/** "Publisher · 2021" from whatever the book declares. */
export function displayImprint(d: DocumentRow): string {
  const pub = typeof d.metadata?.publisher === 'string' ? d.metadata.publisher : '';
  const year = typeof d.metadata?.published === 'string' ? (/\d{4}/.exec(d.metadata.published)?.[0] ?? '') : '';
  return [pub, year].filter(Boolean).join(' · ');
}

export type SortKey = 'recent' | 'title';

export function sortDocuments(docs: DocumentRow[], key: SortKey): DocumentRow[] {
  const out = [...docs];
  if (key === 'title') {
    const c = new Intl.Collator(['ko', 'en'], { numeric: true, sensitivity: 'base' });
    out.sort((a, b) => c.compare(displayTitle(a), displayTitle(b)) || a.id.localeCompare(b.id));
  } else {
    // Read books by last read time; never-opened books after them, newest import first.
    out.sort((a, b) => {
      const ao = a.lastOpenedAt ?? 0;
      const bo = b.lastOpenedAt ?? 0;
      if (ao !== bo) return bo - ao;
      return Number(b.createdAt) - Number(a.createdAt) || a.id.localeCompare(b.id);
    });
  }
  return out;
}

/** Stable generic-cover hue from the title. */
export function coverHue(s: string): number {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) >>> 0;
  return h % 360;
}

export function relativeDay(ms: number, now = Date.now()): string {
  const d = Math.floor((new Date(now).setHours(0, 0, 0, 0) - new Date(ms).setHours(0, 0, 0, 0)) / 86_400_000);
  if (d <= 0) return '오늘';
  if (d === 1) return '어제';
  if (d < 7) return `${d}일 전`;
  if (d < 31) return `${Math.floor(d / 7)}주 전`;
  return new Date(ms).toLocaleDateString('ko-KR');
}
