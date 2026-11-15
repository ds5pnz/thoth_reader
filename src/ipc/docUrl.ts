// Addresses of the app's private document protocol.
//
// A custom protocol is `scheme://localhost/…` on macOS and Linux, but WebView2 (Windows)
// and the Android web view only know http(s), so there Tauri serves the same protocol as
// `http://scheme.localhost/…`. Everything that builds a document URL goes through here.

/** Whether this web view addresses custom protocols as `http://<scheme>.localhost`. */
export function usesHttpProtocolForm(userAgent: string): boolean {
  return /Windows|Android/i.test(userAgent);
}

export function docOrigin(userAgent: string = typeof navigator === 'undefined' ? '' : navigator.userAgent): string {
  return usesHttpProtocolForm(userAgent) ? 'http://thoth-doc.localhost' : 'thoth-doc://localhost';
}

/** `docUrl('<token>/pdf')` → the URL of that document resource on this platform. */
export function docUrl(path: string): string {
  return `${docOrigin()}/${path.replace(/^\/+/, '')}`;
}
