# ADR 0005 — PDF side-by-side keeps the page layout (translated page)

- Status: accepted (user request, 2026-11-14) · Requirement IDs: FR02, FR03, spec §1.1
- Supersedes for PDF original + side-by-side: "왼쪽 페이지와 오른쪽 대응 번역 블록" (a block list).

## Decision
In PDF original view, side-by-side shows a **translated page** next to each original page:
a copy of the page raster (figures, table rules, backgrounds) with every translated text
block drawn over that block's own region, font size fitted to the region. Both pages live in
one scroll container, so they are always aligned without scroll synchronisation. Pages are
scaled to fit the pane width (one page in original-only mode, two otherwise).

## Why
The block list lost the page structure: table cells became a long list of unrelated rows and
figures disappeared. Users read technical PDFs by layout.

## What stays true
- The original page (left) is never modified; the translated page is a separate overlay view.
  Nothing is written back into the PDF and no translated PDF is exported (FR11 unchanged).
- Blocks without a translation, and preserve-policy blocks (code, formulas), keep showing the
  original content. Scan pages show the original raster.
- Translation data, cache keys, revisions and anchors are unchanged; positions still come from
  the stored PDF-space rects through the PDF.js viewport.
- Retranslate / edit / history / exclude are reached by clicking a block (popover with the same
  translation cell as other modes). Interleaved mode still switches to reflow.

## Trade-offs
- A translation longer than its region is shrunk (minimum 4.5px) and may become hard to read;
  the full text is in the popover and the tooltip.
- The overlay background is sampled from the raster around the block; busy backgrounds
  (photos, gradients) can show a visible patch.
- Two canvases per live page (≤5 rows) roughly double raster memory in this mode.
