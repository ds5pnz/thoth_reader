//! OpenAI-compatible Chat Completions adapter (spec §6.1).
//!
//! * `POST {base}/chat/completions`, optional `GET {base}/models`.
//! * `stream: true` -> SSE; only `choices[0].delta.content` is text. Success needs
//!   `finish_reason == "stop"` AND stream termination (`[DONE]` or clean EOF).
//!   `length`, `content_filter`, refusals, `tool_calls` and truncated streams are errors.
//! * Non-streaming JSON path when the stored capability says streaming is unsupported.
//!   A failed request is never re-sent another way.
//! * The Authorization header is built only here, from a Keychain read right before the
//!   request. Redirects are disabled (no credential forwarding). Tools are never sent.

use std::sync::Arc;

use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Map, Value};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use super::http::{error_from_response, invalid_json, json_response, read_error, BodyStream, HttpCore, OutputAssembler};
use super::ollama::{map_parse, messages_json, probe_messages};
use super::sse::{SseEvent, SseParser};
use super::{
    segments_json_schema, GenerationOptions, ModelInfo, ProbeReport, ProbeRequest, ProviderCapabilities, ProviderConfig,
    ProviderDiagnostics, ProviderError, ProviderKind, ResponseFormat, Timeouts, TranslationDelta, TranslationOutput,
    TranslationProvider, TranslationRequest, Usage, MAX_RAW_STREAM_BYTES,
};
use crate::error::{AppError, ErrorCode};
use crate::security::keychain::SecretStore;

pub struct OpenAiCompatibleProvider {
    cfg: ProviderConfig,
    http: HttpCore,
    secrets: Arc<dyn SecretStore>,
}

impl std::fmt::Debug for OpenAiCompatibleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleProvider").field("id", &self.cfg.id).finish_non_exhaustive()
    }
}

impl OpenAiCompatibleProvider {
    pub fn new(cfg: ProviderConfig, secrets: Arc<dyn SecretStore>) -> Result<Self, AppError> {
        Self::with_timeouts(cfg, secrets, Timeouts::REMOTE)
    }

    pub fn with_timeouts(cfg: ProviderConfig, secrets: Arc<dyn SecretStore>, timeouts: Timeouts) -> Result<Self, AppError> {
        let http = HttpCore::new(&cfg.endpoint, cfg.allow_private_http, timeouts)?;
        Ok(Self { cfg, http, secrets })
    }

    fn caps(&self) -> ProviderCapabilities {
        self.cfg.effective_capabilities()
    }

    /// Read the key from the Keychain right before the request and build the header.
    async fn auth_header(&self) -> Result<Option<HeaderValue>, ProviderError> {
        let Some(r) = self.cfg.keychain_ref.clone() else { return Ok(None) };
        let secrets = self.secrets.clone();
        let res = tokio::task::spawn_blocking(move || {
            let secret = secrets.read(&r)?;
            Ok::<_, AppError>(secret.map(|s| HeaderValue::from_str(&format!("Bearer {}", s.expose()))))
        })
        .await
        .map_err(|_| ProviderError::KeychainUnavailable)?;
        match res {
            Ok(Some(Ok(mut hv))) => {
                hv.set_sensitive(true);
                Ok(Some(hv))
            }
            Ok(Some(Err(_))) => Err(ProviderError::KeyMissing), // not a valid header value
            Ok(None) => Err(ProviderError::KeyMissing),
            Err(e) if e.code == ErrorCode::AuthFailed => Err(ProviderError::KeyAccessDenied),
            Err(_) => Err(ProviderError::KeychainUnavailable),
        }
    }

    async fn request(&self, rb: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(match self.auth_header().await? {
            Some(h) => rb.header(AUTHORIZATION, h),
            None => rb,
        })
    }

    async fn post_chat(
        &self,
        payload: &Value,
        cancel: &CancellationToken,
    ) -> Result<(reqwest::Response, super::http::Deadlines, u64), ProviderError> {
        let body = serde_json::to_vec(payload).map_err(|_| ProviderError::BadRequest { status: 0 })?;
        let sent_bytes = body.len() as u64;
        let rb = self
            .http
            .client()
            .post(self.http.endpoint().join("chat/completions"))
            .header(CONTENT_TYPE, "application/json");
        let rb = self.request(rb).await?.body(body);
        let dl = self.http.deadlines();
        let resp = self.http.send(rb, cancel, &dl).await?;
        Ok((resp, dl, sent_bytes))
    }

    /// Consume a successful chat response (SSE or JSON, by Content-Type).
    async fn consume(
        &self,
        resp: reqwest::Response,
        dl: super::http::Deadlines,
        out: &mut OutputAssembler,
        cancel: &CancellationToken,
    ) -> Result<(String, Option<Usage>), ProviderError> {
        let is_sse = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"));
        let mut body = BodyStream::new(resp, dl, MAX_RAW_STREAM_BYTES);
        if !is_sse {
            let mut bytes = Vec::new();
            while let Some(c) = body.next_chunk(cancel).await? {
                bytes.extend_from_slice(&c);
                if bytes.len() > super::MAX_JSON_BODY_BYTES {
                    return Err(ProviderError::OutputTooLarge);
                }
            }
            let v: Value = serde_json::from_slice(&bytes).map_err(invalid_json)?;
            return apply_completion(&v, out, cancel).await;
        }
        let mut parser = SseParser::default();
        let mut st = StreamState::default();
        loop {
            match body.next_chunk(cancel).await? {
                Some(chunk) => {
                    for ev in parser.push(&chunk).map_err(map_parse)? {
                        if st.apply(&ev, out, cancel).await? {
                            return st.finish(true);
                        }
                    }
                }
                None => {
                    for ev in parser.finish().map_err(map_parse)? {
                        if st.apply(&ev, out, cancel).await? {
                            return st.finish(true);
                        }
                    }
                    // Clean EOF counts as termination only together with finish_reason=stop.
                    return st.finish(true);
                }
            }
        }
    }
}

#[derive(Default)]
struct StreamState {
    finish_reason: Option<String>,
    usage: Option<Usage>,
    frames: usize,
}

impl StreamState {
    /// Returns true when `[DONE]` was received.
    async fn apply(&mut self, ev: &SseEvent, out: &mut OutputAssembler, cancel: &CancellationToken) -> Result<bool, ProviderError> {
        let data = ev.data.trim();
        if data == "[DONE]" {
            return Ok(true);
        }
        if data.is_empty() {
            return Ok(false);
        }
        let v: Value = serde_json::from_str(data).map_err(invalid_json)?;
        if !v.is_object() {
            return Err(ProviderError::InvalidOutput("unexpected SSE frame".into()));
        }
        self.frames += 1;
        if v.get("error").is_some_and(|e| !e.is_null()) {
            return Err(ProviderError::Incomplete { finish_reason: "error".into() });
        }
        if let Some(u) = parse_usage(&v) {
            self.usage = Some(u);
        }
        let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|a| a.first()) else {
            return Ok(false);
        };
        if let Some(delta) = choice.get("delta") {
            check_no_tools_or_refusal(delta)?;
            match delta.get("content") {
                None | Some(Value::Null) => {}
                Some(Value::String(s)) => out.push(s, cancel).await?,
                Some(_) => return Err(ProviderError::InvalidOutput("non-string delta content".into())),
            }
        }
        match choice.get("finish_reason") {
            None | Some(Value::Null) => {}
            Some(Value::String(fr)) if fr == "stop" => self.finish_reason = Some("stop".into()),
            Some(Value::String(fr)) => return Err(ProviderError::Incomplete { finish_reason: fr.clone() }),
            Some(_) => return Err(ProviderError::InvalidOutput("invalid finish_reason".into())),
        }
        Ok(false)
    }

    fn finish(self, terminated: bool) -> Result<(String, Option<Usage>), ProviderError> {
        match (self.finish_reason, terminated) {
            (Some(fr), true) => Ok((fr, self.usage)),
            (Some(_), false) => Err(ProviderError::Incomplete { finish_reason: "no_terminal_frame".into() }),
            (None, _) if self.frames == 0 => Err(ProviderError::Incomplete { finish_reason: "no_terminal_frame".into() }),
            (None, _) => Err(ProviderError::Incomplete { finish_reason: "no_finish_reason".into() }),
        }
    }
}

fn non_empty(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(_) => true,
    }
}

fn check_no_tools_or_refusal(msg: &Value) -> Result<(), ProviderError> {
    if non_empty(msg.get("tool_calls")) || non_empty(msg.get("function_call")) {
        return Err(ProviderError::Incomplete { finish_reason: "tool_calls".into() });
    }
    if non_empty(msg.get("refusal")) {
        return Err(ProviderError::Incomplete { finish_reason: "refusal".into() });
    }
    Ok(())
}

fn parse_usage(v: &Value) -> Option<Usage> {
    let u = v.get("usage")?;
    if !u.is_object() {
        return None;
    }
    Some(Usage {
        prompt_tokens: u.get("prompt_tokens").and_then(Value::as_u64).map(|n| n as u32),
        completion_tokens: u.get("completion_tokens").and_then(Value::as_u64).map(|n| n as u32),
    })
}

/// Non-streaming completion object.
async fn apply_completion(
    v: &Value,
    out: &mut OutputAssembler,
    cancel: &CancellationToken,
) -> Result<(String, Option<Usage>), ProviderError> {
    if v.get("error").is_some_and(|e| !e.is_null()) {
        return Err(ProviderError::Incomplete { finish_reason: "error".into() });
    }
    let choice = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or_else(|| ProviderError::InvalidOutput("missing choices".into()))?;
    let msg = choice.get("message").ok_or_else(|| ProviderError::InvalidOutput("missing message".into()))?;
    check_no_tools_or_refusal(msg)?;
    match msg.get("content") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => out.push(s, cancel).await?,
        Some(_) => return Err(ProviderError::InvalidOutput("non-string content".into())),
    }
    match choice.get("finish_reason").and_then(Value::as_str) {
        Some("stop") => Ok(("stop".into(), parse_usage(v))),
        Some(other) => Err(ProviderError::Incomplete { finish_reason: other.to_string() }),
        None => Err(ProviderError::Incomplete { finish_reason: "no_finish_reason".into() }),
    }
}

/// Exact `/chat/completions` body for a request (no secrets / ids / time). Never tools.
pub fn build_payload(caps: &ProviderCapabilities, req: &TranslationRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model_id));
    body.insert("messages".into(), messages_json(&req.messages));
    body.insert("stream".into(), json!(req.stream));
    if caps.temperature {
        if let Some(t) = req.options.temperature {
            body.insert("temperature".into(), json!(t));
        }
    }
    if caps.seed {
        if let Some(s) = req.options.seed {
            body.insert("seed".into(), json!(s));
        }
    }
    if let (Some(field), Some(n)) = (&caps.token_limit_field, req.options.max_output_tokens) {
        body.insert(field.clone(), json!(n));
    }
    if req.response_format == ResponseFormat::JsonSegments && caps.structured_output {
        body.insert(
            "response_format".into(),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "translation_segments",
                    "strict": true,
                    "schema": segments_json_schema(&req.expected_segment_ids)
                }
            }),
        );
    }
    Value::Object(body)
}

fn parse_models(v: &Value) -> Result<Vec<ModelInfo>, ProviderError> {
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::InvalidOutput("unexpected /models response".into()))?;
    Ok(data
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?;
            let ctx = ["context_length", "max_model_len", "context_window"]
                .iter()
                .find_map(|k| m.get(*k).and_then(Value::as_u64))
                .map(|n| n.min(u32::MAX as u64) as u32);
            Some(ModelInfo { id: id.to_string(), digest: None, context_length: ctx })
        })
        .collect())
}

fn probe_request(model: &str, stream: bool, json_mode: bool, caps: &ProviderCapabilities) -> Value {
    let req = TranslationRequest {
        request_id: String::new(),
        job_id: String::new(),
        block_id: String::new(),
        part_index: 0,
        part_count: 1,
        model_id: model.to_string(),
        source_language: "en".into(),
        target_language: "ko".into(),
        messages: probe_messages(json_mode),
        options: GenerationOptions { temperature: Some(0.0), seed: None, max_output_tokens: Some(64) },
        response_format: if json_mode { ResponseFormat::JsonSegments } else { ResponseFormat::Text },
        stream,
        expected_segment_ids: vec!["s0".into()],
    };
    build_payload(caps, &req)
}

#[async_trait::async_trait]
impl TranslationProvider for OpenAiCompatibleProvider {
    async fn probe(&self) -> Result<ProviderCapabilities, ProviderError> {
        let mut caps = self.caps();
        match self.list_models().await {
            Ok(_) => caps.model_listing = true,
            Err(ProviderError::BadRequest { .. } | ProviderError::ModelNotFound | ProviderError::InvalidOutput(_)) => {
                caps.model_listing = false
            }
            Err(e) => return Err(e),
        }
        Ok(caps)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let cancel = CancellationToken::new();
        let dl = self.http.deadlines();
        let rb = self.request(self.http.client().get(self.http.endpoint().join("models"))).await?;
        let resp = self.http.send(rb, &cancel, &dl).await?;
        let v = json_response(resp, &cancel, &dl).await?;
        parse_models(&v)
    }

    async fn translate(
        &self,
        request: TranslationRequest,
        sink: Sender<TranslationDelta>,
        cancel: CancellationToken,
    ) -> Result<TranslationOutput, ProviderError> {
        let payload = build_payload(&self.caps(), &request);
        let (resp, dl, sent_bytes) = self.post_chat(&payload, &cancel).await?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp, &cancel, &dl).await);
        }
        let mut out = OutputAssembler::new(Some(sink), request.part_index);
        let (finish_reason, usage) = self.consume(resp, dl, &mut out, &cancel).await?;
        Ok(TranslationOutput { text: out.into_text(), finish_reason, sent_bytes, usage })
    }
}

#[async_trait::async_trait]
impl ProviderDiagnostics for OpenAiCompatibleProvider {
    async fn probe_model(&self, req: &ProbeRequest) -> Result<ProbeReport, ProviderError> {
        let cancel = CancellationToken::new();
        let mut caps = ProviderCapabilities::default_for(ProviderKind::OpenaiCompatible);
        if let Some(stored) = &self.cfg.capabilities {
            // Keep user-entered limits; everything else is re-detected.
            caps.context_limit = stored.context_limit;
        }
        let mut report = ProbeReport {
            reachable: true,
            auth_ok: true,
            models: None,
            model_available: None,
            model: None,
            capabilities: caps.clone(),
            sent_test_prompt: false,
            warnings: Vec::new(),
        };
        match self.list_models().await {
            Ok(models) => {
                caps.model_listing = true;
                if let Some(id) = req.model_id.as_deref() {
                    let found = models.iter().find(|m| m.id == id).cloned();
                    report.model_available = Some(found.is_some());
                    if let Some(m) = &found {
                        if m.context_length.is_some() {
                            caps.context_limit = m.context_length;
                        }
                    }
                    report.model = found;
                }
                report.models = Some(models);
            }
            Err(ProviderError::Auth { .. }) => {
                report.auth_ok = false;
                report.capabilities = caps;
                return Ok(report);
            }
            Err(ProviderError::BadRequest { .. } | ProviderError::ModelNotFound | ProviderError::InvalidOutput(_)) => {
                caps.model_listing = false;
                report.warnings.push("model_listing_unsupported".into());
            }
            Err(e) => return Err(e),
        }

        let model = match req.model_id.as_deref() {
            Some(m) if req.send_test_prompt && report.model_available != Some(false) => m.to_string(),
            _ => {
                if caps.context_limit.is_none() {
                    report.warnings.push("context_limit_unknown".into());
                }
                report.capabilities = caps;
                return Ok(report);
            }
        };

        // 1) Streaming check with a fixed phrase; adapt known option incompatibilities.
        report.sent_test_prompt = true;
        let mut stream = true;
        let mut ok = false;
        for _ in 0..4 {
            let payload = probe_request(&model, stream, false, &caps);
            let (resp, dl, _) = self.post_chat(&payload, &cancel).await?;
            if resp.status().is_success() {
                let is_sse = resp
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"));
                let mut out = OutputAssembler::new(None, 0);
                match self.consume(resp, dl, &mut out, &cancel).await {
                    Ok(_) | Err(ProviderError::Incomplete { .. }) => {
                        caps.streaming = stream && is_sse;
                        ok = true;
                    }
                    Err(_) => {
                        caps.streaming = false;
                        report.warnings.push("streaming_check_failed".into());
                        ok = true;
                    }
                }
                break;
            }
            let (err, body) = read_error(resp, &cancel, &dl).await;
            match err {
                ProviderError::Auth { .. } => {
                    report.auth_ok = false;
                    break;
                }
                ProviderError::ModelNotFound => {
                    report.model_available = Some(false);
                    break;
                }
                ProviderError::BadRequest { .. } => {
                    if body.contains("max_completion_tokens") && caps.token_limit_field.as_deref() == Some("max_tokens") {
                        caps.token_limit_field = Some("max_completion_tokens".into());
                    } else if body.contains("temperature") && caps.temperature {
                        caps.temperature = false;
                    } else if body.contains("stream") && stream {
                        stream = false;
                    } else {
                        report.warnings.push("test_prompt_rejected".into());
                        break;
                    }
                }
                e => return Err(e),
            }
        }
        if ok {
            report.model_available = Some(true);
            // 2) Structured output check (non-streaming, fixed phrase).
            let mut sc = caps.clone();
            sc.structured_output = true;
            let payload = probe_request(&model, false, true, &sc);
            let (resp, dl, _) = self.post_chat(&payload, &cancel).await?;
            if resp.status().is_success() {
                let mut out = OutputAssembler::new(None, 0);
                if matches!(self.consume(resp, dl, &mut out, &cancel).await, Ok(_) | Err(ProviderError::Incomplete { .. })) {
                    let v: Option<Value> = serde_json::from_str(out.text().trim()).ok();
                    caps.structured_output = v.as_ref().and_then(|v| v.get("segments")).and_then(Value::as_array).is_some();
                }
            } else {
                let _ = read_error(resp, &cancel, &dl).await;
            }
            if !caps.structured_output {
                report.warnings.push("structured_output_unsupported".into());
            }
        }
        if caps.context_limit.is_none() {
            report.warnings.push("context_limit_unknown".into());
        }
        report.capabilities = caps;
        Ok(report)
    }
}
