//! Ollama native API adapter (spec §6.1): `POST /api/chat` (NDJSON stream), `GET /api/tags`,
//! `POST /api/show`.
//!
//! * Success requires `done: true` with `done_reason: "stop"`.
//! * `message.thinking` is never merged into the translation.
//! * `think: false` is sent only when the model was verified to support thinking
//!   (`capabilities.thinking_toggle`, detected from `/api/show`).

use serde_json::{json, Map, Value};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use super::http::{error_from_response, invalid_json, json_response, BodyStream, HttpCore, OutputAssembler};
use super::ndjson::NdjsonParser;
use super::sse::StreamParseError;
use super::{
    segments_json_schema, ChatMessage, ChatRole, ModelInfo, ProbeReport, ProbeRequest, ProviderCapabilities,
    ProviderConfig, ProviderDiagnostics, ProviderError, ProviderKind, ResponseFormat, Timeouts, TranslationDelta,
    TranslationOutput, TranslationProvider, TranslationRequest, Usage, MAX_RAW_STREAM_BYTES, PROBE_TEST_PHRASE,
};
use crate::error::AppError;

pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:11434";

#[derive(Debug, Clone)]
pub struct OllamaProvider {
    cfg: ProviderConfig,
    http: HttpCore,
}

impl OllamaProvider {
    pub fn new(cfg: ProviderConfig) -> Result<Self, AppError> {
        Self::with_timeouts(cfg, Timeouts::OLLAMA)
    }

    pub fn with_timeouts(cfg: ProviderConfig, timeouts: Timeouts) -> Result<Self, AppError> {
        let http = HttpCore::new(&cfg.endpoint, cfg.allow_private_http, timeouts)?;
        Ok(Self { cfg, http })
    }

    fn caps(&self) -> ProviderCapabilities {
        self.cfg.effective_capabilities()
    }

    async fn show(&self, model: &str, cancel: &CancellationToken) -> Result<Value, ProviderError> {
        let dl = self.http.deadlines();
        let rb = self.http.client().post(self.http.endpoint().join("api/show")).json(&json!({ "model": model }));
        let resp = self.http.send(rb, cancel, &dl).await?;
        json_response(resp, cancel, &dl).await
    }

    async fn run_chat(
        &self,
        payload: &Value,
        stream: bool,
        out: &mut OutputAssembler,
        cancel: &CancellationToken,
    ) -> Result<(String, Option<Usage>, u64), ProviderError> {
        let body = serde_json::to_vec(payload).map_err(|_| ProviderError::BadRequest { status: 0 })?;
        let sent_bytes = body.len() as u64;
        let dl = self.http.deadlines();
        let rb = self
            .http
            .client()
            .post(self.http.endpoint().join("api/chat"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        let resp = self.http.send(rb, cancel, &dl).await?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp, cancel, &dl).await);
        }
        let mut body = BodyStream::new(resp, dl, MAX_RAW_STREAM_BYTES);
        if !stream {
            let mut bytes = Vec::new();
            while let Some(c) = body.next_chunk(cancel).await? {
                bytes.extend_from_slice(&c);
                if bytes.len() > super::MAX_JSON_BODY_BYTES {
                    return Err(ProviderError::OutputTooLarge);
                }
            }
            let v: Value = serde_json::from_slice(&bytes).map_err(invalid_json)?;
            let mut state = ChatState::default();
            state.apply(&v, out, cancel).await?;
            return state.finish().map(|(r, u)| (r, u, sent_bytes));
        }
        let mut parser = NdjsonParser::default();
        let mut state = ChatState::default();
        loop {
            let Some(chunk) = body.next_chunk(cancel).await? else { break };
            for rec in parser.push(&chunk).map_err(map_parse)? {
                let v: Value = serde_json::from_str(&rec).map_err(invalid_json)?;
                state.apply(&v, out, cancel).await?;
                if state.done {
                    return state.finish().map(|(r, u)| (r, u, sent_bytes));
                }
            }
        }
        if let Some(rec) = parser.finish().map_err(map_parse)? {
            let v: Value = serde_json::from_str(&rec).map_err(invalid_json)?;
            state.apply(&v, out, cancel).await?;
        }
        state.finish().map(|(r, u)| (r, u, sent_bytes))
    }
}

pub(crate) fn map_parse(e: StreamParseError) -> ProviderError {
    match e {
        StreamParseError::RecordTooLarge => ProviderError::RecordTooLarge,
        StreamParseError::InvalidUtf8 => ProviderError::InvalidOutput("invalid UTF-8 in stream".into()),
    }
}

#[derive(Default)]
struct ChatState {
    done: bool,
    done_reason: Option<String>,
    usage: Option<Usage>,
}

impl ChatState {
    async fn apply(&mut self, v: &Value, out: &mut OutputAssembler, cancel: &CancellationToken) -> Result<(), ProviderError> {
        if !v.is_object() {
            return Err(ProviderError::InvalidOutput("unexpected record".into()));
        }
        if v.get("error").is_some() {
            return Err(ProviderError::Incomplete { finish_reason: "error".into() });
        }
        if let Some(msg) = v.get("message") {
            if msg.get("tool_calls").is_some_and(|t| !t.is_null() && t.as_array().is_none_or(|a| !a.is_empty())) {
                return Err(ProviderError::Incomplete { finish_reason: "tool_calls".into() });
            }
            // `thinking` is deliberately ignored.
            match msg.get("content") {
                None | Some(Value::Null) => {}
                Some(Value::String(s)) => out.push(s, cancel).await?,
                Some(_) => return Err(ProviderError::InvalidOutput("non-string content".into())),
            }
        }
        if v.get("done").and_then(Value::as_bool) == Some(true) {
            self.done = true;
            self.done_reason = v.get("done_reason").and_then(Value::as_str).map(str::to_string);
            let p = v.get("prompt_eval_count").and_then(Value::as_u64).map(|n| n as u32);
            let c = v.get("eval_count").and_then(Value::as_u64).map(|n| n as u32);
            if p.is_some() || c.is_some() {
                self.usage = Some(Usage { prompt_tokens: p, completion_tokens: c });
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<(String, Option<Usage>), ProviderError> {
        if !self.done {
            return Err(ProviderError::Incomplete { finish_reason: "no_terminal_frame".into() });
        }
        match self.done_reason.as_deref() {
            Some("stop") => Ok(("stop".into(), self.usage)),
            Some(other) => Err(ProviderError::Incomplete { finish_reason: other.to_string() }),
            None => Err(ProviderError::Incomplete { finish_reason: "missing_done_reason".into() }),
        }
    }
}

/// Exact `/api/chat` body for a request (no secrets / ids / time).
pub fn build_payload(caps: &ProviderCapabilities, req: &TranslationRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model_id));
    body.insert("messages".into(), messages_json(&req.messages));
    body.insert("stream".into(), json!(req.stream));
    let mut options = Map::new();
    if caps.temperature {
        if let Some(t) = req.options.temperature {
            options.insert("temperature".into(), json!(t));
        }
    }
    if caps.seed {
        if let Some(s) = req.options.seed {
            options.insert("seed".into(), json!(s));
        }
    }
    if let (Some(field), Some(n)) = (&caps.token_limit_field, req.options.max_output_tokens) {
        options.insert(field.clone(), json!(n));
    }
    if !options.is_empty() {
        body.insert("options".into(), Value::Object(options));
    }
    if caps.thinking_toggle {
        body.insert("think".into(), json!(false));
    }
    if req.response_format == ResponseFormat::JsonSegments && caps.structured_output {
        body.insert("format".into(), segments_json_schema(&req.expected_segment_ids));
    }
    Value::Object(body)
}

pub(crate) fn messages_json(msgs: &[ChatMessage]) -> Value {
    Value::Array(msgs.iter().map(|m| json!({"role": m.role.as_str(), "content": m.content})).collect())
}

fn parse_tags(v: &Value) -> Result<Vec<ModelInfo>, ProviderError> {
    let models = v
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::InvalidOutput("unexpected /api/tags response".into()))?;
    Ok(models
        .iter()
        .filter_map(|m| {
            let id = m.get("name").or_else(|| m.get("model")).and_then(Value::as_str)?;
            Some(ModelInfo {
                id: id.to_string(),
                digest: m.get("digest").and_then(Value::as_str).map(str::to_string),
                context_length: None,
            })
        })
        .collect())
}

/// Ollama treats `name` and `name:latest` as the same model.
fn same_model(a: &str, b: &str) -> bool {
    let norm = |s: &str| if s.contains(':') { s.to_string() } else { format!("{s}:latest") };
    norm(a) == norm(b)
}

fn context_length_from_show(v: &Value) -> Option<u32> {
    let info = v.get("model_info")?.as_object()?;
    info.iter()
        .find(|(k, _)| k.ends_with(".context_length"))
        .and_then(|(_, v)| v.as_u64())
        .map(|n| n.min(u32::MAX as u64) as u32)
}

pub(crate) fn probe_messages(json_mode: bool) -> Vec<ChatMessage> {
    let sys = if json_mode {
        "This is a connection check. Translate the user text to Korean. Respond only with JSON of the form {\"segments\":[{\"id\":\"s0\",\"text\":\"...\"}]}."
    } else {
        "This is a connection check. Translate the user text to Korean. Respond with the translation only."
    };
    vec![
        ChatMessage { role: ChatRole::System, content: sys.into() },
        ChatMessage { role: ChatRole::User, content: PROBE_TEST_PHRASE.into() },
    ]
}

#[async_trait::async_trait]
impl TranslationProvider for OllamaProvider {
    async fn probe(&self) -> Result<ProviderCapabilities, ProviderError> {
        self.list_models().await?;
        let mut caps = self.caps();
        caps.model_listing = true;
        caps.streaming = true;
        Ok(caps)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let cancel = CancellationToken::new();
        let dl = self.http.deadlines();
        let resp = self.http.send(self.http.client().get(self.http.endpoint().join("api/tags")), &cancel, &dl).await?;
        let v = json_response(resp, &cancel, &dl).await?;
        parse_tags(&v)
    }

    async fn translate(
        &self,
        request: TranslationRequest,
        sink: Sender<TranslationDelta>,
        cancel: CancellationToken,
    ) -> Result<TranslationOutput, ProviderError> {
        let payload = build_payload(&self.caps(), &request);
        let mut out = OutputAssembler::new(Some(sink), request.part_index);
        let (finish_reason, usage, sent_bytes) = self.run_chat(&payload, request.stream, &mut out, &cancel).await?;
        Ok(TranslationOutput { text: out.into_text(), finish_reason, sent_bytes, usage })
    }
}

#[async_trait::async_trait]
impl ProviderDiagnostics for OllamaProvider {
    async fn probe_model(&self, req: &ProbeRequest) -> Result<ProbeReport, ProviderError> {
        let cancel = CancellationToken::new();
        let models = self.list_models().await?;
        let mut caps = ProviderCapabilities::default_for(ProviderKind::Ollama);
        let mut report = ProbeReport {
            reachable: true,
            auth_ok: true,
            models: Some(models.clone()),
            model_available: None,
            model: None,
            capabilities: caps.clone(),
            sent_test_prompt: false,
            warnings: Vec::new(),
        };
        let Some(model_id) = req.model_id.as_deref() else {
            report.warnings.push("context_limit_unknown".into());
            return Ok(report);
        };
        let found = models.iter().find(|m| same_model(&m.id, model_id)).cloned();
        report.model_available = Some(found.is_some());
        let Some(mut info) = found else {
            return Ok(report);
        };
        match self.show(model_id, &cancel).await {
            Ok(show) => {
                info.context_length = context_length_from_show(&show);
                caps.context_limit = info.context_length;
                let model_caps: Vec<&str> = show
                    .get("capabilities")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                caps.thinking_toggle = model_caps.contains(&"thinking");
            }
            Err(ProviderError::Cancelled) => return Err(ProviderError::Cancelled),
            Err(_) => report.warnings.push("model_details_unavailable".into()),
        }
        if caps.context_limit.is_none() {
            report.warnings.push("context_limit_unknown".into());
        }
        if req.send_test_prompt {
            // Structured-output check with a fixed phrase (never document text).
            let mut probe_caps = caps.clone();
            probe_caps.structured_output = true;
            let probe_req = TranslationRequest {
                request_id: String::new(),
                job_id: String::new(),
                block_id: String::new(),
                part_index: 0,
                part_count: 1,
                model_id: model_id.to_string(),
                source_language: "en".into(),
                target_language: "ko".into(),
                messages: probe_messages(true),
                options: super::GenerationOptions { temperature: Some(0.0), seed: None, max_output_tokens: Some(64) },
                response_format: ResponseFormat::JsonSegments,
                stream: false,
                expected_segment_ids: vec!["s0".into()],
            };
            let payload = build_payload(&probe_caps, &probe_req);
            let mut out = OutputAssembler::new(None, 0);
            report.sent_test_prompt = true;
            match self.run_chat(&payload, false, &mut out, &cancel).await {
                Ok(_) | Err(ProviderError::Incomplete { .. }) => {
                    let parsed: Option<Value> = serde_json::from_str(out.text().trim()).ok();
                    caps.structured_output =
                        parsed.as_ref().and_then(|v| v.get("segments")).and_then(Value::as_array).is_some();
                }
                Err(ProviderError::Auth { .. }) => {
                    report.auth_ok = false;
                }
                Err(e @ (ProviderError::ConnectFailed | ProviderError::Cancelled)) => return Err(e),
                Err(_) => {}
            }
            if !caps.structured_output {
                report.warnings.push("structured_output_unsupported".into());
            }
        }
        report.model = Some(info);
        report.capabilities = caps;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_name_matching() {
        assert!(same_model("llama3", "llama3:latest"));
        assert!(same_model("qwen2.5:7b", "qwen2.5:7b"));
        assert!(!same_model("qwen2.5:7b", "qwen2.5:14b"));
    }

    #[test]
    fn show_context_length() {
        let v = json!({"model_info": {"general.architecture": "llama", "llama.context_length": 131072}});
        assert_eq!(context_length_from_show(&v), Some(131072));
        assert_eq!(context_length_from_show(&json!({})), None);
    }
}
