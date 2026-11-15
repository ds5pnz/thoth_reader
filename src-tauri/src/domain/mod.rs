//! Domain contracts shared by every Rust module (spec §3).
//! These types must not depend on Tauri. TypeScript DTOs are generated with ts-rs.

pub mod canonical_json;
pub mod hashing;
pub mod utf16;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub use hashing::{sha256_hex, sha256_hex_str};

/// Current extractor / sanitizer / splitter versions. Stored as part of the data.
pub mod versions {
    /// 2: heading blocks carry their level as a role ("h1".."h6").
    pub const EPUB_EXTRACTOR: &str = "epub-extractor/2";
    pub const EPUB_SANITIZER: &str = "epub-sanitizer/1";
    /// 2: content carries bold/italic runs (strong/emphasis nodes).
    pub const PDF_EXTRACTOR: &str = "pdf-extractor/4";
    pub const SPLITTER: &str = "1";
    pub const PLACEHOLDER: &str = "1";
    pub const ADAPTER: &str = "1";
    pub const CACHE_KEY: u32 = 1;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum BlockKind {
    Heading,
    Paragraph,
    ListItem,
    Quote,
    Code,
    Table,
    Figure,
    Caption,
    Formula,
    Unknown,
}

impl BlockKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockKind::Heading => "heading",
            BlockKind::Paragraph => "paragraph",
            BlockKind::ListItem => "list_item",
            BlockKind::Quote => "quote",
            BlockKind::Code => "code",
            BlockKind::Table => "table",
            BlockKind::Figure => "figure",
            BlockKind::Caption => "caption",
            BlockKind::Formula => "formula",
            BlockKind::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "heading" => BlockKind::Heading,
            "paragraph" => BlockKind::Paragraph,
            "list_item" => BlockKind::ListItem,
            "quote" => BlockKind::Quote,
            "code" => BlockKind::Code,
            "table" => BlockKind::Table,
            "figure" => BlockKind::Figure,
            "caption" => BlockKind::Caption,
            "formula" => BlockKind::Formula,
            "unknown" => BlockKind::Unknown,
            _ => return None,
        })
    }
    /// Kinds that are never auto-translated regardless of quality.
    pub fn default_policy(&self) -> TranslationPolicy {
        match self {
            BlockKind::Code | BlockKind::Formula | BlockKind::Table | BlockKind::Figure => {
                TranslationPolicy::Preserve
            }
            _ => TranslationPolicy::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ItemRange {
    pub item_index: u32,
    pub start_u16: u32,
    pub end_u16: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TextQuote {
    pub exact: String,
    pub prefix: String,
    pub suffix: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export)]
pub enum SourceLocation {
    #[serde(rename = "pdf", rename_all = "camelCase")]
    Pdf {
        page_index: u32,
        view_box: Vec<f64>,
        rotation: i32,
        rects: Vec<Rect>,
        item_ranges: Vec<ItemRange>,
    },
    #[serde(rename = "epub", rename_all = "camelCase")]
    Epub {
        spine_index: u32,
        href: String,
        canonical_revision: String,
        cfi_range: String,
        text_path: Vec<u32>,
        start_u16: u32,
        end_u16: u32,
        quote: TextQuote,
    },
}

/// `#rrggbb`, lowercase hex only: the one form a colour may take anywhere in the app.
pub fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Allow-listed inline content AST (spec §3.2). Never arbitrary HTML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "t", rename_all = "snake_case")]
#[ts(export)]
pub enum SafeNode {
    Text { v: String },
    Emphasis { c: Vec<SafeNode> },
    Strong { c: Vec<SafeNode> },
    /// Text set in a colour other than the block's own (`v` is `#rrggbb`).
    Color { v: String, c: Vec<SafeNode> },
    InlineCode { v: String },
    /// href is either `res:<resourceId>` (internal) or a validated http(s) URL.
    Link { href: String, c: Vec<SafeNode> },
    Break,
    ImageRef {
        #[serde(rename = "resourceId")]
        #[ts(rename = "resourceId")]
        resource_id: String,
        alt: String,
    },
    MathRef {
        #[serde(rename = "resourceId")]
        #[ts(rename = "resourceId")]
        resource_id: String,
        alt: String,
    },
    TableCell { header: bool, c: Vec<SafeNode> },
}

pub type SafeContent = Vec<SafeNode>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum QualityLevel {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Quality {
    pub level: QualityLevel,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TranslationPolicy {
    Auto,
    ManualOnly,
    Preserve,
}

impl TranslationPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            TranslationPolicy::Auto => "auto",
            TranslationPolicy::ManualOnly => "manual_only",
            TranslationPolicy::Preserve => "preserve",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "auto" => Self::Auto,
            "manual_only" => Self::ManualOnly,
            "preserve" => Self::Preserve,
            _ => return None,
        })
    }
    fn rank(&self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::ManualOnly => 1,
            Self::Preserve => 2,
        }
    }
    /// Effective policy (spec §9.2): extraction limits win; a user override may only
    /// make a block *more* restrictive, never lift preserve/low-quality restrictions.
    pub fn effective(extracted: Self, quality: QualityLevel, user: Option<Self>) -> Self {
        let mut base = extracted;
        if quality == QualityLevel::Low && base == Self::Auto {
            base = Self::ManualOnly;
        }
        match user {
            Some(u) if u.rank() > base.rank() => u,
            _ => base,
        }
    }
}

/// Raw<->source offset mapping entry (UTF-16). Each entry maps a source range to a raw range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct OffsetMapEntry {
    pub source_start: u32,
    pub source_end: u32,
    pub raw_start: u32,
    pub raw_end: u32,
}

/// Where a run of the block's raw text lives in the canonical DOM / PDF text items.
/// For EPUB, `path` is the local CFI path of a text node inside the content document
/// (e.g. "/4/2/3"); offsets are UTF-16 within `raw_text`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TextNodeRef {
    pub path: String,
    pub raw_start_u16: u32,
    pub raw_len_u16: u32,
    /// UTF-16 offset of this run's first char inside the DOM text node.
    #[serde(default)]
    pub node_offset_u16: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BlockMapping {
    /// raw <-> source ranges for every normalization step.
    #[serde(default)]
    pub offsets: Vec<OffsetMapEntry>,
    #[serde(default)]
    pub text_nodes: Vec<TextNodeRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Block {
    pub id: String,
    pub extraction_id: String,
    pub section_id: String,
    pub parent_id: Option<String>,
    pub ordinal: u32,
    pub kind: BlockKind,
    pub raw_text: String,
    pub source_text: String,
    pub source_hash: String,
    pub content: SafeContent,
    pub location: SourceLocation,
    pub quality: Quality,
    pub translation_policy: TranslationPolicy,
    #[serde(default)]
    pub mapping: BlockMapping,
    /// Roles such as running_header / running_footer / footnote. Not translated-policy itself.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl Block {
    /// The policy extraction gave the block, with one correction made at read time: a
    /// figure that carries words (its caption sits in the same block) is translated like
    /// text. Pure pictures, code, formulas and tables stay preserved. Read-time so books
    /// extracted earlier need no re-extraction.
    pub fn base_policy(&self) -> TranslationPolicy {
        if self.kind == BlockKind::Figure && self.translation_policy == TranslationPolicy::Preserve && self.source_text.chars().any(|c| c.is_alphabetic()) {
            TranslationPolicy::Auto
        } else {
            self.translation_policy
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ReaderAnchor {
    pub schema_version: u32,
    pub document_id: String,
    pub extraction_id: String,
    pub block_id: Option<String>,
    pub source_offset_u16: u32,
    pub within_block_ratio: f64,
    pub location: Option<SourceLocation>,
}

impl ReaderAnchor {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1 {
            return Err("unsupported anchor schemaVersion");
        }
        if !(0.0..=1.0).contains(&self.within_block_ratio) || self.within_block_ratio.is_nan() {
            return Err("withinBlockRatio must be in [0,1]");
        }
        if !is_valid_id(&self.document_id) || !is_valid_id(&self.extraction_id) {
            return Err("invalid id");
        }
        if let Some(b) = &self.block_id {
            if !is_valid_id(b) {
                return Err("invalid block id");
            }
        }
        Ok(())
    }
}

/// IDs are lowercase hex or UUID strings, bounded length.
pub fn is_valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
}

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_uuid() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Deterministic block id (spec §3.1): hash of extractionId, section locator,
/// raw range, kind, occurrence. Never just the content.
pub fn derive_block_id(
    extraction_id: &str,
    section_locator: &str,
    raw_range: &str,
    kind: BlockKind,
    occurrence: u32,
) -> String {
    let v = serde_json::json!({
        "extractionId": extraction_id,
        "locator": section_locator,
        "rawRange": raw_range,
        "kind": kind.as_str(),
        "occurrence": occurrence,
    });
    sha256_hex(canonical_json::to_canonical_string(&v).as_bytes())
}

pub fn derive_section_id(extraction_id: &str, ordinal: u32, locator: &str) -> String {
    let v = serde_json::json!({"extractionId": extraction_id, "ordinal": ordinal, "locator": locator});
    sha256_hex(canonical_json::to_canonical_string(&v).as_bytes())
}

/// extractionId = hash(documentId, versions, canonical config).
pub fn derive_extraction_id(document_id: &str, versions: &serde_json::Value, config: &serde_json::Value) -> String {
    let v = serde_json::json!({"documentId": document_id, "versions": versions, "config": config});
    sha256_hex(canonical_json::to_canonical_string(&v).as_bytes())
}

/// NFC + prose whitespace collapse (spec §3.2). Code/formula keep raw text.
pub fn normalize_source_text(raw: &str, kind: BlockKind) -> String {
    use unicode_normalization::UnicodeNormalization;
    if matches!(kind, BlockKind::Code | BlockKind::Formula) {
        return raw.to_string();
    }
    let nfc: String = raw.nfc().collect();
    let mut out = String::with_capacity(nfc.len());
    let mut ws = false;
    for ch in nfc.chars() {
        if ch.is_whitespace() {
            ws = true;
        } else {
            if ws && !out.is_empty() {
                out.push(' ');
            }
            ws = false;
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_override_cannot_lift_restrictions() {
        use TranslationPolicy::*;
        assert_eq!(TranslationPolicy::effective(Preserve, QualityLevel::High, Some(Auto)), Preserve);
        assert_eq!(TranslationPolicy::effective(Auto, QualityLevel::Low, Some(Auto)), ManualOnly);
        assert_eq!(TranslationPolicy::effective(Auto, QualityLevel::High, Some(Preserve)), Preserve);
        assert_eq!(TranslationPolicy::effective(Auto, QualityLevel::Medium, None), Auto);
    }

    #[test]
    fn block_id_distinguishes_duplicates() {
        let a = derive_block_id("e", "spine:1", "0-10", BlockKind::Paragraph, 0);
        let b = derive_block_id("e", "spine:1", "0-10", BlockKind::Paragraph, 1);
        let c = derive_block_id("e", "spine:1", "0-10", BlockKind::Paragraph, 0);
        assert_ne!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn normalize_collapses_prose_not_code() {
        assert_eq!(normalize_source_text("  a \n\t b  ", BlockKind::Paragraph), "a b");
        assert_eq!(normalize_source_text("  a \n b", BlockKind::Code), "  a \n b");
        // NFC: e + combining acute -> é
        assert_eq!(normalize_source_text("e\u{301}", BlockKind::Paragraph), "\u{e9}");
    }
}

#[cfg(test)]
mod base_policy_tests {
    use super::*;

    fn block(kind: BlockKind, policy: TranslationPolicy, text: &str) -> Block {
        Block {
            id: "b".into(),
            extraction_id: "e".into(),
            section_id: "s".into(),
            parent_id: None,
            ordinal: 0,
            kind,
            source_hash: "h".into(),
            raw_text: text.into(),
            source_text: text.into(),
            content: vec![],
            location: SourceLocation::Pdf { page_index: 0, view_box: vec![0.0, 0.0, 612.0, 792.0], rotation: 0, rects: vec![], item_ranges: vec![] },
            quality: Quality { level: QualityLevel::High, reasons: vec![] },
            translation_policy: policy,
            mapping: BlockMapping::default(),
            roles: vec![],
        }
    }

    #[test]
    fn figure_with_caption_is_translatable() {
        use TranslationPolicy::*;
        assert_eq!(block(BlockKind::Figure, Preserve, "Figure 1-1. How it works").base_policy(), Auto);
        assert_eq!(block(BlockKind::Figure, Preserve, "").base_policy(), Preserve, "a bare picture has nothing to translate");
        assert_eq!(block(BlockKind::Figure, Preserve, "1-1 →").base_policy(), Preserve, "no letters");
        assert_eq!(block(BlockKind::Code, Preserve, "let x = 1;").base_policy(), Preserve);
        assert_eq!(block(BlockKind::Paragraph, Auto, "Text").base_policy(), Auto);
        // a user override can still exclude the figure again
        let f = block(BlockKind::Figure, Preserve, "Figure 1-1. How it works");
        assert_eq!(TranslationPolicy::effective(f.base_policy(), f.quality.level, Some(Preserve)), Preserve);
    }
}
