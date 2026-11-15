//! Reader sessions (spec §7.3, §8): sessionId + generation + opaque asset token per open
//! document. Generation bumps cancel all work of the previous generation.

use super::import::Format;
use super::sink::{EventSink, SessionStream};
use crate::domain::new_uuid;
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ReadingMode {
    Parallel,
    Interleaved,
    Original,
    /// Translation only; the source of a paragraph is shown on demand.
    Translation,
}

#[derive(Debug, Clone)]
pub struct ReaderState {
    pub mode: ReadingMode,
    pub auto_paused: bool,
    pub profile_id: Option<String>,
    pub target_language: String,
    pub source_language: Option<String>,
    pub last_viewport_seq: u64,
    pub visible: Vec<String>,
    pub direction_forward: bool,
}

pub struct Session {
    pub id: String,
    pub document_id: String,
    pub format: Format,
    pub window_label: String,
    pub asset_token: String,
    pub managed_file: PathBuf,
    pub extraction_id: RwLock<Option<String>>,
    pub derived_dir: RwLock<Option<PathBuf>>,
    generation: AtomicU64,
    cancel: Mutex<CancellationToken>,
    pub stream: SessionStream,
    pub state: Mutex<ReaderState>,
    /// PDF staging tokens issued by extraction_begin.
    pub extraction_tokens: Mutex<HashMap<String, String>>,
}

impl Session {
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.lock().unwrap().clone()
    }
    pub fn extraction(&self) -> AppResult<String> {
        self.extraction_id.read().unwrap().clone().ok_or_else(|| AppError::invalid("문서 추출이 아직 준비되지 않았습니다."))
    }
    pub fn check_generation(&self, g: u64) -> AppResult<()> {
        if g != self.generation() {
            return Err(AppError::stale());
        }
        Ok(())
    }
    /// Provider/model/target-language change or close: new generation, cancel old work.
    pub fn bump_generation(&self) -> u64 {
        let mut c = self.cancel.lock().unwrap();
        c.cancel();
        *c = CancellationToken::new();
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }
    pub fn emit(&self, kind: &str, job_id: Option<&str>, block_id: Option<&str>, payload: serde_json::Value) {
        self.stream.emit(self.generation(), kind, job_id, block_id, payload);
    }
    pub fn emit_for_generation(&self, generation: u64, kind: &str, job_id: Option<&str>, block_id: Option<&str>, payload: serde_json::Value) {
        // Events of an older generation are never delivered to the new session UI.
        if generation == self.generation() {
            self.stream.emit(generation, kind, job_id, block_id, payload);
        }
    }
}

#[derive(Default)]
pub struct SessionManager {
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    assets: RwLock<HashMap<String, String>>,
}

pub struct OpenParams {
    pub document_id: String,
    pub format: Format,
    pub window_label: String,
    pub managed_file: PathBuf,
    pub sink: Arc<dyn EventSink>,
    pub target_language: String,
    pub source_language: Option<String>,
    pub profile_id: Option<String>,
}

impl SessionManager {
    pub fn open(&self, p: OpenParams) -> Arc<Session> {
        // One active document per window: close previous sessions of this window.
        let old: Vec<String> = self
            .sessions
            .read()
            .unwrap()
            .values()
            .filter(|s| s.window_label == p.window_label)
            .map(|s| s.id.clone())
            .collect();
        for id in old {
            self.close(&id);
        }
        let id = new_uuid();
        let token = format!("{}{}", new_uuid().replace('-', ""), new_uuid().replace('-', ""));
        let s = Arc::new(Session {
            id: id.clone(),
            document_id: p.document_id,
            format: p.format,
            window_label: p.window_label,
            asset_token: token.clone(),
            managed_file: p.managed_file,
            extraction_id: RwLock::new(None),
            derived_dir: RwLock::new(None),
            generation: AtomicU64::new(1),
            cancel: Mutex::new(CancellationToken::new()),
            stream: SessionStream::new(id.clone(), p.sink),
            state: Mutex::new(ReaderState {
                mode: ReadingMode::Parallel,
                auto_paused: false,
                profile_id: p.profile_id,
                target_language: p.target_language,
                source_language: p.source_language,
                last_viewport_seq: 0,
                visible: vec![],
                direction_forward: true,
            }),
            extraction_tokens: Mutex::new(HashMap::new()),
        });
        self.sessions.write().unwrap().insert(id.clone(), s.clone());
        self.assets.write().unwrap().insert(token, id);
        s
    }

    /// Validates that the caller window owns the session.
    pub fn get(&self, session_id: &str, window_label: &str) -> AppResult<Arc<Session>> {
        let s = self.sessions.read().unwrap().get(session_id).cloned().ok_or_else(AppError::stale)?;
        if s.window_label != window_label {
            return Err(AppError::new(crate::error::ErrorCode::PermissionDenied, "이 창에서 접근할 수 없는 문서 세션입니다."));
        }
        Ok(s)
    }

    pub fn get_any(&self, session_id: &str) -> Option<Arc<Session>> {
        self.sessions.read().unwrap().get(session_id).cloned()
    }

    pub fn by_asset_token(&self, token: &str) -> Option<Arc<Session>> {
        let sid = self.assets.read().unwrap().get(token).cloned()?;
        self.get_any(&sid)
    }

    pub fn for_document(&self, document_id: &str) -> Vec<Arc<Session>> {
        self.sessions.read().unwrap().values().filter(|s| s.document_id == document_id).cloned().collect()
    }

    pub fn all(&self) -> Vec<Arc<Session>> {
        self.sessions.read().unwrap().values().cloned().collect()
    }

    pub fn close(&self, session_id: &str) -> Option<Arc<Session>> {
        let s = self.sessions.write().unwrap().remove(session_id)?;
        self.assets.write().unwrap().remove(&s.asset_token);
        s.bump_generation();
        s.stream.emit(s.generation(), "session_closed", None, None, serde_json::json!({}));
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::sink::VecSink;

    fn params(win: &str) -> OpenParams {
        OpenParams {
            document_id: "d".into(),
            format: Format::Pdf,
            window_label: win.into(),
            managed_file: PathBuf::from("/x"),
            sink: Arc::new(VecSink::default()),
            target_language: "ko".into(),
            source_language: Some("en".into()),
            profile_id: None,
        }
    }

    #[test]
    fn token_revoked_on_close_and_window_checked() {
        let m = SessionManager::default();
        let s = m.open(params("main"));
        assert!(m.by_asset_token(&s.asset_token).is_some());
        assert!(m.get(&s.id, "other").is_err());
        let tok = s.cancel_token();
        m.close(&s.id);
        assert!(tok.is_cancelled());
        assert!(m.by_asset_token(&s.asset_token).is_none());
        assert!(m.get(&s.id, "main").is_err());
    }

    #[test]
    fn generation_bump_rejects_stale() {
        let m = SessionManager::default();
        let s = m.open(params("main"));
        let g = s.generation();
        s.bump_generation();
        assert!(s.check_generation(g).is_err());
        assert!(s.check_generation(g + 1).is_ok());
    }

    #[test]
    fn reopening_in_same_window_closes_previous() {
        let m = SessionManager::default();
        let a = m.open(params("main"));
        let b = m.open(params("main"));
        assert!(m.get_any(&a.id).is_none());
        assert!(m.get_any(&b.id).is_some());
    }
}
