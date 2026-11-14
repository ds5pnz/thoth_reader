# ADR 0002 — EPUB blocks and canonical CFI are produced in Rust from the sanitized DOM

- Status: accepted (M0) · Requirement IDs: FR01, FR02, §3.3, §5
- Date: 2026-11-14

## Decision
The Rust `epub_security` module validates the ZIP, sanitizes XHTML/CSS/SVG, writes a flat canonical package
(`x/N.xhtml`, `r/<resourceId>`, a regenerated `package.opf`) and, from the *same* canonical DOM, emits blocks with
canonical CFI ranges and per-text-node maps. epub.js only ever loads this sanitized package through `thoth-doc`.

## Why
- Spec §2 says Rust owns compression checks and sanitizing; generating blocks/CFI from the exact canonical
  serialization avoids drift between "what was sanitized" and "what was indexed".
- The canonical XHTML is served as `application/xhtml+xml` so the browser's XML parser produces the same
  node indices used for CFI.
- The TypeScript side maps React selections to CFI with the stored text-node runs (`cfiAtOffset`).

## Alternatives rejected
- Extracting blocks in the webview from epub.js DOM: CFI depends on runtime DOM and reflow; harder to test headless.

## Consequences
Any sanitizer rule change bumps `canonicalRevision` and therefore the extractionId (new blocks; old history retained).
