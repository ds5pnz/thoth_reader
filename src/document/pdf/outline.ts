// The PDF's own outline (bookmarks) as sidebar TOC entries. Destinations resolve to page
// indices; entries that do not resolve keep their children but are not clickable targets.
import type { PDFDocumentProxy } from 'pdfjs-dist';

export interface PdfTocEntry { label: string; href: string; spineIndex: number; children: PdfTocEntry[] }

type OutlineNode = { title: string; dest: string | unknown[] | null; items?: OutlineNode[] };

const MAX_ENTRIES = 3000;
const MAX_DEPTH = 8;

export function cleanLabel(s: unknown): string {
  // eslint-disable-next-line no-control-regex
  return String(s ?? '').replace(/[\u0000-\u001f\u007f]+/g, ' ').replace(/\s+/g, ' ').trim().slice(0, 300);
}

export async function pageOf(doc: PDFDocumentProxy, dest: string | unknown[] | null): Promise<number | null> {
  try {
    const explicit = typeof dest === 'string' ? await doc.getDestination(dest) : dest;
    const ref = Array.isArray(explicit) ? explicit[0] : null;
    if (typeof ref === 'number') return Number.isInteger(ref) && ref >= 0 && ref < doc.numPages ? ref : null;
    if (ref && typeof ref === 'object') {
      const i = await doc.getPageIndex(ref as Parameters<PDFDocumentProxy['getPageIndex']>[0]);
      return i >= 0 && i < doc.numPages ? i : null;
    }
  } catch {
    // dangling destination
  }
  return null;
}

export async function readPdfOutline(doc: PDFDocumentProxy): Promise<PdfTocEntry[]> {
  let root: OutlineNode[] | null = null;
  try {
    root = (await doc.getOutline()) as OutlineNode[] | null;
  } catch {
    return [];
  }
  if (!root?.length) return [];
  let budget = MAX_ENTRIES;
  const walk = async (nodes: OutlineNode[], depth: number, inherited: number): Promise<PdfTocEntry[]> => {
    const out: PdfTocEntry[] = [];
    let last = inherited;
    for (const n of nodes) {
      if (budget <= 0) break;
      budget -= 1;
      const label = cleanLabel(n.title);
      // An entry without its own target points where the previous one did (section headers).
      const page = (await pageOf(doc, n.dest)) ?? last;
      last = page;
      const children = depth + 1 < MAX_DEPTH && n.items?.length ? await walk(n.items, depth + 1, page) : [];
      if (label || children.length) out.push({ label: label || `${page + 1}쪽`, href: `page:${page}`, spineIndex: page, children });
    }
    return out;
  };
  return walk(root, 0, 0);
}

/** Page and (when the destination names one) the PDF-space y to show at the top. */
export async function resolveDest(doc: PDFDocumentProxy, dest: unknown): Promise<{ page: number; y: number | null } | null> {
  if (typeof dest !== 'string' && !Array.isArray(dest)) return null;
  try {
    const explicit = typeof dest === 'string' ? await doc.getDestination(dest) : dest;
    if (!Array.isArray(explicit)) return null;
    const page = await pageOf(doc, explicit);
    if (page == null) return null;
    const kind = (explicit[1] as { name?: unknown } | undefined)?.name;
    const y = kind === 'XYZ' ? explicit[3] : kind === 'FitH' || kind === 'FitBH' ? explicit[2] : null;
    return { page, y: typeof y === 'number' && Number.isFinite(y) ? y : null };
  } catch {
    return null;
  }
}
