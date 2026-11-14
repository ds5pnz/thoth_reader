//! EPUB security boundary (spec §5): ZIP/package validation, XHTML/SVG/MathML/CSS
//! sanitization into a canonical, deterministic package that epub.js can open, and
//! block + CFI extraction from the canonical DOM (spec §3.1–3.3).
//!
//! Pipeline: [`build_safe_package`] validates the archive (limits, paths, mimetype,
//! container, OPF, encryption), sanitizes every spine document into canonical XHTML
//! (`x/<spineIndex>.xhtml`), every allowed resource into `r/<resourceId>.<ext>`, and
//! writes a fresh minimal `package.opf` + `META-INF/container.xml`.
//! [`extract_blocks`] re-parses one canonical document and produces leaf blocks with
//! epub.js 0.3.93 compatible CFI ranges and text-node mappings.
//!
//! The canonical XHTML is "polyglot": it produces the same DOM under the XML parser
//! (blob URL with `application/xhtml+xml`) and the HTML parser (epub.js' default
//! `srcdoc` method), which is verified with a fixpoint re-parse at build time.

mod blocks;
mod css;
mod dom;
mod html;
mod package;
mod paths;
mod refs;
mod xml;
mod zip_guard;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use std::path::Path;
use ts_rs::TS;

use crate::domain::{self, Block};
use crate::error::AppError;

/// Bumped whenever a sanitizer/serializer rule changes the canonical DOM.
pub const RULES_VERSION: &str = "epub-rules/1";
/// epub.js version whose CFI dialect the canonical CFIs follow.
pub const EPUBJS_VERSION: &str = "0.3.93";

/// Content-Security-Policy injected into every canonical content document head.
pub const CONTENT_CSP: &str = "default-src 'none'; script-src 'none'; connect-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; img-src 'self' blob: thoth-doc:; font-src 'self' thoth-doc:; style-src 'self' 'unsafe-inline' thoth-doc:";

/// Hard input limits (spec §5.1). There is intentionally no "unlimited" option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpubLimits {
    /// Size of the EPUB file on disk.
    pub max_file_bytes: u64,
    /// Number of ZIP entries (central directory records).
    pub max_entries: u64,
    /// Sum of uncompressed sizes (declared before extraction, actual during).
    pub max_total_uncompressed: u64,
    /// Uncompressed size of a single entry (declared and actual).
    pub max_entry_bytes: u64,
    /// Maximum uncompressed:compressed ratio (per entry and overall).
    pub max_ratio: u64,
}

impl Default for EpubLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 500 * 1024 * 1024,
            max_entries: 20_000,
            max_total_uncompressed: 1024 * 1024 * 1024,
            max_entry_bytes: 64 * 1024 * 1024,
            max_ratio: 200,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SafeSpineItem {
    /// Position in the generated package spine (== CFI spine step index).
    pub index: u32,
    /// Itemref id used in the CFI spine assertion `/6/N[idref]`.
    pub idref: String,
    /// Normalized package-internal path of the original content document.
    pub original_href: String,
    /// `x/<index>.xhtml`, relative to the safe package root.
    pub safe_href: String,
    pub linear: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TocEntry {
    pub label: String,
    pub spine_index: u32,
    pub fragment: Option<String>,
    pub children: Vec<TocEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SafeResource {
    pub resource_id: String,
    /// `r/<resourceId>.<ext>`, relative to the safe package root.
    pub safe_href: String,
    pub mime: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SafePackage {
    pub title: String,
    pub language: Option<String>,
    pub canonical_revision: String,
    pub spine: Vec<SafeSpineItem>,
    pub toc: Vec<TocEntry>,
    pub resources: Vec<SafeResource>,
    /// Sorted, de-duplicated restriction codes (`code` or `code:detail`).
    pub restrictions: Vec<String>,
}

/// `canonicalRevision` = sha256(canonical JSON of sanitizer version, epub.js version, rules).
pub fn canonical_revision() -> String {
    let v = serde_json::json!({
        "sanitizer": domain::versions::EPUB_SANITIZER,
        "epubjs": EPUBJS_VERSION,
        "rules": RULES_VERSION,
    });
    domain::canonical_json::canonical_sha256(&v)
}

/// Resource id: first 24 hex chars of sha256(normalized href + canonicalRevision).
pub fn resource_id(normalized_href: &str, canonical_revision: &str) -> String {
    let mut s = String::with_capacity(normalized_href.len() + canonical_revision.len());
    s.push_str(normalized_href);
    s.push_str(canonical_revision);
    domain::sha256_hex_str(&s)[..24].to_string()
}

/// Validate and sanitize `epub_path` into `out_dir` (spec §5). CPU heavy and
/// synchronous: run it on a blocking pool.
pub fn build_safe_package(
    epub_path: &Path,
    out_dir: &Path,
    limits: &EpubLimits,
) -> Result<SafePackage, AppError> {
    package::build(epub_path, out_dir, limits)
}

/// Extract leaf blocks with canonical CFI ranges from a canonical XHTML document
/// produced by [`build_safe_package`].
pub fn extract_blocks(
    xhtml: &str,
    spine_index: u32,
    idref: &str,
    safe_href: &str,
    canonical_revision: &str,
    extraction_id: &str,
    section_id: &str,
) -> Result<Vec<Block>, AppError> {
    blocks::extract(
        xhtml,
        &blocks::SectionInfo {
            spine_index,
            idref,
            safe_href,
            canonical_revision,
            extraction_id,
            section_id,
        },
    )
}
