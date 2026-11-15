// Opens the session's managed PDF through the thoth-doc protocol (Range-capable).
import type { PDFDocumentProxy } from 'pdfjs-dist';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import { loadPdfDocument, type PasswordReason } from './pdfjs';
import { docUrl } from '../../ipc/docUrl';

type PasswordAsker = (reason: PasswordReason) => Promise<string | null>;
let asker: PasswordAsker = async () => null;

/** The reader registers a modal; the password is used for this load only, never stored. */
export function setPasswordAsker(fn: PasswordAsker) {
  asker = fn;
}

type Entry = { p: Promise<{ doc: PDFDocumentProxy; task: { destroy(): Promise<void> } }>; refs: number };
const cache = new Map<string, Entry>();

export function sessionPdfUrl(session: ReaderSessionDto) {
  return docUrl(`${session.assetToken}/pdf`);
}

/** One PDFDocumentProxy per session token shared by the renderer and the extractor. */
export function openSessionPdf(session: ReaderSessionDto): Promise<PDFDocumentProxy> {
  const key = session.assetToken;
  let e = cache.get(key);
  if (!e) {
    const p = loadPdfDocument({ url: sessionPdfUrl(session), onPassword: (r) => asker(r) });
    e = { p, refs: 0 };
    p.catch(() => cache.delete(key));
    cache.set(key, e);
  }
  e.refs += 1;
  return e.p.then((x) => x.doc);
}

/** Release one reference; the worker and network are torn down with the last one. */
export function releaseSessionPdf(session: ReaderSessionDto): void {
  const key = session.assetToken;
  const e = cache.get(key);
  if (!e) return;
  e.refs -= 1;
  if (e.refs <= 0) {
    cache.delete(key);
    void e.p.then((x) => x.task.destroy()).catch(() => {});
  }
}
