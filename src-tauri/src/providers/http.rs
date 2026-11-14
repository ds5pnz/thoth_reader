//! Shared HTTP plumbing for provider adapters: policy-checked client, phase timeouts
//! (connect / first byte / idle / total), bounded body reading and status classification.
//!
//! Nothing here logs request/response bodies, keys or URL queries.

use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::retry_after::parse_retry_after;
use super::{ProviderError, TimeoutPhase, Timeouts};
use crate::error::AppError;
use crate::security::endpoint::{is_policy_error, normalize_endpoint, NormalizedEndpoint, PolicyResolver};

pub const USER_AGENT: &str = concat!("ThothReader/", env!("CARGO_PKG_VERSION"));

/// A reqwest client bound to one validated endpoint.
#[derive(Debug, Clone)]
pub struct HttpCore {
    client: reqwest::Client,
    endpoint: NormalizedEndpoint,
    timeouts: Timeouts,
}

impl HttpCore {
    pub fn new(endpoint_raw: &str, allow_private_http: bool, timeouts: Timeouts) -> Result<Self, AppError> {
        let endpoint = normalize_endpoint(endpoint_raw, allow_private_http)?;
        let resolver = PolicyResolver::new(endpoint.resolve_policy());
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(timeouts.connect)
            .https_only(endpoint.is_https)
            .dns_resolver(resolver)
            .build()
            .map_err(|_| AppError::internal("HTTP 클라이언트를 만들 수 없습니다."))?;
        Ok(Self { client, endpoint, timeouts })
    }

    pub fn endpoint(&self) -> &NormalizedEndpoint {
        &self.endpoint
    }
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
    pub fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    /// Deadlines for a request starting now.
    pub fn deadlines(&self) -> Deadlines {
        Deadlines::start(self.timeouts)
    }

    /// Send and wait for response headers (bounded by the first-byte and total deadlines).
    pub async fn send(
        &self,
        rb: reqwest::RequestBuilder,
        cancel: &CancellationToken,
        dl: &Deadlines,
    ) -> Result<reqwest::Response, ProviderError> {
        let deadline = dl.first_byte.min(dl.total);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            r = tokio::time::timeout_at(deadline, rb.send()) => match r {
                Err(_) => Err(ProviderError::Timeout { phase: dl.phase_at(deadline, TimeoutPhase::FirstByte), maybe_sent: true }),
                Ok(Err(e)) => Err(classify_send_error(&e)),
                Ok(Ok(resp)) => Ok(resp),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    pub first_byte: Instant,
    pub total: Instant,
    pub idle: Duration,
}

impl Deadlines {
    pub fn start(t: Timeouts) -> Self {
        let now = Instant::now();
        Self { first_byte: now + t.first_byte, total: now + t.total, idle: t.idle }
    }
    fn phase_at(&self, deadline: Instant, phase: TimeoutPhase) -> TimeoutPhase {
        if deadline >= self.total {
            TimeoutPhase::Total
        } else {
            phase
        }
    }
}

/// Classify a reqwest send error.
pub fn classify_send_error(e: &reqwest::Error) -> ProviderError {
    let kind = if is_policy_error(e) {
        ProviderError::EndpointRejected("resolved address not allowed".into())
    } else if e.is_connect() && e.is_timeout() {
        ProviderError::Timeout { phase: TimeoutPhase::Connect, maybe_sent: false }
    } else if e.is_connect() {
        if looks_like_tls(e) {
            ProviderError::Tls
        } else {
            ProviderError::ConnectFailed
        }
    } else if e.is_timeout() {
        ProviderError::Timeout { phase: TimeoutPhase::Total, maybe_sent: true }
    } else if e.is_builder() {
        ProviderError::BadRequest { status: 0 }
    } else {
        // Failure after the connection was established: the request may have been sent.
        ProviderError::Disconnected { maybe_sent: true }
    };
    tracing::debug!(target: "provider", "send failed: reason={} detail={}", kind.reason(), redacted_chain(e));
    kind
}

fn looks_like_tls(e: &dyn std::error::Error) -> bool {
    let s = error_chain(e).to_ascii_lowercase();
    ["certificate", "tls", "ssl", "handshake", "rustls", "alert"].iter().any(|k| s.contains(k))
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(err) = cur {
        out.push_str(": ");
        out.push_str(&err.to_string());
        cur = err.source();
    }
    out
}

fn redacted_chain(e: &reqwest::Error) -> String {
    // reqwest includes the URL in Display; ErrWrap omits it so queries/paths never reach logs.
    let mut s = error_chain(&ErrWrap(e));
    s.truncate(300);
    crate::diagnostics::redact(&s)
}

#[derive(Debug)]
struct ErrWrap<'a>(&'a reqwest::Error);
impl std::fmt::Display for ErrWrap<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut kinds = Vec::new();
        if self.0.is_connect() {
            kinds.push("connect");
        }
        if self.0.is_timeout() {
            kinds.push("timeout");
        }
        if self.0.is_request() {
            kinds.push("request");
        }
        if self.0.is_body() {
            kinds.push("body");
        }
        write!(f, "reqwest error [{}]", kinds.join(","))
    }
}
impl std::error::Error for ErrWrap<'_> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        std::error::Error::source(self.0)
    }
}

/// Response body reader enforcing first-byte / idle / total deadlines and a raw size cap.
pub struct BodyStream {
    inner: BoxStream<'static, reqwest::Result<Bytes>>,
    dl: Deadlines,
    got_first: bool,
    raw_bytes: usize,
    raw_limit: usize,
}

impl BodyStream {
    pub fn new(resp: reqwest::Response, dl: Deadlines, raw_limit: usize) -> Self {
        Self { inner: resp.bytes_stream().boxed(), dl, got_first: false, raw_bytes: 0, raw_limit }
    }

    pub fn raw_bytes(&self) -> usize {
        self.raw_bytes
    }

    /// Next non-empty chunk; `Ok(None)` at clean end of body.
    pub async fn next_chunk(&mut self, cancel: &CancellationToken) -> Result<Option<Bytes>, ProviderError> {
        loop {
            let (phase, phase_deadline) = if self.got_first {
                (TimeoutPhase::Idle, Instant::now() + self.dl.idle)
            } else {
                (TimeoutPhase::FirstByte, self.dl.first_byte)
            };
            let deadline = phase_deadline.min(self.dl.total);
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                r = tokio::time::timeout_at(deadline, self.inner.next()) => r,
            };
            match next {
                Err(_) => {
                    return Err(ProviderError::Timeout { phase: self.dl.phase_at(deadline, phase), maybe_sent: true })
                }
                Ok(None) => return Ok(None),
                Ok(Some(Err(e))) => {
                    tracing::debug!(target: "provider", "body read failed: {}", redacted_chain(&e));
                    if e.is_timeout() {
                        return Err(ProviderError::Timeout { phase, maybe_sent: true });
                    }
                    return Err(ProviderError::Disconnected { maybe_sent: true });
                }
                Ok(Some(Ok(b))) => {
                    if b.is_empty() {
                        continue;
                    }
                    self.got_first = true;
                    self.raw_bytes += b.len();
                    if self.raw_bytes > self.raw_limit {
                        return Err(ProviderError::OutputTooLarge);
                    }
                    return Ok(Some(b));
                }
            }
        }
    }

    /// Read the whole (bounded) body.
    pub async fn read_all(mut self, cancel: &CancellationToken) -> Result<Vec<u8>, ProviderError> {
        let mut out = Vec::new();
        while let Some(c) = self.next_chunk(cancel).await? {
            out.extend_from_slice(&c);
        }
        Ok(out)
    }
}

/// Turn a non-success response into a classified error. The body is read (bounded, best
/// effort) only to recognise context / model errors and is never logged.
pub async fn error_from_response(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    dl: &Deadlines,
) -> ProviderError {
    read_error(resp, cancel, dl).await.0
}

/// Like [`error_from_response`] but also returns the lowercased, bounded body for
/// capability detection (probe only). The body must never be logged.
pub async fn read_error(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    dl: &Deadlines,
) -> (ProviderError, String) {
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let mut body = Vec::new();
    let mut stream = BodyStream::new(resp, *dl, usize::MAX);
    // Short, bounded read; errors just leave the body empty.
    let read = async {
        while let Ok(Some(c)) = stream.next_chunk(cancel).await {
            body.extend_from_slice(&c);
            if body.len() >= super::MAX_ERROR_BODY_BYTES {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), read).await;
    if cancel.is_cancelled() {
        return (ProviderError::Cancelled, String::new());
    }
    let text = String::from_utf8_lossy(&body).to_ascii_lowercase();
    let err = classify_status(status, &headers, &text, SystemTime::now());
    tracing::debug!(target: "provider", "http error: status={} reason={}", status, err.reason());
    (err, text)
}

/// Accumulates output text (bounded by `MAX_OUTPUT_BYTES`) and forwards deltas to the sink.
/// A closed sink does not abort the request; the final output still matters.
pub struct OutputAssembler {
    text: String,
    sink: Option<tokio::sync::mpsc::Sender<super::TranslationDelta>>,
    part_index: u32,
}

impl OutputAssembler {
    pub fn new(sink: Option<tokio::sync::mpsc::Sender<super::TranslationDelta>>, part_index: u32) -> Self {
        Self { text: String::new(), sink, part_index }
    }

    pub async fn push(&mut self, s: &str, cancel: &CancellationToken) -> Result<(), ProviderError> {
        if s.is_empty() {
            return Ok(());
        }
        if self.text.len() + s.len() > super::MAX_OUTPUT_BYTES {
            return Err(ProviderError::OutputTooLarge);
        }
        self.text.push_str(s);
        if let Some(sink) = &self.sink {
            let delta = super::TranslationDelta { part_index: self.part_index, text: s.to_string() };
            let sent = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                r = sink.send(delta) => r.is_ok(),
            };
            if !sent {
                self.sink = None;
            }
        }
        Ok(())
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn into_text(self) -> String {
        self.text
    }
}

/// Send `payload` as a JSON POST / plain GET and return the parsed (bounded) JSON body of a
/// successful response.
pub async fn json_response(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    dl: &Deadlines,
) -> Result<serde_json::Value, ProviderError> {
    if !resp.status().is_success() {
        return Err(error_from_response(resp, cancel, dl).await);
    }
    let bytes = BodyStream::new(resp, *dl, super::MAX_JSON_BODY_BYTES).read_all(cancel).await?;
    serde_json::from_slice(&bytes).map_err(invalid_json)
}

/// Map an HTTP status (+ headers/body hints) to a [`ProviderError`].
pub fn classify_status(status: u16, headers: &HeaderMap, body: &str, now: SystemTime) -> ProviderError {
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| parse_retry_after(v, now));
    let lower = body.to_ascii_lowercase();
    let mentions_context = [
        "context_length_exceeded",
        "context length",
        "maximum context",
        "context window",
        "too many tokens",
        "prompt is too long",
        "exceeds the context",
    ]
    .iter()
    .any(|k| lower.contains(k));
    let model_missing = lower.contains("model")
        && (lower.contains("not found") || lower.contains("does not exist") || lower.contains("model_not_found"));
    match status {
        401 | 403 => ProviderError::Auth { status },
        429 => ProviderError::RateLimited { retry_after },
        413 => ProviderError::ContextTooLarge,
        404 if lower.contains("model") => ProviderError::ModelNotFound,
        400 | 422 if mentions_context => ProviderError::ContextTooLarge,
        400 | 422 if model_missing => ProviderError::ModelNotFound,
        408 => ProviderError::Http { status, retry_after, safe_to_retry: false },
        // 503 / 529 signal "not processed, come back later".
        503 | 529 => ProviderError::Http { status, retry_after, safe_to_retry: true },
        500..=599 => ProviderError::Http { status, retry_after, safe_to_retry: false },
        _ => ProviderError::BadRequest { status },
    }
}

/// Map a `serde_json` failure on a provider response.
pub fn invalid_json(_e: serde_json::Error) -> ProviderError {
    ProviderError::InvalidOutput("malformed JSON in provider response".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderValue, RETRY_AFTER};

    #[test]
    fn status_classification() {
        let now = SystemTime::now();
        let mut h = HeaderMap::new();
        assert_eq!(classify_status(401, &h, "", now), ProviderError::Auth { status: 401 });
        assert_eq!(classify_status(403, &h, "", now), ProviderError::Auth { status: 403 });
        assert_eq!(classify_status(429, &h, "", now), ProviderError::RateLimited { retry_after: None });
        h.insert(RETRY_AFTER, HeaderValue::from_static("12"));
        assert_eq!(
            classify_status(429, &h, "", now),
            ProviderError::RateLimited { retry_after: Some(Duration::from_secs(12)) }
        );
        assert_eq!(
            classify_status(503, &h, "", now),
            ProviderError::Http { status: 503, retry_after: Some(Duration::from_secs(12)), safe_to_retry: true }
        );
        let h = HeaderMap::new();
        assert_eq!(
            classify_status(500, &h, "", now),
            ProviderError::Http { status: 500, retry_after: None, safe_to_retry: false }
        );
        assert_eq!(classify_status(404, &h, r#"{"error":"model 'x' not found"}"#, now), ProviderError::ModelNotFound);
        assert_eq!(classify_status(404, &h, "nope", now), ProviderError::BadRequest { status: 404 });
        assert_eq!(
            classify_status(400, &h, r#"{"error":{"code":"context_length_exceeded"}}"#, now),
            ProviderError::ContextTooLarge
        );
        assert_eq!(classify_status(400, &h, "bad", now), ProviderError::BadRequest { status: 400 });
        assert_eq!(classify_status(302, &h, "", now), ProviderError::BadRequest { status: 302 });
        assert_eq!(classify_status(413, &h, "", now), ProviderError::ContextTooLarge);
    }
}
