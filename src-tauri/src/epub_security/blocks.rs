//! Block + CFI extraction from canonical XHTML (spec §3.1–3.3).
//!
//! CFI dialect = epub.js 0.3.93 (`EpubCFI.fromRange` / `pathTo` / `joinSteps`):
//! * the content path starts below `<html>` (`/4` is `<body>` because `<head>` is
//!   the first element child),
//! * element step = `(index among element children + 1) * 2`, with `[id]` when the
//!   element has an id,
//! * text step = `1 + 2 * (index among *text-node* children)` — epub.js counts only
//!   text siblings, which differs from strict EPUB CFI 1.1 when two elements are
//!   adjacent without text in between,
//! * a range is `epubcfi(base!common,/start:off,/end:off)` where `common` is the
//!   longest shared step prefix excluding the start's final (text) step.

use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;

use super::dom::{Element, Node, Ns};
use super::xml::parse_xml;
use crate::domain::utf16::len_u16;
use crate::domain::{
    derive_block_id, normalize_source_text, sha256_hex_str, Block, BlockKind, BlockMapping, OffsetMapEntry, Quality,
    QualityLevel, SafeNode, SourceLocation, TextNodeRef, TextQuote, TranslationPolicy,
};
use crate::error::AppError;

pub(crate) struct SectionInfo<'a> {
    pub spine_index: u32,
    pub idref: &'a str,
    pub safe_href: &'a str,
    pub canonical_revision: &'a str,
    pub extraction_id: &'a str,
    pub section_id: &'a str,
}

const QUOTE_EXACT_U16: u32 = 256;
const QUOTE_CONTEXT_U16: u32 = 32;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Step {
    pub n: u32,
    pub id: Option<String>,
}

pub(crate) fn steps_to_string(steps: &[Step], with_ids: bool) -> String {
    let mut s = String::new();
    for st in steps {
        s.push('/');
        s.push_str(&st.n.to_string());
        if with_ids {
            if let Some(id) = &st.id {
                s.push('[');
                s.push_str(id);
                s.push(']');
            }
        }
    }
    s
}

enum Piece {
    Run { steps: Vec<Step>, text: String },
    Sep(char),
}

struct Draft {
    kind: BlockKind,
    parent: Option<usize>,
    elem_steps: Vec<Step>,
    suffix: String,
    pieces: Vec<Piece>,
    content: Vec<SafeNode>,
    rtl: bool,
    footnote: bool,
    has_image: bool,
}

impl Draft {
    fn new(kind: BlockKind, steps: &[Step], suffix: String, inh: &Inh) -> Self {
        Draft {
            kind,
            parent: None,
            elem_steps: steps.to_vec(),
            suffix,
            pieces: Vec::new(),
            content: Vec::new(),
            rtl: inh.rtl,
            footnote: inh.footnote,
            has_image: false,
        }
    }
    fn has_text(&self) -> bool {
        self.pieces.iter().any(|p| matches!(p, Piece::Run { text, .. } if !text.trim().is_empty()))
    }
}

#[derive(Clone)]
struct Inh {
    ctx_kind: Option<BlockKind>,
    rtl: bool,
    footnote: bool,
}

const BLOCK_TAGS: &[&str] = &[
    "p", "div", "section", "article", "aside", "nav", "header", "footer", "h1", "h2", "h3", "h4", "h5", "h6",
    "hgroup", "blockquote", "pre", "hr", "ul", "ol", "li", "dl", "dt", "dd", "figure", "figcaption", "table",
    "caption", "thead", "tbody", "tfoot", "tr", "td", "th", "colgroup", "col", "address",
];

fn is_block(e: &Element) -> bool {
    (e.ns == Ns::Html && BLOCK_TAGS.contains(&e.name.as_str()))
        || (e.is(Ns::MathMl, "math") && e.attr("display") == Some("block"))
}

fn contains_block(e: &Element) -> bool {
    e.child_elements().any(|c| is_block(c) || (c.ns == Ns::Html && contains_block(c)))
}

fn has_image(e: &Element) -> bool {
    e.child_elements().any(|c| c.is_html("img") || c.ns == Ns::Svg || has_image(c))
}

/// Children with their epub.js CFI steps.
fn children_with_steps<'a>(el: &'a Element, base: &[Step]) -> Vec<(&'a Node, Vec<Step>)> {
    let mut out = Vec::with_capacity(el.children.len());
    let (mut ek, mut tk) = (0u32, 0u32);
    for c in &el.children {
        let step = match c {
            Node::Element(e) => {
                ek += 1;
                Step { n: ek * 2, id: e.attr("id").filter(|s| !s.is_empty()).map(str::to_string) }
            }
            Node::Text(_) => {
                tk += 1;
                Step { n: 1 + 2 * (tk - 1), id: None }
            }
        };
        let mut s = base.to_vec();
        s.push(step);
        out.push((c, s));
    }
    out
}

fn heading_kind(name: &str) -> bool {
    matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
}

fn image_resource_id(src: &str) -> String {
    let file = src.rsplit('/').next().unwrap_or(src);
    file.split('.').next().unwrap_or(file).to_string()
}

fn merge_text_nodes(nodes: Vec<SafeNode>) -> Vec<SafeNode> {
    let mut out: Vec<SafeNode> = Vec::with_capacity(nodes.len());
    for n in nodes {
        if let SafeNode::Text { v } = &n {
            if v.is_empty() {
                continue;
            }
            if let Some(SafeNode::Text { v: prev }) = out.last_mut() {
                prev.push_str(v);
                continue;
            }
        }
        out.push(n);
    }
    out
}

struct Walker {
    drafts: Vec<Draft>,
}

impl Walker {
    fn update_inh(e: &Element, inh: &Inh) -> Inh {
        let mut i = inh.clone();
        match e.attr("dir") {
            Some("rtl") => i.rtl = true,
            Some("ltr") => i.rtl = false,
            _ => {}
        }
        if let Some(t) = e.attr("data-epub-type") {
            if t.split_whitespace().any(|t| matches!(t, "footnote" | "endnote" | "rearnote" | "note" | "footnotes" | "endnotes")) {
                i.footnote = true;
            }
        }
        i
    }

    fn walk_flow(&mut self, container: &Element, steps: &[Step], inh: &Inh, group_kind: BlockKind) {
        let mut group: Option<Draft> = None;
        let mut gidx = 0u32;
        for (child, cs) in children_with_steps(container, steps) {
            match child {
                Node::Text(t) => {
                    if group.is_none() && t.trim().is_empty() {
                        continue;
                    }
                    let g = group.get_or_insert_with(|| Draft::new(group_kind, steps, format!("#g{gidx}"), inh));
                    g.pieces.push(Piece::Run { steps: cs, text: t.clone() });
                    g.content.push(SafeNode::Text { v: t.clone() });
                }
                Node::Element(e) => {
                    let block_like = is_block(e) || (e.ns == Ns::Html && contains_block(e));
                    if block_like {
                        if let Some(g) = group.take() {
                            self.push_group(g);
                            gidx += 1;
                        }
                        let ci = Self::update_inh(e, inh);
                        if is_block(e) {
                            self.block(e, &cs, &ci);
                        } else {
                            self.walk_flow(e, &cs, &ci, group_kind);
                        }
                    } else {
                        if group.is_none() && e.text_content().trim().is_empty() && !has_image(e) && !e.is_html("img") {
                            continue;
                        }
                        let g = group.get_or_insert_with(|| Draft::new(group_kind, steps, format!("#g{gidx}"), inh));
                        let nodes = Self::collect_one(e, &cs, g);
                        g.content.extend(nodes);
                    }
                }
            }
        }
        if let Some(g) = group.take() {
            self.push_group(g);
        }
    }

    fn push_group(&mut self, mut g: Draft) {
        if !g.has_text() {
            if !g.has_image {
                return;
            }
            g.kind = BlockKind::Figure;
        }
        g.content = merge_text_nodes(std::mem::take(&mut g.content));
        self.drafts.push(g);
    }

    fn block(&mut self, e: &Element, steps: &[Step], inh: &Inh) {
        let name = e.name.as_str();
        if e.ns == Ns::MathMl {
            self.leaf(e, steps, BlockKind::Formula, inh, None, &[]);
            return;
        }
        match name {
            "hr" | "col" | "colgroup" => {}
            "table" => self.table(e, steps, inh),
            "pre" => {
                self.leaf(e, steps, BlockKind::Code, inh, None, &[]);
            }
            "figure" if has_image(e) => self.figure(e, steps, inh),
            _ => {
                let mut ci = inh.clone();
                let kind = if heading_kind(name) {
                    BlockKind::Heading
                } else {
                    match name {
                        "li" => {
                            ci.ctx_kind = Some(BlockKind::ListItem);
                            BlockKind::ListItem
                        }
                        "blockquote" => {
                            ci.ctx_kind = Some(BlockKind::Quote);
                            BlockKind::Quote
                        }
                        "figcaption" | "caption" => BlockKind::Caption,
                        _ => inh.ctx_kind.unwrap_or(BlockKind::Paragraph),
                    }
                };
                if contains_block(e) {
                    self.walk_flow(e, steps, &ci, kind);
                } else {
                    self.leaf(e, steps, kind, &ci, None, &[]);
                }
            }
        }
    }

    /// A leaf block: all descendant text belongs to this block.
    fn leaf(&mut self, e: &Element, steps: &[Step], kind: BlockKind, inh: &Inh, parent: Option<usize>, skip: &[&str]) -> Option<usize> {
        let mut d = Draft::new(kind, steps, String::new(), inh);
        d.parent = parent;
        let nodes = Self::collect_children(e, steps, &mut d, skip);
        d.content = match kind {
            BlockKind::Code => vec![SafeNode::InlineCode { v: e.text_content() }],
            BlockKind::Formula => {
                let alt = e.attr("alttext").map(str::to_string).unwrap_or_else(|| e.text_content().trim().to_string());
                vec![SafeNode::Text { v: alt }]
            }
            _ => merge_text_nodes(nodes),
        };
        if !matches!(kind, BlockKind::Code | BlockKind::Formula | BlockKind::Figure | BlockKind::Table) && !d.has_text() {
            if !d.has_image {
                return None;
            }
            d.kind = BlockKind::Figure;
        }
        if kind == BlockKind::Code && d.pieces.is_empty() {
            return None;
        }
        self.drafts.push(d);
        Some(self.drafts.len() - 1)
    }

    fn table(&mut self, e: &Element, steps: &[Step], inh: &Inh) {
        let mut d = Draft::new(BlockKind::Table, steps, String::new(), inh);
        let mut captions: Vec<(&Element, Vec<Step>)> = Vec::new();
        let mut rows: Vec<(&Element, Vec<Step>)> = Vec::new();
        for (c, cs) in children_with_steps(e, steps) {
            let Node::Element(ce) = c else { continue };
            match ce.name.as_str() {
                "caption" => captions.push((ce, cs)),
                "tr" => rows.push((ce, cs)),
                "thead" | "tbody" | "tfoot" => {
                    for (r, rs) in children_with_steps(ce, &cs) {
                        if let Node::Element(re) = r {
                            if re.name == "tr" {
                                rows.push((re, rs));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        for (ri, (row, rs)) in rows.iter().enumerate() {
            if ri > 0 {
                d.pieces.push(Piece::Sep('\n'));
                d.content.push(SafeNode::Break);
            }
            let mut first_cell = true;
            for (c, cs) in children_with_steps(row, rs) {
                let Node::Element(cell) = c else { continue };
                if cell.name != "td" && cell.name != "th" {
                    continue;
                }
                if !first_cell {
                    d.pieces.push(Piece::Sep('\t'));
                }
                first_cell = false;
                let nodes = Self::collect_children(cell, &cs, &mut d, &[]);
                d.content.push(SafeNode::TableCell { header: cell.name == "th", c: merge_text_nodes(nodes) });
            }
        }
        self.drafts.push(d);
        let tidx = self.drafts.len() - 1;
        for (cap, cs) in captions {
            let ci = Self::update_inh(cap, inh);
            self.leaf(cap, &cs, BlockKind::Caption, &ci, Some(tidx), &[]);
        }
    }

    fn figure(&mut self, e: &Element, steps: &[Step], inh: &Inh) {
        let mut d = Draft::new(BlockKind::Figure, steps, String::new(), inh);
        let nodes = Self::collect_children(e, steps, &mut d, &["figcaption"]);
        d.content = merge_text_nodes(nodes);
        self.drafts.push(d);
        let fidx = self.drafts.len() - 1;
        for (c, cs) in children_with_steps(e, steps) {
            if let Node::Element(ce) = c {
                if ce.is_html("figcaption") {
                    let ci = Self::update_inh(ce, inh);
                    self.leaf(ce, &cs, BlockKind::Caption, &ci, Some(fidx), &[]);
                }
            }
        }
    }

    fn collect_children(e: &Element, steps: &[Step], d: &mut Draft, skip: &[&str]) -> Vec<SafeNode> {
        let mut nodes = Vec::new();
        for (c, cs) in children_with_steps(e, steps) {
            match c {
                Node::Text(t) => {
                    d.pieces.push(Piece::Run { steps: cs, text: t.clone() });
                    nodes.push(SafeNode::Text { v: t.clone() });
                }
                Node::Element(ce) => {
                    if ce.ns == Ns::Html && skip.contains(&ce.name.as_str()) {
                        continue;
                    }
                    nodes.extend(Self::collect_one(ce, &cs, d));
                }
            }
        }
        nodes
    }

    fn collect_one(e: &Element, steps: &[Step], d: &mut Draft) -> Vec<SafeNode> {
        match e.ns {
            Ns::Svg => {
                d.has_image = true;
                return Vec::new();
            }
            Ns::MathMl => {
                // Inline formula: its text nodes are part of the DOM text.
                let _ = Self::collect_children(e, steps, d, &[]);
                let alt = e.attr("alttext").map(str::to_string).unwrap_or_else(|| e.text_content());
                return vec![SafeNode::Text { v: alt }];
            }
            Ns::Html => {}
            _ => return Vec::new(),
        }
        let name = e.name.as_str();
        // Block children inside a leaf/table cell: keep words apart.
        if is_block(e) && !d.pieces.is_empty() {
            d.pieces.push(Piece::Sep('\n'));
        }
        match name {
            "br" => {
                d.pieces.push(Piece::Sep('\n'));
                vec![SafeNode::Break]
            }
            "img" => {
                d.has_image = true;
                let src = e.attr("src").unwrap_or("");
                vec![SafeNode::ImageRef { resource_id: image_resource_id(src), alt: e.attr("alt").unwrap_or("").to_string() }]
            }
            "rt" | "rp" => Vec::new(),
            "em" | "i" | "cite" | "dfn" | "var" => {
                vec![SafeNode::Emphasis { c: merge_text_nodes(Self::collect_children(e, steps, d, &[])) }]
            }
            "strong" | "b" => vec![SafeNode::Strong { c: merge_text_nodes(Self::collect_children(e, steps, d, &[])) }],
            "code" | "kbd" | "samp" => {
                let _ = Self::collect_children(e, steps, d, &[]);
                vec![SafeNode::InlineCode { v: e.text_content() }]
            }
            "a" => {
                let c = merge_text_nodes(Self::collect_children(e, steps, d, &[]));
                match e.attr("href") {
                    Some(h) if !h.is_empty() => vec![SafeNode::Link { href: h.to_string(), c }],
                    _ => c,
                }
            }
            _ => Self::collect_children(e, steps, d, &[]),
        }
    }
}

/// epub.js `fromRange` common-path computation.
fn range_cfi(base: &str, start: &[Step], so: u32, end: &[Step], eo: u32) -> String {
    let mut common = 0;
    for i in 0..start.len() {
        if i < end.len() && start[i] == end[i] {
            if i == start.len() - 1 {
                // epub.js compares terminal objects by identity: never equal.
                break;
            }
            common += 1;
        } else {
            break;
        }
    }
    let common_str = {
        let s = steps_to_string(&start[..common], true);
        if s.is_empty() {
            "/".to_string()
        } else {
            s
        }
    };
    format!(
        "epubcfi({base}!{common_str},{}:{so},{}:{eo})",
        steps_to_string(&start[common..], true),
        steps_to_string(&end[common..], true)
    )
}

fn take_prefix_u16(s: &str, max: u32) -> String {
    let mut n = 0;
    let mut out = String::new();
    for c in s.chars() {
        let l = c.len_utf16() as u32;
        if n + l > max {
            break;
        }
        n += l;
        out.push(c);
    }
    out
}

fn take_suffix_u16(s: &str, max: u32) -> String {
    let mut n = 0;
    let mut chars: Vec<char> = Vec::new();
    for c in s.chars().rev() {
        let l = c.len_utf16() as u32;
        if n + l > max {
            break;
        }
        n += l;
        chars.push(c);
    }
    chars.into_iter().rev().collect()
}

/// raw <-> source mapping at word granularity (UTF-16 offsets).
fn word_map(raw: &str, source: &str, kind: BlockKind) -> Vec<OffsetMapEntry> {
    let whole = || {
        if raw.is_empty() && source.is_empty() {
            Vec::new()
        } else {
            vec![OffsetMapEntry { source_start: 0, source_end: len_u16(source), raw_start: 0, raw_end: len_u16(raw) }]
        }
    };
    if matches!(kind, BlockKind::Code | BlockKind::Formula) {
        return whole();
    }
    let mut entries = Vec::new();
    let mut built = String::new();
    let mut built_u16 = 0u32;
    let mut u16_pos = 0u32;
    let mut word_start: Option<(usize, u32)> = None;
    let mut flush = |s: usize, s16: u32, e: usize, e16: u32, built: &mut String, built_u16: &mut u32| {
        let w: String = raw[s..e].nfc().collect();
        if !built.is_empty() {
            built.push(' ');
            *built_u16 += 1;
        }
        let ss = *built_u16;
        *built_u16 += len_u16(&w);
        built.push_str(&w);
        entries.push(OffsetMapEntry { source_start: ss, source_end: *built_u16, raw_start: s16, raw_end: e16 });
    };
    for (b, c) in raw.char_indices() {
        if c.is_whitespace() {
            if let Some((s, s16)) = word_start.take() {
                flush(s, s16, b, u16_pos, &mut built, &mut built_u16);
            }
        } else if word_start.is_none() {
            word_start = Some((b, u16_pos));
        }
        u16_pos += c.len_utf16() as u32;
    }
    if let Some((s, s16)) = word_start.take() {
        flush(s, s16, raw.len(), u16_pos, &mut built, &mut built_u16);
    }
    if built == source {
        entries
    } else {
        whole()
    }
}

struct Finished {
    raw: String,
    runs: Vec<(Vec<Step>, TextNodeRef)>,
}

fn finish_text(d: &Draft) -> Finished {
    let mut full = String::new();
    for p in &d.pieces {
        match p {
            Piece::Run { text, .. } => full.push_str(text),
            Piece::Sep(c) => full.push(*c),
        }
    }
    let (a, b) = if d.kind == BlockKind::Code {
        (0, full.len())
    } else {
        let a = full.char_indices().find(|(_, c)| !c.is_whitespace()).map(|(i, _)| i).unwrap_or(full.len());
        let b = full
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(a);
        (a, b.max(a))
    };
    let raw = full[a..b].to_string();
    let mut runs = Vec::new();
    let mut pos = 0usize;
    for p in &d.pieces {
        let len = match p {
            Piece::Run { text, .. } => text.len(),
            Piece::Sep(c) => c.len_utf8(),
        };
        if let Piece::Run { steps, text } = p {
            let s = pos.max(a);
            let e = (pos + len).min(b);
            if s < e {
                let sub = &text[s - pos..e - pos];
                runs.push((
                    steps.clone(),
                    TextNodeRef {
                        path: steps_to_string(steps, true),
                        raw_start_u16: len_u16(&full[a..s]),
                        raw_len_u16: len_u16(sub),
                        node_offset_u16: len_u16(&text[..s - pos]),
                    },
                ));
            }
        }
        pos += len;
    }
    Finished { raw, runs }
}

pub(crate) fn extract(xhtml: &str, info: &SectionInfo<'_>) -> Result<Vec<Block>, AppError> {
    let root = parse_xml(xhtml, false, true)
        .map_err(|_| AppError::invalid("안전화된 EPUB 문서를 해석할 수 없습니다."))?;
    if !root.is_html("html") {
        return Err(AppError::invalid("안전화된 EPUB 문서 형식이 올바르지 않습니다."));
    }
    // Document-level restriction flags written by the sanitizer.
    let mut doc_flags: Vec<String> = Vec::new();
    let mut body: Option<(&Element, Vec<Step>)> = None;
    for (c, cs) in children_with_steps(&root, &[]) {
        let Node::Element(e) = c else { continue };
        if e.is_html("head") {
            for m in e.child_elements() {
                if m.is_html("meta") && m.attr("name") == Some("thoth-restrictions") {
                    doc_flags.extend(m.attr("content").unwrap_or("").split_whitespace().map(str::to_string));
                }
            }
        } else if e.is_html("body") && body.is_none() {
            body = Some((e, cs));
        }
    }
    let Some((body, body_steps)) = body else { return Ok(Vec::new()) };
    let base_inh = Inh {
        ctx_kind: None,
        rtl: root.attr("dir") == Some("rtl") || doc_flags.iter().any(|f| f == "rtl"),
        footnote: false,
    };
    let inh = Walker::update_inh(body, &base_inh);
    let mut w = Walker { drafts: Vec::new() };
    w.walk_flow(body, &body_steps, &inh, BlockKind::Paragraph);

    let base = format!("/6/{}[{}]", (info.spine_index + 1) * 2, info.idref);
    let locator = format!("spine:{}:{}", info.spine_index, info.safe_href);
    let fixed = doc_flags.iter().any(|f| f == "fixed_layout");
    let vertical = doc_flags.iter().any(|f| f == "vertical_writing");

    let finished: Vec<Finished> = w.drafts.iter().map(finish_text).collect();
    let sources: Vec<String> = w
        .drafts
        .iter()
        .zip(&finished)
        .map(|(d, f)| normalize_source_text(&f.raw, d.kind))
        .collect();

    let mut occurrences: HashMap<(String, BlockKind), u32> = HashMap::new();
    let mut ids: Vec<String> = Vec::with_capacity(w.drafts.len());
    for d in &w.drafts {
        let raw_range = format!("{}{}", steps_to_string(&d.elem_steps, false), d.suffix);
        let occ = occurrences.entry((raw_range.clone(), d.kind)).or_insert(0);
        ids.push(derive_block_id(info.extraction_id, &locator, &raw_range, d.kind, *occ));
        *occ += 1;
    }

    let mut blocks = Vec::with_capacity(w.drafts.len());
    for (i, d) in w.drafts.iter().enumerate() {
        let f = &finished[i];
        let source = &sources[i];
        let cfi = match (f.runs.first(), f.runs.last()) {
            (Some((s_steps, s)), Some((e_steps, e))) => {
                range_cfi(&base, s_steps, s.node_offset_u16, e_steps, e.node_offset_u16 + e.raw_len_u16)
            }
            _ => format!("epubcfi({base}!{})", steps_to_string(&d.elem_steps, true)),
        };
        let mut reasons = Vec::new();
        if fixed {
            reasons.push("fixed_layout".to_string());
        }
        if d.rtl {
            reasons.push("rtl".to_string());
        }
        if vertical {
            reasons.push("vertical_writing".to_string());
        }
        let mut policy = d.kind.default_policy();
        if policy == TranslationPolicy::Auto && !reasons.is_empty() {
            policy = TranslationPolicy::ManualOnly;
        }
        let quality = Quality {
            level: if reasons.is_empty() { QualityLevel::High } else { QualityLevel::Medium },
            reasons,
        };
        let prefix = if i > 0 { take_suffix_u16(&sources[i - 1], QUOTE_CONTEXT_U16) } else { String::new() };
        let suffix = sources.get(i + 1).map(|s| take_prefix_u16(s, QUOTE_CONTEXT_U16)).unwrap_or_default();
        let mut roles = Vec::new();
        if d.footnote {
            roles.push("footnote".to_string());
        }
        blocks.push(Block {
            id: ids[i].clone(),
            extraction_id: info.extraction_id.to_string(),
            section_id: info.section_id.to_string(),
            parent_id: d.parent.map(|p| ids[p].clone()),
            ordinal: i as u32,
            kind: d.kind,
            raw_text: f.raw.clone(),
            source_text: source.clone(),
            source_hash: sha256_hex_str(source),
            content: d.content.clone(),
            location: SourceLocation::Epub {
                spine_index: info.spine_index,
                href: info.safe_href.to_string(),
                canonical_revision: info.canonical_revision.to_string(),
                cfi_range: cfi,
                text_path: d.elem_steps.iter().map(|s| s.n).collect(),
                start_u16: 0,
                end_u16: len_u16(&f.raw),
                quote: TextQuote { exact: take_prefix_u16(source, QUOTE_EXACT_U16), prefix, suffix },
            },
            quality,
            translation_policy: policy,
            mapping: BlockMapping {
                offsets: word_map(&f.raw, source, d.kind),
                text_nodes: f.runs.iter().map(|(_, r)| r.clone()).collect(),
            },
            roles,
        });
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epubjs_range_format() {
        let st = |v: &[u32]| v.iter().map(|&n| Step { n, id: None }).collect::<Vec<_>>();
        assert_eq!(
            range_cfi("/6/4[c]", &st(&[4, 2, 6, 1]), 0, &st(&[4, 2, 6, 1]), 12),
            "epubcfi(/6/4[c]!/4/2/6,/1:0,/1:12)"
        );
        assert_eq!(
            range_cfi("/6/4[c]", &st(&[4, 2, 6, 2, 1]), 0, &st(&[4, 2, 6, 3]), 5),
            "epubcfi(/6/4[c]!/4/2/6,/2/1:0,/3:5)"
        );
    }

    #[test]
    fn word_map_tracks_nfc_and_whitespace() {
        let raw = "  e\u{301}t\u{e9}\n  du  ";
        let src = normalize_source_text(raw, BlockKind::Paragraph);
        let m = word_map(raw, &src, BlockKind::Paragraph);
        assert_eq!(src, "\u{e9}t\u{e9} du");
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].source_start, m[0].source_end, m[0].raw_start, m[0].raw_end), (0, 3, 2, 6));
        assert_eq!((m[1].source_start, m[1].source_end), (4, 6));
    }
}
