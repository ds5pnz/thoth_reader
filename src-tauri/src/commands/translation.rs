//! Viewport demand, explicit requests, cancellation, providers, profiles and cache commands.

use super::core::*;
use crate::document::session::ReadingMode;
use crate::domain::{new_uuid, now_ms};
use crate::error::{AppError, AppResult, ErrorCode};
use crate::providers::{ProviderCapabilities, ProviderKind, TranslationProvider};
use crate::storage::repo::{misc, translations};
use crate::translation::prepare;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::{State, WebviewWindow};
use ts_rs::TS;

type Core<'a> = State<'a, Arc<AppCore>>;

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ViewportArgs {
    pub session_id: String,
    pub generation: u64,
    pub sequence: u64,
    pub visible_ids: Vec<String>,
    /// "forward" | "backward"
    pub direction: String,
}

#[tauri::command]
pub async fn reader_set_viewport(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: ViewportArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        check_ids(&args.visible_ids, 100, "visibleIds")?;
        core.translation.set_viewport(&s, args.generation, args.sequence, args.visible_ids, args.direction != "backward").await
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TranslationRequestArgs {
    pub session_id: String,
    pub block_id: String,
    pub force: bool,
}

#[tauri::command]
pub async fn translation_request(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: TranslationRequestArgs) -> Result<Wire, ()> {
    let core2 = core.inner().clone();
    let label = window.label().to_string();
    Ok(with_receipt(&core, &meta, "translation_request", &args, || async move {
        let s = core2.sessions.get(&args.session_id, &label)?;
        check_id(&args.block_id, "blockId")?;
        core2.translation.request(&s, &args.block_id, args.force).await
    })
    .await)
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TranslationCancelArgs {
    pub session_id: String,
    pub job_id: String,
}

#[tauri::command]
pub async fn translation_cancel(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: TranslationCancelArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        check_id(&args.job_id, "jobId")?;
        core.translation.cancel_job(&s, &args.job_id).await
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ReaderModeArgs {
    pub session_id: String,
    pub mode: ReadingMode,
}

#[tauri::command]
pub async fn reader_set_mode(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: ReaderModeArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        core.translation.set_mode(&s, args.mode).await?;
        Ok(serde_json::json!({"mode": args.mode}))
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PauseArgs {
    pub session_id: String,
    pub paused: bool,
    pub cancel_running: bool,
}

#[tauri::command]
pub async fn translation_pause(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: PauseArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        let n = core.translation.set_paused(&s, args.paused, args.cancel_running).await?;
        Ok(serde_json::json!({"paused": args.paused, "cancelled": n}))
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SwitchProfileArgs {
    pub session_id: String,
    pub profile_id: Option<String>,
    pub target_language: Option<String>,
    pub source_language: Option<String>,
}

/// Provider/model/language change: bumps generation and cancels old work (spec §7.3).
#[tauri::command]
pub async fn reader_switch_profile(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: SwitchProfileArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        if let Some(p) = &args.profile_id {
            check_id(p, "profileId")?;
            translations::profile_get(&core.db.reader, p).await?.ok_or_else(|| AppError::not_found("번역 프로필"))?;
        }
        if let Some(t) = &args.target_language {
            check_lang(t)?;
        }
        if let Some(src) = &args.source_language {
            check_lang(src)?;
            crate::storage::repo::documents::merge_metadata(&core.db.writer, &s.document_id, &serde_json::json!({"sourceLanguage": src})).await?;
        }
        let g = core.translation.switch_profile(&s, args.profile_id, args.target_language, args.source_language).await?;
        Ok(serde_json::json!({"generation": g}))
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SnapshotArgs {
    pub session_id: String,
    pub after_sequence: u64,
    pub block_ids: Vec<String>,
}

/// Re-subscribe path: current jobs + heads for given blocks + latest sequence.
#[tauri::command]
pub async fn reader_get_snapshot(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: SnapshotArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        check_ids(&args.block_ids, 1000, "blockIds")?;
        let ex = s.extraction()?;
        let mut blocks = Vec::new();
        for id in &args.block_ids {
            if let Ok(b) = crate::storage::repo::extraction::get_block_in_extraction(&core.db.reader, id, &ex).await {
                blocks.push(b);
            }
        }
        let display = core.translation.display_for_blocks(&s, &blocks).await?;
        Ok(serde_json::json!({"generation": s.generation(), "sequence": s.stream.current_sequence(), "display": display, "queue": core.translation.queue_stats()}))
    }
    .await;
    Ok(respond(&meta, r))
}

// ------------------------------------------------------------------ providers

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ProviderSaveArgs {
    /// Existing provider to update (label/enabled/secret only). None => create.
    pub provider_id: Option<String>,
    pub kind: Option<String>,
    pub label: Option<String>,
    pub endpoint: Option<String>,
    pub account_namespace: Option<String>,
    pub allow_private_http: Option<bool>,
    pub concurrency: Option<u8>,
    pub enabled: Option<bool>,
    /// New secret (write-only). Empty string clears it.
    #[ts(optional)]
    pub new_secret: Option<String>,
}

#[derive(Serialize)]
struct RedactedSave<'a> {
    provider_id: &'a Option<String>,
    kind: &'a Option<String>,
    label: &'a Option<String>,
    endpoint: &'a Option<String>,
    account_namespace: &'a Option<String>,
    allow_private_http: &'a Option<bool>,
    concurrency: &'a Option<u8>,
    enabled: &'a Option<bool>,
    secret_hash: Option<String>,
}

#[tauri::command]
pub async fn provider_save(core: Core<'_>, meta: CommandMeta, args: ProviderSaveArgs) -> Result<Wire, ()> {
    // Receipts never contain the secret itself.
    let redacted = RedactedSave {
        provider_id: &args.provider_id,
        kind: &args.kind,
        label: &args.label,
        endpoint: &args.endpoint,
        account_namespace: &args.account_namespace,
        allow_private_http: &args.allow_private_http,
        concurrency: &args.concurrency,
        enabled: &args.enabled,
        secret_hash: args.new_secret.as_ref().map(|s| crate::domain::sha256_hex_str(s)),
    };
    let core2 = core.inner().clone();
    let w = with_receipt(&core, &meta, "provider_save", &redacted, || async move { save_provider(&core2, args).await }).await;
    Ok(w)
}

async fn save_provider(core: &AppCore, args: ProviderSaveArgs) -> AppResult<translations::ProviderRow> {
    if args.new_secret.as_ref().map(|s| s.len() > 4096).unwrap_or(false) {
        return Err(AppError::limit("키가 너무 깁니다."));
    }
    if let Some(l) = &args.label {
        if l.chars().count() > 80 {
            return Err(AppError::invalid("이름이 너무 깁니다."));
        }
    }
    match &args.provider_id {
        Some(id) => {
            check_id(id, "providerId")?;
            let row = translations::provider_get(&core.db.reader, id).await?.ok_or_else(|| AppError::not_found("Provider"))?;
            // Endpoint/namespace changes are new providers (immutable semantics).
            if args.endpoint.is_some() || args.account_namespace.is_some() || args.kind.is_some() {
                return Err(AppError::invalid("endpoint 또는 namespace 변경은 새 Provider로 저장해 주세요."));
            }
            let mut kref: Option<Option<String>> = None;
            if let Some(sec) = &args.new_secret {
                let r = row.keychain_ref.clone().unwrap_or_else(new_uuid);
                if sec.is_empty() {
                    let _ = core.translation.secrets.delete(&r);
                    kref = Some(None);
                } else {
                    // Keychain failure => the setting is NOT reported as saved.
                    core.translation.secrets.store(&r, sec)?;
                    kref = Some(Some(r));
                }
            }
            translations::provider_update_mutable(&core.db.writer, id, args.label.as_deref(), args.enabled, kref.as_ref().map(|k| k.as_deref()), None).await?;
            core.translation.invalidate_provider(id);
            translations::provider_get(&core.db.reader, id).await?.ok_or_else(|| AppError::not_found("Provider"))
        }
        None => {
            let kind = ProviderKind::parse(args.kind.as_deref().unwrap_or("")).ok_or_else(|| AppError::invalid("Provider 종류가 올바르지 않습니다."))?;
            let raw = args.endpoint.clone().or_else(|| kind.default_endpoint().map(|s| s.to_string())).ok_or_else(|| AppError::invalid("endpoint를 입력해 주세요."))?;
            let ep = crate::security::endpoint::normalize_endpoint(&raw, args.allow_private_http.unwrap_or(false))?;
            let ns = args.account_namespace.clone().unwrap_or_else(|| if kind == ProviderKind::Ollama { "local-default".into() } else { "default".into() });
            if ns.is_empty() || ns.len() > 80 {
                return Err(AppError::invalid("accountNamespace가 올바르지 않습니다."));
            }
            let id = new_uuid();
            let mut kref = None;
            if let Some(sec) = args.new_secret.as_ref().filter(|s| !s.is_empty()) {
                let r = new_uuid();
                core.translation.secrets.store(&r, sec)?;
                kref = Some(r);
            }
            let caps = serde_json::json!({
                "allowPrivateHttp": args.allow_private_http.unwrap_or(false),
                "concurrency": args.concurrency.unwrap_or(kind.default_concurrency()).clamp(1, 4),
            });
            let row = translations::ProviderRow {
                id: id.clone(),
                kind: kind.as_str().into(),
                label: args.label.clone().unwrap_or_else(|| if kind == ProviderKind::Ollama { "Ollama".into() } else { "OpenAI 호환".into() }),
                endpoint: ep.identity_string.clone(),
                account_namespace: ns,
                key_configured: kref.is_some(),
                keychain_ref: kref,
                capabilities: caps,
                // Remote endpoints require explicit activation after the privacy notice.
                enabled: args.enabled.unwrap_or(false),
                cache_epoch: 0,
                created_at: now_ms(),
            };
            translations::provider_insert(&core.db.writer, &row).await?;
            Ok(row)
        }
    }
}

#[tauri::command]
pub async fn provider_list(core: Core<'_>, meta: CommandMeta) -> Result<Wire, ()> {
    let r = translations::provider_list(&core.db.reader).await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ProviderIdArgs {
    pub provider_id: String,
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ProbeResult {
    pub ok: bool,
    pub error: Option<crate::error::WireError>,
    #[ts(type = "Record<string, unknown> | null")]
    pub capabilities: Option<serde_json::Value>,
    pub models: Vec<crate::providers::ModelInfo>,
    pub sent_test_prompt: bool,
}

/// Connection check: endpoint, auth, models. Never sends document text.
#[tauri::command]
pub async fn provider_probe(core: Core<'_>, meta: CommandMeta, args: ProviderIdArgs) -> Result<Wire, ()> {
    let r = async {
        check_id(&args.provider_id, "providerId")?;
        let row = translations::provider_get(&core.db.reader, &args.provider_id).await?.ok_or_else(|| AppError::not_found("Provider"))?;
        let cfg = prepare::provider_config(&row)?;
        core.translation.invalidate_provider(&row.id);
        let provider: Arc<dyn TranslationProvider> = (crate::translation::service::default_factory())(&cfg, core.translation.secrets.clone())?;
        let caps = provider.probe().await;
        let models = provider.list_models().await.unwrap_or_default();
        match caps {
            Ok(c) => {
                let mut stored = row.capabilities.clone();
                if let (Some(obj), Ok(cv)) = (stored.as_object_mut(), serde_json::to_value(&c)) {
                    obj.insert("probed".into(), cv);
                    obj.insert("probedAt".into(), serde_json::json!(now_ms()));
                }
                translations::provider_update_mutable(&core.db.writer, &row.id, None, None, None, Some(&stored)).await?;
                Ok(ProbeResult { ok: true, error: None, capabilities: serde_json::to_value(&c).ok(), models, sent_test_prompt: c.streaming || c.structured_output })
            }
            Err(e) => Ok(ProbeResult { ok: false, error: Some(crate::error::WireError::from(&e.to_app_error())), capabilities: None, models, sent_test_prompt: false }),
        }
    }
    .await;
    Ok(respond(&meta, r))
}

#[tauri::command]
pub async fn provider_new_cache_epoch(core: Core<'_>, meta: CommandMeta, args: ProviderIdArgs) -> Result<Wire, ()> {
    let core2 = core.inner().clone();
    Ok(with_receipt(&core, &meta, "provider_new_cache_epoch", &args, || async move {
        check_id(&args.provider_id, "providerId")?;
        let e = translations::provider_bump_epoch(&core2.db.writer, &args.provider_id).await?;
        core2.translation.invalidate_provider(&args.provider_id);
        Ok(serde_json::json!({"cacheEpoch": e}))
    })
    .await)
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ProfileCreateArgs {
    pub provider_id: String,
    pub model_id: String,
    pub source_language: String,
    pub target_language: String,
    pub temperature: Option<f64>,
    pub seed: Option<i64>,
    pub max_output_tokens: Option<u32>,
    pub context_limit: Option<u32>,
    pub model_digest: Option<String>,
}

#[tauri::command]
pub async fn profile_create(core: Core<'_>, meta: CommandMeta, args: ProfileCreateArgs) -> Result<Wire, ()> {
    let core2 = core.inner().clone();
    Ok(with_receipt(&core, &meta, "profile_create", &args, || async move {
        check_id(&args.provider_id, "providerId")?;
        check_lang(&args.source_language)?;
        check_lang(&args.target_language)?;
        if args.model_id.trim().is_empty() || args.model_id.len() > 200 || args.model_id.chars().any(|c| c.is_control()) {
            return Err(AppError::invalid("모델 ID가 올바르지 않습니다."));
        }
        if let Some(t) = args.temperature {
            if !(0.0..=2.0).contains(&t) {
                return Err(AppError::invalid("temperature는 0~2 범위입니다."));
            }
        }
        let row = translations::provider_get(&core2.db.reader, &args.provider_id).await?.ok_or_else(|| AppError::not_found("Provider"))?;
        prepare::create_profile(&core2.db, &row, &args.model_id, &args.source_language, &args.target_language, args.temperature, args.seed, args.max_output_tokens, args.context_limit, args.model_digest.as_deref()).await
    })
    .await)
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CacheInvalidateArgs {
    pub document_id: Option<String>,
    pub profile_id: Option<String>,
}

/// Removes cache pointers only; revisions/history stay (spec §7.4).
#[tauri::command]
pub async fn cache_invalidate(core: Core<'_>, meta: CommandMeta, args: CacheInvalidateArgs) -> Result<Wire, ()> {
    let core2 = core.inner().clone();
    Ok(with_receipt(&core, &meta, "cache_invalidate", &args, || async move {
        let n = match (&args.document_id, &args.profile_id) {
            (Some(d), None) => {
                check_id(d, "documentId")?;
                translations::cache_invalidate_document(&core2.db.writer, d).await?
            }
            (None, Some(p)) => {
                check_id(p, "profileId")?;
                translations::cache_invalidate_profile(&core2.db.writer, p).await?
            }
            _ => return Err(AppError::invalid("documentId 또는 profileId 중 하나를 지정해 주세요.")),
        };
        Ok(serde_json::json!({"removedPointers": n, "historyKept": true}))
    })
    .await)
}

#[tauri::command]
pub async fn storage_usage(core: Core<'_>, meta: CommandMeta) -> Result<Wire, ()> {
    let r = async {
        let (rev_bytes,): (i64,) = sqlx::query_as("select coalesce(sum(length(text) + length(metadata_json)), 0) from translation_revisions").fetch_one(&core.db.reader).await?;
        let db_size = std::fs::metadata(&core.db.path).map(|m| m.len()).unwrap_or(0);
        let warn: u64 = misc::setting_get(&core.db.reader, "diskWarningBytes").await?.and_then(|v| v.0.as_u64()).unwrap_or(1 << 30);
        Ok(serde_json::json!({"translationBytes": rev_bytes, "dbBytes": db_size, "warningBytes": warn, "overWarning": rev_bytes as u64 >= warn}))
    }
    .await;
    Ok(respond(&meta, r))
}

#[allow(dead_code)]
fn _caps_type_check(_: ProviderCapabilities) -> ErrorCode {
    ErrorCode::Internal
}
