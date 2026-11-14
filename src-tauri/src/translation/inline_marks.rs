//! Bold / italic preservation across translation.
//!
//! The source block's inline emphasis (from its SafeContent AST) is sent to the model as
//! paired `<b>…</b>` / `<i>…</i>` tags around the affected words. The answer is parsed back
//! into plain text + mark ranges; the stored translation stays plain text (search and editing
//! are unaffected) and the ranges live in the revision metadata. Nothing here ever becomes
//! HTML: the tags are a private, fixed four-token grammar.
//!
//! If the tags come back missing or unbalanced, only the emphasis is dropped — the
//! translation itself is kept.

use crate::domain::{Block, BlockKind, SafeNode};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Mark {
    pub start_u16: u32,
    pub end_u16: u32,
    /// "b" (bold), "i" (italic) or "c:#rrggbb" (coloured text)
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceMarks {
    /// No emphasis worth carrying.
    None,
    /// The whole block is uniformly emphasised: no tags are sent, the whole translation is marked.
    Whole(char),
    /// Source text with inline tags, ready to be translated.
    Inline(String),
}

const TAGS: [&str; 6] = ["<b>", "</b>", "<i>", "</i>", "<c>", "</c>"];

fn has_literal_tag(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    TAGS.iter().any(|t| l.contains(t))
}

/// style bits: 1 = bold, 2 = italic, 4 = the block's accent colour (the first colour met;
/// further colours are not carried).
fn walk(nodes: &[SafeNode], style: u8, out: &mut Vec<(char, u8)>, accent: &mut Option<String>) -> bool {
    for n in nodes {
        match n {
            SafeNode::Text { v } => out.extend(v.chars().map(|c| (c, style))),
            SafeNode::InlineCode { v } => out.extend(v.chars().map(|c| (c, style))),
            SafeNode::Strong { c } => {
                if !walk(c, style | 1, out, accent) {
                    return false;
                }
            }
            SafeNode::Emphasis { c } => {
                if !walk(c, style | 2, out, accent) {
                    return false;
                }
            }
            SafeNode::Color { v, c } => {
                let on = crate::domain::is_hex_color(v) && *accent.get_or_insert_with(|| v.clone()) == *v;
                if !walk(c, if on { style | 4 } else { style }, out, accent) {
                    return false;
                }
            }
            SafeNode::Link { c, .. } => {
                if !walk(c, style, out, accent) {
                    return false;
                }
            }
            SafeNode::Break => out.push(('\n', style)),
            SafeNode::ImageRef { .. } | SafeNode::MathRef { .. } => {}
            SafeNode::TableCell { .. } => return false,
        }
    }
    true
}

/// Collapse whitespace like `normalize_source_text` (without NFC), keeping styles.
fn normalize(chars: &[(char, u8)]) -> Vec<(char, u8)> {
    let mut out: Vec<(char, u8)> = Vec::with_capacity(chars.len());
    let mut ws = false;
    for &(c, s) in chars {
        if c.is_whitespace() {
            ws = true;
        } else {
            if ws && !out.is_empty() {
                out.push((' ', 0));
            }
            ws = false;
            out.push((c, s));
        }
    }
    // A space carries the style its two neighbours share (bold stays open across the
    // space between a bold word and a bold-italic word).
    for i in 1..out.len().saturating_sub(1) {
        if out[i].0 == ' ' {
            out[i].1 = out[i - 1].1 & out[i + 1].1;
        }
    }
    out
}

/// Decide how (and whether) a block's emphasis can be carried through translation.
pub fn analyze(b: &Block) -> SourceMarks {
    analyze_full(b).0
}

/// [`analyze`] plus the colour that `<c>` stands for in this block, when it uses one.
pub fn analyze_full(b: &Block) -> (SourceMarks, Option<String>) {
    if !matches!(b.kind, BlockKind::Paragraph | BlockKind::ListItem | BlockKind::Quote | BlockKind::Caption) {
        return (SourceMarks::None, None);
    }
    if has_literal_tag(&b.source_text) {
        return (SourceMarks::None, None); // the text itself talks about <b>/<i>: stay literal
    }
    let mut chars = Vec::new();
    let mut accent = None;
    if !walk(&b.content, 0, &mut chars, &mut accent) {
        return (SourceMarks::None, None);
    }
    let norm = normalize(&chars);
    let plain: String = norm.iter().map(|(c, _)| *c).collect();
    if plain != b.source_text {
        return (SourceMarks::None, None); // cannot align styles with the hashed source text
    }
    let letters: Vec<u8> = norm.iter().filter(|(c, _)| !c.is_whitespace()).map(|(_, s)| *s).collect();
    if letters.is_empty() || letters.iter().all(|s| *s == 0) {
        return (SourceMarks::None, None);
    }
    if letters.iter().all(|s| *s == letters[0]) {
        // Uniform: no tags needed. A block that is one colour throughout is not "highlighted".
        return match letters[0] & 3 {
            0 => (SourceMarks::None, None),
            s => (SourceMarks::Whole(if s & 1 == 1 { 'b' } else { 'i' }), None),
        };
    }
    // Emit tags at style transitions (bold outside italic, colour innermost).
    let mut out = String::with_capacity(plain.len() + 32);
    let mut cur = 0u8;
    let close = |from: u8, to: u8, out: &mut String| {
        if from & 2 == 2 && to & 2 == 0 {
            out.push_str("</i>");
        }
        if from & 1 == 1 && to & 1 == 0 {
            // italic nested inside bold must be closed first
            if from & 2 == 2 && to & 2 == 2 {
                out.push_str("</i>");
            }
            out.push_str("</b>");
            if from & 2 == 2 && to & 2 == 2 {
                out.push_str("<i>");
            }
        }
    };
    let open = |from: u8, to: u8, out: &mut String| {
        if from & 1 == 0 && to & 1 == 1 {
            if from & 2 == 2 && to & 2 == 2 {
                out.push_str("</i>");
            }
            out.push_str("<b>");
            if from & 2 == 2 && to & 2 == 2 {
                out.push_str("<i>");
            }
        }
        if from & 2 == 0 && to & 2 == 2 {
            out.push_str("<i>");
        }
    };
    for &(c, s) in &norm {
        if s != cur {
            // Colour is the innermost pair: it closes before, and reopens after, any change
            // to bold/italic, so every pair stays properly nested.
            if cur & 4 == 4 && (s & 4 == 0 || s & 3 != cur & 3) {
                out.push_str("</c>");
            }
            close(cur & 3, s & 3, &mut out);
            open(cur & 3, s & 3, &mut out);
            if s & 4 == 4 && (cur & 4 == 0 || s & 3 != cur & 3) {
                out.push_str("<c>");
            }
            cur = s;
        }
        out.push(c);
    }
    if cur & 4 == 4 {
        out.push_str("</c>");
    }
    close(cur & 3, 0, &mut out);
    let uses_color = out.contains("<c>");
    (SourceMarks::Inline(out), if uses_color { accent } else { None })
}

/// Remove the tag tokens (used when emphasis cannot be trusted).
pub fn strip_tags(s: &str) -> String {
    let mut out = s.to_string();
    for t in ["<b>", "</b>", "<i>", "</i>", "<c>", "</c>", "<B>", "</B>", "<I>", "</I>", "<C>", "</C>"] {
        out = out.replace(t, "");
    }
    out
}

/// Parse a translated string with tags into plain text and mark ranges (UTF-16).
/// Unbalanced or crossing tags => all tags are stripped and no marks are returned.
pub fn extract(text: &str) -> (String, Vec<Mark>) {
    let mut plain = String::with_capacity(text.len());
    let mut u16_len: u32 = 0;
    let mut stack: Vec<(char, u32)> = Vec::new();
    let mut marks: Vec<Mark> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let lower4: String = rest.chars().take(4).collect::<String>().to_ascii_lowercase();
        let tok = if lower4.starts_with("<b>") {
            Some((true, 'b', 3))
        } else if lower4.starts_with("<i>") {
            Some((true, 'i', 3))
        } else if lower4.starts_with("</b>") {
            Some((false, 'b', 4))
        } else if lower4.starts_with("</i>") {
            Some((false, 'i', 4))
        } else if lower4.starts_with("<c>") {
            Some((true, 'c', 3))
        } else if lower4.starts_with("</c>") {
            Some((false, 'c', 4))
        } else {
            None
        };
        match tok {
            Some((true, k, n)) => {
                stack.push((k, u16_len));
                rest = &rest[n..];
            }
            Some((false, k, n)) => {
                match stack.pop() {
                    Some((ok, start)) if ok == k => {
                        if u16_len > start {
                            marks.push(Mark { start_u16: start, end_u16: u16_len, kind: k.to_string() });
                        }
                    }
                    _ => return (strip_tags(text), vec![]),
                }
                rest = &rest[n..];
            }
            None => {
                let c = rest.chars().next().unwrap();
                plain.push(c);
                u16_len += c.len_utf16() as u32;
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    if !stack.is_empty() {
        return (strip_tags(text), vec![]);
    }
    // Trim marks to exclude leading/trailing whitespace inside them.
    let units: Vec<u16> = plain.encode_utf16().collect();
    for m in &mut marks {
        while m.start_u16 < m.end_u16 && units[m.start_u16 as usize] == b' ' as u16 {
            m.start_u16 += 1;
        }
        while m.end_u16 > m.start_u16 && units[m.end_u16 as usize - 1] == b' ' as u16 {
            m.end_u16 -= 1;
        }
    }
    marks.retain(|m| m.end_u16 > m.start_u16);
    marks.sort_by_key(|m| (m.start_u16, std::cmp::Reverse(m.end_u16)));
    (plain, marks)
}

/// Split parts may cut through a tag pair: close what is open at the end of a part and
/// reopen it at the start of the next, so every part is balanced on its own.
pub fn rebalance_parts(parts: &mut [String]) {
    let mut open: Vec<char> = Vec::new();
    for p in parts.iter_mut() {
        let prefix: String = open.iter().map(|k| format!("<{k}>")).collect();
        let mut state = open.clone();
        let mut rest = p.as_str();
        while let Some(i) = rest.find('<') {
            let tail = &rest[i..];
            let l: String = tail.chars().take(4).collect::<String>().to_ascii_lowercase();
            if l.starts_with("<b>") {
                state.push('b');
            } else if l.starts_with("<i>") {
                state.push('i');
            } else if l.starts_with("<c>") {
                state.push('c');
            } else if l.starts_with("</b>") || l.starts_with("</i>") || l.starts_with("</c>") {
                state.pop();
            }
            rest = &tail[1..];
        }
        let suffix: String = state.iter().rev().map(|k| format!("</{k}>")).collect();
        *p = format!("{prefix}{p}{suffix}");
        open = state;
    }
}

/// Marks from stored revision metadata.
pub fn marks_of(metadata: &serde_json::Value) -> Vec<Mark> {
    metadata.get("marks").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::*;

    fn block(content: Vec<SafeNode>, source: &str, kind: BlockKind) -> Block {
        Block {
            id: "a".into(),
            extraction_id: "e".into(),
            section_id: "s".into(),
            parent_id: None,
            ordinal: 0,
            kind,
            raw_text: source.into(),
            source_text: source.into(),
            source_hash: sha256_hex_str(source),
            content,
            location: SourceLocation::Pdf { page_index: 0, view_box: vec![0.0, 0.0, 1.0, 1.0], rotation: 0, rects: vec![], item_ranges: vec![] },
            quality: Quality { level: QualityLevel::High, reasons: vec![] },
            translation_policy: TranslationPolicy::Auto,
            mapping: BlockMapping::default(),
            roles: vec![],
        }
    }
    fn t(v: &str) -> SafeNode {
        SafeNode::Text { v: v.into() }
    }

    #[test]
    fn leading_bold_term_becomes_inline_tags() {
        let b = block(vec![SafeNode::Strong { c: vec![t("Memory")] }, t(": 16 GB is provided.")], "Memory: 16 GB is provided.", BlockKind::Paragraph);
        assert_eq!(analyze(&b), SourceMarks::Inline("<b>Memory</b>: 16 GB is provided.".into()));
    }

    #[test]
    fn whitespace_between_bold_words_stays_inside_and_text_must_match_source() {
        let b = block(vec![SafeNode::Strong { c: vec![t("Radio"), t("  module")] }, t(" x")], "Radio module x", BlockKind::Paragraph);
        assert_eq!(analyze(&b), SourceMarks::Inline("<b>Radio module</b> x".into()));
        let mismatch = block(vec![SafeNode::Strong { c: vec![t("Radio")] }], "Something else", BlockKind::Paragraph);
        assert_eq!(analyze(&mismatch), SourceMarks::None);
    }

    #[test]
    fn whole_block_and_unsupported_cases() {
        let whole = block(vec![SafeNode::Strong { c: vec![t("All bold")] }], "All bold", BlockKind::ListItem);
        assert_eq!(analyze(&whole), SourceMarks::Whole('b'));
        let plain = block(vec![t("No emphasis")], "No emphasis", BlockKind::Paragraph);
        assert_eq!(analyze(&plain), SourceMarks::None);
        let heading = block(vec![SafeNode::Strong { c: vec![t("A")] }, t(" b")], "A b", BlockKind::Heading);
        assert_eq!(analyze(&heading), SourceMarks::None);
        let literal = block(vec![SafeNode::Strong { c: vec![t("Use")] }, t(" the <b> tag")], "Use the <b> tag", BlockKind::Paragraph);
        assert_eq!(analyze(&literal), SourceMarks::None);
    }

    #[test]
    fn nested_bold_italic_round_trips() {
        let b = block(vec![t("a "), SafeNode::Strong { c: vec![t("b "), SafeNode::Emphasis { c: vec![t("c")] }] }, t(" d")], "a b c d", BlockKind::Paragraph);
        let SourceMarks::Inline(m) = analyze(&b) else { panic!() };
        assert_eq!(m, "a <b>b <i>c</i></b> d");
        let (plain, marks) = extract(&m);
        assert_eq!(plain, "a b c d");
        assert_eq!(marks, vec![Mark { start_u16: 2, end_u16: 5, kind: "b".into() }, Mark { start_u16: 4, end_u16: 5, kind: "i".into() }]);
    }

    #[test]
    fn extract_handles_korean_offsets_and_broken_tags() {
        let (p, m) = extract("<b>메모리</b>: 16GB가 제공됩니다.");
        assert_eq!(p, "메모리: 16GB가 제공됩니다.");
        assert_eq!(m, vec![Mark { start_u16: 0, end_u16: 3, kind: "b".into() }]);
        // unbalanced => translation kept, emphasis dropped
        let (p2, m2) = extract("<b>메모리: 16GB");
        assert_eq!(p2, "메모리: 16GB");
        assert!(m2.is_empty());
        let (p3, m3) = extract("<b>a<i>b</b>c</i>");
        assert_eq!(p3, "abc");
        assert!(m3.is_empty());
        // surrogate pairs count as two UTF-16 units
        let (_, m4) = extract("😀 <b>굵게</b>");
        assert_eq!(m4[0].start_u16, 3);
    }

    #[test]
    fn parts_are_rebalanced() {
        let mut parts = vec!["a <b>bold starts.".to_string(), "still bold</b> end.".to_string()];
        rebalance_parts(&mut parts);
        assert_eq!(parts[0], "a <b>bold starts.</b>");
        assert_eq!(parts[1], "<b>still bold</b> end.");
    }

    #[test]
    fn coloured_words_travel_as_c_tags() {
        let red = |c: Vec<SafeNode>| SafeNode::Color { v: "#b00000".into(), c };
        let b = block(vec![t("See "), red(vec![t("Cloud Versus Self-Hosting")]), t(" for more.")], "See Cloud Versus Self-Hosting for more.", BlockKind::ListItem);
        let (m, accent) = analyze_full(&b);
        assert_eq!(m, SourceMarks::Inline("See <c>Cloud Versus Self-Hosting</c> for more.".into()));
        assert_eq!(accent.as_deref(), Some("#b00000"));
        // colour inside bold: colour is the innermost pair and pairs never cross
        let b = block(vec![t("a "), SafeNode::Strong { c: vec![t("bold "), red(vec![t("red")])] }, t(" z")], "a bold red z", BlockKind::Paragraph);
        assert_eq!(analyze_full(&b).0, SourceMarks::Inline("a <b>bold <c>red</c></b> z".into()));
        let b = block(vec![red(vec![t("red "), SafeNode::Strong { c: vec![t("both")] }]), t(" z")], "red both z", BlockKind::Paragraph);
        let (m, _) = analyze_full(&b);
        assert_eq!(m, SourceMarks::Inline("<c>red </c><b><c>both</c></b> z".into()));
        if let SourceMarks::Inline(tagged) = m {
            let (plain, marks) = extract(&tagged);
            assert_eq!(plain, "red both z");
            assert!(marks.iter().any(|m| m.kind == "b" && (m.start_u16, m.end_u16) == (4, 8)));
            assert_eq!(marks.iter().filter(|m| m.kind == "c").count(), 2);
        }
        // a second colour and malformed values are not carried; one-colour blocks are plain
        let blue = SafeNode::Color { v: "#0000ff".into(), c: vec![t("blue")] };
        let b = block(vec![t("x "), red(vec![t("red")]), t(" "), blue, t(" y")], "x red blue y", BlockKind::Paragraph);
        assert_eq!(analyze_full(&b).0, SourceMarks::Inline("x <c>red</c> blue y".into()));
        let bad = block(vec![t("x "), SafeNode::Color { v: "red;background:url(x)".into(), c: vec![t("y")] }], "x y", BlockKind::Paragraph);
        assert_eq!(analyze_full(&bad), (SourceMarks::None, None));
        let all = block(vec![red(vec![t("all red")])], "all red", BlockKind::Paragraph);
        assert_eq!(analyze_full(&all), (SourceMarks::None, None));
        // translated answer -> ranges; a lost closing tag drops the marks but keeps the text
        let (plain, marks) = extract("자세한 내용은 <c>클라우드 대 자체 호스팅</c>을 보세요.");
        assert_eq!(plain, "자세한 내용은 클라우드 대 자체 호스팅을 보세요.");
        assert_eq!(marks, vec![Mark { start_u16: 8, end_u16: 21, kind: "c".into() }]);
        assert_eq!(extract("앞 <c>뒤"), ("앞 뒤".to_string(), vec![]));
    }
}
