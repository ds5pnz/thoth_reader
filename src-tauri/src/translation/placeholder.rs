//! Placeholder protection (spec §6.3).
//!
//! Code spans, formulas, URLs, inline code and caller-marked non-translatable strings are
//! replaced with collision-checked tokens such as `⟦P1⟧` before translation and restored
//! afterwards. A token scheme is used only if its delimiters never occur in the source;
//! otherwise the next scheme is tried. Validation detects missing, duplicated, unknown and
//! mutated tokens. Version: [`crate::domain::versions::PLACEHOLDER`].

use serde::{Deserialize, Serialize};

use crate::domain::utf16;
use crate::domain::versions;
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceholderKind {
    InlineCode,
    Formula,
    Url,
    NonTranslatable,
}

/// A range (UTF-16, in the source text) to protect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtectSpan {
    pub start_u16: u32,
    pub end_u16: u32,
    pub kind: PlaceholderKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Placeholder {
    pub token: String,
    pub original: String,
    pub kind: PlaceholderKind,
    pub start_u16: u32,
    pub end_u16: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtectedText {
    pub version: String,
    /// Token delimiters and id prefix, e.g. ("⟦", "⟧", "P").
    pub open: String,
    pub close: String,
    pub prefix: String,
    /// The delimiters never occur in the source, so any stray delimiter in the output is a
    /// mutated token.
    pub clean_delimiters: bool,
    /// Source with protected spans replaced by tokens (what is sent to the model).
    pub text: String,
    pub placeholders: Vec<Placeholder>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "token", rename_all = "snake_case")]
pub enum PlaceholderIssue {
    Missing(String),
    Duplicate(String),
    Unknown(String),
    Mutated(String),
}

const SCHEMES: &[(&str, &str)] = &[("⟦", "⟧"), ("⟪", "⟫"), ("⦃", "⦄"), ("〖", "〗")];
const MAX_INLINE_MATH_CHARS: usize = 300;

impl ProtectedText {
    pub fn token_for(&self, n: usize) -> String {
        format!("{}{}{}{}", self.open, self.prefix, n, self.close)
    }

    /// Example token for prompts (`⟦P1⟧`).
    pub fn example_token(&self) -> String {
        self.token_for(1)
    }

    pub fn has_placeholders(&self) -> bool {
        !self.placeholders.is_empty()
    }

    /// Validate placeholder integrity of a model output.
    pub fn check(&self, translated: &str) -> Vec<PlaceholderIssue> {
        let mut issues = Vec::new();
        let mut rest = translated.to_string();
        for p in &self.placeholders {
            let n = translated.matches(p.token.as_str()).count();
            match n {
                0 => issues.push(PlaceholderIssue::Missing(p.token.clone())),
                1 => {}
                _ => issues.push(PlaceholderIssue::Duplicate(p.token.clone())),
            }
            rest = rest.replace(p.token.as_str(), "");
        }
        // Leftovers: unknown well-formed tokens or broken fragments.
        let marker = format!("{}{}", self.open, self.prefix);
        let mut search = rest.as_str();
        while let Some(i) = search.find(&marker) {
            let after = &search[i + marker.len()..];
            let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            let tail = &after[digits.len()..];
            if !digits.is_empty() && tail.starts_with(self.close.as_str()) {
                issues.push(PlaceholderIssue::Unknown(format!("{marker}{digits}{}", self.close)));
            } else {
                issues.push(PlaceholderIssue::Mutated(format!("{marker}{digits}")));
            }
            search = after;
        }
        if self.clean_delimiters {
            let without_marker = rest.replace(&marker, "");
            let stray_open = without_marker.contains(self.open.as_str());
            let stray_close = without_marker.matches(self.close.as_str()).count()
                > issues.iter().filter(|i| matches!(i, PlaceholderIssue::Unknown(_))).count();
            if stray_open || stray_close {
                issues.push(PlaceholderIssue::Mutated("stray delimiter".into()));
            }
        }
        issues
    }

    /// Restore originals. Fails with the list of issues if integrity is violated.
    pub fn restore(&self, translated: &str) -> Result<String, Vec<PlaceholderIssue>> {
        let issues = self.check(translated);
        if !issues.is_empty() {
            return Err(issues);
        }
        if self.placeholders.is_empty() {
            return Ok(translated.to_string());
        }
        // Single pass so restored originals are never rescanned.
        let mut out = String::with_capacity(translated.len());
        let mut i = 0;
        let bytes = translated.as_bytes();
        'outer: while i < bytes.len() {
            if translated[i..].starts_with(self.open.as_str()) {
                for p in &self.placeholders {
                    if translated[i..].starts_with(p.token.as_str()) {
                        out.push_str(&p.original);
                        i += p.token.len();
                        continue 'outer;
                    }
                }
            }
            let ch = translated[i..].chars().next().expect("char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
        Ok(out)
    }
}

/// Protect auto-detected spans plus caller-provided `extra` spans (which win on overlap).
pub fn protect(text: &str, extra: &[ProtectSpan]) -> AppResult<ProtectedText> {
    let mut spans: Vec<(usize, usize, PlaceholderKind, u8)> = Vec::new();
    for s in extra {
        let (Some(a), Some(b)) = (utf16::u16_to_byte(text, s.start_u16), utf16::u16_to_byte(text, s.end_u16)) else {
            return Err(AppError::invalid("보존 구간 범위가 올바르지 않습니다."));
        };
        if a >= b {
            return Err(AppError::invalid("보존 구간 범위가 올바르지 않습니다."));
        }
        spans.push((a, b, s.kind, 0));
    }
    for (a, b, k) in detect_byte_spans(text) {
        spans.push((a, b, k, 1));
    }
    protect_byte_spans(text, spans)
}

/// Auto-detected spans only (UTF-16 offsets), sorted and non-overlapping.
pub fn detect_spans(text: &str) -> Vec<ProtectSpan> {
    detect_byte_spans(text)
        .into_iter()
        .map(|(a, b, kind)| ProtectSpan {
            start_u16: utf16::byte_to_u16(text, a).unwrap_or(0),
            end_u16: utf16::byte_to_u16(text, b).unwrap_or(0),
            kind,
        })
        .collect()
}

fn protect_byte_spans(text: &str, mut spans: Vec<(usize, usize, PlaceholderKind, u8)>) -> AppResult<ProtectedText> {
    // Priority: earlier start, then caller spans, then longer.
    spans.sort_by(|x, y| x.0.cmp(&y.0).then(x.3.cmp(&y.3)).then(y.1.cmp(&x.1)));
    let mut accepted: Vec<(usize, usize, PlaceholderKind)> = Vec::new();
    for (a, b, k, _) in spans {
        if accepted.last().is_some_and(|l| a < l.1) {
            continue;
        }
        accepted.push((a, b, k));
    }
    let (open, close, prefix, clean) = choose_scheme(text, accepted.len())?;
    let mut out = String::with_capacity(text.len());
    let mut placeholders = Vec::with_capacity(accepted.len());
    let mut last = 0;
    for (n, (a, b, kind)) in accepted.into_iter().enumerate() {
        out.push_str(&text[last..a]);
        let token = format!("{open}{prefix}{}{close}", n + 1);
        out.push_str(&token);
        placeholders.push(Placeholder {
            token,
            original: text[a..b].to_string(),
            kind,
            start_u16: utf16::byte_to_u16(text, a).unwrap_or(0),
            end_u16: utf16::byte_to_u16(text, b).unwrap_or(0),
        });
        last = b;
    }
    out.push_str(&text[last..]);
    Ok(ProtectedText {
        version: versions::PLACEHOLDER.to_string(),
        open: open.to_string(),
        close: close.to_string(),
        prefix,
        clean_delimiters: clean,
        text: out,
        placeholders,
    })
}

/// Pick the first scheme whose delimiters never occur in the source. If every scheme
/// collides, fall back to the first scheme with a prefix whose marker (`⟦Q`) and tokens
/// do not occur in the source.
fn choose_scheme(text: &str, count: usize) -> AppResult<(&'static str, &'static str, String, bool)> {
    for (o, c) in SCHEMES {
        if !text.contains(o) && !text.contains(c) {
            return Ok((o, c, "P".to_string(), true));
        }
    }
    let (o, c) = SCHEMES[0];
    for prefix in ["P", "Q", "R", "X", "Y", "Z", "PH", "TPH"] {
        let marker = format!("{o}{prefix}");
        if text.contains(&marker) {
            continue;
        }
        if (1..=count).all(|n| !text.contains(&format!("{marker}{n}{c}"))) {
            return Ok((o, c, prefix.to_string(), false));
        }
    }
    Err(AppError::invalid("보존 구간 표시를 만들 수 없습니다."))
}

/// Detect inline code, formulas and URLs. Byte offsets, sorted, non-overlapping.
fn detect_byte_spans(text: &str) -> Vec<(usize, usize, PlaceholderKind)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // Backtick code spans: a run of N backticks closed by a run of exactly N.
        if c == b'`' {
            let n = run_len(b, i, b'`');
            if let Some(end) = find_backtick_close(b, i + n, n) {
                out.push((i, end, PlaceholderKind::InlineCode));
                i = end;
                continue;
            }
            i += n;
            continue;
        }
        // Display math $$...$$
        if c == b'$' && b.get(i + 1) == Some(&b'$') {
            if let Some(rel) = text[i + 2..].find("$$") {
                if rel > 0 {
                    let end = i + 2 + rel + 2;
                    out.push((i, end, PlaceholderKind::Formula));
                    i = end;
                    continue;
                }
            }
            i += 2;
            continue;
        }
        // \( ... \) and \[ ... \]
        if c == b'\\' && matches!(b.get(i + 1), Some(b'(') | Some(b'[')) {
            let close = if b[i + 1] == b'(' { "\\)" } else { "\\]" };
            if let Some(rel) = text[i + 2..].find(close) {
                let end = i + 2 + rel + 2;
                out.push((i, end, PlaceholderKind::Formula));
                i = end;
                continue;
            }
        }
        // Inline math $...$ (not currency: no digit/space right after the opening $).
        if c == b'$' {
            if let Some(end) = find_inline_math(text, i) {
                out.push((i, end, PlaceholderKind::Formula));
                i = end;
                continue;
            }
        }
        // URLs at a word start.
        if (c == b'h' || c == b'w') && (i == 0 || !is_word_byte(b[i - 1])) {
            let rest = &text[i..];
            let starts = rest.starts_with("https://") || rest.starts_with("http://") || rest.starts_with("www.");
            if starts {
                let end = url_end(text, i);
                let min = if rest.starts_with("www.") { 5 } else { 9 };
                if end - i >= min {
                    out.push((i, end, PlaceholderKind::Url));
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn run_len(b: &[u8], i: usize, ch: u8) -> usize {
    b[i..].iter().take_while(|&&x| x == ch).count()
}

fn find_backtick_close(b: &[u8], mut i: usize, n: usize) -> Option<usize> {
    let start = i;
    while i < b.len() {
        if b[i] == b'`' {
            let m = run_len(b, i, b'`');
            if m == n && i > start {
                return Some(i + m);
            }
            i += m;
        } else {
            i += 1;
        }
    }
    None
}

fn find_inline_math(text: &str, i: usize) -> Option<usize> {
    let after = &text[i + 1..];
    let first = after.chars().next()?;
    if first.is_whitespace() || first.is_ascii_digit() || first == '$' {
        return None;
    }
    let mut prev = first;
    for (off, ch) in after.char_indices().skip(1) {
        if off > MAX_INLINE_MATH_CHARS * 4 || ch == '\n' {
            return None;
        }
        if ch == '$' {
            if prev.is_whitespace() || prev == '\\' {
                return None;
            }
            let next = after[off + 1..].chars().next();
            if next.is_some_and(|n| n.is_ascii_digit()) {
                return None;
            }
            return Some(i + 1 + off + 1);
        }
        prev = ch;
    }
    None
}

fn url_end(text: &str, start: usize) -> usize {
    let mut end = start;
    for (off, ch) in text[start..].char_indices() {
        if ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '`' | '\u{3000}') || (ch as u32) >= 0x2E80 {
            break;
        }
        end = start + off + ch.len_utf8();
    }
    // Trim trailing punctuation and unbalanced closing brackets.
    loop {
        let s = &text[start..end];
        let Some(last) = s.chars().last() else { break };
        let trim = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' => true,
            ')' => s.matches('(').count() < s.matches(')').count(),
            ']' => s.matches('[').count() < s.matches(']').count(),
            '}' => s.matches('{').count() < s.matches('}').count(),
            _ => false,
        };
        if !trim {
            break;
        }
        end -= last.len_utf8();
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn originals(p: &ProtectedText) -> Vec<&str> {
        p.placeholders.iter().map(|x| x.original.as_str()).collect()
    }

    #[test]
    fn detects_and_restores() {
        let src = "Run `cargo test` and see https://example.com/a_(b). Energy $E=mc^2$ costs $5 or $10. \\(x+1\\) and $$\\sum x$$.";
        let p = protect(src, &[]).unwrap();
        assert_eq!(
            originals(&p),
            vec!["`cargo test`", "https://example.com/a_(b)", "$E=mc^2$", "\\(x+1\\)", "$$\\sum x$$"]
        );
        assert_eq!(p.open, "⟦");
        assert!(p.text.starts_with("Run ⟦P1⟧ and see ⟦P2⟧. Energy ⟦P3⟧ costs $5 or $10."));
        // Translation reorders tokens; restoration is exact.
        let translated = "⟦P2⟧ 참조. ⟦P1⟧ 실행. 에너지 ⟦P3⟧, ⟦P4⟧ 및 ⟦P5⟧.";
        let restored = p.restore(translated).unwrap();
        assert_eq!(
            restored,
            "https://example.com/a_(b) 참조. `cargo test` 실행. 에너지 $E=mc^2$, \\(x+1\\) 및 $$\\sum x$$."
        );
        // u16 offsets point back into the source.
        for ph in &p.placeholders {
            assert_eq!(utf16::slice_u16(src, ph.start_u16, ph.end_u16), Some(ph.original.as_str()));
        }
    }

    #[test]
    fn url_trailing_punctuation_and_cjk() {
        let p = protect("See (https://a.example/x?y=1). 링크 www.example.org에서", &[]).unwrap();
        assert_eq!(originals(&p), vec!["https://a.example/x?y=1", "www.example.org"]);
    }

    #[test]
    fn collision_switches_scheme() {
        let src = "Literal ⟦P1⟧ text with `code`";
        let p = protect(src, &[]).unwrap();
        assert_eq!(p.open, "⟪");
        assert!(p.clean_delimiters);
        assert_eq!(p.text, "Literal ⟦P1⟧ text with ⟪P1⟫");
        assert_eq!(p.restore("문자 ⟦P1⟧ 와 ⟪P1⟫").unwrap(), "문자 ⟦P1⟧ 와 `code`");

        let all = "⟦ ⟧ ⟪ ⟫ ⦃ ⦄ 〖 〗 ⟦P1⟧ `x`";
        let p = protect(all, &[]).unwrap();
        assert!(!p.clean_delimiters);
        assert_eq!(p.prefix, "Q");
        assert_eq!(p.placeholders[0].token, "⟦Q1⟧");
        assert!(!all.contains(&p.placeholders[0].token));
        assert_eq!(p.restore("⟦ ⟧ ⟪ ⟫ ⦃ ⦄ 〖 〗 ⟦P1⟧ ⟦Q1⟧").unwrap(), all);
    }

    #[test]
    fn integrity_issues() {
        let p = protect("a `x` b `y` c", &[]).unwrap();
        assert_eq!(p.check("⟦P1⟧ ⟦P2⟧"), vec![]);
        assert_eq!(p.check("⟦P1⟧"), vec![PlaceholderIssue::Missing("⟦P2⟧".into())]);
        assert_eq!(p.check("⟦P1⟧ ⟦P1⟧ ⟦P2⟧"), vec![PlaceholderIssue::Duplicate("⟦P1⟧".into())]);
        assert_eq!(p.check("⟦P1⟧ ⟦P2⟧ ⟦P3⟧"), vec![PlaceholderIssue::Unknown("⟦P3⟧".into())]);
        let mutated = p.check("⟦P1⟧ ⟦ P2⟧");
        assert!(mutated.contains(&PlaceholderIssue::Missing("⟦P2⟧".into())));
        assert!(mutated.iter().any(|i| matches!(i, PlaceholderIssue::Mutated(_))));
        let mutated = p.check("⟦P1⟧ P2⟧");
        assert!(mutated.iter().any(|i| matches!(i, PlaceholderIssue::Mutated(_))));
        let mutated = p.check("⟦P1⟧ ⟦P2");
        assert!(mutated.iter().any(|i| matches!(i, PlaceholderIssue::Mutated(_))));
        assert!(p.restore("⟦P1⟧").is_err());
    }

    #[test]
    fn extra_spans_and_validation() {
        let src = "Use Thoth Reader 😀 daily";
        // "Thoth Reader" = u16 4..16
        let p = protect(src, &[ProtectSpan { start_u16: 4, end_u16: 16, kind: PlaceholderKind::NonTranslatable }]).unwrap();
        assert_eq!(p.text, "Use ⟦P1⟧ 😀 daily");
        assert!(protect(src, &[ProtectSpan { start_u16: 18, end_u16: 19, kind: PlaceholderKind::NonTranslatable }]).is_err());
        let none = protect("plain text, $ 5 and 3$", &[]).unwrap();
        assert!(none.placeholders.is_empty());
        assert_eq!(none.text, "plain text, $ 5 and 3$");
    }

    #[test]
    fn deterministic() {
        let s = "x `a` y https://e.com z";
        assert_eq!(protect(s, &[]).unwrap(), protect(s, &[]).unwrap());
        assert_eq!(detect_spans(s).len(), 2);
    }
}
