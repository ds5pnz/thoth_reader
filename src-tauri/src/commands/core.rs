//! Command envelope, receipts and validation helpers (spec §8.1).

use crate::document::import::ImportRegistry;
use crate::document::paths::AppPaths;
use crate::document::session::SessionManager;
use crate::domain::canonical_json::canonical_sha256;
use crate::error::{AppError, AppResult, WireError};
use crate::storage::repo::misc::{self, Receipt};
use crate::storage::Db;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use ts_rs::TS;

pub const MAX_COMMAND_JSON: usize = 256 * 1024;

pub struct AppCore {
    pub db: Db,
    pub paths: AppPaths,
    pub sessions: SessionManager,
    pub imports: ImportRegistry,
    pub translation: Arc<crate::translation::service::TranslationService>,
    pub allowed_origins: Vec<&'static str>,
    pub library_assets: crate::document::asset::LibraryAssets,
    /// Serializes EPUB safe-package builds (import, backfill and open may race).
    pub epub_prepare: tokio::sync::Mutex<()>,
    /// Serializes imports (duplicate check + insert).
    pub import_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CommandMeta {
    pub api_version: u32,
    pub request_id: String,
}

impl CommandMeta {
    pub fn validate(&self) -> AppResult<()> {
        if self.api_version != 1 {
            return Err(AppError::invalid("지원하지 않는 API 버전입니다."));
        }
        if self.request_id.is_empty() || self.request_id.len() > 80 || !self.request_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(AppError::invalid("requestId가 올바르지 않습니다."));
        }
        Ok(())
    }
}

/// Wire shape: `{ok:true, requestId, data}` | `{ok:false, requestId, error}` (TS type in src/ipc/client.ts).
pub type Wire = serde_json::Value;

pub fn ok_value(request_id: &str, data: serde_json::Value) -> Wire {
    serde_json::json!({"ok": true, "requestId": request_id, "data": data})
}

pub fn err_value(request_id: &str, e: &AppError) -> Wire {
    serde_json::json!({"ok": false, "requestId": request_id, "error": WireError::from(e)})
}

pub fn respond<T: Serialize>(meta: &CommandMeta, r: AppResult<T>) -> Wire {
    let r = meta.validate().and(r);
    match r.and_then(|v| serde_json::to_value(v).map_err(AppError::from)) {
        Ok(v) => ok_value(&meta.request_id, v),
        Err(e) => err_value(&meta.request_id, &e),
    }
}

/// Mutation wrapper: requestId replay returns the stored result; same id + different
/// payload => CONFLICT. Successful results are stored after the work commits.
pub async fn with_receipt<T, F, Fut>(core: &AppCore, meta: &CommandMeta, command: &str, payload: serde_json::Result<serde_json::Value>, f: F) -> Wire
where
    T: Serialize,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = AppResult<T>>,
{
    if let Err(e) = meta.validate() {
        return err_value(&meta.request_id, &e);
    }
    let payload_json = match payload {
        Ok(v) => v,
        Err(e) => return err_value(&meta.request_id, &AppError::from(e)),
    };
    if payload_json.to_string().len() > MAX_COMMAND_JSON {
        return err_value(&meta.request_id, &AppError::limit("요청이 256KiB를 넘습니다."));
    }
    let hash = canonical_sha256(&payload_json);
    match misc::receipt_check(&core.db.reader, &meta.request_id, command, &hash).await {
        Ok(Receipt::Replay(v)) => return ok_value(&meta.request_id, v),
        Ok(Receipt::New) => {}
        Err(e) => return err_value(&meta.request_id, &e),
    }
    if let Err(e) = core.db.ensure_writable() {
        return err_value(&meta.request_id, &e);
    }
    match f().await.and_then(|v| serde_json::to_value(v).map_err(AppError::from)) {
        Ok(v) => {
            let _ = misc::receipt_store(&core.db.writer, &meta.request_id, command, &hash, &v).await;
            ok_value(&meta.request_id, v)
        }
        Err(e) => err_value(&meta.request_id, &e),
    }
}

pub fn check_id(s: &str, what: &str) -> AppResult<()> {
    if crate::domain::is_valid_id(s) {
        Ok(())
    } else {
        Err(AppError::invalid(format!("{what}가 올바르지 않습니다.")))
    }
}

pub fn check_ids(v: &[String], max: usize, what: &str) -> AppResult<()> {
    if v.len() > max {
        return Err(AppError::limit(format!("{what}는 최대 {max}개입니다.")));
    }
    v.iter().try_for_each(|s| check_id(s, what))
}

pub fn check_lang(tag: &str) -> AppResult<()> {
    // BCP 47 shape check (letters/digits/hyphen, 2..35 chars).
    let ok = (2..=35).contains(&tag.len())
        && tag.split('-').all(|p| !p.is_empty() && p.len() <= 8 && p.bytes().all(|b| b.is_ascii_alphanumeric()))
        && tag.split('-').next().map(|p| p.bytes().all(|b| b.is_ascii_alphabetic())).unwrap_or(false);
    if ok { Ok(()) } else { Err(AppError::invalid("언어 태그가 올바르지 않습니다.")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lang_tags() {
        assert!(check_lang("en").is_ok());
        assert!(check_lang("ko-KR").is_ok());
        assert!(check_lang("zh-Hant-TW").is_ok());
        assert!(check_lang("1x").is_err());
        assert!(check_lang("en_US").is_err());
        assert!(check_lang("").is_err());
    }
}

/// Serialize command args for receipt hashing (evaluated before the work closure).
pub fn json_of<T: Serialize>(v: &T) -> serde_json::Result<serde_json::Value> {
    serde_json::to_value(v)
}
