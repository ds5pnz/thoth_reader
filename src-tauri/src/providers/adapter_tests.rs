//! Adapter integration tests (T09 / T10): wiremock for whole responses, a raw TCP server
//! for exact chunking, stalls and truncated chunked bodies.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::ollama::OllamaProvider;
use super::openai_compatible::OpenAiCompatibleProvider;
use super::*;
use crate::security::keychain::MemorySecretStore;

const KEY_REF: &str = "0192f6c4-8b7e-7cc3-9d2a-3f1e5b6a7c8d";

/// A closed local port is refused at once on macOS and Linux. Windows keeps retrying the
/// connection for a couple of seconds, so there the same situation ends as a connect
/// timeout; either way nothing was sent.
fn never_connected(e: &ProviderError) -> bool {
    matches!(e, ProviderError::ConnectFailed) || (cfg!(windows) && matches!(e, ProviderError::Timeout { phase: TimeoutPhase::Connect, maybe_sent: false }))
}

fn timeouts() -> Timeouts {
    Timeouts {
        connect: Duration::from_secs(2),
        first_byte: Duration::from_secs(3),
        idle: Duration::from_millis(400),
        total: Duration::from_secs(10),
    }
}

fn cfg(kind: ProviderKind, endpoint: &str, key: bool, caps: Option<ProviderCapabilities>) -> ProviderConfig {
    ProviderConfig {
        id: "p1".into(),
        kind,
        endpoint: endpoint.into(),
        account_namespace: "default".into(),
        keychain_ref: key.then(|| KEY_REF.to_string()),
        allow_private_http: false,
        concurrency: 1,
        capabilities: caps,
    }
}

fn store_with_key() -> Arc<MemorySecretStore> {
    let s = Arc::new(MemorySecretStore::new());
    s.store(KEY_REF, "sk-test-secret").unwrap();
    s
}

fn openai(endpoint: &str) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::with_timeouts(cfg(ProviderKind::OpenaiCompatible, endpoint, true, None), store_with_key(), timeouts())
        .unwrap()
}

fn openai_t(endpoint: &str, t: Timeouts) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::with_timeouts(cfg(ProviderKind::OpenaiCompatible, endpoint, true, None), store_with_key(), t).unwrap()
}

fn ollama(endpoint: &str, caps: Option<ProviderCapabilities>) -> OllamaProvider {
    OllamaProvider::with_timeouts(cfg(ProviderKind::Ollama, endpoint, false, caps), timeouts()).unwrap()
}

fn req(stream: bool, format: ResponseFormat) -> TranslationRequest {
    TranslationRequest {
        request_id: "req-1".into(),
        job_id: "job-1".into(),
        block_id: "blk".into(),
        part_index: 2,
        part_count: 3,
        model_id: "m1".into(),
        source_language: "en".into(),
        target_language: "ko".into(),
        messages: vec![
            ChatMessage { role: ChatRole::System, content: "sys".into() },
            ChatMessage { role: ChatRole::User, content: "{\"segments\":[{\"id\":\"s0\",\"text\":\"Hi\"}]}".into() },
        ],
        options: GenerationOptions { temperature: Some(0.2), seed: Some(7), max_output_tokens: Some(512) },
        response_format: format,
        stream,
        expected_segment_ids: vec!["s0".into()],
    }
}

async fn run(p: &dyn TranslationProvider, r: TranslationRequest) -> (Result<TranslationOutput, ProviderError>, String) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    let res = p.translate(r, tx, CancellationToken::new()).await;
    let mut deltas = String::new();
    while let Ok(d) = rx.try_recv() {
        assert_eq!(d.part_index, 2);
        deltas.push_str(&d.text);
    }
    (res, deltas)
}

fn sse(frames: &[&str]) -> String {
    frames.iter().map(|f| format!("data: {f}\n\n")).collect()
}

fn delta(s: &str) -> String {
    json!({"choices":[{"index":0,"delta":{"content":s},"finish_reason":null}]}).to_string()
}

fn finish(reason: &str) -> String {
    json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}).to_string()
}

async fn sse_server(body: impl Into<Vec<u8>>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.into(), "text/event-stream"))
        .mount(&server)
        .await;
    server
}

// ----- raw server -----------------------------------------------------------------------

struct RawResponse {
    content_type: &'static str,
    parts: Vec<(u64, Vec<u8>)>,
    /// Send the terminating zero-length chunk.
    complete: bool,
    /// Keep the connection open this long after the last part (stall).
    hold_ms: u64,
}

async fn raw_server(resp: RawResponse) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let resp = Arc::new(resp);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let resp = resp.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                // read headers + content-length body
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(h) = find(&buf, b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..h]).to_ascii_lowercase();
                        let cl = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        while buf.len() < h + 4 + cl {
                            let n = sock.read(&mut tmp).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        }
                        break;
                    }
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                    resp.content_type
                );
                if sock.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                let _ = sock.flush().await;
                for (delay, bytes) in &resp.parts {
                    tokio::time::sleep(Duration::from_millis(*delay)).await;
                    let chunk = [format!("{:x}\r\n", bytes.len()).as_bytes(), bytes, b"\r\n"].concat();
                    if sock.write_all(&chunk).await.is_err() {
                        return;
                    }
                    let _ = sock.flush().await;
                }
                if resp.complete {
                    let _ = sock.write_all(b"0\r\n\r\n").await;
                    let _ = sock.flush().await;
                }
                tokio::time::sleep(Duration::from_millis(resp.hold_ms)).await;
            });
        }
    });
    format!("http://{addr}")
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// Split bytes at the given offsets into delayed parts.
fn split_at(bytes: &[u8], cuts: &[usize]) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut prev = 0;
    for &c in cuts.iter().chain([bytes.len()].iter()) {
        out.push((15, bytes[prev..c].to_vec()));
        prev = c;
    }
    out
}

// ----- OpenAI-compatible: SSE ------------------------------------------------------------

#[tokio::test]
async fn sse_utf8_split_across_network_chunks() {
    let body = sse(&[&delta("안녕"), &delta("하세요 😀"), &finish("stop"), "[DONE]"]);
    let bytes = body.as_bytes();
    // cut inside the 3-byte '안' and inside the 4-byte emoji
    let first = body.find("안").unwrap() + 1;
    let emoji = body.find('😀').unwrap() + 2;
    let url = raw_server(RawResponse {
        content_type: "text/event-stream",
        parts: split_at(bytes, &[first, emoji]),
        complete: true,
        hold_ms: 0,
    })
    .await;
    let p = openai(&format!("{url}/v1"));
    let (res, deltas) = run(&p, req(true, ResponseFormat::Text)).await;
    let out = res.unwrap();
    assert_eq!(out.text, "안녕하세요 😀");
    assert_eq!(deltas, out.text);
    assert_eq!(out.finish_reason, "stop");
    assert!(out.sent_bytes > 0);
}

#[tokio::test]
async fn sse_crlf_multiline_comments_empty_deltas_done() {
    // CRLF line endings, a comment, an empty delta, a role-only delta, a JSON object split
    // over two data: lines, usage frame, then [DONE].
    let body = [
        ": keep-alive\r\n\r\n",
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\r\n\r\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":null}]}\r\n\r\n",
        "data: {\"choices\":[{\"delta\":\r\ndata: {\"content\":\"번역\"},\"finish_reason\":null}]}\r\n\r\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"문\"},\"finish_reason\":\"stop\"}]}\r\n\r\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3}}\r\n\r\n",
        "data: [DONE]\r\n\r\n",
    ]
    .concat();
    let server = sse_server(body).await;
    let p = openai(&format!("{}/v1/", server.uri()));
    let (res, _) = run(&p, req(true, ResponseFormat::Text)).await;
    let out = res.unwrap();
    assert_eq!(out.text, "번역문");
    assert_eq!(out.usage, Some(Usage { prompt_tokens: Some(11), completion_tokens: Some(3) }));

    // Authorization header from the (memory) Keychain; no tools; known options only.
    let reqs = server.received_requests().await.unwrap();
    let r = &reqs[0];
    assert_eq!(r.headers.get("authorization").unwrap().to_str().unwrap(), "Bearer sk-test-secret");
    let body: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 512);
    assert_eq!(body["temperature"], 0.2);
    assert!(body.get("seed").is_none(), "seed not a default-known option for compat APIs");
    assert!(body.get("tools").is_none() && body.get("functions").is_none());
    assert!(body.get("response_format").is_none());
}

#[tokio::test]
async fn sse_clean_eof_after_stop_without_done_is_ok_but_missing_finish_is_error() {
    let server = sse_server(sse(&[&delta("a"), &finish("stop")])).await;
    let (res, _) = run(&openai(&format!("{}/v1", server.uri())), req(true, ResponseFormat::Text)).await;
    assert_eq!(res.unwrap().text, "a");

    let server = sse_server(sse(&[&delta("a"), "[DONE]"])).await;
    let (res, _) = run(&openai(&format!("{}/v1", server.uri())), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::Incomplete { finish_reason: "no_finish_reason".into() }));

    let server = sse_server("").await;
    let (res, _) = run(&openai(&format!("{}/v1", server.uri())), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::Incomplete { finish_reason: "no_terminal_frame".into() }));
}

#[tokio::test]
async fn sse_length_content_filter_tool_calls_refusal_are_errors() {
    for (frames, reason) in [
        (vec![delta("partial"), finish("length"), "[DONE]".to_string()], "length"),
        (vec![delta("x"), finish("content_filter"), "[DONE]".to_string()], "content_filter"),
        (
            vec![
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":"{}"}}]},"finish_reason":null}]})
                    .to_string(),
                finish("tool_calls"),
            ],
            "tool_calls",
        ),
        (vec![finish("tool_calls")], "tool_calls"),
        (vec![json!({"choices":[{"delta":{"refusal":"I can't"},"finish_reason":null}]}).to_string()], "refusal"),
    ] {
        let refs: Vec<&str> = frames.iter().map(String::as_str).collect();
        let server = sse_server(sse(&refs)).await;
        let (res, _) = run(&openai(&format!("{}/v1", server.uri())), req(true, ResponseFormat::Text)).await;
        assert_eq!(res, Err(ProviderError::Incomplete { finish_reason: reason.into() }), "{reason}");
    }
}

#[tokio::test]
async fn sse_malformed_json_frame_is_invalid_output() {
    let server = sse_server(format!("data: {}\n\ndata: {{\"choices\":[\n\n", delta("a"))).await;
    let (res, _) = run(&openai(&format!("{}/v1", server.uri())), req(true, ResponseFormat::Text)).await;
    assert!(matches!(res, Err(ProviderError::InvalidOutput(_))), "{res:?}");
    assert_eq!(res.unwrap_err().retry_class(), RetryClass::DoNotRetry);
}

#[tokio::test]
async fn sse_record_over_256k_and_output_over_1mib() {
    let big = "a".repeat(MAX_RECORD_BYTES + 10);
    let server = sse_server(sse(&[&delta(&big), &finish("stop")])).await;
    let (res, _) = run(&openai(&format!("{}/v1", server.uri())), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::RecordTooLarge));

    let chunk = "b".repeat(200 * 1024);
    let mut frames: Vec<String> = (0..6).map(|_| delta(&chunk)).collect();
    frames.push(finish("stop"));
    let refs: Vec<&str> = frames.iter().map(String::as_str).collect();
    let server = sse_server(sse(&refs)).await;
    let (res, _) = run(&openai(&format!("{}/v1", server.uri())), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::OutputTooLarge));
}

#[tokio::test]
async fn sse_truncated_chunked_body_is_disconnected() {
    let body = format!("data: {}\n\n", delta("half"));
    let url = raw_server(RawResponse {
        content_type: "text/event-stream",
        parts: vec![(0, body.into_bytes())],
        complete: false,
        hold_ms: 0,
    })
    .await;
    let (res, deltas) = run(&openai(&format!("{url}/v1")), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::Disconnected { maybe_sent: true }));
    assert_eq!(deltas, "half"); // preview only; never a success
    assert_eq!(res.unwrap_err().retry_class(), RetryClass::SentUnknown);
}

#[tokio::test]
async fn idle_timeout_after_first_frame() {
    let url = raw_server(RawResponse {
        content_type: "text/event-stream",
        parts: vec![(0, format!("data: {}\n\n", delta("x")).into_bytes())],
        complete: false,
        hold_ms: 3000,
    })
    .await;
    let start = std::time::Instant::now();
    let (res, _) = run(&openai(&format!("{url}/v1")), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::Timeout { phase: TimeoutPhase::Idle, maybe_sent: true }));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn first_byte_and_total_timeouts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(1500)))
        .mount(&server)
        .await;
    let t = Timeouts { first_byte: Duration::from_millis(200), ..timeouts() };
    let (res, _) = run(&openai_t(&format!("{}/v1", server.uri()), t), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::Timeout { phase: TimeoutPhase::FirstByte, maybe_sent: true }));

    // Slow trickle: each frame within idle, total exceeded.
    let parts: Vec<(u64, Vec<u8>)> = (0..20).map(|_| (100, format!("data: {}\n\n", delta("x")).into_bytes())).collect();
    let url = raw_server(RawResponse { content_type: "text/event-stream", parts, complete: true, hold_ms: 0 }).await;
    let t = Timeouts { total: Duration::from_millis(600), ..timeouts() };
    let (res, _) = run(&openai_t(&format!("{url}/v1"), t), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::Timeout { phase: TimeoutPhase::Total, maybe_sent: true }));
}

#[tokio::test]
async fn cancel_mid_stream() {
    let parts: Vec<(u64, Vec<u8>)> = (0..50).map(|_| (50, format!("data: {}\n\n", delta("x")).into_bytes())).collect();
    let url = raw_server(RawResponse { content_type: "text/event-stream", parts, complete: true, hold_ms: 0 }).await;
    let p = openai(&format!("{url}/v1"));
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let (tx, _rx) = tokio::sync::mpsc::channel(1024);
    assert_eq!(p.translate(req(true, ResponseFormat::Text), tx, cancel).await, Err(ProviderError::Cancelled));
}

// ----- OpenAI-compatible: HTTP errors -----------------------------------------------------

async fn status_server(t: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(t).mount(&server).await;
    server
}

#[tokio::test]
async fn http_status_mapping() {
    let s = status_server(ResponseTemplate::new(429).insert_header("Retry-After", "5")).await;
    let (res, _) = run(&openai(&format!("{}/v1", s.uri())), req(true, ResponseFormat::Text)).await;
    let e = res.unwrap_err();
    assert_eq!(e, ProviderError::RateLimited { retry_after: Some(Duration::from_secs(5)) });
    assert_eq!(retry_decision(&e, 1, 0.0), RetryDecision::RetryAfter(Duration::from_secs(5)));

    let s = status_server(ResponseTemplate::new(429).insert_header("Retry-After", "120")).await;
    let (res, _) = run(&openai(&format!("{}/v1", s.uri())), req(true, ResponseFormat::Text)).await;
    assert_eq!(retry_decision(&res.unwrap_err(), 1, 0.0), RetryDecision::ManualResume);

    let s = status_server(ResponseTemplate::new(401).set_body_string("{\"error\":\"bad key sk-test-secret\"}")).await;
    let (res, _) = run(&openai(&format!("{}/v1", s.uri())), req(true, ResponseFormat::Text)).await;
    let e = res.unwrap_err();
    assert_eq!(e, ProviderError::Auth { status: 401 });
    assert_eq!(e.retry_class(), RetryClass::StopProvider);
    assert!(!format!("{:?}", e.to_app_error()).contains("sk-test"));

    let s = status_server(ResponseTemplate::new(500)).await;
    let (res, _) = run(&openai(&format!("{}/v1", s.uri())), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::Http { status: 500, retry_after: None, safe_to_retry: false }));

    let s = status_server(ResponseTemplate::new(503)).await;
    let (res, _) = run(&openai(&format!("{}/v1", s.uri())), req(true, ResponseFormat::Text)).await;
    assert_eq!(res.unwrap_err().retry_class(), RetryClass::RetrySafe);

    let s = status_server(
        ResponseTemplate::new(400).set_body_string("{\"error\":{\"code\":\"context_length_exceeded\"}}"),
    )
    .await;
    let (res, _) = run(&openai(&format!("{}/v1", s.uri())), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::ContextTooLarge));

    let s = status_server(ResponseTemplate::new(404).set_body_string("{\"error\":\"The model `m1` does not exist\"}")).await;
    let (res, _) = run(&openai(&format!("{}/v1", s.uri())), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::ModelNotFound));
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let target = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&target).await;
    let s = status_server(
        ResponseTemplate::new(307).insert_header("Location", format!("{}/v1/chat/completions", target.uri()).as_str()),
    )
    .await;
    let (res, _) = run(&openai(&format!("{}/v1", s.uri())), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::BadRequest { status: 307 }));
    assert!(target.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn connect_refused_is_connect_failed() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let (res, _) = run(&openai(&format!("http://127.0.0.1:{port}/v1")), req(true, ResponseFormat::Text)).await;
    let e = res.unwrap_err();
    assert!(never_connected(&e), "{e:?}");
    assert!(!e.maybe_sent());
    assert_eq!(retry_decision(&e, 1, 0.0), RetryDecision::RetryAfter(Duration::from_secs(1)));
}

#[tokio::test]
async fn keychain_missing_locked_denied_send_nothing() {
    let server = sse_server(sse(&[&delta("a"), &finish("stop")])).await;
    let ep = format!("{}/v1", server.uri());
    let empty = Arc::new(MemorySecretStore::new());
    let p = OpenAiCompatibleProvider::with_timeouts(cfg(ProviderKind::OpenaiCompatible, &ep, true, None), empty.clone(), timeouts())
        .unwrap();
    let (res, _) = run(&p, req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::KeyMissing));
    empty.set_locked(true);
    let (res, _) = run(&p, req(true, ResponseFormat::Text)).await;
    let e = res.unwrap_err();
    assert_eq!(e, ProviderError::KeychainUnavailable);
    assert_eq!(e.to_app_error().code, crate::error::ErrorCode::KeychainUnavailable);
    empty.set_denied(true);
    let (res, _) = run(&p, req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::KeyAccessDenied));
    assert!(server.received_requests().await.unwrap().is_empty());

    // No keychain_ref: no Authorization header at all.
    let p = OpenAiCompatibleProvider::with_timeouts(cfg(ProviderKind::OpenaiCompatible, &ep, false, None), empty, timeouts()).unwrap();
    let (res, _) = run(&p, req(true, ResponseFormat::Text)).await;
    assert!(res.is_ok());
    assert!(server.received_requests().await.unwrap()[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn non_stream_json_path_and_structured_format() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"index":0,"message":{"role":"assistant","content":"{\"segments\":[{\"id\":\"s0\",\"text\":\"안녕\"}]}"},"finish_reason":"stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        })))
        .mount(&server)
        .await;
    let caps = ProviderCapabilities {
        streaming: false,
        structured_output: true,
        seed: true,
        token_limit_field: Some("max_completion_tokens".into()),
        ..ProviderCapabilities::default_for(ProviderKind::OpenaiCompatible)
    };
    let p = OpenAiCompatibleProvider::with_timeouts(
        cfg(ProviderKind::OpenaiCompatible, &format!("{}/v1", server.uri()), true, Some(caps.clone())),
        store_with_key(),
        timeouts(),
    )
    .unwrap();
    let r = req(false, ResponseFormat::JsonSegments);
    let (res, deltas) = run(&p, r.clone()).await;
    let out = res.unwrap();
    assert_eq!(out.text, "{\"segments\":[{\"id\":\"s0\",\"text\":\"안녕\"}]}");
    assert_eq!(deltas, out.text);
    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent, request_payload(ProviderKind::OpenaiCompatible, &caps, &r), "body == request_payload");
    assert_eq!(sent["stream"], false);
    assert_eq!(sent["seed"], 7);
    assert_eq!(sent["max_completion_tokens"], 512);
    assert_eq!(sent["response_format"]["type"], "json_schema");
    assert_eq!(out.sent_bytes as usize, server.received_requests().await.unwrap()[0].body.len());

    // Non-stream length / tool calls -> errors.
    let server = status_server(ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{"message":{"content":"part"},"finish_reason":"length"}]
    })))
    .await;
    let (res, _) = run(&openai(&format!("{}/v1", server.uri())), req(false, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::Incomplete { finish_reason: "length".into() }));
    let server = status_server(ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{"message":{"content":null,"tool_calls":[{"id":"1"}]},"finish_reason":"tool_calls"}]
    })))
    .await;
    let (res, _) = run(&openai(&format!("{}/v1", server.uri())), req(false, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::Incomplete { finish_reason: "tool_calls".into() }));
}

#[tokio::test]
async fn models_listing_and_probe() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "m1", "max_model_len": 32768}, {"id": "m2"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[&delta("{\"segments\":[{\"id\":\"s0\",\"text\":\"안녕\"}]}"), &finish("stop"), "[DONE]"]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let p = openai(&format!("{}/v1", server.uri()));
    let models = p.list_models().await.unwrap();
    assert_eq!(models[0], ModelInfo { id: "m1".into(), digest: None, context_length: Some(32768) });
    assert!(p.probe().await.unwrap().model_listing);

    let no_prompt = p.probe_model(&ProbeRequest { model_id: Some("m1".into()), send_test_prompt: false }).await.unwrap();
    assert!(!no_prompt.sent_test_prompt);
    assert_eq!(no_prompt.model_available, Some(true));
    assert_eq!(no_prompt.capabilities.context_limit, Some(32768));
    assert_eq!(server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "POST").count(), 0);

    let rep = p.probe_model(&ProbeRequest { model_id: Some("m1".into()), send_test_prompt: true }).await.unwrap();
    assert!(rep.sent_test_prompt);
    assert!(rep.capabilities.streaming);
    // The SSE mock answers the structured check too (content is valid segments JSON).
    assert!(rep.capabilities.structured_output);
    // The test prompt is the fixed phrase, never document text.
    for r in server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "POST") {
        let b: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(b["messages"][1]["content"], PROBE_TEST_PHRASE);
        assert!(b.get("tools").is_none());
    }
}

#[tokio::test]
async fn models_unsupported_and_auth_failure_in_probe() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
    let p = openai(&format!("{}/v1", server.uri()));
    assert!(!p.probe().await.unwrap().model_listing);
    let rep = p.probe_model(&ProbeRequest { model_id: Some("m1".into()), send_test_prompt: false }).await.unwrap();
    assert_eq!(rep.models, None);
    assert!(rep.warnings.contains(&"model_listing_unsupported".to_string()));

    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    let p = openai(&format!("{}/v1", server.uri()));
    assert_eq!(p.probe().await, Err(ProviderError::Auth { status: 401 }));
    let rep = p.probe_model(&ProbeRequest::default()).await.unwrap();
    assert!(!rep.auth_ok);
}

#[tokio::test]
async fn server_ignoring_stream_flag_returns_json() {
    // stream:true requested, server answers application/json: parsed, not re-sent.
    let server = status_server(ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{"message":{"content":"ok"},"finish_reason":"stop"}]
    })))
    .await;
    let (res, _) = run(&openai(&format!("{}/v1", server.uri())), req(true, ResponseFormat::Text)).await;
    assert_eq!(res.unwrap().text, "ok");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

// ----- Ollama ----------------------------------------------------------------------------

fn nd(v: Value) -> String {
    format!("{v}\n")
}

#[tokio::test]
async fn ollama_ndjson_split_records_thinking_ignored() {
    let body = [
        nd(json!({"model":"m1","message":{"role":"assistant","content":"","thinking":"let me think"},"done":false})),
        nd(json!({"model":"m1","message":{"role":"assistant","content":"한국"},"done":false})),
        nd(json!({"model":"m1","message":{"role":"assistant","content":"어 😀"},"done":false})),
        nd(json!({"model":"m1","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":9,"eval_count":4})),
    ]
    .concat();
    let b = body.as_bytes();
    let cut1 = body.find("한").unwrap() + 2; // inside a Hangul syllable
    let cut2 = body.find('😀').unwrap() + 1;
    let cut3 = body.find("\"done\":true").unwrap();
    let url = raw_server(RawResponse {
        content_type: "application/x-ndjson",
        parts: split_at(b, &[10, cut1, cut2, cut3]),
        complete: true,
        hold_ms: 0,
    })
    .await;
    let (res, deltas) = run(&ollama(&url, None), req(true, ResponseFormat::Text)).await;
    let out = res.unwrap();
    assert_eq!(out.text, "한국어 😀");
    assert_eq!(deltas, "한국어 😀");
    assert!(!out.text.contains("think"));
    assert_eq!(out.usage, Some(Usage { prompt_tokens: Some(9), completion_tokens: Some(4) }));
}

#[tokio::test]
async fn ollama_incomplete_streams_are_errors() {
    // done:false then EOF
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            nd(json!({"message":{"content":"부분"},"done":false})),
            "application/x-ndjson",
        ))
        .mount(&server)
        .await;
    let (res, _) = run(&ollama(&server.uri(), None), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::Incomplete { finish_reason: "no_terminal_frame".into() }));

    // done_reason length
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            [nd(json!({"message":{"content":"a"},"done":false})), nd(json!({"done":true,"done_reason":"length"}))].concat(),
            "application/x-ndjson",
        ))
        .mount(&server)
        .await;
    let (res, _) = run(&ollama(&server.uri(), None), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::Incomplete { finish_reason: "length".into() }));

    // mid-stream error object
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(nd(json!({"error":"model crashed"})), "application/x-ndjson"))
        .mount(&server)
        .await;
    let (res, _) = run(&ollama(&server.uri(), None), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::Incomplete { finish_reason: "error".into() }));

    // malformed record
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{\"message\":\n", "application/x-ndjson"))
        .mount(&server)
        .await;
    let (res, _) = run(&ollama(&server.uri(), None), req(true, ResponseFormat::Text)).await;
    assert!(matches!(res, Err(ProviderError::InvalidOutput(_))));

    // record > 256 KiB without newline
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("x".repeat(MAX_RECORD_BYTES + 1), "application/x-ndjson"))
        .mount(&server)
        .await;
    let (res, _) = run(&ollama(&server.uri(), None), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::RecordTooLarge));
}

#[tokio::test]
async fn ollama_stopped_and_model_missing() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let p = ollama(&format!("http://127.0.0.1:{port}"), None);
    let (res, _) = run(&p, req(true, ResponseFormat::Text)).await;
    assert!(never_connected(&res.unwrap_err()));
    assert!(never_connected(&p.list_models().await.unwrap_err()));
    assert_eq!(ProviderError::ConnectFailed.to_app_error().code, crate::error::ErrorCode::ProviderUnavailable);

    let server = status_server(ResponseTemplate::new(404).set_body_string("{\"error\":\"model 'm1' not found\"}")).await;
    let (res, _) = run(&ollama(&server.uri(), None), req(true, ResponseFormat::Text)).await;
    assert_eq!(res, Err(ProviderError::ModelNotFound));
}

#[tokio::test]
async fn ollama_payload_options_and_think_flag() {
    let caps = ProviderCapabilities::default_for(ProviderKind::Ollama);
    let r = req(true, ResponseFormat::JsonSegments);
    let p = request_payload(ProviderKind::Ollama, &caps, &r);
    assert_eq!(p["options"], json!({"temperature": 0.2, "seed": 7, "num_predict": 512}));
    assert!(p.get("think").is_none(), "think only for verified models");
    assert!(p.get("format").is_none(), "no schema without structured capability");
    let caps2 = ProviderCapabilities { thinking_toggle: true, structured_output: true, ..caps };
    let p = request_payload(ProviderKind::Ollama, &caps2, &r);
    assert_eq!(p["think"], false);
    assert_eq!(p["format"]["properties"]["segments"]["items"]["properties"]["id"]["enum"], json!(["s0"]));

    // Non-stream path sends stream:false and the exact payload.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "message": {"role":"assistant","content":"완료","thinking":"hidden"}, "done": true, "done_reason": "stop"
        })))
        .mount(&server)
        .await;
    let prov = ollama(&server.uri(), Some(caps2.clone()));
    let r = req(false, ResponseFormat::Text);
    let (res, _) = run(&prov, r.clone()).await;
    assert_eq!(res.unwrap().text, "완료");
    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent, request_payload(ProviderKind::Ollama, &caps2, &r));
    assert_eq!(sent["stream"], false);
}

#[tokio::test]
async fn ollama_tags_show_probe() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"name": "m1:latest", "model": "m1:latest", "digest": "abc123", "size": 1}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model_info": {"qwen3.context_length": 40960}, "capabilities": ["completion", "thinking"]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "message": {"content": "{\"segments\":[{\"id\":\"s0\",\"text\":\"안녕하세요.\"}]}"}, "done": true, "done_reason": "stop"
        })))
        .mount(&server)
        .await;
    let p = ollama(&server.uri(), None);
    let models = p.list_models().await.unwrap();
    assert_eq!(models, vec![ModelInfo { id: "m1:latest".into(), digest: Some("abc123".into()), context_length: None }]);
    let caps = p.probe().await.unwrap();
    assert!(caps.streaming && caps.model_listing);

    let rep = p.probe_model(&ProbeRequest { model_id: Some("m1".into()), send_test_prompt: false }).await.unwrap();
    assert_eq!(rep.model_available, Some(true));
    assert_eq!(rep.capabilities.context_limit, Some(40960));
    assert!(rep.capabilities.thinking_toggle);
    assert!(!rep.capabilities.structured_output);
    assert!(!rep.sent_test_prompt);
    assert_eq!(rep.model.unwrap().digest.as_deref(), Some("abc123"));

    let rep = p.probe_model(&ProbeRequest { model_id: Some("m1".into()), send_test_prompt: true }).await.unwrap();
    assert!(rep.sent_test_prompt);
    assert!(rep.capabilities.structured_output);
    let chat = server.received_requests().await.unwrap().into_iter().find(|r| r.url.path() == "/api/chat").unwrap();
    let b: Value = serde_json::from_slice(&chat.body).unwrap();
    assert_eq!(b["think"], false);
    assert_eq!(b["messages"][1]["content"], PROBE_TEST_PHRASE);

    let rep = p.probe_model(&ProbeRequest { model_id: Some("missing".into()), send_test_prompt: true }).await.unwrap();
    assert_eq!(rep.model_available, Some(false));
}

#[tokio::test]
async fn build_provider_validates_endpoint() {
    let store: Arc<dyn crate::security::keychain::SecretStore> = Arc::new(MemorySecretStore::new());
    let bad = cfg(ProviderKind::OpenaiCompatible, "http://api.example.com/v1", false, None);
    assert!(build_provider(&bad, store.clone()).is_err());
    let ok = cfg(ProviderKind::Ollama, ollama::DEFAULT_ENDPOINT, false, None);
    assert!(build_provider(&ok, store.clone()).is_ok());
    let mut c = ok.clone();
    c.concurrency = 5;
    assert!(build_provider(&c, store).is_err());
}
