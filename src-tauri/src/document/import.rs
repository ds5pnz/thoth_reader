//! Journaled import (spec §2.2, §9.3): OS-selected paths become single-use import tokens;
//! the file is copied into a temp file inside the app dir while hashing, sniffed, then
//! atomically renamed. Restart recovery reconciles journal, files and DB.

use super::paths::AppPaths;
use crate::domain::{hashing::StreamHasher, new_uuid, now_ms};
use crate::error::{AppError, AppResult, ErrorCode};
use crate::storage::repo::{documents, misc};
use crate::storage::Db;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use ts_rs::TS;

pub const MAX_FILE_BYTES: u64 = 500 * 1024 * 1024;
const TOKEN_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ImportCandidate {
    pub import_token: String,
    /// File name only, never the full path.
    pub file_name: String,
    pub byte_size: u64,
}

/// Paths are accepted only at this stage (OS dialog or drag and drop).
#[derive(Default)]
pub struct ImportRegistry {
    tokens: Mutex<HashMap<String, (PathBuf, Instant)>>,
}

impl ImportRegistry {
    pub fn register(&self, path: PathBuf) -> Option<ImportCandidate> {
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() {
            return None;
        }
        let token = new_uuid();
        let file_name = path.file_name()?.to_string_lossy().to_string();
        let mut t = self.tokens.lock().unwrap();
        t.retain(|_, (_, at)| at.elapsed() < TOKEN_TTL);
        t.insert(token.clone(), (path, Instant::now()));
        Some(ImportCandidate { import_token: token, file_name, byte_size: meta.len() })
    }
    /// Single use.
    pub fn take(&self, token: &str) -> Option<PathBuf> {
        let mut t = self.tokens.lock().unwrap();
        match t.remove(token) {
            Some((p, at)) if at.elapsed() < TOKEN_TTL => Some(p),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Pdf,
    Epub,
}

impl Format {
    pub fn as_str(&self) -> &'static str {
        match self {
            Format::Pdf => "pdf",
            Format::Epub => "epub",
        }
    }
}

/// Signature sniffing; extension is never trusted.
pub fn sniff(head: &[u8]) -> Option<Format> {
    if head.len() >= 5 && head.windows(5).take(1024).any(|w| w == b"%PDF-") {
        return Some(Format::Pdf);
    }
    if head.starts_with(b"PK\x03\x04") {
        // EPUB OCF: first entry should be the uncompressed "mimetype" file. Deeper
        // validation happens in epub_security; here we only accept ZIP containers that
        // look like EPUB.
        if head.len() > 58 && &head[30..38] == b"mimetype" {
            let rest = &head[38..head.len().min(38 + 40)];
            if rest.windows(20).any(|w| w == b"application/epub+zip") {
                return Some(Format::Epub);
            }
        }
        return Some(Format::Epub); // ZIP without canonical mimetype: let the validator restrict/reject
    }
    None
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ImportResult {
    pub document_id: String,
    pub state: String,
    pub deduplicated: bool,
    pub format: String,
}

fn title_from_name(name: &str) -> String {
    let stem = Path::new(name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let t: String = stem.chars().filter(|c| !c.is_control()).take(200).collect();
    if t.trim().is_empty() { "제목 없음".into() } else { t }
}

/// Copy `src` into a temp file inside `documents/` while hashing; enforces the size cap on
/// actual bytes read (not metadata). Blocking: call from spawn_blocking.
fn copy_and_hash(src: &Path, tmp: &Path) -> AppResult<(String, u64, Vec<u8>)> {
    let mut f = std::fs::File::open(src).map_err(|_| AppError::invalid("파일을 열 수 없습니다. 접근 권한을 확인해 주세요."))?;
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(tmp)?;
    let mut hasher = StreamHasher::default();
    let mut buf = vec![0u8; 1 << 20];
    let mut total: u64 = 0;
    let mut head = Vec::with_capacity(1024);
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_FILE_BYTES {
            drop(out);
            let _ = std::fs::remove_file(tmp);
            return Err(AppError::limit("파일이 500MiB를 넘어 가져올 수 없습니다."));
        }
        if head.len() < 1024 {
            let take = (1024 - head.len()).min(n);
            head.extend_from_slice(&buf[..take]);
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
    }
    out.sync_all()?;
    Ok((hasher.finish(), total, head))
}

pub async fn import_path(db: &Db, paths: &AppPaths, src: PathBuf, file_name: String) -> AppResult<ImportResult> {
    db.ensure_writable()?;
    let tmp_rel = format!("documents/.tmp-{}", new_uuid());
    let tmp = paths.resolve(&tmp_rel).ok_or_else(|| AppError::internal("경로 오류"))?;
    let op = misc::fileop_begin(&db.writer, "-", "import", &serde_json::json!({"tmp": tmp_rel})).await?;
    let tmp2 = tmp.clone();
    let copied = tokio::task::spawn_blocking(move || copy_and_hash(&src, &tmp2))
        .await
        .map_err(|_| AppError::internal("가져오기 작업이 중단되었습니다."))?;
    let (sha, size, head) = match copied {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            misc::fileop_set_state(&db.writer, &op, "failed").await?;
            return Err(e);
        }
    };
    let Some(format) = sniff(&head) else {
        let _ = std::fs::remove_file(&tmp);
        misc::fileop_set_state(&db.writer, &op, "failed").await?;
        return Err(AppError::new(ErrorCode::UnsupportedFormat, "PDF 또는 EPUB 파일만 가져올 수 있습니다."));
    };
    if let Some(existing) = documents::by_sha(&db.reader, &sha).await? {
        let _ = std::fs::remove_file(&tmp);
        misc::fileop_set_state(&db.writer, &op, "complete").await?;
        return Ok(ImportResult { document_id: existing.id, state: existing.state, deduplicated: true, format: existing.format });
    }
    let target_rel = format!("documents/{}.{}", sha, format.as_str());
    let target = paths.resolve(&target_rel).ok_or_else(|| AppError::internal("경로 오류"))?;
    let doc_id = new_uuid();
    let title = title_from_name(&file_name);
    let manifest = serde_json::json!({"tmp": tmp_rel, "target": target_rel, "sha256": sha, "format": format.as_str(), "title": title, "byteSize": size, "documentId": doc_id});
    sqlx::query("update file_operations set document_id = ?, manifest_json = ? where id = ?")
        .bind(&doc_id)
        .bind(manifest.to_string())
        .bind(&op)
        .execute(&db.writer)
        .await?;
    std::fs::rename(&tmp, &target)?;
    // make the rename durable
    if let Ok(dir) = std::fs::File::open(&paths.documents) {
        let _ = dir.sync_all();
    }
    misc::fileop_set_state(&db.writer, &op, "files_applied").await?;
    insert_doc_row(db, &doc_id, &sha, format.as_str(), &title, &target_rel, size as i64).await?;
    misc::fileop_set_state(&db.writer, &op, "complete").await?;
    Ok(ImportResult { document_id: doc_id, state: "indexing".into(), deduplicated: false, format: format.as_str().into() })
}

async fn insert_doc_row(db: &Db, id: &str, sha: &str, format: &str, title: &str, rel: &str, size: i64) -> AppResult<()> {
    let mut conn = db.writer.acquire().await?;
    let row = documents::DocumentRow {
        id: id.into(),
        sha256: sha.into(),
        format: format.into(),
        title: title.into(),
        managed_path: rel.into(),
        byte_size: size,
        state: "indexing".into(),
        active_extraction_id: None,
        metadata: serde_json::json!({}),
        created_at: now_ms(),
        last_opened_at: None,
    };
    documents::insert(&mut conn, &row).await
}

/// Startup reconciliation of interrupted imports/deletes.
pub async fn recover(db: &Db, paths: &AppPaths) -> AppResult<()> {
    if db.read_only_reason.is_some() {
        return Ok(());
    }
    for (id, doc_id, kind, state, manifest) in misc::fileops_unfinished(&db.reader).await? {
        let s = |k: &str| manifest.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
        match kind.as_str() {
            "import" => {
                let tmp = s("tmp").and_then(|r| paths.resolve(&r));
                let target = s("target").and_then(|r| paths.resolve(&r));
                let target_exists = target.as_ref().map(|t| t.exists()).unwrap_or(false);
                let row_exists = documents::get(&db.reader, &doc_id).await?.is_some();
                if target_exists && !row_exists {
                    if let (Some(sha), Some(fmt), Some(title), Some(rel)) = (s("sha256"), s("format"), s("title"), s("target")) {
                        let verified = target.as_ref().and_then(|t| hash_file(t).ok()).map(|h| h == sha).unwrap_or(false);
                        if verified && documents::by_sha(&db.reader, &sha).await?.is_none() {
                            let size = manifest.get("byteSize").and_then(|v| v.as_i64()).unwrap_or(0);
                            insert_doc_row(db, &doc_id, &sha, &fmt, &title, &rel, size).await?;
                            misc::fileop_set_state(&db.writer, &id, "complete").await?;
                            continue;
                        }
                    }
                }
                if let Some(t) = tmp {
                    let _ = std::fs::remove_file(t);
                }
                misc::fileop_set_state(&db.writer, &id, if row_exists { "complete" } else { "failed" }).await?;
            }
            "delete" => {
                if state == "planned" {
                    misc::delete_document_rows(&db.writer, &doc_id).await?;
                }
                remove_document_files(paths, &manifest);
                misc::fileop_set_state(&db.writer, &id, "complete").await?;
            }
            _ => {}
        }
    }
    // Orphan temp files without a journal.
    if let Ok(rd) = std::fs::read_dir(&paths.documents) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with(".tmp-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}

fn hash_file(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = StreamHasher::default();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish())
}

fn remove_document_files(paths: &AppPaths, manifest: &serde_json::Value) {
    if let Some(rel) = manifest.get("managed").and_then(|v| v.as_str()) {
        // Only app-managed relative paths are ever deleted; external originals never are.
        if rel.starts_with("documents/") {
            if let Some(p) = paths.resolve(rel) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    if let Some(arr) = manifest.get("derived").and_then(|v| v.as_array()) {
        for d in arr.iter().filter_map(|v| v.as_str()) {
            if d.starts_with("derived/") {
                if let Some(p) = paths.resolve(d) {
                    let _ = std::fs::remove_dir_all(p);
                }
            }
        }
    }
}

/// Delete a document: journal first, DB rows in one transaction, then files (spec §9.3).
pub async fn delete_document(db: &Db, paths: &AppPaths, document_id: &str) -> AppResult<()> {
    db.ensure_writable()?;
    let doc = documents::get(&db.reader, document_id).await?.ok_or_else(|| AppError::not_found("문서"))?;
    let extractions: Vec<String> = sqlx::query_scalar("select id from extractions where document_id = ?")
        .bind(document_id)
        .fetch_all(&db.reader)
        .await?;
    let manifest = serde_json::json!({
        "managed": doc.managed_path,
        "derived": extractions.iter().map(|e| format!("derived/{e}")).collect::<Vec<_>>(),
        "sha256": doc.sha256,
    });
    let op = misc::fileop_begin(&db.writer, document_id, "delete", &manifest).await?;
    misc::delete_document_rows(&db.writer, document_id).await?;
    misc::fileop_set_state(&db.writer, &op, "db_committed").await?;
    remove_document_files(paths, &manifest);
    crate::document::epub::remove_cover(paths, document_id);
    misc::fileop_set_state(&db.writer, &op, "complete").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> (Db, AppPaths, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(d.path().join("app")).unwrap();
        let db = Db::open(&paths.db_file(), &paths.backups).await.unwrap();
        (db, paths, d)
    }

    #[tokio::test]
    async fn import_dedupes_and_keeps_managed_copy() {
        let (db, paths, d) = setup().await;
        let src = d.path().join("book.pdf");
        std::fs::write(&src, b"%PDF-1.7\n%stuff\n").unwrap();
        let r1 = import_path(&db, &paths, src.clone(), "book.pdf".into()).await.unwrap();
        assert!(!r1.deduplicated);
        // modify external original: managed copy must not change
        std::fs::write(&src, b"%PDF-1.7\nchanged\n").unwrap();
        let doc = documents::get(&db.reader, &r1.document_id).await.unwrap().unwrap();
        let managed = paths.resolve(&doc.managed_path).unwrap();
        assert_eq!(std::fs::read(&managed).unwrap(), b"%PDF-1.7\n%stuff\n");
        // same bytes again => dedupe
        std::fs::write(&src, b"%PDF-1.7\n%stuff\n").unwrap();
        let r2 = import_path(&db, &paths, src.clone(), "copy.pdf".into()).await.unwrap();
        assert!(r2.deduplicated);
        assert_eq!(r2.document_id, r1.document_id);
    }

    #[tokio::test]
    async fn rejects_unknown_signature() {
        let (db, paths, d) = setup().await;
        let src = d.path().join("x.pdf");
        std::fs::write(&src, b"not a pdf").unwrap();
        let e = import_path(&db, &paths, src, "x.pdf".into()).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedFormat);
        assert_eq!(std::fs::read_dir(&paths.documents).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn delete_keeps_external_original_and_recovers() {
        let (db, paths, d) = setup().await;
        let src = d.path().join("b.pdf");
        std::fs::write(&src, b"%PDF-1.4 abc").unwrap();
        let r = import_path(&db, &paths, src.clone(), "b.pdf".into()).await.unwrap();
        // Simulate crash after journal 'planned' for delete (T20)
        let doc = documents::get(&db.reader, &r.document_id).await.unwrap().unwrap();
        misc::fileop_begin(&db.writer, &r.document_id, "delete", &serde_json::json!({"managed": doc.managed_path, "derived": []})).await.unwrap();
        recover(&db, &paths).await.unwrap();
        assert!(documents::get(&db.reader, &r.document_id).await.unwrap().is_none());
        assert!(!paths.resolve(&doc.managed_path).unwrap().exists());
        assert!(src.exists(), "external original must be preserved");
        assert!(misc::fileops_unfinished(&db.reader).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn recover_resumes_import_after_rename() {
        let (db, paths, _d) = setup().await;
        let sha = crate::domain::sha256_hex(b"%PDF-1.5 x");
        let rel = format!("documents/{sha}.pdf");
        std::fs::write(paths.resolve(&rel).unwrap(), b"%PDF-1.5 x").unwrap();
        let doc_id = new_uuid();
        let op = misc::fileop_begin(&db.writer, &doc_id, "import", &serde_json::json!({"tmp": "documents/.tmp-zz", "target": rel, "sha256": sha, "format": "pdf", "title": "t", "byteSize": 10, "documentId": doc_id})).await.unwrap();
        sqlx::query("update file_operations set document_id = ? where id = ?").bind(&doc_id).bind(&op).execute(&db.writer).await.unwrap();
        misc::fileop_set_state(&db.writer, &op, "files_applied").await.unwrap();
        recover(&db, &paths).await.unwrap();
        assert!(documents::get(&db.reader, &doc_id).await.unwrap().is_some());
    }

    #[test]
    fn registry_tokens_single_use() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.epub");
        std::fs::write(&f, b"PK\x03\x04").unwrap();
        let r = ImportRegistry::default();
        let c = r.register(f).unwrap();
        assert_eq!(c.file_name, "a.epub");
        assert!(r.take(&c.import_token).is_some());
        assert!(r.take(&c.import_token).is_none());
        assert!(r.take("forged").is_none());
    }
}
