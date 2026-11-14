//! Output validation (spec §6.3).
//!
//! Checks normal termination, size, empty output, tool-call responses, segment count / ids
//! (JSON mode) and placeholder integrity, then restores placeholders. Any failure is
//! `ProviderError::InvalidOutput` (INVALID_OUTPUT, never retried automatically). A target
//! language mismatch is only a warning.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::providers::{ProviderError, ResponseFormat, TranslationOutput, MAX_OUTPUT_BYTES};
use crate::translation::placeholder::{PlaceholderIssue, ProtectedText};

#[derive(Debug, Clone, Copy)]
pub struct ExpectedSegment<'a> {
    pub id: &'a str,
    pub protected: &'a ProtectedText,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatedSegment {
    pub id: String,
    /// Translation with placeholders restored.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValidationWarning {
    LanguageMismatch { segment_id: String, target_language: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatedOutput {
    pub segments: Vec<ValidatedSegment>,
    pub warnings: Vec<ValidationWarning>,
}

fn invalid(reason: &str) -> ProviderError {
    ProviderError::InvalidOutput(reason.to_string())
}

/// Validate a provider output against the expected segments.
pub fn validate_output(
    output: &TranslationOutput,
    format: ResponseFormat,
    expected: &[ExpectedSegment<'_>],
    target_language: &str,
) -> Result<ValidatedOutput, ProviderError> {
    if output.finish_reason != "stop" {
        return Err(ProviderError::Incomplete { finish_reason: output.finish_reason.clone() });
    }
    if output.text.len() > MAX_OUTPUT_BYTES {
        return Err(ProviderError::OutputTooLarge);
    }
    let raw = output.text.trim();
    if raw.is_empty() {
        return Err(invalid("empty_output"));
    }
    if looks_like_tool_call(raw) {
        return Err(invalid("tool_response"));
    }
    if expected.is_empty() {
        return Err(invalid("no_expected_segments"));
    }
    let pairs: Vec<(String, String)> = match format {
        ResponseFormat::JsonSegments => {
            let got = parse_segments_json(raw)?;
            if got.len() != expected.len() {
                return Err(invalid("segment_count"));
            }
            let mut seen = std::collections::HashSet::new();
            for (id, _) in &got {
                if !seen.insert(id.as_str()) {
                    return Err(invalid("segment_id_duplicate"));
                }
            }
            // Ids must match exactly; order follows `expected`.
            expected
                .iter()
                .map(|e| {
                    got.iter()
                        .find(|(id, _)| id == e.id)
                        .map(|(id, t)| (id.clone(), t.clone()))
                        .ok_or_else(|| invalid("segment_id"))
                })
                .collect::<Result<_, _>>()?
        }
        ResponseFormat::Text => {
            if expected.len() != 1 {
                return Err(invalid("segment_count"));
            }
            vec![(expected[0].id.to_string(), raw.to_string())]
        }
    };

    let mut segments = Vec::with_capacity(pairs.len());
    let mut warnings = Vec::new();
    for ((id, text), exp) in pairs.into_iter().zip(expected) {
        let text = text.trim().to_string();
        if text.is_empty() && !exp.protected.text.trim().is_empty() {
            return Err(invalid("empty_segment"));
        }
        let restored = exp.protected.restore(&text).map_err(|issues| invalid(placeholder_reason(&issues)))?;
        // Language heuristic on the text without placeholders.
        let mut plain = text.clone();
        for p in &exp.protected.placeholders {
            plain = plain.replace(p.token.as_str(), " ");
        }
        if language_mismatch(&plain, target_language) {
            warnings.push(ValidationWarning::LanguageMismatch { segment_id: id.clone(), target_language: target_language.to_string() });
        }
        segments.push(ValidatedSegment { id, text: restored });
    }
    Ok(ValidatedOutput { segments, warnings })
}

fn placeholder_reason(issues: &[PlaceholderIssue]) -> &'static str {
    match issues.first() {
        Some(PlaceholderIssue::Missing(_)) => "placeholder_missing",
        Some(PlaceholderIssue::Duplicate(_)) => "placeholder_duplicate",
        Some(PlaceholderIssue::Unknown(_)) => "placeholder_unknown",
        Some(PlaceholderIssue::Mutated(_)) | None => "placeholder_mutated",
    }
}

/// Parse `{"segments":[{"id","text"}]}`, tolerating surrounding whitespace and a single
/// Markdown code fence. Extra keys are ignored.
pub fn parse_segments_json(raw: &str) -> Result<Vec<(String, String)>, ProviderError> {
    let s = strip_code_fence(raw.trim());
    let v: Value = serde_json::from_str(s).map_err(|_| invalid("malformed_json"))?;
    let arr = v.get("segments").and_then(Value::as_array).ok_or_else(|| invalid("missing_segments"))?;
    arr.iter()
        .map(|seg| {
            let id = seg.get("id").and_then(Value::as_str).ok_or_else(|| invalid("segment_id"))?;
            let text = seg.get("text").and_then(Value::as_str).ok_or_else(|| invalid("segment_text"))?;
            Ok((id.to_string(), text.to_string()))
        })
        .collect()
}

fn strip_code_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else { return s };
    let rest = rest.strip_prefix("json").or_else(|| rest.strip_prefix("JSON")).unwrap_or(rest);
    rest.strip_suffix("```").map(str::trim).unwrap_or(s)
}

/// Heuristic detection of tool / function-call shaped responses.
pub fn looks_like_tool_call(raw: &str) -> bool {
    let s = raw.trim_start();
    if s.starts_with("<tool_call>") || s.starts_with("<function_call") || s.starts_with("<|tool_call") {
        return true;
    }
    if s.starts_with('{') || s.starts_with("```") {
        if let Ok(v) = serde_json::from_str::<Value>(strip_code_fence(s)) {
            return v.get("tool_calls").is_some()
                || v.get("function_call").is_some()
                || (v.get("name").is_some() && (v.get("arguments").is_some() || v.get("parameters").is_some()));
        }
    }
    false
}

/// True when the text clearly is not in the target script. Only a few scripts are checked;
/// other languages never warn. Short texts never warn.
pub fn language_mismatch(text: &str, target_language: &str) -> bool {
    let lang = target_language.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    let in_script: fn(char) -> bool = match lang.as_str() {
        "ko" => |c| matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}'),
        "ja" => |c| matches!(c, '\u{3040}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}'),
        "zh" => |c| matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}'),
        "en" | "fr" | "de" | "es" | "it" | "pt" | "nl" | "sv" | "da" | "no" | "nb" | "fi" | "pl" | "cs" | "vi" | "id" | "tr" => {
            |c| c.is_alphabetic() && (c.is_ascii_alphabetic() || ('\u{00C0}'..='\u{024F}').contains(&c) || ('\u{1E00}'..='\u{1EFF}').contains(&c))
        }
        _ => return false,
    };
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() < 4 {
        return false;
    }
    let hits = letters.iter().filter(|&&c| in_script(c)).count();
    (hits as f64) / (letters.len() as f64) < 0.3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translation::placeholder::protect;

    fn out(text: &str) -> TranslationOutput {
        TranslationOutput { text: text.into(), finish_reason: "stop".into(), sent_bytes: 0, usage: None }
    }

    #[test]
    fn json_segments_ok_and_restored() {
        let p0 = protect("Run `ls` now", &[]).unwrap();
        let p1 = protect("Second", &[]).unwrap();
        let exp = [ExpectedSegment { id: "s0", protected: &p0 }, ExpectedSegment { id: "s1", protected: &p1 }];
        let raw = r#"```json
{"segments":[{"id":"s1","text":"두 번째"},{"id":"s0","text":"지금 ⟦P1⟧ 실행"}]}
```"#;
        let v = validate_output(&out(raw), ResponseFormat::JsonSegments, &exp, "ko").unwrap();
        assert_eq!(v.segments[0], ValidatedSegment { id: "s0".into(), text: "지금 `ls` 실행".into() });
        assert_eq!(v.segments[1].text, "두 번째");
        assert!(v.warnings.is_empty());
    }

    #[test]
    fn json_failures() {
        let p0 = protect("Run `ls` now", &[]).unwrap();
        let exp = [ExpectedSegment { id: "s0", protected: &p0 }];
        let cases = [
            (r#"{"segments":[]}"#, "segment_count"),
            (r#"{"segments":[{"id":"s9","text":"⟦P1⟧"}]}"#, "segment_id"),
            (r#"{"segments":[{"id":"s0","text":"실행"}]}"#, "placeholder_missing"),
            (r#"{"segments":[{"id":"s0","text":"⟦P1⟧ ⟦P1⟧"}]}"#, "placeholder_duplicate"),
            (r#"{"segments":[{"id":"s0","text":"⟦P 1⟧ 실행"}]}"#, "placeholder_missing"),
            (r#"{"segments":[{"id":"s0","text":""}]}"#, "empty_segment"),
            (r#"{"segments":[{"id":"s0","text":"x"#, "malformed_json"),
            (r#"{"tool_calls":[{"function":{"name":"x"}}]}"#, "tool_response"),
            ("   ", "empty_output"),
        ];
        for (raw, reason) in cases {
            assert_eq!(
                validate_output(&out(raw), ResponseFormat::JsonSegments, &exp, "ko"),
                Err(ProviderError::InvalidOutput(reason.into())),
                "{raw}"
            );
        }
        let mut o = out("⟦P1⟧");
        o.finish_reason = "length".into();
        assert!(matches!(
            validate_output(&o, ResponseFormat::JsonSegments, &exp, "ko"),
            Err(ProviderError::Incomplete { .. })
        ));
        let big = out(&"가".repeat(MAX_OUTPUT_BYTES / 3 + 1));
        assert_eq!(validate_output(&big, ResponseFormat::Text, &exp, "ko"), Err(ProviderError::OutputTooLarge));
    }

    #[test]
    fn text_mode_and_language_warning() {
        let p = protect("See https://a.example for details", &[]).unwrap();
        let exp = [ExpectedSegment { id: "s0", protected: &p }];
        let v = validate_output(&out("  자세한 내용은 ⟦P1⟧ 참조 \n"), ResponseFormat::Text, &exp, "ko").unwrap();
        assert_eq!(v.segments[0].text, "자세한 내용은 https://a.example 참조");
        let v = validate_output(&out("See ⟦P1⟧ for more details"), ResponseFormat::Text, &exp, "ko").unwrap();
        assert_eq!(v.warnings.len(), 1);
        assert!(validate_output(&out("<tool_call>{}</tool_call>"), ResponseFormat::Text, &exp, "ko").is_err());
        assert!(!language_mismatch("안녕하세요 world", "ko"));
        assert!(language_mismatch("Hello there friend", "ko-KR"));
        assert!(!language_mismatch("Hi", "ko"));
        assert!(!language_mismatch("Hello there", "xx"));
        assert!(language_mismatch("안녕하세요 여러분", "en"));
    }
}
