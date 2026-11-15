//! OpenAI Responses API paid for by the user's ChatGPT plan (Sign in with ChatGPT).
//!
//! * `POST {base}/responses` with the OAuth access token as the bearer credential,
//!   `store: false` and `stream: true` on every request (required by plan usage).
//! * The system message goes in `instructions`; explicit system items are rejected. Fields
//!   plan usage does not accept (temperature, token limits, metadata, …) are never sent.
//! * A request succeeded only when `response.completed` arrived. `response.failed`,
//!   `response.incomplete`, refusals and interrupted streams are errors.
//! * Usage-limit and eligibility errors stop the request; nothing is retried another way.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use super::http::{invalid_json, json_response, read_error, BodyStream, HttpCore, OutputAssembler};
use super::ollama::map_parse;
use super::sse::{SseEvent, SseParser};
use super::{
    ChatRole, ModelInfo, ProbeReport, ProbeRequest, ProviderCapabilities, ProviderConfig, ProviderDiagnostics, ProviderError, ProviderKind,
    Timeouts, TranslationDelta, TranslationOutput, TranslationProvider, TranslationRequest, Usage, MAX_RAW_STREAM_BYTES,
};
use crate::error::AppError;
use crate::security::chatgpt_auth::{self, AccessError, Endpoints};
use crate::security::keychain::SecretStore;

pub const ENDPOINT: &str = "https://api.openai.com/v1";
/// Translation needs no deliberate reasoning; the lowest effort keeps plan usage down.
const REASONING_EFFORT: &str = "low";

pub struct ChatGptProvider {
    cfg: ProviderConfig,
    http: HttpCore,
    secrets: Arc<dyn SecretStore>,
    auth: Endpoints,
    /// Set after the API rejected the `reasoning` field for the chosen model.
    no_reasoning: AtomicBool,
}

impl std::fmt::Debug for ChatGptProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatGptProvider").field("id", &self.cfg.id).finish_non_exhaustive()
    }
}

/// The exact request body. `reasoning` is dropped only after the API refused it.
pub fn build_payload(req: &TranslationRequest, reasoning: bool) -> Value {
    let instructions: Vec<&str> = req.messages.iter().filter(|m| m.role == ChatRole::System).map(|m| m.content.as_str()).collect();
    let input: Vec<Value> = req
        .messages
        .iter()
        .filter(|m| m.role != ChatRole::System)
        .map(|m| json!({"role": m.role.as_str(), "content": m.content}))
        .collect();
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(req.model_id));
    if !instructions.is_empty() {
        body.insert("instructions".into(), json!(instructions.join("\n")));
    }
    body.insert("input".into(), Value::Array(input));
    if reasoning {
        body.insert("reasoning".into(), json!({"effort": REASONING_EFFORT}));
    }
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    Value::Object(body)
}

/// Plan-usage error codes (documented under "Errors and recovery").
fn classify_code(code: &str) -> Option<ProviderError> {
    Some(match code {
        "subscription_sharing_usage_limit_exceeded" => ProviderError::UsageLimit,
        "subscription_sharing_user_not_eligible" | "chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context" | "subscription_sharing_route_not_supported" => {
            ProviderError::NotEligible
        }
        "subscription_sharing_invalid_user" => ProviderError::SignInRequired,
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => ProviderError::Http { status: 503, retry_after: None, safe_to_retry: true },
        "subscription_sharing_unsupported_capability" => ProviderError::BadRequest { status: 400 },
        _ => return None,
    })
}

/// A non-success HTTP answer. `body` is the lowercased, bounded error body.
fn classify_http(fallback: ProviderError, status: u16, body: &str) -> ProviderError {
    for code in [
        "subscription_sharing_usage_limit_exceeded",
        "subscription_sharing_user_not_eligible",
        "subscription_sharing_invalid_user",
        "subscription_sharing_usage_unavailable",
        "subscription_sharing_user_unavailable",
        "subscription_sharing_unsupported_capability",
        "subscription_sharing_route_not_supported",
        "chatpass_v2_scope_not_authorized",
        "chatpass_v2_invalid_authorization_context",
    ] {
        if body.contains(code) {
            if let Some(e) = classify_code(code) {
                return e;
            }
        }
    }
    match status {
        // the signed identity or plan permission was not accepted
        401 => ProviderError::SignInRequired,
        403 => ProviderError::NotEligible,
        _ => fallback,
    }
}

#[derive(Default)]
struct StreamState {
    completed: bool,
    usage: Option<Usage>,
    frames: usize,
}

impl StreamState {
    /// Returns true once the terminal `response.completed` event was applied.
    async fn apply(&mut self, ev: &SseEvent, out: &mut OutputAssembler, cancel: &CancellationToken) -> Result<bool, ProviderError> {
        let data = ev.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(false);
        }
        let v: Value = serde_json::from_str(data).map_err(invalid_json)?;
        self.frames += 1;
        let kind = v.get("type").and_then(Value::as_str).or(ev.event.as_deref()).unwrap_or("");
        match kind {
            "response.output_text.delta" => {
                if let Some(d) = v.get("delta").and_then(Value::as_str) {
                    out.push(d, cancel).await?;
                }
            }
            "response.refusal.delta" | "response.refusal.done" => return Err(ProviderError::Incomplete { finish_reason: "refusal".into() }),
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                return Err(ProviderError::Incomplete { finish_reason: "tool_calls".into() })
            }
            "response.completed" => {
                let u = v.pointer("/response/usage");
                self.usage = u.map(|u| Usage {
                    prompt_tokens: u.get("input_tokens").and_then(Value::as_u64).map(|n| n as u32),
                    completion_tokens: u.get("output_tokens").and_then(Value::as_u64).map(|n| n as u32),
                });
                self.completed = true;
                return Ok(true);
            }
            "response.failed" | "error" => {
                let code = v.pointer("/response/error/code").or_else(|| v.pointer("/error/code")).or_else(|| v.get("code")).and_then(Value::as_str).unwrap_or("");
                return Err(classify_code(code).unwrap_or(ProviderError::Incomplete { finish_reason: "failed".into() }));
            }
            "response.incomplete" => {
                let reason = v.pointer("/response/incomplete_details/reason").and_then(Value::as_str).unwrap_or("incomplete");
                return Err(ProviderError::Incomplete { finish_reason: reason.to_string() });
            }
            _ => {}
        }
        Ok(false)
    }
}

impl ChatGptProvider {
    pub fn with_timeouts(cfg: ProviderConfig, secrets: Arc<dyn SecretStore>, timeouts: Timeouts) -> Result<Self, AppError> {
        Self::with_auth_endpoints(cfg, secrets, timeouts, Endpoints::default())
    }

    pub fn with_auth_endpoints(cfg: ProviderConfig, secrets: Arc<dyn SecretStore>, timeouts: Timeouts, auth: Endpoints) -> Result<Self, AppError> {
        let http = HttpCore::new(&cfg.endpoint, cfg.allow_private_http, timeouts)?;
        Ok(Self { cfg, http, secrets, auth, no_reasoning: AtomicBool::new(false) })
    }

    /// The bearer header, from a Keychain read (and a refresh when needed) right before the request.
    async fn auth_header(&self) -> Result<HeaderValue, ProviderError> {
        let Some(r) = self.cfg.keychain_ref.as_deref() else { return Err(ProviderError::SignInRequired) };
        let token = chatgpt_auth::access_token(&self.auth, self.secrets.clone(), r).await.map_err(|e| match e {
            AccessError::SignInRequired => ProviderError::SignInRequired,
            AccessError::PlanNotEnabled => ProviderError::NotEligible,
            AccessError::KeychainUnavailable => ProviderError::KeychainUnavailable,
            AccessError::Transient => ProviderError::ConnectFailed,
        })?;
        let mut hv = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| ProviderError::SignInRequired)?;
        hv.set_sensitive(true);
        Ok(hv)
    }

    async fn run(&self, request: &TranslationRequest, reasoning: bool, sink: Option<Sender<TranslationDelta>>, cancel: &CancellationToken) -> Result<TranslationOutput, (ProviderError, String)> {
        let payload = build_payload(request, reasoning);
        let body = serde_json::to_vec(&payload).map_err(|_| (ProviderError::BadRequest { status: 0 }, String::new()))?;
        let sent_bytes = body.len() as u64;
        let auth = self.auth_header().await.map_err(|e| (e, String::new()))?;
        let rb = self.http.client().post(self.http.endpoint().join("responses")).header(CONTENT_TYPE, "application/json").header(AUTHORIZATION, auth).body(body);
        let dl = self.http.deadlines();
        let resp = self.http.send(rb, cancel, &dl).await.map_err(|e| (e, String::new()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let (err, text) = read_error(resp, cancel, &dl).await;
            return Err((classify_http(err, status, &text), text));
        }
        let mut out = OutputAssembler::new(sink, request.part_index);
        let mut stream = BodyStream::new(resp, dl, MAX_RAW_STREAM_BYTES);
        let mut parser = SseParser::default();
        let mut st = StreamState::default();
        let none = |e: ProviderError| (e, String::new());
        'read: loop {
            let chunk = stream.next_chunk(cancel).await.map_err(none)?;
            let events = match &chunk {
                Some(c) => parser.push(c).map_err(map_parse).map_err(none)?,
                None => parser.finish().map_err(map_parse).map_err(none)?,
            };
            for ev in &events {
                if st.apply(ev, &mut out, cancel).await.map_err(none)? {
                    break 'read;
                }
            }
            if chunk.is_none() {
                break;
            }
        }
        if !st.completed {
            return Err(none(ProviderError::Incomplete { finish_reason: if st.frames == 0 { "no_terminal_frame".into() } else { "no_completed_event".into() } }));
        }
        Ok(TranslationOutput { text: out.into_text(), finish_reason: "stop".into(), sent_bytes, usage: st.usage })
    }
}

fn parse_models(v: &Value) -> Result<Vec<ModelInfo>, ProviderError> {
    // Plan usage answers {"models":[{slug, display_name, visibility}]}; keep the standard
    // {"data":[{id}]} shape working as well.
    if let Some(models) = v.get("models").and_then(Value::as_array) {
        return Ok(models
            .iter()
            .filter(|m| m.get("visibility").and_then(Value::as_str).map(|x| x == "list").unwrap_or(true))
            .filter_map(|m| m.get("slug").and_then(Value::as_str))
            .map(|slug| ModelInfo { id: slug.to_string(), digest: None, context_length: None })
            .collect());
    }
    let data = v.get("data").and_then(Value::as_array).ok_or_else(|| ProviderError::InvalidOutput("unexpected /models response".into()))?;
    Ok(data.iter().filter_map(|m| m.get("id").and_then(Value::as_str)).map(|id| ModelInfo { id: id.to_string(), digest: None, context_length: None }).collect())
}

#[async_trait::async_trait]
impl TranslationProvider for ChatGptProvider {
    async fn probe(&self) -> Result<ProviderCapabilities, ProviderError> {
        self.list_models().await?;
        Ok(ProviderCapabilities::default_for(ProviderKind::Chatgpt))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let cancel = CancellationToken::new();
        let dl = self.http.deadlines();
        let rb = self.http.client().get(self.http.endpoint().join("models")).header(AUTHORIZATION, self.auth_header().await?);
        let resp = self.http.send(rb, &cancel, &dl).await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let (err, text) = read_error(resp, &cancel, &dl).await;
            return Err(classify_http(err, status, &text));
        }
        parse_models(&json_response(resp, &cancel, &dl).await?)
    }

    async fn translate(&self, request: TranslationRequest, sink: Sender<TranslationDelta>, cancel: CancellationToken) -> Result<TranslationOutput, ProviderError> {
        let reasoning = !self.no_reasoning.load(Ordering::Relaxed);
        match self.run(&request, reasoning, Some(sink.clone()), &cancel).await {
            Ok(out) => Ok(out),
            // The model does not take a reasoning effort: nothing was generated, so the same
            // request is sent once more without the field (and later ones start without it).
            Err((ProviderError::BadRequest { .. }, body)) if reasoning && body.contains("reasoning") => {
                self.no_reasoning.store(true, Ordering::Relaxed);
                self.run(&request, false, Some(sink), &cancel).await.map_err(|e| e.0)
            }
            Err((e, _)) => Err(e),
        }
    }
}

#[async_trait::async_trait]
impl ProviderDiagnostics for ChatGptProvider {
    /// Lists the account's models. No test prompt is sent: it would spend plan usage.
    async fn probe_model(&self, req: &ProbeRequest) -> Result<ProbeReport, ProviderError> {
        let caps = ProviderCapabilities::default_for(ProviderKind::Chatgpt);
        let mut report = ProbeReport { reachable: true, auth_ok: true, models: None, model_available: None, model: None, capabilities: caps, sent_test_prompt: false, warnings: Vec::new() };
        match self.list_models().await {
            Ok(models) => {
                if let Some(id) = req.model_id.as_deref() {
                    report.model = models.iter().find(|m| m.id == id).cloned();
                    report.model_available = Some(report.model.is_some());
                }
                report.models = Some(models);
            }
            Err(ProviderError::SignInRequired | ProviderError::NotEligible | ProviderError::Auth { .. }) => report.auth_ok = false,
            Err(e) => return Err(e),
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ChatMessage, GenerationOptions, ResponseFormat};

    fn req(messages: Vec<ChatMessage>) -> TranslationRequest {
        TranslationRequest {
            request_id: "r".into(),
            job_id: "j".into(),
            block_id: "b".into(),
            part_index: 0,
            part_count: 1,
            model_id: "gpt-test".into(),
            source_language: "en".into(),
            target_language: "ko".into(),
            messages,
            options: GenerationOptions { temperature: Some(0.2), seed: Some(1), max_output_tokens: Some(100) },
            response_format: ResponseFormat::Text,
            stream: true,
            expected_segment_ids: vec!["s0".into()],
        }
    }

    #[test]
    fn payload_follows_the_plan_usage_rules() {
        let p = build_payload(&req(vec![ChatMessage { role: ChatRole::System, content: "Translate.".into() }, ChatMessage { role: ChatRole::User, content: "Hello".into() }]), true);
        assert_eq!(p["model"], "gpt-test");
        assert_eq!(p["instructions"], "Translate.");
        assert_eq!(p["input"], json!([{"role": "user", "content": "Hello"}]), "no system item in input");
        assert_eq!(p["store"], false);
        assert_eq!(p["stream"], true);
        assert_eq!(p["reasoning"]["effort"], "low");
        for k in ["temperature", "max_output_tokens", "metadata", "top_p", "user", "previous_response_id", "prompt", "truncation", "tools", "seed"] {
            assert!(p.get(k).is_none(), "{k} must not be sent");
        }
        // a request without a system message has no instructions at all
        let p = build_payload(&req(vec![ChatMessage { role: ChatRole::User, content: "Hello".into() }]), false);
        assert!(p.get("instructions").is_none() && p.get("reasoning").is_none());
    }

    #[test]
    fn plan_usage_errors_are_classified() {
        let fb = || ProviderError::BadRequest { status: 400 };
        assert_eq!(classify_http(fb(), 429, r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}"#), ProviderError::UsageLimit);
        assert_eq!(classify_http(fb(), 403, r#"{"error":{"code":"subscription_sharing_user_not_eligible"}}"#), ProviderError::NotEligible);
        assert_eq!(classify_http(fb(), 401, r#"{"error":{"code":"subscription_sharing_invalid_user"}}"#), ProviderError::SignInRequired);
        assert_eq!(classify_http(fb(), 401, r#"{"detail":"not accepted"}"#), ProviderError::SignInRequired);
        assert_eq!(classify_http(fb(), 403, r#"{"detail":"region"}"#), ProviderError::NotEligible);
        assert!(matches!(classify_http(fb(), 503, r#"{"error":{"code":"subscription_sharing_usage_unavailable"}}"#), ProviderError::Http { safe_to_retry: true, .. }));
        assert_eq!(classify_http(fb(), 400, "{}"), fb());
        // a usage limit never retries on its own, and never stops the provider as "auth"
        assert_eq!(ProviderError::UsageLimit.retry_class(), crate::providers::RetryClass::DoNotRetry);
        assert_eq!(ProviderError::SignInRequired.retry_class(), crate::providers::RetryClass::StopProvider);
    }

    async fn feed(events: &[&str]) -> Result<(String, bool), ProviderError> {
        let cancel = CancellationToken::new();
        let mut out = OutputAssembler::new(None, 0);
        let mut st = StreamState::default();
        for e in events {
            if st.apply(&SseEvent { event: None, data: (*e).to_string() }, &mut out, &cancel).await? {
                break;
            }
        }
        Ok((out.into_text(), st.completed))
    }

    #[tokio::test]
    async fn stream_needs_the_completed_event() {
        let ok = feed(&[
            r#"{"type":"response.created"}"#,
            r#"{"type":"response.output_text.delta","delta":"안녕"}"#,
            r#"{"type":"response.output_text.delta","delta":"하세요"}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":3}}}"#,
        ])
        .await
        .unwrap();
        assert_eq!(ok, ("안녕하세요".to_string(), true));
        // ended without the terminal event
        assert_eq!(feed(&[r#"{"type":"response.output_text.delta","delta":"안"}"#]).await.unwrap().1, false);
        // a usage limit can arrive after streaming began
        assert_eq!(
            feed(&[r#"{"type":"response.output_text.delta","delta":"안"}"#, r#"{"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}}"#]).await,
            Err(ProviderError::UsageLimit)
        );
        assert_eq!(feed(&[r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#]).await, Err(ProviderError::Incomplete { finish_reason: "max_output_tokens".into() }));
        assert_eq!(feed(&[r#"{"type":"response.refusal.delta","delta":"no"}"#]).await, Err(ProviderError::Incomplete { finish_reason: "refusal".into() }));
    }

    #[test]
    fn model_list_keeps_listed_models_in_server_order() {
        let v = json!({"models": [{"slug": "b", "display_name": "B", "visibility": "list"}, {"slug": "hidden", "visibility": "hide"}, {"slug": "a", "visibility": "list"}]});
        assert_eq!(parse_models(&v).unwrap().iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["b", "a"]);
        assert_eq!(parse_models(&json!({"data": [{"id": "x"}]})).unwrap()[0].id, "x");
        assert!(parse_models(&json!({})).is_err());
    }
}
