//! Deterministic in-process provider for scheduler / integration tests.
//!
//! * Output is derived from the request only: for each segment in the prompt's user JSON
//!   (`translation::prompt` format) the text becomes `"[<target>] <source text>"`, so
//!   placeholders survive. JSON-segment requests get a `{"segments":[...]}` body.
//! * Delays use `tokio::time` (`tokio::time::pause()` works if the `test-util` feature is enabled).
//! * Failures are scripted per call (FIFO); after the script is exhausted the default
//!   behaviour applies. Every received request is recorded.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use super::{
    ChatRole, ModelInfo, ProbeReport, ProbeRequest, ProviderCapabilities, ProviderDiagnostics, ProviderError,
    ProviderKind, ResponseFormat, TranslationDelta, TranslationOutput, TranslationProvider, TranslationRequest, Usage,
};

/// What a single `translate` call does.
#[derive(Debug, Clone, PartialEq)]
pub enum MockStep {
    /// Deterministic success (or the given text).
    Succeed { text: Option<String> },
    /// Fail before producing any delta.
    Fail(ProviderError),
    /// Stream `chunks` deltas, then fail.
    FailAfterChunks { chunks: usize, error: ProviderError },
    /// Never complete (until cancelled).
    Hang,
}

#[derive(Debug, Clone)]
pub struct MockConfig {
    /// Delay before the first delta.
    pub first_chunk_delay: Duration,
    /// Delay between deltas.
    pub chunk_delay: Duration,
    /// Delay after the last delta before returning.
    pub complete_delay: Duration,
    /// Number of deltas the output is split into (≥1).
    pub chunks: usize,
    pub capabilities: ProviderCapabilities,
    pub models: Vec<ModelInfo>,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            first_chunk_delay: Duration::ZERO,
            chunk_delay: Duration::ZERO,
            complete_delay: Duration::ZERO,
            chunks: 3,
            capabilities: ProviderCapabilities {
                structured_output: true,
                ..ProviderCapabilities::default_for(ProviderKind::OpenaiCompatible)
            },
            models: vec![ModelInfo { id: "mock-model".into(), digest: Some("sha256:mock".into()), context_length: Some(8192) }],
        }
    }
}

#[derive(Debug, Default)]
pub struct MockProvider {
    config: Mutex<MockConfig>,
    script: Mutex<VecDeque<MockStep>>,
    requests: Mutex<Vec<TranslationRequest>>,
    calls: AtomicUsize,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl MockProvider {
    pub fn new(config: MockConfig) -> Self {
        Self { config: Mutex::new(config), ..Default::default() }
    }

    /// Queue behaviours for the next calls (FIFO).
    pub fn script(&self, steps: impl IntoIterator<Item = MockStep>) {
        self.script.lock().unwrap().extend(steps);
    }

    pub fn set_config(&self, config: MockConfig) {
        *self.config.lock().unwrap() = config;
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Highest number of concurrent `translate` calls observed.
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<TranslationRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// The deterministic output the mock produces for `req`.
    pub fn expected_output(req: &TranslationRequest) -> String {
        let segments = extract_segments(req);
        match req.response_format {
            ResponseFormat::JsonSegments => {
                let segs: Vec<Value> = segments
                    .iter()
                    .map(|(id, text)| json!({"id": id, "text": format!("[{}] {}", req.target_language, text)}))
                    .collect();
                json!({ "segments": segs }).to_string()
            }
            ResponseFormat::Text => {
                let text = segments.first().map(|(_, t)| t.clone()).unwrap_or_default();
                format!("[{}] {}", req.target_language, text)
            }
        }
    }
}

struct InFlight<'a>(&'a AtomicUsize);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Segments from the prompt's user JSON (`{"segments":[{"id","text"}]}`); falls back to
/// the raw user message as a single segment.
fn extract_segments(req: &TranslationRequest) -> Vec<(String, String)> {
    let user = req.messages.iter().rev().find(|m| m.role == ChatRole::User).map(|m| m.content.as_str()).unwrap_or("");
    if let Ok(v) = serde_json::from_str::<Value>(user) {
        if let Some(arr) = v.get("segments").and_then(Value::as_array) {
            return arr
                .iter()
                .filter_map(|s| {
                    Some((s.get("id")?.as_str()?.to_string(), s.get("text")?.as_str()?.to_string()))
                })
                .collect();
        }
    }
    let id = req.expected_segment_ids.first().cloned().unwrap_or_else(|| "s0".into());
    vec![(id, user.to_string())]
}

fn split_chunks(s: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let n = n.max(1).min(chars.len().max(1));
    let size = chars.len().div_ceil(n).max(1);
    chars.chunks(size).map(|c| c.iter().collect()).collect()
}

async fn sleep_or_cancel(d: Duration, cancel: &CancellationToken) -> Result<(), ProviderError> {
    if d.is_zero() {
        return if cancel.is_cancelled() { Err(ProviderError::Cancelled) } else { Ok(()) };
    }
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ProviderError::Cancelled),
        _ = tokio::time::sleep(d) => Ok(()),
    }
}

#[async_trait::async_trait]
impl TranslationProvider for MockProvider {
    async fn probe(&self) -> Result<ProviderCapabilities, ProviderError> {
        Ok(self.config.lock().unwrap().capabilities.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(self.config.lock().unwrap().models.clone())
    }

    async fn translate(
        &self,
        request: TranslationRequest,
        sink: Sender<TranslationDelta>,
        cancel: CancellationToken,
    ) -> Result<TranslationOutput, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        let _guard = InFlight(&self.in_flight);
        self.requests.lock().unwrap().push(request.clone());
        let cfg = self.config.lock().unwrap().clone();
        let step = self.script.lock().unwrap().pop_front().unwrap_or(MockStep::Succeed { text: None });

        sleep_or_cancel(cfg.first_chunk_delay, &cancel).await?;
        let (text, fail_after) = match step {
            MockStep::Fail(e) => return Err(e),
            MockStep::Hang => {
                cancel.cancelled().await;
                return Err(ProviderError::Cancelled);
            }
            MockStep::Succeed { text } => (text.unwrap_or_else(|| Self::expected_output(&request)), None),
            MockStep::FailAfterChunks { chunks, error } => (Self::expected_output(&request), Some((chunks, error))),
        };
        let mut sink = Some(sink);
        for (i, chunk) in split_chunks(&text, cfg.chunks).into_iter().enumerate() {
            if let Some((n, e)) = &fail_after {
                if i >= *n {
                    return Err(e.clone());
                }
            }
            if i > 0 {
                sleep_or_cancel(cfg.chunk_delay, &cancel).await?;
            }
            if let Some(s) = &sink {
                if s.send(TranslationDelta { part_index: request.part_index, text: chunk }).await.is_err() {
                    sink = None;
                }
            }
        }
        if let Some((_, e)) = fail_after {
            return Err(e);
        }
        sleep_or_cancel(cfg.complete_delay, &cancel).await?;
        Ok(TranslationOutput {
            sent_bytes: request.messages.iter().map(|m| m.content.len() as u64).sum(),
            text,
            finish_reason: "stop".into(),
            usage: Some(Usage { prompt_tokens: None, completion_tokens: None }),
        })
    }
}

#[async_trait::async_trait]
impl ProviderDiagnostics for MockProvider {
    async fn probe_model(&self, req: &ProbeRequest) -> Result<ProbeReport, ProviderError> {
        let cfg = self.config.lock().unwrap().clone();
        let model = req.model_id.as_ref().and_then(|id| cfg.models.iter().find(|m| &m.id == id).cloned());
        Ok(ProbeReport {
            reachable: true,
            auth_ok: true,
            models: Some(cfg.models.clone()),
            model_available: req.model_id.as_ref().map(|_| model.is_some()),
            model,
            capabilities: cfg.capabilities,
            sent_test_prompt: false,
            warnings: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ChatMessage, GenerationOptions};

    fn req(format: ResponseFormat) -> TranslationRequest {
        TranslationRequest {
            request_id: "r".into(),
            job_id: "j".into(),
            block_id: "b".into(),
            part_index: 0,
            part_count: 1,
            model_id: "mock-model".into(),
            source_language: "en".into(),
            target_language: "ko".into(),
            messages: vec![
                ChatMessage { role: ChatRole::System, content: "sys".into() },
                ChatMessage {
                    role: ChatRole::User,
                    content: r#"{"blockId":"b","segments":[{"id":"s0","text":"Hello ⟦P1⟧"}]}"#.into(),
                },
            ],
            options: GenerationOptions::default(),
            response_format: format,
            stream: true,
            expected_segment_ids: vec!["s0".into()],
        }
    }

    #[tokio::test]
    async fn deterministic_output_delays_and_script() {
        let m = MockProvider::new(MockConfig {
            first_chunk_delay: Duration::from_millis(60),
            chunk_delay: Duration::from_millis(20),
            ..Default::default()
        });
        m.script([MockStep::Fail(ProviderError::RateLimited { retry_after: None })]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let e = m.translate(req(ResponseFormat::Text), tx.clone(), CancellationToken::new()).await.unwrap_err();
        assert_eq!(e, ProviderError::RateLimited { retry_after: None });

        let start = tokio::time::Instant::now();
        let out = m.translate(req(ResponseFormat::Text), tx, CancellationToken::new()).await.unwrap();
        assert_eq!(out.text, "[ko] Hello ⟦P1⟧");
        assert!(start.elapsed() >= Duration::from_millis(100));
        let mut streamed = String::new();
        while let Ok(d) = rx.try_recv() {
            streamed.push_str(&d.text);
        }
        assert_eq!(streamed, out.text);
        assert_eq!(m.calls(), 2);

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let out = m.translate(req(ResponseFormat::JsonSegments), tx, CancellationToken::new()).await.unwrap();
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["segments"][0]["text"], "[ko] Hello ⟦P1⟧");
    }

    #[tokio::test]
    async fn hang_until_cancel_and_fail_after_chunks() {
        let m = MockProvider::new(MockConfig::default());
        m.script([
            MockStep::Hang,
            MockStep::FailAfterChunks { chunks: 1, error: ProviderError::Disconnected { maybe_sent: true } },
        ]);
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            c2.cancel();
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        assert_eq!(m.translate(req(ResponseFormat::Text), tx.clone(), cancel).await, Err(ProviderError::Cancelled));
        assert_eq!(
            m.translate(req(ResponseFormat::Text), tx, CancellationToken::new()).await,
            Err(ProviderError::Disconnected { maybe_sent: true })
        );
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }
}
