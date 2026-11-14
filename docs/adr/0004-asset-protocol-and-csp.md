# ADR 0004 — `thoth-doc` asset protocol, CSP and command permissions

- Status: accepted (M0, to be re-verified in WKWebView each release) · Requirement IDs: §5.3, §8.4, AC08
- Date: 2026-11-14

- URL form: `thoth-doc://localhost/<64-hex session asset token>/pdf` or `/epub/<relative path>`. No OS paths.
  Tokens are random per session, revoked on close, and bound to one document and format.
- Handler: GET/HEAD only, single `Range` → 206, invalid → 416, `nosniff`, `no-store`, MIME allow-list by extension,
  `safe_join` (no `..`, no absolute, no symlink, canonical-prefix check). CORS echo only for the app origins.
- XHTML responses carry a document CSP with `script-src 'none'; connect-src 'none'; object-src 'none';
  base-uri 'none'; form-action 'none'`, images/fonts only from the asset origin.
- App CSP: scripts only from the bundle (`'self'`), `worker-src 'self' blob:` for PDF.js, no `unsafe-eval`, no wildcards.
- Capabilities: `build.rs` uses `AppManifest::commands` so app commands require explicit `allow-*` permissions;
  the main window gets those plus `core:event:allow-listen/unlisten` only. No fs/http/shell/sql/dialog/opener
  permissions are granted to the frontend; the file dialog and URL opening happen in Rust commands.
- Navigation: the main webview only allows the bundled origin (and the dev server in debug); new windows are denied.
