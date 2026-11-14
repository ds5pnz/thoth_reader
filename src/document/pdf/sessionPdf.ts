// Opens the session's managed PDF through the thoth-doc protocol (Range-capable).
import type { PDFDocumentProxy } from 'pdfjs-dist';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import { loadPdfDocument, type PasswordReason } from './pdfjs';

type PasswordAsker = (reason: PasswordReason) => Promise<string | null>;
let asker: PasswordAsker = async () => null;

/** The reader registers a modal; the password is used for this load only, never stored. */
export function setPasswordAsker(fn: PasswordAsker) {
  asker = fn;
}

const cache = new Map<string, Promise<PDFDocumentProxy>>();

export function sessionPdfUrl(session: ReaderSessionDto) {
  return `thoth-doc://localhost/${session.assetToken}/pdf`;
}

/** One PDFDocumentProxy per session token shared by the renderer and the extractor. */
export function openSessionPdf(session: ReaderSessionDto): Promise<PDFDocumentProxy> {
  const key = session.assetToken;
  let p = cache.get(key);
  if (!p) {
    p = loadPdfDocument({ url: sessionPdfUrl(session), onPassword: (r) => asker(r) }).then((x) => x.doc);
    p.catch(() => cache.delete(key));
    cache.set(key, p);
  }
  return p.then((d) => wrap(d, key));
}

// destroy() from one consumer must not kill the shared document; real destroy on release.
const refs = new Map<string, number>();
function wrap(d: PDFDocumentProxy, key: string): PDFDocumentProxy {
  refs.set(key, (refs.get(key) ?? 0) + 1);
  return new Proxy(d, {
    get(target, prop, recv) {
      if (prop === 'destroy') {
        return async () => {
          const n = (refs.get(key) ?? 1) - 1;
          refs.set(key, n);
          if (n <= 0) {
            refs.delete(key);
            cache.delete(key);
            await target.destroy();
          }
        };
      }
      const v = Reflect.get(target, prop, recv);
      return typeof v === 'function' ? v.bind(target) : v;
    },
  });
}
