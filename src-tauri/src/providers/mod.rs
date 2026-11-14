//! Translation providers (spec §6.1, §7.2).
//!
//! * [`TranslationProvider`] is the contract every adapter implements (exactly spec §6.1).
//! * [`TranslationRequest`] is an immutable, secret-free snapshot; API keys are read from
//!   the Keychain by the adapter right before the HTTP request.
//! * [`ProviderError`] classifies failures for the retry policy ([`RetryClass`],
//!   [`retry_decision`]).
//! * [`request_payload`] produces the exact JSON body an adapter sends for a request, so
//!   the cache layer can hash exactly what goes over the wire (`requestManifestHash`).

#[cfg(test)]
mod adapter_tests;
pub mod http;
pub mod mock;
pub mod ndjson;
pub mod ollama;
pub mod openai_compatible;
pub mod retry_after;
pub mod sse;

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, ErrorCode};
use crate::security::keychain::SecretStore;

/// Maximum assembled output text per request (spec §6.3).
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Maximum size of a single SSE event / NDJSON record (spec §6.3).
pub const MAX_RECORD_BYTES: usize = 256 * 1024;
/// Hard cap on raw response bytes (frame overhead included) for one request.
pub const MAX_RAW_STREAM_BYTES: usize = 8 * 1024 * 1024;
/// Hard cap on a non-streaming JSON response body.
pub const MAX_JSON_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Error bodies are read (never logged) only up to this size to classify the failure.
pub const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
/// Maximum attempts per part (spec §7.2).
pub const MAX_ATTEMPTS: u32 = 3;
/// Longest Retry-After honoured automatically; longer waits require manual resume.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------------------
// Configuration & capabilities
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Ollama,
    OpenaiCompatible,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Ollama => "ollama",
            ProviderKind::OpenaiCompatible => "openai_compatible",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ollama" => Some(Self::Ollama),
            "openai_compatible" => Some(Self::OpenaiCompatible),
            _ => None,
        }
    }
    /// Default endpoint suggested in the UI.
    pub fn default_endpoint(&self) -> Option<&'static str> {
        match self {
            ProviderKind::Ollama => Some(ollama::DEFAULT_ENDPOINT),
            ProviderKind::OpenaiCompatible => None,
        }
    }
    /// Default per-provider concurrency (spec §7.1).
    pub fn default_concurrency(&self) -> u8 {
        match self {
            ProviderKind::Ollama => 1,
            ProviderKind::OpenaiCompatible => 2,
        }
    }
    pub fn default_timeouts(&self) -> Timeouts {
        match self {
            ProviderKind::Ollama => Timeouts::OLLAMA,
            ProviderKind::OpenaiCompatible => Timeouts::REMOTE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub model_listing: bool,
    pub context_limit: Option<u32>,
    /// "max_tokens" / "max_completion_tokens" / "num_predict"; `None` = never send a limit.
    pub token_limit_field: Option<String>,
    pub temperature: bool,
    pub seed: bool,
    pub structured_output: bool,
    /// Only true when the model was verified to accept a thinking toggle (`think:false`).
    #[serde(default)]
    pub thinking_toggle: bool,
}

impl ProviderCapabilities {
    /// Conservative defaults used before a probe stored real capabilities.
    pub fn default_for(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::Ollama => Self {
                streaming: true,
                model_listing: true,
                context_limit: None,
                token_limit_field: Some("num_predict".into()),
                temperature: true,
                seed: true,
                structured_output: false,
                thinking_toggle: false,
            },
            ProviderKind::OpenaiCompatible => Self {
                streaming: true,
                model_listing: false,
                context_limit: None,
                token_limit_field: Some("max_tokens".into()),
                temperature: true,
                seed: false,
                structured_output: false,
                thinking_toggle: false,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub id: String,
    pub kind: ProviderKind,
    /// Normalized endpoint (see `security::endpoint::normalize_endpoint`).
    pub endpoint: String,
    pub account_namespace: String,
    /// Keychain account (UUID). `None` = no API key (e.g. local servers).
    pub keychain_ref: Option<String>,
    pub allow_private_http: bool,
    /// 1..=4 (spec §7.1).
    pub concurrency: u8,
    pub capabilities: Option<ProviderCapabilities>,
}

impl ProviderConfig {
    pub fn effective_capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone().unwrap_or_else(|| ProviderCapabilities::default_for(self.kind))
    }
    pub fn validate(&self) -> Result<(), AppError> {
        if !(1..=4).contains(&self.concurrency) {
            return Err(AppError::invalid("동시 요청 수는 1~4 사이여야 합니다."));
        }
        if self.account_namespace.trim().is_empty() || self.account_namespace.len() > 128 {
            return Err(AppError::invalid("계정 구분 이름이 올바르지 않습니다."));
        }
        if let Some(r) = &self.keychain_ref {
            crate::security::keychain::validate_ref(r)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub digest: Option<String>,
    pub context_length: Option<u32>,
}

// ---------------------------------------------------------------------------------------
// Request / response snapshots
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

impl ChatRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChatRole::System => "system",
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationOptions {
    pub temperature: Option<f64>,
    pub seed: Option<i64>,
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonSegments,
}

impl ResponseFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResponseFormat::Text => "text",
            ResponseFormat::JsonSegments => "json_segments",
        }
    }
}

/// Immutable, secret-free request snapshot (spec §6.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslationRequest {
    pub request_id: String,
    pub job_id: String,
    pub block_id: String,
    pub part_index: u32,
    pub part_count: u32,
    pub model_id: String,
    pub source_language: String,
    pub target_language: String,
    pub messages: Vec<ChatMessage>,
    pub options: GenerationOptions,
    pub response_format: ResponseFormat,
    pub stream: bool,
    pub expected_segment_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslationDelta {
    pub part_index: u32,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslationOutput {
    pub text: String,
    pub finish_reason: String,
    /// Bytes of the request body that was sent.
    pub sent_bytes: u64,
    pub usage: Option<Usage>,
}

// ---------------------------------------------------------------------------------------
// Errors & retry classification
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutPhase {
    Connect,
    FirstByte,
    Idle,
    Total,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The request was definitely not sent (refused, DNS failure, unreachable).
    #[error("connection failed before the request was sent")]
    ConnectFailed,
    /// The endpoint (or the address it resolved to) violates the endpoint policy.
    #[error("endpoint rejected by policy: {0}")]
    EndpointRejected(String),
    #[error("TLS failure")]
    Tls,
    #[error("timeout ({phase:?}, maybe_sent={maybe_sent})")]
    Timeout { phase: TimeoutPhase, maybe_sent: bool },
    #[error("connection lost after the request may have been sent")]
    Disconnected { maybe_sent: bool },
    #[error("HTTP {status}")]
    Http { status: u16, retry_after: Option<Duration>, safe_to_retry: bool },
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    /// 401 / 403.
    #[error("authentication failed (HTTP {status})")]
    Auth { status: u16 },
    /// A Keychain reference is configured but no key is stored.
    #[error("API key missing")]
    KeyMissing,
    /// Keychain access was denied by the user.
    #[error("keychain access denied")]
    KeyAccessDenied,
    /// Keychain locked / not available.
    #[error("keychain unavailable")]
    KeychainUnavailable,
    #[error("model not found")]
    ModelNotFound,
    #[error("request rejected (HTTP {status})")]
    BadRequest { status: u16 },
    #[error("context too large")]
    ContextTooLarge,
    #[error("invalid output: {0}")]
    InvalidOutput(String),
    /// Stream ended without a normal completion (length / refusal / tool_calls / no terminal frame).
    #[error("incomplete output: {finish_reason}")]
    Incomplete { finish_reason: String },
    #[error("cancelled")]
    Cancelled,
    #[error("stream record too large")]
    RecordTooLarge,
    #[error("output too large")]
    OutputTooLarge,
}

/// How the scheduler should treat a failure (spec §7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryClass {
    /// Not sent / explicitly rejected without processing: retry with backoff (≤3 attempts).
    RetrySafe,
    /// Deterministic failure: FAILED, no automatic retry.
    DoNotRetry,
    /// Stop the whole provider (auth / keychain / endpoint policy) and ask the user.
    StopProvider,
    /// The source may have been sent and processed (possible double billing): FAILED,
    /// user-initiated retry only, with a "may be charged again" notice.
    SentUnknown,
}

impl ProviderError {
    /// Stable machine reason for diagnostics / job metadata (never contains content).
    pub fn reason(&self) -> &'static str {
        match self {
            Self::ConnectFailed => "connect_failed",
            Self::EndpointRejected(_) => "endpoint_rejected",
            Self::Tls => "tls",
            Self::Timeout { phase: TimeoutPhase::Connect, .. } => "timeout_connect",
            Self::Timeout { phase: TimeoutPhase::FirstByte, .. } => "timeout_first_byte",
            Self::Timeout { phase: TimeoutPhase::Idle, .. } => "timeout_idle",
            Self::Timeout { phase: TimeoutPhase::Total, .. } => "timeout_total",
            Self::Disconnected { .. } => "disconnected",
            Self::Http { .. } => "http_status",
            Self::RateLimited { .. } => "rate_limited",
            Self::Auth { .. } => "auth",
            Self::KeyMissing => "key_missing",
            Self::KeyAccessDenied => "keychain_denied",
            Self::KeychainUnavailable => "keychain_unavailable",
            Self::ModelNotFound => "model_not_found",
            Self::BadRequest { .. } => "bad_request",
            Self::ContextTooLarge => "context_too_large",
            Self::InvalidOutput(_) => "invalid_output",
            Self::Incomplete { .. } => "incomplete",
            Self::Cancelled => "cancelled",
            Self::RecordTooLarge => "record_too_large",
            Self::OutputTooLarge => "output_too_large",
        }
    }

    /// Whether the source text may have reached the server.
    pub fn maybe_sent(&self) -> bool {
        match self {
            Self::ConnectFailed
            | Self::EndpointRejected(_)
            | Self::Tls
            | Self::KeyMissing
            | Self::KeyAccessDenied
            | Self::KeychainUnavailable => false,
            Self::Timeout { maybe_sent, .. } | Self::Disconnected { maybe_sent } => *maybe_sent,
            Self::Cancelled => true,
            _ => true,
        }
    }

    pub fn retry_class(&self) -> RetryClass {
        match self {
            Self::ConnectFailed => RetryClass::RetrySafe,
            Self::Timeout { maybe_sent: false, .. } => RetryClass::RetrySafe,
            Self::Timeout { maybe_sent: true, .. } => RetryClass::SentUnknown,
            Self::Disconnected { .. } => RetryClass::SentUnknown,
            Self::Http { safe_to_retry: true, .. } => RetryClass::RetrySafe,
            Self::Http { safe_to_retry: false, .. } => RetryClass::SentUnknown,
            Self::RateLimited { .. } => RetryClass::RetrySafe,
            Self::Auth { .. }
            | Self::KeyMissing
            | Self::KeyAccessDenied
            | Self::KeychainUnavailable
            | Self::EndpointRejected(_) => RetryClass::StopProvider,
            Self::Tls
            | Self::ModelNotFound
            | Self::BadRequest { .. }
            | Self::ContextTooLarge
            | Self::InvalidOutput(_)
            | Self::Incomplete { .. }
            | Self::Cancelled
            | Self::RecordTooLarge
            | Self::OutputTooLarge => RetryClass::DoNotRetry,
        }
    }

    pub fn to_app_error(&self) -> AppError {
        use ErrorCode::*;
        let (code, msg): (ErrorCode, &str) = match self {
            Self::ConnectFailed => (ProviderUnavailable, "번역 서버에 연결할 수 없습니다. 서버가 실행 중인지 확인해 주세요."),
            Self::EndpointRejected(_) => (ProviderUnavailable, "허용되지 않은 번역 서버 주소입니다. 서버 주소 설정을 확인해 주세요."),
            Self::Tls => (ProviderUnavailable, "보안 연결(TLS)에 실패했습니다. 서버 인증서를 확인해 주세요."),
            Self::Timeout { .. } => (ProviderUnavailable, "번역 서버 응답 시간이 초과되었습니다."),
            Self::Disconnected { .. } => (ProviderUnavailable, "번역 도중 연결이 끊어졌습니다. 다시 시도하면 새 요청이 발생할 수 있습니다."),
            Self::Http { .. } => (ProviderUnavailable, "번역 서버에서 오류가 발생했습니다."),
            Self::RateLimited { .. } => (RateLimited, "요청 한도를 초과했습니다. 잠시 후 자동으로 다시 시도합니다."),
            Self::Auth { .. } => (AuthFailed, "인증에 실패했습니다. API 키를 확인해 주세요."),
            Self::KeyMissing => (AuthFailed, "저장된 API 키가 없습니다. 키를 다시 입력해 주세요."),
            Self::KeyAccessDenied => (AuthFailed, "키체인 접근이 거부되었습니다. 접근을 허용하거나 키를 다시 입력해 주세요."),
            Self::KeychainUnavailable => (KeychainUnavailable, "키체인이 잠겨 있거나 사용할 수 없습니다. 잠금을 해제한 뒤 다시 시도해 주세요."),
            Self::ModelNotFound => (InvalidArgument, "선택한 모델을 찾을 수 없습니다. 모델 설정을 확인해 주세요."),
            Self::BadRequest { .. } => (InvalidArgument, "번역 서버가 요청을 거절했습니다. 모델과 옵션 설정을 확인해 주세요."),
            Self::ContextTooLarge => (ContextTooLarge, "모델 문맥 한도를 초과했습니다. 문맥 한도 설정을 확인해 주세요."),
            Self::InvalidOutput(_) | Self::RecordTooLarge | Self::OutputTooLarge => {
                (InvalidOutput, "번역 결과가 올바르지 않습니다. 다시 시도하거나 다른 모델을 사용해 주세요.")
            }
            Self::Incomplete { .. } => (InvalidOutput, "번역 결과가 정상적으로 끝나지 않았습니다."),
            Self::Cancelled => (Cancelled, "번역이 취소되었습니다."),
        };
        let mut e = AppError::new(code, msg)
            .with("reason", self.reason())
            .with("retryClass", serde_json::to_value(self.retry_class()).unwrap_or_default())
            .with("maybeSent", self.maybe_sent());
        match self {
            Self::Http { status, .. } | Self::Auth { status } | Self::BadRequest { status } => {
                e = e.with("status", *status);
            }
            Self::Incomplete { finish_reason } => {
                e = e.with("finishReason", finish_reason.clone());
            }
            _ => {}
        }
        if let Some(d) = self.retry_after() {
            e = e.with("retryAfterMs", d.as_millis() as u64);
        }
        e
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } | Self::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// Outcome of the retry policy for one failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Re-queue after this delay (job state RETRY_WAIT with nextAttemptAt = now + delay).
    RetryAfter(Duration),
    /// Keep RETRY_WAIT with nextAttemptAt = NULL; wait for the user to resume (Retry-After > 60s).
    ManualResume,
    /// Do not retry automatically.
    GiveUp,
}

/// Base backoff for the delay after failed attempt `attempt` (1-based): 1s, 3s, then none.
/// `jitter` in [-1.0, 1.0] scales ±20% (inject it for tests; see [`random_jitter`]).
pub fn retry_policy(attempt: u32, jitter: f64) -> Option<Duration> {
    let base_ms: f64 = match attempt {
        1 => 1000.0,
        2 => 3000.0,
        _ => return None,
    };
    let j = if jitter.is_finite() { jitter.clamp(-1.0, 1.0) } else { 0.0 };
    Some(Duration::from_millis((base_ms * (1.0 + 0.2 * j)).round() as u64))
}

/// Full decision for a failed attempt `attempt` (1-based, the attempt that just failed).
pub fn retry_decision(err: &ProviderError, attempt: u32, jitter: f64) -> RetryDecision {
    if err.retry_class() != RetryClass::RetrySafe || attempt >= MAX_ATTEMPTS {
        return RetryDecision::GiveUp;
    }
    if let ProviderError::RateLimited { retry_after: Some(d) } | ProviderError::Http { retry_after: Some(d), .. } = err {
        if *d > MAX_RETRY_AFTER {
            return RetryDecision::ManualResume;
        }
        return RetryDecision::RetryAfter(*d);
    }
    match retry_policy(attempt, jitter) {
        Some(d) => RetryDecision::RetryAfter(d),
        None => RetryDecision::GiveUp,
    }
}

/// Uniform jitter in [-1, 1].
pub fn random_jitter() -> f64 {
    use rand::Rng;
    rand::rng().random_range(-1.0..=1.0)
}

// ---------------------------------------------------------------------------------------
// Timeouts
// ---------------------------------------------------------------------------------------

/// Per-request timeouts. reqwest handles `connect`; first-byte / idle / total are enforced
/// around the response stream with tokio timers (see `providers::http`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    pub first_byte: Duration,
    pub idle: Duration,
    pub total: Duration,
}

impl Timeouts {
    pub const REMOTE: Timeouts = Timeouts {
        connect: Duration::from_secs(5),
        first_byte: Duration::from_secs(30),
        idle: Duration::from_secs(30),
        total: Duration::from_secs(120),
    };
    /// Ollama cold model loads need a longer first response.
    pub const OLLAMA: Timeouts = Timeouts {
        connect: Duration::from_secs(5),
        first_byte: Duration::from_secs(120),
        idle: Duration::from_secs(30),
        total: Duration::from_secs(300),
    };
}

// ---------------------------------------------------------------------------------------
// Provider contract
// ---------------------------------------------------------------------------------------

#[async_trait::async_trait]
pub trait TranslationProvider: Send + Sync {
    async fn probe(&self) -> Result<ProviderCapabilities, ProviderError>;
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;
    async fn translate(
        &self,
        request: TranslationRequest,
        sink: Sender<TranslationDelta>,
        cancel: CancellationToken,
    ) -> Result<TranslationOutput, ProviderError>;
}

/// Connection check input (spec §6.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeRequest {
    pub model_id: Option<String>,
    /// Allow a tiny fixed test phrase (never document text) to detect streaming /
    /// structured-output support. Shown to the user as a check action.
    pub send_test_prompt: bool,
}

/// The fixed phrase used by connection checks. Never document text.
pub const PROBE_TEST_PHRASE: &str = "Hello.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    pub reachable: bool,
    /// False only when the server answered 401/403.
    pub auth_ok: bool,
    /// `None` when the provider does not support model listing.
    pub models: Option<Vec<ModelInfo>>,
    /// `None` when unknown (no model id given, or listing unsupported and no test prompt).
    pub model_available: Option<bool>,
    pub model: Option<ModelInfo>,
    pub capabilities: ProviderCapabilities,
    /// True when a test prompt was actually sent to the model.
    pub sent_test_prompt: bool,
    /// Stable warning codes for the UI (e.g. "context_limit_unknown").
    pub warnings: Vec<String>,
}

#[async_trait::async_trait]
pub trait ProviderDiagnostics: Send + Sync {
    async fn probe_model(&self, req: &ProbeRequest) -> Result<ProbeReport, ProviderError>;
}

/// Everything the app needs from a provider.
pub trait Provider: TranslationProvider + ProviderDiagnostics {}
impl<T: TranslationProvider + ProviderDiagnostics> Provider for T {}

/// Build the adapter for a stored provider config.
pub fn build_provider(
    cfg: &ProviderConfig,
    secrets: Arc<dyn SecretStore>,
) -> Result<Arc<dyn Provider>, AppError> {
    build_provider_with_timeouts(cfg, secrets, cfg.kind.default_timeouts())
}

pub fn build_provider_with_timeouts(
    cfg: &ProviderConfig,
    secrets: Arc<dyn SecretStore>,
    timeouts: Timeouts,
) -> Result<Arc<dyn Provider>, AppError> {
    cfg.validate()?;
    Ok(match cfg.kind {
        ProviderKind::Ollama => Arc::new(ollama::OllamaProvider::with_timeouts(cfg.clone(), timeouts)?),
        ProviderKind::OpenaiCompatible => Arc::new(openai_compatible::OpenAiCompatibleProvider::with_timeouts(
            cfg.clone(),
            secrets,
            timeouts,
        )?),
    })
}

/// The exact JSON body the adapter of `kind` sends for `req` (no secrets, no requestId,
/// no time values). Hash this per part for `requestManifestHash`
/// (see `translation::prompt::request_manifest_hash`).
pub fn request_payload(
    kind: ProviderKind,
    caps: &ProviderCapabilities,
    req: &TranslationRequest,
) -> serde_json::Value {
    match kind {
        ProviderKind::Ollama => ollama::build_payload(caps, req),
        ProviderKind::OpenaiCompatible => openai_compatible::build_payload(caps, req),
    }
}

/// JSON schema for the structured segment response.
pub fn segments_json_schema(expected_ids: &[String]) -> serde_json::Value {
    let id_schema = if expected_ids.is_empty() {
        serde_json::json!({"type": "string"})
    } else {
        serde_json::json!({"type": "string", "enum": expected_ids})
    };
    serde_json::json!({
        "type": "object",
        "properties": {
            "segments": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"id": id_schema, "text": {"type": "string"}},
                    "required": ["id", "text"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["segments"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_schedule_with_jitter() {
        assert_eq!(retry_policy(1, 0.0), Some(Duration::from_millis(1000)));
        assert_eq!(retry_policy(2, 0.0), Some(Duration::from_millis(3000)));
        assert_eq!(retry_policy(3, 0.0), None);
        assert_eq!(retry_policy(1, 1.0), Some(Duration::from_millis(1200)));
        assert_eq!(retry_policy(1, -1.0), Some(Duration::from_millis(800)));
        assert_eq!(retry_policy(2, 1.0), Some(Duration::from_millis(3600)));
        assert_eq!(retry_policy(2, -1.0), Some(Duration::from_millis(2400)));
        // out of range jitter is clamped
        assert_eq!(retry_policy(1, 5.0), Some(Duration::from_millis(1200)));
        for _ in 0..100 {
            let d = retry_policy(1, random_jitter()).unwrap();
            assert!(d >= Duration::from_millis(800) && d <= Duration::from_millis(1200));
        }
    }

    #[test]
    fn retry_decisions() {
        let e = ProviderError::ConnectFailed;
        assert_eq!(retry_decision(&e, 1, 0.0), RetryDecision::RetryAfter(Duration::from_secs(1)));
        assert_eq!(retry_decision(&e, 2, 0.0), RetryDecision::RetryAfter(Duration::from_secs(3)));
        assert_eq!(retry_decision(&e, 3, 0.0), RetryDecision::GiveUp);

        let rl = ProviderError::RateLimited { retry_after: Some(Duration::from_secs(7)) };
        assert_eq!(retry_decision(&rl, 1, 0.9), RetryDecision::RetryAfter(Duration::from_secs(7)));
        let rl60 = ProviderError::RateLimited { retry_after: Some(Duration::from_secs(60)) };
        assert_eq!(retry_decision(&rl60, 1, 0.0), RetryDecision::RetryAfter(Duration::from_secs(60)));
        let rl_long = ProviderError::RateLimited { retry_after: Some(Duration::from_secs(61)) };
        assert_eq!(retry_decision(&rl_long, 1, 0.0), RetryDecision::ManualResume);
        let rl_none = ProviderError::RateLimited { retry_after: None };
        assert_eq!(retry_decision(&rl_none, 2, -1.0), RetryDecision::RetryAfter(Duration::from_millis(2400)));
        assert_eq!(retry_decision(&rl, 3, 0.0), RetryDecision::GiveUp);

        let safe5 = ProviderError::Http { status: 503, retry_after: None, safe_to_retry: true };
        assert_eq!(retry_decision(&safe5, 1, 0.0), RetryDecision::RetryAfter(Duration::from_secs(1)));
        let unsafe5 = ProviderError::Http { status: 500, retry_after: None, safe_to_retry: false };
        assert_eq!(retry_decision(&unsafe5, 1, 0.0), RetryDecision::GiveUp);
        assert_eq!(unsafe5.retry_class(), RetryClass::SentUnknown);

        for e in [
            ProviderError::Auth { status: 401 },
            ProviderError::Tls,
            ProviderError::InvalidOutput("x".into()),
            ProviderError::Disconnected { maybe_sent: true },
            ProviderError::Timeout { phase: TimeoutPhase::Idle, maybe_sent: true },
            ProviderError::ModelNotFound,
            ProviderError::Cancelled,
        ] {
            assert_eq!(retry_decision(&e, 1, 0.0), RetryDecision::GiveUp, "{e:?}");
        }
        assert_eq!(
            retry_decision(&ProviderError::Timeout { phase: TimeoutPhase::Connect, maybe_sent: false }, 1, 0.0),
            RetryDecision::RetryAfter(Duration::from_secs(1))
        );
    }

    #[test]
    fn error_classes_and_app_codes() {
        assert_eq!(ProviderError::Auth { status: 403 }.retry_class(), RetryClass::StopProvider);
        assert_eq!(ProviderError::Auth { status: 401 }.to_app_error().code, ErrorCode::AuthFailed);
        assert_eq!(ProviderError::KeychainUnavailable.to_app_error().code, ErrorCode::KeychainUnavailable);
        assert_eq!(ProviderError::RateLimited { retry_after: None }.to_app_error().code, ErrorCode::RateLimited);
        assert_eq!(ProviderError::ContextTooLarge.to_app_error().code, ErrorCode::ContextTooLarge);
        assert_eq!(ProviderError::OutputTooLarge.to_app_error().code, ErrorCode::InvalidOutput);
        assert_eq!(ProviderError::Cancelled.to_app_error().code, ErrorCode::Cancelled);
        assert_eq!(ProviderError::ConnectFailed.to_app_error().code, ErrorCode::ProviderUnavailable);
        assert!(!ProviderError::ConnectFailed.maybe_sent());
        let e = ProviderError::Incomplete { finish_reason: "length".into() }.to_app_error();
        assert_eq!(e.details["finishReason"], "length");
        assert_eq!(e.details["retryClass"], "do_not_retry");
    }

    #[test]
    fn caps_serde_roundtrip() {
        let c = ProviderCapabilities::default_for(ProviderKind::Ollama);
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["tokenLimitField"], "num_predict");
        let back: ProviderCapabilities = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
        assert_eq!(serde_json::to_value(ProviderKind::OpenaiCompatible).unwrap(), "openai_compatible");
    }
}
