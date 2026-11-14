//! Build the immutable per-block request snapshot: profile context, glossary matches,
//! placeholders, deterministic split, exact provider payloads and the cache descriptor.
//! What is hashed is exactly what is sent (spec §7.4).

use super::cache_key::{self, DescriptorInput};
use super::placeholder::{self, ProtectedText};
use super::prompt::{self, GlossaryEntry, ManifestPart, PromptContext, PromptInput, PromptSegment};
use super::splitter::{self, SplitBudget};
use super::validation::{self, ExpectedSegment};
use crate::document::session::Session;
use crate::domain::canonical_json::canonical_sha256;
use crate::domain::{new_uuid, now_ms, Block, BlockKind};
use crate::error::{AppError, AppResult};
use crate::providers::{
    GenerationOptions, ProviderCapabilities, ProviderConfig, ProviderKind, ResponseFormat, TranslationOutput, TranslationRequest,
};
use crate::storage::repo::{documents, extraction, misc, translations};
use crate::storage::Db;
use std::collections::HashMap;

pub const SEGMENT_ID: &str = "s0";

pub fn provider_config(row: &translations::ProviderRow) -> AppResult<ProviderConfig> {
    let kind = ProviderKind::parse(&row.kind).ok_or_else(|| AppError::invalid("Provider 종류가 올바르지 않습니다."))?;
    let caps: Option<ProviderCapabilities> = row.capabilities.get("probed").and_then(|v| serde_json::from_value(v.clone()).ok());
    let cfg = ProviderConfig {
        id: row.id.clone(),
        kind,
        endpoint: row.endpoint.clone(),
        account_namespace: row.account_namespace.clone(),
        keychain_ref: row.keychain_ref.clone(),
        allow_private_http: row.capabilities.get("allowPrivateHttp").and_then(|v| v.as_bool()).unwrap_or(false),
        concurrency: row.capabilities.get("concurrency").and_then(|v| v.as_u64()).map(|v| v as u8).unwrap_or(kind.default_concurrency()).clamp(1, 4),
        capabilities: caps,
    };
    cfg.validate()?;
    Ok(cfg)
}

/// New immutable profile (or the identical existing one).
#[allow(clippy::too_many_arguments)]
pub async fn create_profile(
    db: &Db,
    row: &translations::ProviderRow,
    model_id: &str,
    src: &str,
    tgt: &str,
    temperature: Option<f64>,
    seed: Option<i64>,
    max_output_tokens: Option<u32>,
    context_limit: Option<u32>,
    model_digest: Option<&str>,
) -> AppResult<translations::ProfileRow> {
    let cfg = provider_config(row)?;
    let caps = cfg.effective_capabilities();
    let snapshot = serde_json::json!({
        "providerId": row.id,
        "providerKind": cfg.kind.as_str(),
        "endpointIdentity": row.endpoint,
        "accountNamespace": row.account_namespace,
        "modelId": model_id.trim(),
        "modelDigest": model_digest,
        "sourceLanguage": src,
        "targetLanguage": tgt,
        "options": {
            "temperature": if caps.temperature { temperature } else { None },
            "seed": if caps.seed { seed } else { None },
            "maxOutputTokens": max_output_tokens,
        },
        "responseFormat": if caps.structured_output { "json_segments" } else { "text" },
        "contextLimit": context_limit.or(caps.context_limit),
        "capabilities": caps,
        "adapterVersion": crate::domain::versions::ADAPTER,
        "promptHash": prompt::prompt_template_hash(),
    });
    let p = translations::ProfileRow {
        id: new_uuid(),
        provider_id: row.id.clone(),
        profile_hash: canonical_sha256(&snapshot),
        source_language: src.into(),
        target_language: tgt.into(),
        model_id: model_id.trim().into(),
        snapshot,
        created_at: now_ms(),
    };
    translations::profile_get_or_create(&db.writer, &p).await
}

#[derive(Debug, Clone)]
pub struct ProfileContext {
    pub profile: translations::ProfileRow,
    pub provider: translations::ProviderRow,
    pub kind: ProviderKind,
    pub caps: ProviderCapabilities,
    pub response_format: ResponseFormat,
    pub options: GenerationOptions,
    pub context_limit: Option<u32>,
    pub model_fingerprint: String,
    pub glossary: Vec<GlossaryEntry>,
    pub glossary_version: String,
    pub style: prompt::TranslationStyle,
}

/// The session's effective profile: same settings as `base`, languages from the session.
pub async fn profile_context(db: &Db, base: &translations::ProfileRow, src: &str, tgt: &str) -> AppResult<ProfileContext> {
    let provider = translations::provider_get(&db.reader, &base.provider_id).await?.ok_or_else(|| AppError::not_found("Provider"))?;
    let profile = if base.source_language == src && base.target_language == tgt {
        base.clone()
    } else {
        let mut snap = base.snapshot.clone();
        snap["sourceLanguage"] = serde_json::json!(src);
        snap["targetLanguage"] = serde_json::json!(tgt);
        translations::profile_get_or_create(&db.writer, &translations::ProfileRow {
            id: new_uuid(),
            provider_id: base.provider_id.clone(),
            profile_hash: canonical_sha256(&snap),
            source_language: src.into(),
            target_language: tgt.into(),
            model_id: base.model_id.clone(),
            snapshot: snap,
            created_at: now_ms(),
        })
        .await?
    };
    let snap = &profile.snapshot;
    let caps: ProviderCapabilities = serde_json::from_value(snap["capabilities"].clone()).unwrap_or_else(|_| provider_config(&provider).map(|c| c.effective_capabilities()).unwrap_or_else(|_| ProviderCapabilities::default_for(ProviderKind::Ollama)));
    let kind = ProviderKind::parse(&provider.kind).unwrap_or(ProviderKind::Ollama);
    let response_format = if snap["responseFormat"].as_str() == Some("json_segments") { ResponseFormat::JsonSegments } else { ResponseFormat::Text };
    let options = GenerationOptions {
        temperature: snap["options"]["temperature"].as_f64(),
        seed: snap["options"]["seed"].as_i64(),
        max_output_tokens: snap["options"]["maxOutputTokens"].as_u64().map(|v| v as u32),
    };
    // Unverifiable remote aliases use the provider cache epoch ("캐시 새로 시작").
    let model_fingerprint = match snap["modelDigest"].as_str() {
        Some(d) if !d.is_empty() => format!("digest:{d}"),
        _ => format!("epoch:{}", provider.cache_epoch),
    };
    let (glossary, glossary_version) = match misc::setting_get(&db.reader, "glossary").await? {
        Some((v, ver)) => (
            v.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| Some(GlossaryEntry { source: e.get("source")?.as_str()?.to_string(), target: e.get("target")?.as_str()?.to_string() }))
                        .collect()
                })
                .unwrap_or_default(),
            format!("v{ver}"),
        ),
        None => (vec![], "v0".into()),
    };
    let style = misc::setting_get(&db.reader, "translationStyle").await?.and_then(|v| v.0.as_str().map(prompt::TranslationStyle::parse)).unwrap_or_default();
    Ok(ProfileContext {
        style,
        context_limit: snap["contextLimit"].as_u64().map(|v| v as u32),
        profile,
        provider,
        kind,
        caps,
        response_format,
        options,
        model_fingerprint,
        glossary,
        glossary_version,
    })
}

/// Precomputed previous-leaf / heading context for blocks of one loaded section.
pub struct SectionContext {
    map: HashMap<String, (Option<String>, Option<String>)>,
}

fn is_context_kind(k: BlockKind) -> bool {
    matches!(k, BlockKind::Paragraph | BlockKind::ListItem | BlockKind::Quote | BlockKind::Caption)
}

impl SectionContext {
    pub fn new(blocks: &[Block]) -> Self {
        let mut map = HashMap::new();
        let mut prev: Option<String> = None;
        let mut heading: Option<String> = None;
        let mut sorted: Vec<&Block> = blocks.iter().collect();
        sorted.sort_by_key(|b| (b.section_id.clone(), b.ordinal));
        let mut cur_section = String::new();
        for b in sorted {
            if b.section_id != cur_section {
                cur_section = b.section_id.clone();
                prev = None;
                heading = None;
            }
            map.insert(b.id.clone(), (prev.clone(), heading.clone()));
            if is_context_kind(b.kind) {
                prev = Some(b.source_text.clone());
            }
            if b.kind == BlockKind::Heading {
                heading = Some(b.source_text.clone());
            }
        }
        Self { map }
    }
}

#[derive(Debug, Clone)]
pub struct PreparedPart {
    pub request: TranslationRequest,
    pub protected: ProtectedText,
    pub start_u16: u32,
    pub end_u16: u32,
}

#[derive(Debug, Clone)]
pub struct PreparedJob {
    pub provider_id: String,
    pub profile_id: String,
    pub target_language: String,
    pub source_hash: String,
    pub source_text: String,
    /// Emphasis handling decided for this block (see `inline_marks`).
    pub has_inline_marks: bool,
    pub whole_mark: Option<char>,
    pub block_id: String,
    pub cache_key: String,
    pub descriptor: serde_json::Value,
    pub response_format: ResponseFormat,
    pub parts: Vec<PreparedPart>,
}

impl PreparedJob {
    /// Secret-free immutable snapshot stored with the job row.
    pub fn snapshot_json(&self) -> serde_json::Value {
        serde_json::json!({
            "profileId": self.profile_id,
            "blockId": self.block_id,
            "sourceHash": self.source_hash,
            "cacheKey": self.cache_key,
            "descriptor": self.descriptor,
            "parts": self.parts.iter().map(|p| serde_json::json!({"index": p.request.part_index, "start": p.start_u16, "end": p.end_u16})).collect::<Vec<_>>(),
        })
    }
}

pub async fn prepare(db: &Db, s: &Session, p: &ProfileContext, b: &Block, ctx: Option<&SectionContext>) -> AppResult<PreparedJob> {
    if b.source_text.trim().is_empty() {
        return Err(AppError::invalid("빈 원문은 번역하지 않습니다."));
    }
    // Context: previous source paragraph + section heading (never previous translations).
    let (prev, heading) = match ctx.and_then(|c| c.map.get(&b.id).cloned()) {
        Some(v) => v,
        None => {
            let (pb, hb) = extraction::context_blocks(&db.reader, b).await?;
            (pb.map(|x| x.source_text), hb.map(|x| x.source_text))
        }
    };
    let context = if prev.is_none() && heading.is_none() { PromptContext::empty() } else { PromptContext::build(heading.as_deref(), prev.as_deref()) };
    let lower = b.source_text.to_lowercase();
    let glossary: Vec<GlossaryEntry> = p.glossary.iter().filter(|g| !g.source.is_empty() && lower.contains(&g.source.to_lowercase())).cloned().collect();
    let glossary_hash = prompt::glossary_hash(&p.glossary_version, &glossary);

    let overhead = 600 + crate::translation::splitter::estimate_tokens(&context.to_json().to_string());
    let max_out = p.options.max_output_tokens.unwrap_or_else(|| p.context_limit.map(|l| (l / 3).clamp(256, 8192)).unwrap_or(4096));
    let budget = if p.context_limit.is_some() { SplitBudget::for_model(p.context_limit, overhead, max_out)? } else { SplitBudget::default() };
    // Inline emphasis (bold/italic) travels as paired tags inside the text to translate.
    let marks = super::inline_marks::analyze(b);
    let text_in: &str = match &marks {
        super::inline_marks::SourceMarks::Inline(m) => m.as_str(),
        _ => b.source_text.as_str(),
    };
    let has_inline = matches!(marks, super::inline_marks::SourceMarks::Inline(_));
    let spans = placeholder::detect_spans(text_in);
    let mut split = splitter::split_source(text_in, budget, &spans);
    if has_inline && split.len() > 1 {
        let mut texts: Vec<String> = split.iter().map(|p| p.text.clone()).collect();
        super::inline_marks::rebalance_parts(&mut texts);
        for (p, t) in split.iter_mut().zip(texts) {
            p.text = t;
        }
    }
    let part_count = split.len() as u32;
    let src_lang = p.profile.source_language.clone();
    let tgt_lang = p.profile.target_language.clone();
    let mut parts = Vec::with_capacity(split.len());
    let mut payloads = Vec::with_capacity(split.len());
    for sp in &split {
        let protected = placeholder::protect(&sp.text, &[])?;
        let messages = prompt::build_messages(&PromptInput {
            block_id: b.id.clone(),
            segments: vec![PromptSegment { id: SEGMENT_ID.into(), text: protected.text.clone() }],
            source_language: src_lang.clone(),
            target_language: tgt_lang.clone(),
            context: context.clone(),
            glossary: glossary.clone(),
            response_format: p.response_format,
            placeholder_example: if protected.has_placeholders() { Some(protected.example_token()) } else { None },
            style: p.style,
            inline_tags: has_inline && (sp.text.contains("<b>") || sp.text.contains("<i>")),
        })?;
        let request = TranslationRequest {
            request_id: String::new(),
            job_id: String::new(),
            block_id: b.id.clone(),
            part_index: sp.index,
            part_count,
            model_id: p.profile.model_id.clone(),
            source_language: src_lang.clone(),
            target_language: tgt_lang.clone(),
            messages,
            options: p.options.clone(),
            response_format: p.response_format,
            stream: p.caps.streaming,
            expected_segment_ids: vec![SEGMENT_ID.into()],
        };
        payloads.push(crate::providers::request_payload(p.kind, &p.caps, &request));
        parts.push(PreparedPart { request, protected, start_u16: sp.start_u16, end_u16: sp.end_u16 });
    }
    let manifest: Vec<ManifestPart> = parts
        .iter()
        .zip(payloads.iter())
        .map(|(pp, pl)| ManifestPart { part_index: pp.request.part_index, start_u16: pp.start_u16, end_u16: pp.end_u16, payload: pl })
        .collect();
    let manifest_hash = prompt::request_manifest_hash(&manifest);
    let doc_sha = document_sha(db, &s.document_id).await?;
    let descriptor = cache_key::descriptor(&DescriptorInput {
        document_id: &s.document_id,
        document_sha256: &doc_sha,
        extraction_id: &b.extraction_id,
        block_id: &b.id,
        source_hash: &b.source_hash,
        source_language: &src_lang,
        target_language: &tgt_lang,
        provider_kind: p.kind.as_str(),
        endpoint_identity: &p.provider.endpoint,
        account_namespace: &p.provider.account_namespace,
        model_id: &p.profile.model_id,
        model_fingerprint: &p.model_fingerprint,
        profile_hash: &p.profile.profile_hash,
        prompt_hash: &prompt::prompt_template_hash(),
        glossary_hash: &glossary_hash,
        context_hash: &context.hash(),
        request_manifest_hash: &manifest_hash,
    });
    Ok(PreparedJob {
        provider_id: p.provider.id.clone(),
        profile_id: p.profile.id.clone(),
        target_language: tgt_lang,
        source_hash: b.source_hash.clone(),
        source_text: b.source_text.clone(),
        has_inline_marks: has_inline,
        whole_mark: match marks { super::inline_marks::SourceMarks::Whole(k) => Some(k), _ => None },
        block_id: b.id.clone(),
        cache_key: cache_key::cache_key(&descriptor),
        descriptor,
        response_format: p.response_format,
        parts,
    })
}

async fn document_sha(db: &Db, document_id: &str) -> AppResult<String> {
    documents::get(&db.reader, document_id).await?.map(|d| d.sha256).ok_or_else(|| AppError::not_found("문서"))
}

/// Validate + restore one part's output. Err = INVALID_OUTPUT reason (never retried).
pub fn validate_part(pj: &PreparedJob, index: usize, out: &TranslationOutput) -> Result<String, String> {
    let part = pj.parts.get(index).ok_or("part_index")?;
    let v = validation::validate_output(out, pj.response_format, &[ExpectedSegment { id: SEGMENT_ID, protected: &part.protected }], &pj.target_language)
        .map_err(|e| e.reason().to_string())?;
    v.segments.into_iter().next().map(|s| s.text.trim().to_string()).ok_or_else(|| "no_segment".to_string())
}

/// Final stored text + emphasis ranges for a completed job.
pub fn finish_text(pj: &PreparedJob, joined: &str) -> (String, Vec<super::inline_marks::Mark>) {
    if pj.has_inline_marks {
        return super::inline_marks::extract(joined);
    }
    match pj.whole_mark {
        Some(k) if !joined.is_empty() => (
            joined.to_string(),
            vec![super::inline_marks::Mark { start_u16: 0, end_u16: crate::domain::utf16::len_u16(joined), kind: k.to_string() }],
        ),
        _ => (joined.to_string(), vec![]),
    }
}

/// Join part translations in order (no space for CJK targets).
pub fn join_parts(parts: &[String], target_language: &str) -> String {
    let cjk = matches!(target_language.split('-').next(), Some("ja" | "zh"));
    parts.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(if cjk { "" } else { " " })
}

pub fn language_warning(text: &str, target_language: &str) -> bool {
    // Short strings are mostly names/titles that legitimately stay in the source script.
    text.chars().filter(|c| c.is_alphabetic()).count() >= 40 && validation::language_mismatch(text, target_language)
}

/// Name of a script leaked into the translation, if any (shown as a "확인 필요" hint).
pub fn script_warning(pj: &PreparedJob, text: &str) -> Option<&'static str> {
    validation::unexpected_script(&pj.source_text, text, &pj.target_language)
}
