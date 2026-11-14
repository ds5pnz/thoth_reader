//! Library, import, open/close, extraction and block commands.

use super::core::*;
use crate::document::import::{self, Format, ImportCandidate};
use crate::document::session::{OpenParams, Session};
use crate::document::sink::{EventSink, StreamEnvelope};
use crate::domain::{Block, ReaderAnchor};
use crate::error::{AppError, AppResult, ErrorCode};
use crate::storage::repo::{documents, extraction, misc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::ipc::Channel;
use tauri::{State, WebviewWindow};
use ts_rs::TS;

pub struct ChannelSink(pub Channel<StreamEnvelope>);
impl EventSink for ChannelSink {
    fn send(&self, env: StreamEnvelope) {
        // Channel failures never affect DB commits or job cleanup.
        let _ = self.0.send(env);
    }
}

type Core<'a> = State<'a, Arc<AppCore>>;

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct LibraryListArgs {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct LibraryPage {
    pub documents: Vec<documents::DocumentRow>,
    pub next_cursor: Option<String>,
}

#[tauri::command]
pub async fn library_list(core: Core<'_>, meta: CommandMeta, args: LibraryListArgs) -> Result<Wire, ()> {
    let r = async {
        if args.cursor.as_ref().map(|c| c.len() > 200).unwrap_or(false) {
            return Err(AppError::invalid("cursor가 올바르지 않습니다."));
        }
        let (documents, next_cursor) = documents::list(&core.db.reader, args.cursor.as_deref(), args.limit.unwrap_or(50)).await?;
        Ok(LibraryPage { documents, next_cursor })
    }
    .await;
    Ok(respond(&meta, r))
}

/// Native file picker. Paths stay in Rust; the UI only receives single-use import tokens.
#[tauri::command]
pub async fn import_pick(core: Core<'_>, window: WebviewWindow, meta: CommandMeta) -> Result<Wire, ()> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    window.dialog().file().add_filter("PDF / EPUB", &["pdf", "epub"]).set_parent(&window).pick_files(move |files| {
        let _ = tx.send(files);
    });
    let files = rx.await.ok().flatten().unwrap_or_default();
    let cands: Vec<ImportCandidate> = files
        .into_iter()
        .filter_map(|f| f.into_path().ok())
        .filter_map(|p| core.imports.register(p))
        .collect();
    Ok(respond(&meta, Ok(cands)))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DocumentImportArgs {
    pub import_token: String,
}

#[tauri::command]
pub async fn document_import(core: Core<'_>, meta: CommandMeta, args: DocumentImportArgs) -> Result<Wire, ()> {
    let core2 = core.inner().clone();
    let w = with_receipt(&core, &meta, "document_import", json_of(&args), || async move {
        let path = core2.imports.take(&args.import_token).ok_or_else(|| AppError::invalid("가져오기 토큰이 만료되었습니다. 파일을 다시 선택해 주세요."))?;
        // One import at a time: the duplicate check and the insert are not atomic.
        let _one = core2.import_lock.lock().await;
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let r = import::import_path(&core2.db, &core2.paths, path, name).await?;
        if r.format == "epub" && !r.deduplicated {
            // Sanitize now so the library can show the cover and metadata right away.
            let (c, id) = (core2.clone(), r.document_id.clone());
            tokio::spawn(async move {
                let _ = crate::document::epub::prepare_package(&c, &id).await;
            });
        }
        Ok(r)
    })
    .await;
    Ok(w)
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DocumentOpenArgs {
    pub document_id: String,
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ReaderSessionDto {
    pub session_id: String,
    pub generation: u64,
    pub document_id: String,
    pub extraction_id: Option<String>,
    pub asset_token: String,
    pub format: String,
    pub title: String,
    pub state: String,
    #[ts(type = "Record<string, unknown>")]
    pub metadata: serde_json::Value,
    pub position: Option<ReaderAnchor>,
    #[ts(type = "Record<string, unknown> | null")]
    pub position_view: Option<serde_json::Value>,
    pub position_version: i64,
    pub sequence: u64,
}

#[tauri::command]
pub async fn document_open(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: DocumentOpenArgs, on_event: Channel<StreamEnvelope>) -> Result<Wire, ()> {
    let r = async {
        check_id(&args.document_id, "documentId")?;
        let doc = documents::get(&core.db.reader, &args.document_id).await?.ok_or_else(|| AppError::not_found("문서"))?;
        let format = if doc.format == "pdf" { Format::Pdf } else { Format::Epub };
        let managed = core.paths.resolve(&doc.managed_path).ok_or_else(|| AppError::internal("문서 경로 오류"))?;
        if !managed.exists() {
            return Err(AppError::not_found("문서 원본 사본"));
        }
        let (target, source, profile) = core.translation.session_defaults().await?;
        let s = core.sessions.open(OpenParams {
            document_id: doc.id.clone(),
            format,
            window_label: window.label().to_string(),
            managed_file: managed,
            sink: Arc::new(ChannelSink(on_event)),
            target_language: target,
            source_language: doc.metadata.get("sourceLanguage").and_then(|v| v.as_str()).map(|s| s.to_string()).or(source),
            profile_id: profile,
        });
        let _ = documents::touch_opened(&core.db.writer, &doc.id).await;
        if format == Format::Epub {
            if let Err(e) = crate::document::epub::ensure_extraction(core.inner().clone(), s.clone(), doc.clone()).await {
                core.sessions.close(&s.id); // do not leave a session (and asset token) for a failed open
                return Err(e);
            }
        } else if let Some(ex) = &doc.active_extraction_id {
            *s.extraction_id.write().unwrap() = Some(ex.clone());
        }
        let pos = misc::position_get(&core.db.reader, &doc.id).await?;
        let doc = documents::get(&core.db.reader, &doc.id).await?.unwrap_or(doc);
        let extraction_id = s.extraction_id.read().unwrap().clone();
        Ok(ReaderSessionDto {
            session_id: s.id.clone(),
            generation: s.generation(),
            document_id: doc.id,
            extraction_id,
            asset_token: s.asset_token.clone(),
            format: doc.format,
            title: doc.title,
            state: doc.state,
            metadata: doc.metadata,
            position_version: pos.as_ref().map(|p| p.2).unwrap_or(0),
            position_view: pos.as_ref().map(|p| p.1.clone()),
            position: pos.map(|p| p.0),
            sequence: s.stream.current_sequence(),
        })
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DocumentCloseArgs {
    pub session_id: String,
    pub generation: u64,
    pub final_anchor: Option<ReaderAnchor>,
    #[ts(type = "Record<string, unknown> | null")]
    pub view: Option<serde_json::Value>,
}

#[tauri::command]
pub async fn document_close(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: DocumentCloseArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        if let Some(a) = &args.final_anchor {
            if a.validate().is_ok() && a.document_id == s.document_id {
                let cur = misc::position_get(&core.db.reader, &s.document_id).await?.map(|p| p.2).unwrap_or(0);
                let _ = misc::position_save(&core.db.writer, &s.document_id, a, args.view.as_ref().unwrap_or(&serde_json::json!({})), cur).await;
            }
        }
        // Close the session (and revoke its asset token) even when cancelling jobs fails.
        let cancelled = core.translation.cancel_session(&s).await;
        core.sessions.close(&s.id);
        Ok(serde_json::json!({"closed": true, "cancelledJobs": cancelled?}))
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DocumentDeleteArgs {
    pub document_id: String,
    pub delete_history: bool,
}

#[tauri::command]
pub async fn document_delete(core: Core<'_>, meta: CommandMeta, args: DocumentDeleteArgs) -> Result<Wire, ()> {
    let core2 = core.inner().clone();
    Ok(with_receipt(&core, &meta, "document_delete", json_of(&args), || async move {
        check_id(&args.document_id, "documentId")?;
        if !args.delete_history {
            return Err(AppError::invalid("v1.0은 이력을 포함한 문서 전체 삭제만 지원합니다."));
        }
        for s in core2.sessions.for_document(&args.document_id) {
            let _ = core2.translation.cancel_session(&s).await;
            core2.sessions.close(&s.id);
        }
        // Wait for an in-flight EPUB package build so it cannot publish files for a deleted book.
        let _prep = core2.epub_prepare.lock().await;
        import::delete_document(&core2.db, &core2.paths, &args.document_id).await?;
        Ok(serde_json::json!({"deleted": true}))
    })
    .await)
}

// ------------------------------------------------------------------ library covers

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct LibraryThumbBeginArgs {
    pub document_id: String,
}

/// Lets the library render page 1 of one PDF as its cover: a 2-minute read token for that
/// file only. No reader session is opened and `lastOpenedAt` does not change.
#[tauri::command]
pub async fn library_thumb_begin(core: Core<'_>, meta: CommandMeta, args: LibraryThumbBeginArgs) -> Result<Wire, ()> {
    let r = async {
        check_id(&args.document_id, "documentId")?;
        let doc = documents::get(&core.db.reader, &args.document_id).await?.ok_or_else(|| AppError::not_found("문서"))?;
        if doc.format != "pdf" {
            return Err(AppError::invalid("PDF 문서만 표지를 만들 수 있습니다."));
        }
        let managed = core.paths.resolve(&doc.managed_path).filter(|p| p.is_file()).ok_or_else(|| AppError::not_found("문서 원본 사본"))?;
        Ok(serde_json::json!({"token": core.library_assets.issue(managed)}))
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct LibraryCoverSetArgs {
    pub document_id: String,
    pub token: String,
    /// JPEG/PNG/WebP thumbnail rendered by the library, base64; absent when rendering failed.
    pub image_base64: Option<String>,
    pub title: Option<String>,
    pub authors: Option<Vec<String>>,
}

fn clean_meta_text(s: &str, max: usize) -> Option<String> {
    let t: String = s.chars().filter(|c| !c.is_control()).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
    let t: String = t.chars().take(max).collect();
    (!t.is_empty()).then_some(t)
}

#[tauri::command]
pub async fn library_cover_set(core: Core<'_>, meta: CommandMeta, args: LibraryCoverSetArgs) -> Result<Wire, ()> {
    use base64::Engine;
    let r = async {
        check_id(&args.document_id, "documentId")?;
        core.db.ensure_writable()?;
        if args.token.len() > 64 {
            return Err(AppError::invalid("token이 올바르지 않습니다."));
        }
        core.library_assets.revoke(&args.token);
        let doc = documents::get(&core.db.reader, &args.document_id).await?.ok_or_else(|| AppError::not_found("문서"))?;
        if doc.format != "pdf" {
            return Err(AppError::invalid("PDF 문서만 표지를 만들 수 있습니다."));
        }
        let mut has_cover = false;
        if let Some(b64) = args.image_base64.as_deref() {
            if b64.len() > 2_800_000 {
                return Err(AppError::invalid("표지 이미지가 너무 큽니다."));
            }
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|_| AppError::invalid("표지 이미지 형식이 올바르지 않습니다."))?;
            has_cover = crate::document::epub::store_cover(&core.paths, &doc.id, &bytes);
            if !has_cover {
                return Err(AppError::invalid("표지 이미지 형식이 올바르지 않습니다."));
            }
        }
        let authors: Vec<String> = args.authors.unwrap_or_default().iter().filter_map(|a| clean_meta_text(a, 200)).take(8).collect();
        let mut patch = serde_json::json!({
            "authors": authors,
            "hasCover": has_cover,
            "coverVersion": crate::domain::now_ms(),
            "libraryMeta": crate::document::epub::LIBRARY_META_VERSION,
        });
        if let Some(t) = args.title.as_deref().and_then(|t| clean_meta_text(t, 300)) {
            patch["pdfTitle"] = serde_json::json!(t);
        }
        documents::merge_metadata(&core.db.writer, &doc.id, &patch).await?;
        Ok(serde_json::json!({"hasCover": has_cover}))
    }
    .await;
    Ok(respond(&meta, r))
}

// ------------------------------------------------------------------ sections & blocks

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SessionArgs {
    pub session_id: String,
}

#[tauri::command]
pub async fn sections_list(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: SessionArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        let ex = s.extraction()?;
        extraction::list_sections(&core.db.reader, &ex).await
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BlocksGetArgs {
    pub session_id: String,
    pub section_id: String,
    pub cursor: Option<u32>,
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BlocksPage {
    pub blocks: Vec<Block>,
    pub display: Vec<crate::translation::service::BlockDisplay>,
    pub next_cursor: Option<u32>,
}

#[tauri::command]
pub async fn blocks_get(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: BlocksGetArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        check_id(&args.section_id, "sectionId")?;
        let ex = s.extraction()?;
        let limit = args.limit.unwrap_or(500).clamp(1, 1000);
        let blocks = extraction::blocks_of_section(&core.db.reader, &ex, &args.section_id, args.cursor, limit).await?;
        let next_cursor = if blocks.len() as u32 == limit { blocks.last().map(|b| b.ordinal) } else { None };
        let display = core.translation.display_for_blocks(&s, &blocks).await?;
        Ok(BlocksPage { blocks, display, next_cursor })
    }
    .await;
    Ok(respond(&meta, r))
}

// ------------------------------------------------------------------ PDF extraction (frontend extractor)

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ExtractionBeginArgs {
    pub session_id: String,
    #[ts(type = "Record<string, string>")]
    pub extractor_versions: serde_json::Value,
    pub page_count: u32,
    pub title: Option<String>,
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ExtractionBeginResult {
    pub extraction_id: String,
    pub extraction_token: String,
    pub state: String,
    pub sections: Vec<extraction::SectionRow>,
}

fn validate_versions(v: &serde_json::Value) -> AppResult<()> {
    let obj = v.as_object().ok_or_else(|| AppError::invalid("extractorVersions가 올바르지 않습니다."))?;
    if obj.len() > 16 || obj.iter().any(|(k, v)| k.len() > 64 || !v.is_string() || v.as_str().unwrap().len() > 64) {
        return Err(AppError::invalid("extractorVersions가 올바르지 않습니다."));
    }
    if obj.get("pdfExtractor").and_then(|v| v.as_str()) != Some(crate::domain::versions::PDF_EXTRACTOR) {
        return Err(AppError::invalid("지원하지 않는 PDF 추출기 버전입니다."));
    }
    Ok(())
}

#[tauri::command]
pub async fn extraction_begin(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: ExtractionBeginArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        if s.format != Format::Pdf {
            return Err(AppError::invalid("PDF 문서에서만 사용할 수 있습니다."));
        }
        validate_versions(&args.extractor_versions)?;
        if args.page_count == 0 || args.page_count > 100_000 {
            return Err(AppError::limit("페이지 수가 올바르지 않습니다."));
        }
        let config = serde_json::json!({"pageCount": args.page_count});
        let ex_id = crate::domain::derive_extraction_id(&s.document_id, &args.extractor_versions, &config);
        let config_hash = crate::domain::canonical_json::canonical_sha256(&serde_json::json!({"versions": args.extractor_versions, "config": config}));
        let state = extraction::ensure_extraction(&core.db.writer, &ex_id, &s.document_id, &config_hash, &args.extractor_versions).await?;
        let have: i64 = sqlx::query_scalar("select count(*) from sections where extraction_id = ?").bind(&ex_id).fetch_one(&core.db.reader).await?;
        if have != args.page_count as i64 {
            // One transaction: thousands of single inserts would stall the only writer.
            let mut tx = core.db.writer.begin().await?;
            for p in 0..args.page_count {
                let loc = format!("page:{p}");
                let sid = crate::domain::derive_section_id(&ex_id, p, &loc);
                sqlx::query("insert into sections(id, extraction_id, ordinal, locator_json, state) values (?,?,?,?, 'pending') on conflict(id) do nothing")
                    .bind(&sid)
                    .bind(&ex_id)
                    .bind(p as i64)
                    .bind(serde_json::json!({"kind": "pdf", "pageIndex": p}).to_string())
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
        }
        if let Some(t) = args.title.as_deref().map(|t| t.trim()).filter(|t| !t.is_empty() && t.len() < 400) {
            let _ = documents::merge_metadata(&core.db.writer, &s.document_id, &serde_json::json!({"pdfTitle": t, "pageCount": args.page_count})).await;
        } else {
            let _ = documents::merge_metadata(&core.db.writer, &s.document_id, &serde_json::json!({"pageCount": args.page_count})).await;
        }
        // Activate immediately so published pages are readable while indexing continues.
        extraction::activate(&core.db.writer, &s.document_id, &ex_id, if state == "ready" { "ready" } else { "indexing" }).await?;
        *s.extraction_id.write().unwrap() = Some(ex_id.clone());
        let token = crate::domain::new_uuid();
        s.extraction_tokens.lock().unwrap().insert(token.clone(), ex_id.clone());
        let sections = extraction::list_sections(&core.db.reader, &ex_id).await?;
        Ok(ExtractionBeginResult { extraction_id: ex_id, extraction_token: token, state, sections })
    }
    .await;
    Ok(respond(&meta, r))
}

fn token_extraction(s: &Session, token: &str) -> AppResult<String> {
    s.extraction_tokens.lock().unwrap().get(token).cloned().ok_or_else(|| AppError::invalid("추출 토큰이 올바르지 않습니다."))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BlocksCommitBatchArgs {
    pub session_id: String,
    pub extraction_token: String,
    pub section_id: String,
    pub batch_index: u32,
    pub blocks: Vec<Block>,
}

#[tauri::command]
pub async fn blocks_commit_batch(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: BlocksCommitBatchArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        let ex = token_extraction(&s, &args.extraction_token)?;
        check_id(&args.section_id, "sectionId")?;
        core.db.ensure_writable()?;
        let (checksum, count) = extraction::stage_batch(&core.db.writer, &ex, &args.section_id, args.batch_index, &args.blocks).await?;
        Ok(serde_json::json!({"checksum": checksum, "count": count}))
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ExtractionFinalizeSectionArgs {
    pub session_id: String,
    pub extraction_token: String,
    pub section_id: String,
    pub expected_count: u32,
    pub checksum: String,
    /// Page-level diagnostics (quality reasons) to keep in section metadata.
    pub failed: Option<bool>,
}

#[tauri::command]
pub async fn extraction_finalize_section(core: Core<'_>, window: WebviewWindow, meta: CommandMeta, args: ExtractionFinalizeSectionArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        let ex = token_extraction(&s, &args.extraction_token)?;
        check_id(&args.section_id, "sectionId")?;
        if args.failed == Some(true) {
            extraction::mark_section_failed_in(&core.db.writer, &ex, &args.section_id).await?;
        } else {
            extraction::finalize_section(&core.db.writer, &ex, &args.section_id, args.expected_count, &args.checksum).await?;
        }
        // Document state: ready when every section is ready, partial when any failed.
        let row: (i64, i64, i64) = sqlx::query_as(
            "select count(*), sum(state = 'ready'), sum(state = 'failed') from sections where extraction_id = ?",
        )
        .bind(&ex)
        .fetch_one(&core.db.reader)
        .await?;
        let (total, ready, failed) = row;
        let doc_state = if ready == total { "ready" } else if ready + failed == total { "partial" } else { "indexing" };
        if ready + failed == total {
            extraction::set_extraction_state(&core.db.writer, &ex, if failed > 0 { "partial" } else { "ready" }).await?;
        }
        documents::set_state(&core.db.writer, &s.document_id, doc_state).await?;
        s.emit("index_progress", None, None, serde_json::json!({"ready": ready, "failed": failed, "total": total, "sectionId": args.section_id}));
        Ok(serde_json::json!({"ready": ready, "failed": failed, "total": total, "documentState": doc_state}))
    }
    .await;
    Ok(respond(&meta, r))
}

// ------------------------------------------------------------------ external links

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct OpenExternalArgs {
    pub url: String,
}

/// Explicit user click only. http(s) validated in Rust; everything else refused.
#[tauri::command]
pub async fn open_external(meta: CommandMeta, args: OpenExternalArgs, app: tauri::AppHandle) -> Result<Wire, ()> {
    let r = (|| {
        let u = url::Url::parse(&args.url).map_err(|_| AppError::invalid("열 수 없는 링크입니다."))?;
        if !matches!(u.scheme(), "http" | "https") || u.username() != "" || u.password().is_some() || u.host_str().is_none() || args.url.len() > 2048 {
            return Err(AppError::new(ErrorCode::PermissionDenied, "http 또는 https 링크만 열 수 있습니다."));
        }
        use tauri_plugin_opener::OpenerExt;
        app.opener().open_url(u.as_str(), None::<&str>).map_err(|_| AppError::internal("브라우저를 열 수 없습니다."))?;
        Ok(serde_json::json!({"opened": true}))
    })();
    Ok(respond(&meta, r))
}
