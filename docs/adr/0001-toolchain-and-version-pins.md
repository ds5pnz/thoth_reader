# ADR 0001 — Toolchain and version pins

- Status: accepted (M0) · Requirement IDs: §2, FR10
- Date: 2026-11-14

## Decision
| Item | Pinned |
| --- | --- |
| Rust toolchain | 1.99.0 (`src-tauri/rust-toolchain.toml`) |
| Tauri | 2.12.5 (crate), CLI/API 2.12.4, tauri-build 2.7.4 |
| React / React DOM | 19.3.1 |
| TypeScript | 6.0.3 |
| Vite / Vitest | 8.3.8 / 5.1.0 |
| PDF.js (`pdfjs-dist`) | 6.5.438 — API and worker bundled from the same package |
| epub.js | 0.3.93 |
| SQLx | 0.9.0 with `libsqlite3-sys` 0.37.0 `bundled` → SQLite **3.51.3** |
| Exact versions | `package-lock.json`, `src-tauri/Cargo.lock` |

## Context / alternatives
- The machine default Rust (1.93) cannot build SQLx 0.9 (requires ≥1.94). A project-local toolchain file pins 1.99.0 without changing the user's default.
- SQLx 0.9 requires `libsqlite3-sys < 0.38`. 0.37.0 bundles SQLite 3.51.3, exactly the spec minimum (WAL fixes). The app checks `sqlite_version()` and FTS5 at startup and refuses to run on an older build (no implicit fallback to the system SQLite).
- Tauri 3 is alpha; spec mandates the Tauri 2 major.

## Consequences
Upgrading any pinned item requires a new ADR and re-running the M0 checks (CSP, Range, CFI, WKWebView).
