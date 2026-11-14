//! Position, bookmarks, search, settings, edits, history and preferences.

use super::core::*;
use crate::domain::{utf16, ReaderAnchor, TranslationPolicy};
use crate::error::{AppError, AppResult};
use crate::storage::repo::{extraction, misc, translations};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::{State, WebviewWindow};
use ts_rs::TS;

type Core<'a> = State<'a, Arc<AppCore>>;

pub const MAX_EDIT_BYTES: usize = 128 * 1024;

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SavePositionArgs {
    pub session_id: String,
    pub anchor: ReaderAnchor,
    #[ts(type = "Record<string, unknown>")]
    pub view: serde_json::Value,
    pub expected_version: i64,
}

#[tauri::command]
pub async fn reader_save_position(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: SavePositionArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        args.anchor.validate().map_err(AppError::invalid)?;
        if args.anchor.document_id != s.document_id || args.view.to_string().len() > 4096 {
            return Err(AppError::invalid("위치 정보가 올바르지 않습니다."));
        }
        core.db.ensure_writable()?;
        let v = misc::position_save(&core.db.writer, &s.document_id, &args.anchor, &args.view, args.expected_version).await?;
        Ok(serde_json::json!({"version": v}))
    }
    .await;
    Ok(respond(&meta, r))
}

// ------------------------------------------------------------------ bookmarks

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BookmarkPutArgs {
    pub document_id: String,
    pub bookmark_id: Option<String>,
    pub anchor: ReaderAnchor,
    pub label: String,
}

#[tauri::command]
pub async fn bookmark_put(core: Core<'_>, meta: CommandMeta, args: BookmarkPutArgs) -> Result<Wire, ()> {
    let core2 = core.inner().clone();
    Ok(with_receipt(&core, &meta, "bookmark_put", &args, || async move {
        check_id(&args.document_id, "documentId")?;
        if let Some(b) = &args.bookmark_id {
            check_id(b, "bookmarkId")?;
        }
        args.anchor.validate().map_err(AppError::invalid)?;
        if args.anchor.document_id != args.document_id || args.label.chars().count() > 200 {
            return Err(AppError::invalid("북마크 정보가 올바르지 않습니다."));
        }
        misc::bookmark_put(&core2.db.writer, args.bookmark_id.as_deref(), &args.document_id, &args.anchor, args.label.trim()).await
    })
    .await)
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BookmarkDeleteArgs {
    pub document_id: String,
    pub bookmark_id: String,
}

#[tauri::command]
pub async fn bookmark_delete(core: Core<'_>, meta: CommandMeta, args: BookmarkDeleteArgs) -> Result<Wire, ()> {
    let core2 = core.inner().clone();
    Ok(with_receipt(&core, &meta, "bookmark_delete", &args, || async move {
        check_id(&args.document_id, "documentId")?;
        check_id(&args.bookmark_id, "bookmarkId")?;
        Ok(serde_json::json!({"deleted": misc::bookmark_delete(&core2.db.writer, &args.document_id, &args.bookmark_id).await?}))
    })
    .await)
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DocumentArgs {
    pub document_id: String,
}

#[tauri::command]
pub async fn bookmark_list(core: Core<'_>, meta: CommandMeta, args: DocumentArgs) -> Result<Wire, ()> {
    let r = async {
        check_id(&args.document_id, "documentId")?;
        misc::bookmark_list(&core.db.reader, &args.document_id).await
    }
    .await;
    Ok(respond(&meta, r))
}

// ------------------------------------------------------------------ search

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SearchArgs {
    pub session_id: String,
    pub query: String,
    /// "source" | "translation" | "all"
    pub scope: String,
    pub cursor: Option<u32>,
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SearchPage {
    pub hits: Vec<misc::SearchHit>,
    pub next_cursor: Option<u32>,
    /// "trigram" (≥3 chars) or "substring" (1–2 chars)
    pub method: String,
    pub indexed_sections: i64,
    pub total_sections: i64,
}

#[tauri::command]
pub async fn search_query(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: SearchArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        if !matches!(args.scope.as_str(), "source" | "translation" | "all") {
            return Err(AppError::invalid("검색 범위가 올바르지 않습니다."));
        }
        let ex = s.extraction()?;
        let lang = s.state.lock().unwrap().target_language.clone();
        let (hits, next_cursor, fts) = misc::search(&core.db.reader, &s.document_id, &ex, &args.query, &args.scope, Some(&lang), args.cursor.unwrap_or(0), 50).await?;
        let (total, ready): (i64, i64) = sqlx::query_as("select count(*), coalesce(sum(state='ready'),0) from sections where extraction_id = ?")
            .bind(&ex)
            .fetch_one(&core.db.reader)
            .await?;
        Ok(SearchPage { hits, next_cursor, method: if fts { "trigram".into() } else { "substring".into() }, indexed_sections: ready, total_sections: total })
    }
    .await;
    Ok(respond(&meta, r))
}

// ------------------------------------------------------------------ settings

pub const SETTING_KEYS: &[&str] = &[
    "theme", "fontSize", "fontFamily", "lineHeight", "defaultTargetLanguage", "defaultSourceLanguage",
    "activeProfileId", "autoTranslate", "syncScroll", "glossary", "promptTemplate", "diskWarningBytes",
    "readingMode", "pdfView", "privacyAcknowledged",
];

pub fn validate_setting(key: &str, v: &serde_json::Value) -> AppResult<()> {
    let ok = match key {
        "theme" => matches!(v.as_str(), Some("system" | "light" | "dark")),
        "fontSize" => v.as_f64().map(|f| (10.0..=40.0).contains(&f)).unwrap_or(false),
        "lineHeight" => v.as_f64().map(|f| (1.0..=3.0).contains(&f)).unwrap_or(false),
        "fontFamily" => v.as_str().map(|s| s.len() <= 100).unwrap_or(false),
        "defaultTargetLanguage" | "defaultSourceLanguage" => v.as_str().map(|s| check_lang(s).is_ok()).unwrap_or(false),
        "activeProfileId" => v.is_null() || v.as_str().map(crate::domain::is_valid_id).unwrap_or(false),
        "autoTranslate" | "syncScroll" | "privacyAcknowledged" => v.is_boolean(),
        "glossary" => v.as_array().map(|a| a.len() <= 500 && a.iter().all(|e| e.get("source").and_then(|x| x.as_str()).map(|s| !s.is_empty() && s.len() < 200).unwrap_or(false) && e.get("target").and_then(|x| x.as_str()).map(|s| s.len() < 200).unwrap_or(false))).unwrap_or(false),
        "promptTemplate" => v.as_str().map(|s| s.len() <= 4000).unwrap_or(false) || v.is_null(),
        "diskWarningBytes" => v.as_u64().is_some(),
        "readingMode" => matches!(v.as_str(), Some("parallel" | "interleaved" | "original")),
        "pdfView" => matches!(v.as_str(), Some("original" | "reflow")),
        _ => false,
    };
    if ok { Ok(()) } else { Err(AppError::invalid(format!("설정 값이 올바르지 않습니다: {key}"))) }
}

#[tauri::command]
pub async fn settings_get(core: Core<'_>, meta: CommandMeta) -> Result<Wire, ()> {
    let r = async {
        let all = misc::settings_all(&core.db.reader).await?;
        let mut m = serde_json::Map::new();
        for (k, v, ver) in all {
            if SETTING_KEYS.contains(&k.as_str()) {
                m.insert(k, serde_json::json!({"value": v, "version": ver}));
            }
        }
        Ok(serde_json::Value::Object(m))
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SettingsPatchArgs {
    pub key: String,
    #[ts(type = "unknown")]
    pub value: serde_json::Value,
    pub expected_version: Option<i64>,
}

#[tauri::command]
pub async fn settings_patch(core: Core<'_>, meta: CommandMeta, args: SettingsPatchArgs) -> Result<Wire, ()> {
    let core2 = core.inner().clone();
    Ok(with_receipt(&core, &meta, "settings_patch", &args, || async move {
        if !SETTING_KEYS.contains(&args.key.as_str()) {
            return Err(AppError::invalid("허용되지 않은 설정 키입니다."));
        }
        validate_setting(&args.key, &args.value)?;
        let v = misc::setting_put(&core2.db.writer, &args.key, &args.value, args.expected_version).await?;
        core2.translation.on_settings_changed(&args.key).await;
        Ok(serde_json::json!({"key": args.key, "version": v}))
    })
    .await)
}

// ------------------------------------------------------------------ edits & history

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SaveEditArgs {
    pub session_id: String,
    pub block_id: String,
    pub target_language: String,
    pub text: String,
    pub expected_head_version: i64,
}

#[tauri::command]
pub async fn translation_save_edit(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: SaveEditArgs) -> Result<Wire, ()> {
    let core2 = core.inner().clone();
    let label = window.label().to_string();
    let request_id = meta.request_id.clone();
    Ok(with_receipt(&core, &meta, "translation_save_edit", &args, || async move {
        let s = core2.sessions.get(&args.session_id, &label)?;
        check_id(&args.block_id, "blockId")?;
        check_lang(&args.target_language)?;
        if args.text.len() > MAX_EDIT_BYTES {
            return Err(AppError::limit("번역 수정은 128KiB 이내여야 합니다."));
        }
        let b = extraction::get_block_in_extraction(&core2.db.reader, &args.block_id, &s.extraction()?).await?;
        let (rev, v) = translations::save_manual(&core2.db.writer, &b.id, &args.target_language, &b.source_hash, &args.text, args.expected_head_version, &request_id).await?;
        core2.translation.emit_head(&s, &b.id).await;
        Ok(serde_json::json!({"revisionId": rev, "headVersion": v}))
    })
    .await)
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ApplyRevisionArgs {
    pub session_id: String,
    pub revision_id: String,
    pub expected_head_version: i64,
}

#[tauri::command]
pub async fn translation_apply_revision(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: ApplyRevisionArgs) -> Result<Wire, ()> {
    let core2 = core.inner().clone();
    let label = window.label().to_string();
    Ok(with_receipt(&core, &meta, "translation_apply_revision", &args, || async move {
        let s = core2.sessions.get(&args.session_id, &label)?;
        check_id(&args.revision_id, "revisionId")?;
        let rev = translations::revision_get(&core2.db.reader, &args.revision_id).await?.ok_or_else(|| AppError::not_found("번역 이력"))?;
        let b = extraction::get_block_in_extraction(&core2.db.reader, &rev.block_id, &s.extraction()?).await?;
        let (applied, v) = translations::apply_revision(&core2.db.writer, &rev.id, &b.source_hash, args.expected_head_version).await?;
        core2.translation.emit_head(&s, &b.id).await;
        Ok(serde_json::json!({"appliedRevisionId": applied, "headVersion": v}))
    })
    .await)
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct HistoryArgs {
    pub session_id: String,
    pub block_id: String,
    pub target_language: String,
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct HistoryPage {
    pub revisions: Vec<translations::RevisionRow>,
    pub head: translations::HeadRow,
    pub next_cursor: Option<String>,
}

#[tauri::command]
pub async fn translation_history(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: HistoryArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        check_id(&args.block_id, "blockId")?;
        check_lang(&args.target_language)?;
        extraction::get_block_in_extraction(&core.db.reader, &args.block_id, &s.extraction()?).await?;
        if args.cursor.as_ref().map(|c| c.len() > 120).unwrap_or(false) {
            return Err(AppError::invalid("cursor가 올바르지 않습니다."));
        }
        let (revisions, next_cursor) = translations::history(&core.db.reader, &args.block_id, &args.target_language, args.cursor.as_deref(), 50).await?;
        let head = translations::head_get(&core.db.reader, &args.block_id, &args.target_language).await?;
        Ok(HistoryPage { revisions, head, next_cursor })
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BlockPreferenceArgs {
    pub session_id: String,
    pub block_id: String,
    pub translation_override: Option<TranslationPolicy>,
    pub expected_version: i64,
}

#[tauri::command]
pub async fn block_set_preference(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: BlockPreferenceArgs) -> Result<Wire, ()> {
    let core2 = core.inner().clone();
    let label = window.label().to_string();
    Ok(with_receipt(&core, &meta, "block_set_preference", &args, || async move {
        let s = core2.sessions.get(&args.session_id, &label)?;
        check_id(&args.block_id, "blockId")?;
        let b = extraction::get_block_in_extraction(&core2.db.reader, &args.block_id, &s.extraction()?).await?;
        let v = translations::preference_set(&core2.db.writer, &b.id, args.translation_override.map(|p| p.as_str()), args.expected_version).await?;
        let effective = TranslationPolicy::effective(b.translation_policy, b.quality.level, args.translation_override);
        if effective != TranslationPolicy::Auto {
            core2.translation.cancel_block_auto(&s, &b.id).await;
        }
        core2.translation.emit_head(&s, &b.id).await;
        Ok(serde_json::json!({"version": v, "effectivePolicy": effective}))
    })
    .await)
}

/// Resolve a selection inside a block to a UTF-16-safe range (used for quoting / CFI mapping).
pub fn clamp_selection(text: &str, start: u32, end: u32) -> Option<(u32, u32)> {
    if utf16::is_valid_range_u16(text, start, end) {
        Some((start, end))
    } else {
        None
    }
}
