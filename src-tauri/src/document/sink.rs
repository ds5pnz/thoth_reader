//! Session event sink abstraction so domain services don't depend on Tauri Channel.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct StreamEnvelope {
    pub api_version: u32,
    pub session_id: String,
    pub generation: u64,
    pub sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    #[serde(rename = "type")]
    #[ts(rename = "type", type = "'job_state' | 'translation_delta' | 'translation_committed' | 'index_progress' | 'session_closed' | 'import_progress' | 'batch_progress'")]
    pub kind: String,
    #[ts(type = "Record<string, unknown>")]
    pub payload: serde_json::Value,
}

pub trait EventSink: Send + Sync {
    /// Must never block on the UI; failures are ignored (DB is the source of truth).
    fn send(&self, env: StreamEnvelope);
}

/// Monotonic per-session sequence + sink.
pub struct SessionStream {
    pub session_id: String,
    seq: AtomicU64,
    sink: Arc<dyn EventSink>,
}

impl SessionStream {
    pub fn new(session_id: String, sink: Arc<dyn EventSink>) -> Self {
        Self { session_id, seq: AtomicU64::new(0), sink }
    }
    pub fn current_sequence(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }
    pub fn emit(&self, generation: u64, kind: &str, job_id: Option<&str>, block_id: Option<&str>, payload: serde_json::Value) {
        let sequence = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.sink.send(StreamEnvelope {
            api_version: 1,
            session_id: self.session_id.clone(),
            generation,
            sequence,
            job_id: job_id.map(|s| s.to_string()),
            block_id: block_id.map(|s| s.to_string()),
            kind: kind.into(),
            payload,
        });
    }
}

/// Test sink collecting envelopes.
#[derive(Default)]
pub struct VecSink(pub std::sync::Mutex<Vec<StreamEnvelope>>);
impl EventSink for VecSink {
    fn send(&self, env: StreamEnvelope) {
        self.0.lock().unwrap().push(env);
    }
}
