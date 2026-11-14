//! Deterministic BlockPart splitter (spec §6.3).
//!
//! A block's `sourceText` is split into contiguous parts of at most `max_u16` UTF-16 code
//! units (default 2,000) and, optionally, at most `max_utf8_bytes` bytes (the conservative
//! token estimate when no tokenizer is available: 1 token ≤ 1 UTF-8 byte). Cut points are
//! chosen from sentence boundaries (UAX #29), then line breaks, then whitespace, then
//! grapheme cluster boundaries — never inside a grapheme or a protected (placeholder) span.
//! If a single grapheme or protected span is larger than the budget the part exceeds the
//! budget rather than cutting it. Parts concatenate back to the exact source.
//! Version: [`crate::domain::versions::SPLITTER`].

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

use crate::error::{AppError, ErrorCode};
use crate::translation::placeholder::{detect_spans, ProtectSpan};

pub const DEFAULT_PART_MAX_U16: u32 = 2000;
/// Smallest input budget (tokens) we accept before reporting CONTEXT_TOO_LARGE.
pub const MIN_INPUT_TOKENS: u32 = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitPart {
    pub index: u32,
    pub start_u16: u32,
    pub end_u16: u32,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitBudget {
    pub max_u16: u32,
    pub max_utf8_bytes: Option<u32>,
}

impl Default for SplitBudget {
    fn default() -> Self {
        Self { max_u16: DEFAULT_PART_MAX_U16, max_utf8_bytes: None }
    }
}

impl SplitBudget {
    pub fn u16(max_u16: u32) -> Self {
        Self { max_u16: max_u16.max(1), max_utf8_bytes: None }
    }

    /// Budget for a model: input tokens = context_limit − prompt/context overhead − output
    /// budget, estimated as UTF-8 bytes. Unknown context limit → default u16 budget only
    /// (the UI must ask the user for a limit).
    pub fn for_model(
        context_limit: Option<u32>,
        overhead_tokens: u32,
        max_output_tokens: u32,
    ) -> Result<Self, AppError> {
        let Some(limit) = context_limit else { return Ok(Self::default()) };
        let avail = limit.saturating_sub(overhead_tokens).saturating_sub(max_output_tokens);
        if avail < MIN_INPUT_TOKENS {
            return Err(AppError::new(
                ErrorCode::ContextTooLarge,
                "모델 문맥 한도가 너무 작습니다. 문맥 한도 또는 출력 길이 설정을 확인해 주세요.",
            ));
        }
        Ok(Self { max_u16: DEFAULT_PART_MAX_U16, max_utf8_bytes: Some(avail) })
    }
}

/// Conservative token estimate without a tokenizer: UTF-8 byte count.
pub fn estimate_tokens(text: &str) -> u32 {
    text.len().min(u32::MAX as usize) as u32
}

/// Split using auto-detected placeholder spans as no-cut zones.
pub fn split_for_translation(text: &str, budget: SplitBudget) -> Vec<SplitPart> {
    split_source(text, budget, &detect_spans(text))
}

/// Split `text`; `avoid` spans (UTF-16) are never cut.
pub fn split_source(text: &str, budget: SplitBudget, avoid: &[ProtectSpan]) -> Vec<SplitPart> {
    if text.is_empty() {
        return Vec::new();
    }
    let max_u16 = budget.max_u16.max(1) as usize;
    let max_bytes = budget.max_utf8_bytes.map(|b| b.max(1) as usize).unwrap_or(usize::MAX);

    // u16 offset for each char boundary (byte -> u16 via binary search).
    let mut bounds: Vec<(usize, usize)> = Vec::with_capacity(text.len() + 1);
    let mut u = 0usize;
    for (b, ch) in text.char_indices() {
        bounds.push((b, u));
        u += ch.len_utf16();
    }
    bounds.push((text.len(), u));
    let to_u16 = |byte: usize| -> usize {
        let i = bounds.binary_search_by_key(&byte, |x| x.0).expect("char boundary");
        bounds[i].1
    };

    // Avoid zones in bytes.
    let zones: Vec<(usize, usize)> = avoid
        .iter()
        .filter_map(|s| {
            let a = bounds.iter().find(|x| x.1 == s.start_u16 as usize)?.0;
            let b = bounds.iter().find(|x| x.1 == s.end_u16 as usize)?.0;
            (a < b).then_some((a, b))
        })
        .collect();
    let allowed = |b: usize| !zones.iter().any(|&(s, e)| s < b && b < e);

    let graphemes: Vec<usize> = text.grapheme_indices(true).map(|(i, _)| i).skip(1).chain([text.len()]).collect();
    let is_grapheme = |b: usize| graphemes.binary_search(&b).is_ok();
    let sentences: Vec<usize> = text
        .split_sentence_bound_indices()
        .map(|(i, _)| i)
        .skip(1)
        .filter(|&b| is_grapheme(b))
        .collect();
    let lines: Vec<usize> = text
        .match_indices('\n')
        .map(|(i, _)| i + 1)
        .filter(|&b| b < text.len() && is_grapheme(b))
        .collect();
    let spaces: Vec<usize> = text
        .char_indices()
        .zip(text.char_indices().skip(1))
        .filter(|((_, a), (_, b))| a.is_whitespace() && !b.is_whitespace())
        .map(|(_, (i, _))| i)
        .filter(|&b| is_grapheme(b))
        .collect();
    let tiers: [&[usize]; 4] = [&sentences, &lines, &spaces, &graphemes];

    let fits = |a: usize, b: usize| to_u16(b) - to_u16(a) <= max_u16 && b - a <= max_bytes;

    let mut parts = Vec::new();
    let mut pos = 0usize;
    while pos < text.len() {
        let cut = if fits(pos, text.len()) {
            text.len()
        } else {
            // Largest grapheme boundary within budget.
            let limit = graphemes.iter().copied().skip_while(|&g| g <= pos).take_while(|&g| fits(pos, g)).last();
            let mut chosen = None;
            if let Some(limit) = limit {
                for tier in tiers {
                    if let Some(c) = tier.iter().copied().rev().find(|&c| c > pos && c <= limit && allowed(c)) {
                        chosen = Some(c);
                        break;
                    }
                }
            }
            // Oversized grapheme / protected span: take the first allowed boundary after it.
            chosen.unwrap_or_else(|| graphemes.iter().copied().find(|&g| g > pos && allowed(g)).unwrap_or(text.len()))
        };
        parts.push(SplitPart {
            index: parts.len() as u32,
            start_u16: to_u16(pos) as u32,
            end_u16: to_u16(cut) as u32,
            text: text[pos..cut].to_string(),
        });
        pos = cut;
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::utf16;

    fn check_invariants(text: &str, parts: &[SplitPart]) {
        let joined: String = parts.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(joined, text);
        let mut prev = 0;
        for (i, p) in parts.iter().enumerate() {
            assert_eq!(p.index as usize, i);
            assert_eq!(p.start_u16, prev);
            assert!(p.end_u16 > p.start_u16);
            assert_eq!(utf16::slice_u16(text, p.start_u16, p.end_u16), Some(p.text.as_str()));
            prev = p.end_u16;
        }
        assert_eq!(prev, utf16::len_u16(text));
    }

    #[test]
    fn single_part_when_small() {
        let parts = split_for_translation("Short paragraph.", SplitBudget::default());
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].end_u16, 16);
        assert!(split_for_translation("", SplitBudget::default()).is_empty());
    }

    #[test]
    fn sentence_boundaries_preferred() {
        let text = "First sentence here. Second one is longer here. Third.";
        let parts = split_source(text, SplitBudget::u16(30), &[]);
        check_invariants(text, &parts);
        assert_eq!(parts[0].text, "First sentence here. ");
        assert_eq!(parts[1].text, "Second one is longer here. ");
        assert_eq!(parts[2].text, "Third.");
    }

    #[test]
    fn falls_back_to_lines_spaces_graphemes() {
        let text = "line one without stop\nline two without stop";
        let parts = split_source(text, SplitBudget::u16(25), &[]);
        check_invariants(text, &parts);
        assert_eq!(parts[0].text, "line one without stop\n");

        let text = "aaaa bbbb cccc dddd";
        let parts = split_source(text, SplitBudget::u16(12), &[]);
        check_invariants(text, &parts);
        assert_eq!(parts[0].text, "aaaa bbbb ");

        let text = "abcdefghij";
        let parts = split_source(text, SplitBudget::u16(4), &[]);
        check_invariants(text, &parts);
        assert_eq!(parts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn never_splits_graphemes_emoji_combining() {
        // family emoji (ZWJ sequence, 11 u16), flag (4 u16), e + combining acute
        let text = "👨‍👩‍👧‍👦🇰🇷e\u{301}e\u{301}e\u{301}한국어😀😀";
        for budget in 1..=14 {
            let parts = split_source(text, SplitBudget::u16(budget), &[]);
            check_invariants(text, &parts);
            for p in &parts {
                // every part starts and ends at grapheme boundaries
                let gs: Vec<&str> = p.text.graphemes(true).collect();
                assert_eq!(gs.concat(), p.text);
                assert!(!p.text.starts_with('\u{301}') && !p.text.starts_with('\u{200d}'));
                if p.text.graphemes(true).count() > 1 {
                    assert!(p.end_u16 - p.start_u16 <= budget, "budget {budget}: {:?}", p.text);
                }
            }
        }
    }

    #[test]
    fn placeholders_not_split() {
        let text = "See https://example.com/very/long/path/here now";
        let parts = split_for_translation(text, SplitBudget::u16(20));
        check_invariants(text, &parts);
        assert!(parts.iter().any(|p| p.text.contains("https://example.com/very/long/path/here")));
    }

    #[test]
    fn byte_budget_for_cjk() {
        let text = "가나다라마바사아자차카타파하";
        let parts = split_source(text, SplitBudget { max_u16: 2000, max_utf8_bytes: Some(12) }, &[]);
        check_invariants(text, &parts);
        assert!(parts.iter().all(|p| p.text.len() <= 12));
        assert_eq!(parts.len(), 4);
        assert_eq!(estimate_tokens("가a"), 4);
    }

    #[test]
    fn deterministic_and_default_budget() {
        let sentence = "This is a reasonably long sentence used for splitting tests. ";
        let text = sentence.repeat(100); // 6,100 u16
        let a = split_for_translation(&text, SplitBudget::default());
        let b = split_for_translation(&text, SplitBudget::default());
        assert_eq!(a, b);
        check_invariants(&text, &a);
        assert!(a.len() >= 4);
        assert!(a.iter().all(|p| p.end_u16 - p.start_u16 <= 2000));
        assert!(a.iter().all(|p| p.text.ends_with(". ")));
    }

    #[test]
    fn model_budget() {
        assert_eq!(SplitBudget::for_model(None, 500, 1000).unwrap(), SplitBudget::default());
        assert_eq!(SplitBudget::for_model(Some(4096), 1000, 1000).unwrap().max_utf8_bytes, Some(2096));
        assert_eq!(SplitBudget::for_model(Some(1000), 500, 480).unwrap_err().code, ErrorCode::ContextTooLarge);
    }
}
