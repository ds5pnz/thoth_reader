//! Sign in with ChatGPT end to end against a local stand-in for OpenAI's servers:
//! code exchange, ID-token verification, token refresh, and plan-usage Responses requests.

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::chatgpt::ChatGptProvider;
use super::*;
use crate::security::chatgpt_auth::{self, AccessError, Attempt, Callback, Credential, Endpoints, PLAN_SCOPE};
use crate::security::keychain::MemorySecretStore;

/// Throwaway RSA key generated for these tests only (PKCS#8 DER, base64). It signs the fake
/// ID tokens below and protects nothing.
const TEST_ONLY_SIGNING_KEY: &str = "MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCseX3prDMVz3lYQr/WNQjdhD6Dg7xaOjmjQ1l5mvaFhsHCq4ESXqR2e3lyyiJCwnbjEj9SPnTYj9UX1iyqNlut247SvQ/r7pU7pfJRVkJpaTUeILUtAI+yAZpnaHMopiFVp7hL0V5cMsi6wFIkhraWWcyIEQheUYPFm4Wt6bKpQvP6gxRe8RlRU0p+9oCyLUUBdwc1Kg3bmN/97pTO8+HG8b4M+XAgT0TAw7XmH6Qntd2iJUEgi8ggiB6uz40R7jbk3/bLQWxbrp1DkzzyDTq4CPew62vu6LaHNXeoEN3tE8SS144IkkZJpzu0oBrpM0yXcM82QFZP3Vy3WdCj59J/AgMBAAECggEADznpMBWKq6nq+CwUYXsXHZniJvFx/F6C3h3rqpJP9yPEEUUwxN1kFC2nCseQ1Lye4EGpI2qYyCbn1LiMrY4qSSPxQjt8n1iAW4lf5hGrv+s7duhuS4XEKckochPvhAQpPIrs+IyIPF14s7vsxIVmsx6ZsYgcvq7gPN6+ThyBMEivyY027Xo07Rc/s1q/j3rrq0Knoj38BAhvFhqisev6ItP8sEbfR5VM6kUaMU0KcHdohJ5JjSrEFXs5qmmxhgV9qI5XHONSYyQZfu8NYEcrr24ZnifAYd7frEPfjx42FQ6nvvyNOuJw6RxmyeuLbDXyDzILw5QmKyV9tV7Z9W6OyQKBgQDvdfqzlGekIG7VVh4NGgCUXW+DJ+/KcWyYYEsWotxiZwujVPRoVDdI3UkYg+7cMM7zLDziLlN9Q3TzThMwOBfENE5pugLqfhhutO6cCxkglPSNSoAfKlZ7XWOaNqsUhF5BlHXhT+adch2R8IfpOGqVdlkO8XHhBJEZABcCQ+CVjQKBgQC4YxixBm1UAdIAQtQLmmy1jxYImzggyHk4cdTWZDJ00qf1VTwjUEsrtZvHnzJ9Hq/jP0GUpGtMZrqu4c3QTgA7SkEQrZwMcTunO5ifsWAYuVpeUvLhFjI0iO1hPcGmZcVU2ElFJhB6iO0PzPz7Sr+PGA/J0DQ5LbJg/7pcy2iHOwKBgGeAgC80uyNm3WEayvxwu8VlKXjwPyYqpnyqSmprVIS384DSjJ23oPm+pASTqh/EfP8ApQRCb9XZ3Wf43DRgWm11CluKig4fGKJpVCVgaqj5mUak6AIQg7bo489ZR9Mjb7Y63ETaC10MYRmedLBqsS8PK2lZFRbkqW676jYRbrZVAoGAIyHGh5xI4nPCVohlLJzMz78mnJcul5h6+FVAD2Vh875bgDQwVC4tVHOy7dMYZJyfOLZJwY+eXh1Ivp+JfoIOKz3XrdzWWcyNaCzDxNGgFLXkUQqO+nInOLlKOXfx2MmVUrgvJby98Qr72EHfheXVH+eewn4oJSjezvqKwLmE/hcCgYBCeVqc35rhT2kC6mrAy2hl92T7twjEJfwF6CTF1U+c/3Sed/UyPQjazem1vSi2TOAmXHpk0ufnKYb0YMsX3PPeDV9I6ktQQ5ZQ1LtyLbsvKn6DKQiqn2NRHymkUzN46RHhi62GJ9kG2VVEfpxhr7FblEh3yA9R+IRnIG/gLcYl/w==";
const KEY_REF: &str = "0192f6c4-8b7e-7cc3-9d2a-3f1e5b6a7c8e";
const ALL_SCOPES: &str = "chatgpt.tokens.use.direct email offline_access openid profile resource.invoke";

fn key() -> ring::rsa::KeyPair {
    ring::rsa::KeyPair::from_pkcs8(&STANDARD.decode(TEST_ONLY_SIGNING_KEY).unwrap()).unwrap()
}

fn jwks() -> Value {
    let comps: ring::rsa::PublicKeyComponents<Vec<u8>> = key().public().into();
    json!({"keys": [{"kty": "RSA", "kid": "k1", "alg": "RS256", "use": "sig", "n": URL_SAFE_NO_PAD.encode(comps.n), "e": URL_SAFE_NO_PAD.encode(comps.e)}]})
}

fn id_token(claims: Value) -> String {
    let h = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","kid":"k1","typ":"JWT"}"#);
    let p = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let kp = key();
    let mut sig = vec![0u8; kp.public().modulus_len()];
    kp.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), format!("{h}.{p}").as_bytes(), &mut sig).unwrap();
    format!("{h}.{p}.{}", URL_SAFE_NO_PAD.encode(sig))
}

fn endpoints(server: &MockServer) -> Endpoints {
    Endpoints { issuer: "https://auth.openai.test".into(), authorize: format!("{}/authorize", server.uri()), token: format!("{}/token", server.uri()), discovery: format!("{}/discovery", server.uri()) }
}

async fn mount_discovery(server: &MockServer) {
    Mock::given(method("GET")).and(path("/discovery")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"jwks_uri": format!("{}/jwks", server.uri()), "revocation_endpoint": format!("{}/revoke", server.uri())}))).mount(server).await;
    Mock::given(method("GET")).and(path("/jwks")).respond_with(ResponseTemplate::new(200).set_body_json(jwks())).mount(server).await;
}

fn claims(sub: &str, aud: &str, nonce: &str) -> Value {
    json!({"iss": "https://auth.openai.test", "sub": sub, "aud": aud, "exp": chatgpt_auth::now_secs() + 600, "nonce": nonce, "email": "reader@example.com"})
}

fn attempt(saved: Option<&str>) -> Attempt {
    Attempt { state: "state-1".into(), nonce: "nonce-1".into(), verifier: "verifier-1".into(), redirect_uri: "http://127.0.0.1:1455/auth/callback".into(), saved_client: saved.map(str::to_string) }
}

fn callback(client: Option<&str>) -> Callback {
    Callback { code: Some("code-1".into()), state: Some("state-1".into()), client_id: client.map(str::to_string), error: None }
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

#[test]
fn id_token_signature_and_claims_are_checked() {
    let now = chatgpt_auth::now_secs();
    let good = id_token(claims("user-1", "oaiapp_1", "n"));
    assert_eq!(chatgpt_auth::verify_id_token(&good, &jwks(), "https://auth.openai.test", "oaiapp_1", Some("n"), now).unwrap(), ("user-1".to_string(), Some("reader@example.com".to_string())));
    // wrong audience, issuer, nonce, expiry
    assert!(chatgpt_auth::verify_id_token(&good, &jwks(), "https://auth.openai.test", "oaiapp_2", Some("n"), now).is_err());
    assert!(chatgpt_auth::verify_id_token(&good, &jwks(), "https://evil.test", "oaiapp_1", Some("n"), now).is_err());
    assert!(chatgpt_auth::verify_id_token(&good, &jwks(), "https://auth.openai.test", "oaiapp_1", Some("other"), now).is_err());
    assert!(chatgpt_auth::verify_id_token(&good, &jwks(), "https://auth.openai.test", "oaiapp_1", Some("n"), now + 3600).is_err());
    // a payload swapped under a valid signature
    let parts: Vec<&str> = good.split('.').collect();
    let forged = format!("{}.{}.{}", parts[0], URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims("admin", "oaiapp_1", "n")).unwrap()), parts[2]);
    assert!(chatgpt_auth::verify_id_token(&forged, &jwks(), "https://auth.openai.test", "oaiapp_1", Some("n"), now).is_err());
    // a key set that does not hold the signing key
    assert!(chatgpt_auth::verify_id_token(&good, &json!({"keys": []}), "https://auth.openai.test", "oaiapp_1", Some("n"), now).is_err());
}

#[tokio::test]
async fn first_sign_in_registers_the_issued_client_and_stores_verified_identity() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("client_id=oaiapp_new"))
        .and(body_string_contains("code_verifier=verifier-1"))
        .and(body_string_contains("resource=https%3A%2F%2Fapi.openai.com%2Fv1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "at-1", "refresh_token": "rt-1", "id_token": id_token(claims("user-1", "oaiapp_new", "nonce-1")), "token_type": "Bearer", "expires_in": 3600, "scope": ALL_SCOPES})))
        .expect(1)
        .mount(&server)
        .await;
    let ep = endpoints(&server);
    let signed = chatgpt_auth::finish_sign_in(&ep, &http(), &attempt(None), "urn:uuid:host", None, callback(Some("oaiapp_new"))).await.unwrap();
    assert!(signed.new_registration);
    let c = signed.credential;
    assert_eq!((c.client_id.as_str(), c.subject.as_str(), c.email.as_deref()), ("oaiapp_new", "user-1", Some("reader@example.com")));
    assert_eq!(c.ext_agent_host_id, "urn:uuid:host");
    assert!(c.signed_in() && c.plan_enabled());
}

#[tokio::test]
async fn sign_in_is_refused_when_the_answer_does_not_belong_to_this_attempt() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    let ep = endpoints(&server);
    // wrong state: nothing is exchanged
    let mut cb = callback(Some("oaiapp_new"));
    cb.state = Some("someone-else".into());
    assert!(chatgpt_auth::finish_sign_in(&ep, &http(), &attempt(None), "h", None, cb).await.is_err());
    // consent declined
    let denied = Callback { code: None, state: Some("state-1".into()), client_id: None, error: Some("access_denied".into()) };
    assert_eq!(chatgpt_auth::finish_sign_in(&ep, &http(), &attempt(None), "h", None, denied).await.err().unwrap().code, crate::error::ErrorCode::Cancelled);
    // a registration that did not issue a client id
    assert!(chatgpt_auth::finish_sign_in(&ep, &http(), &attempt(None), "h", None, callback(None)).await.is_err());
    assert!(chatgpt_auth::finish_sign_in(&ep, &http(), &attempt(None), "h", None, callback(Some("dynamic_agent_client"))).await.is_err());
    // reauthorization must not switch to another client
    assert!(chatgpt_auth::finish_sign_in(&ep, &http(), &attempt(Some("oaiapp_1")), "h", None, callback(Some("oaiapp_other"))).await.is_err());
    assert!(server.received_requests().await.unwrap().iter().all(|r| r.url.path() != "/token"), "no code exchange happened");
}

#[tokio::test]
async fn signing_in_again_must_be_the_same_account_and_plan_scope_is_required() {
    let server = MockServer::start().await;
    mount_discovery(&server).await;
    // the nonce in the token does not match this attempt
    Mock::given(method("POST")).and(path("/token")).and(body_string_contains("code=bad-nonce")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "a", "refresh_token": "r", "id_token": id_token(claims("user-1", "oaiapp_1", "other-nonce")), "expires_in": 3600, "scope": ALL_SCOPES}))).mount(&server).await;
    // another account answered
    Mock::given(method("POST")).and(path("/token")).and(body_string_contains("code=other-user")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "a", "refresh_token": "r", "id_token": id_token(claims("user-2", "oaiapp_1", "nonce-1")), "expires_in": 3600, "scope": ALL_SCOPES}))).mount(&server).await;
    // signed in, but plan usage was not granted
    Mock::given(method("POST")).and(path("/token")).and(body_string_contains("code=no-plan")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "a", "refresh_token": "r", "id_token": id_token(claims("user-1", "oaiapp_1", "nonce-1")), "expires_in": 3600, "scope": "email openid profile"}))).mount(&server).await;
    let ep = endpoints(&server);
    let saved = Credential { email: None, issuer: ep.issuer.clone(), subject: "user-1".into(), client_id: "oaiapp_1".into(), ext_agent_host_id: "h".into(), id_token: String::new(), access_token: String::new(), refresh_token: String::new(), expires_in: 0, scopes: vec![], saved_at: 0 };
    let cb = |code: &str| Callback { code: Some(code.into()), state: Some("state-1".into()), client_id: None, error: None };
    assert!(chatgpt_auth::finish_sign_in(&ep, &http(), &attempt(Some("oaiapp_1")), "h", Some(&saved), cb("bad-nonce")).await.is_err());
    assert!(chatgpt_auth::finish_sign_in(&ep, &http(), &attempt(Some("oaiapp_1")), "h", Some(&saved), cb("other-user")).await.is_err());
    let no_plan = chatgpt_auth::finish_sign_in(&ep, &http(), &attempt(Some("oaiapp_1")), "h", Some(&saved), cb("no-plan")).await.unwrap();
    assert!(!no_plan.new_registration && no_plan.credential.signed_in() && !no_plan.credential.plan_enabled());
}

fn stored(store: &MemorySecretStore, saved_at: u64, scopes: &str) {
    let c = Credential {
        email: Some("reader@example.com".into()),
        issuer: "https://auth.openai.test".into(),
        subject: "user-1".into(),
        client_id: "oaiapp_1".into(),
        ext_agent_host_id: "urn:uuid:host".into(),
        id_token: "idt".into(),
        access_token: "at-old".into(),
        refresh_token: "rt-old".into(),
        expires_in: 3600,
        scopes: scopes.split_whitespace().map(str::to_string).collect(),
        saved_at,
    };
    chatgpt_auth::save(store, KEY_REF, &c).unwrap();
}

#[tokio::test]
async fn access_token_is_refreshed_near_expiry_and_the_rotated_token_is_kept() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=rt-old"))
        .and(body_string_contains("client_id=oaiapp_1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "at-new", "refresh_token": "rt-new", "expires_in": 3600, "scope": ALL_SCOPES})))
        .expect(1)
        .mount(&server)
        .await;
    let ep = endpoints(&server);
    let store = Arc::new(MemorySecretStore::new());
    // fresh token: used as is, no request
    stored(&store, chatgpt_auth::now_secs(), ALL_SCOPES);
    assert_eq!(chatgpt_auth::access_token(&ep, store.clone(), KEY_REF).await.unwrap(), "at-old");
    // expired: refreshed once, both tokens replaced together
    stored(&store, chatgpt_auth::now_secs() - 4000, ALL_SCOPES);
    assert_eq!(chatgpt_auth::access_token(&ep, store.clone(), KEY_REF).await.unwrap(), "at-new");
    let c = chatgpt_auth::load(store.as_ref(), KEY_REF).unwrap().unwrap();
    assert_eq!((c.access_token.as_str(), c.refresh_token.as_str()), ("at-new", "rt-new"));
    assert_eq!(chatgpt_auth::access_token(&ep, store.clone(), KEY_REF).await.unwrap(), "at-new");
}

#[tokio::test]
async fn unusable_refresh_token_clears_tokens_but_keeps_the_registration() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"}))).mount(&server).await;
    let ep = endpoints(&server);
    let store = Arc::new(MemorySecretStore::new());
    stored(&store, chatgpt_auth::now_secs() - 4000, ALL_SCOPES);
    assert_eq!(chatgpt_auth::access_token(&ep, store.clone(), KEY_REF).await, Err(AccessError::SignInRequired));
    let c = chatgpt_auth::load(store.as_ref(), KEY_REF).unwrap().unwrap();
    assert!(!c.signed_in() && c.client_id == "oaiapp_1" && c.subject == "user-1");
    // without the plan scope a valid sign-in still does not allow requests
    stored(&store, chatgpt_auth::now_secs(), "email openid profile");
    assert_eq!(chatgpt_auth::access_token(&ep, store.clone(), KEY_REF).await, Err(AccessError::PlanNotEnabled));
    // nothing stored
    assert_eq!(chatgpt_auth::access_token(&ep, Arc::new(MemorySecretStore::new()), KEY_REF).await, Err(AccessError::SignInRequired));
    assert!(PLAN_SCOPE.starts_with("chatgpt."));
}

fn provider(server: &MockServer, store: Arc<MemorySecretStore>) -> ChatGptProvider {
    let cfg = ProviderConfig { id: "p1".into(), kind: ProviderKind::Chatgpt, endpoint: format!("{}/v1", server.uri()), account_namespace: "chatgpt-test".into(), keychain_ref: Some(KEY_REF.into()), allow_private_http: false, concurrency: 2, capabilities: None };
    let t = Timeouts { connect: Duration::from_secs(2), first_byte: Duration::from_secs(3), idle: Duration::from_secs(2), total: Duration::from_secs(10) };
    ChatGptProvider::with_auth_endpoints(cfg, store, t, endpoints(server)).unwrap()
}

fn request() -> TranslationRequest {
    TranslationRequest {
        request_id: "req-1".into(),
        job_id: "job-1".into(),
        block_id: "blk".into(),
        part_index: 0,
        part_count: 1,
        model_id: "gpt-test".into(),
        source_language: "en".into(),
        target_language: "ko".into(),
        messages: vec![ChatMessage { role: ChatRole::System, content: "Translate to Korean.".into() }, ChatMessage { role: ChatRole::User, content: "Hello".into() }],
        options: GenerationOptions { temperature: Some(0.2), seed: None, max_output_tokens: Some(256) },
        response_format: ResponseFormat::Text,
        stream: true,
        expected_segment_ids: vec!["s0".into()],
    }
}

fn sse(events: &[Value]) -> ResponseTemplate {
    let body: String = events.iter().map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap(), e)).collect();
    ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_raw(body.into_bytes(), "text/event-stream")
}

#[tokio::test]
async fn translate_streams_a_responses_request_with_the_oauth_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer at-old"))
        .respond_with(sse(&[
            json!({"type": "response.created"}),
            json!({"type": "response.output_text.delta", "delta": "안녕"}),
            json!({"type": "response.output_text.delta", "delta": "하세요"}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 9, "output_tokens": 4}}}),
        ]))
        .expect(1)
        .mount(&server)
        .await;
    let store = Arc::new(MemorySecretStore::new());
    stored(&store, chatgpt_auth::now_secs(), ALL_SCOPES);
    let p = provider(&server, store);
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let out = p.translate(request(), tx, CancellationToken::new()).await.unwrap();
    assert_eq!(out.text, "안녕하세요");
    assert_eq!(out.usage, Some(Usage { prompt_tokens: Some(9), completion_tokens: Some(4) }));
    let mut streamed = String::new();
    while let Ok(d) = rx.try_recv() {
        streamed.push_str(&d.text);
    }
    assert_eq!(streamed, "안녕하세요");
    let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent, request_payload(ProviderKind::Chatgpt, &ProviderCapabilities::default_for(ProviderKind::Chatgpt), &request()), "body == request_payload");
    assert_eq!((sent["store"].clone(), sent["stream"].clone()), (json!(false), json!(true)));
    assert_eq!(sent["instructions"], "Translate to Korean.");
    assert!(sent.get("temperature").is_none() && sent.get("max_output_tokens").is_none());
}

#[tokio::test]
async fn usage_limit_and_sign_in_errors_stop_the_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(ResponseTemplate::new(429).set_body_json(json!({"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "limit"}}))).expect(1).mount(&server).await;
    let store = Arc::new(MemorySecretStore::new());
    stored(&store, chatgpt_auth::now_secs(), ALL_SCOPES);
    let p = provider(&server, store);
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    assert_eq!(p.translate(request(), tx, CancellationToken::new()).await, Err(ProviderError::UsageLimit));
    // not signed in: nothing is sent at all
    let server2 = MockServer::start().await;
    let p2 = provider(&server2, Arc::new(MemorySecretStore::new()));
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    assert_eq!(p2.translate(request(), tx, CancellationToken::new()).await, Err(ProviderError::SignInRequired));
    assert!(server2.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_model_without_reasoning_effort_is_asked_again_without_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_string_contains("\"reasoning\""))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {"code": "subscription_sharing_unsupported_capability", "param": "reasoning.effort", "message": "unsupported"}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(sse(&[json!({"type": "response.output_text.delta", "delta": "네"}), json!({"type": "response.completed", "response": {}})])).mount(&server).await;
    let store = Arc::new(MemorySecretStore::new());
    stored(&store, chatgpt_auth::now_secs(), ALL_SCOPES);
    let p = provider(&server, store);
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    assert_eq!(p.translate(request(), tx, CancellationToken::new()).await.unwrap().text, "네");
    // the next request starts without the field
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    assert_eq!(p.translate(request(), tx, CancellationToken::new()).await.unwrap().text, "네");
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 3);
    assert!(!String::from_utf8_lossy(&reqs[2].body).contains("\"reasoning\""));
}

#[tokio::test]
async fn models_come_from_the_account_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/v1/models")).and(header("authorization", "Bearer at-old")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [{"slug": "gpt-a", "display_name": "A", "visibility": "list"}, {"slug": "gpt-hidden", "visibility": "hide"}]}))).mount(&server).await;
    let store = Arc::new(MemorySecretStore::new());
    stored(&store, chatgpt_auth::now_secs(), ALL_SCOPES);
    let p = provider(&server, store);
    let rep = p.probe_model(&ProbeRequest { model_id: Some("gpt-a".into()), send_test_prompt: true }).await.unwrap();
    assert!(rep.auth_ok && !rep.sent_test_prompt, "no prompt is spent on a check");
    assert_eq!(rep.models.unwrap().iter().map(|m| m.id.clone()).collect::<Vec<_>>(), vec!["gpt-a"]);
    assert_eq!(rep.model_available, Some(true));
    // signed out: the check reports it instead of failing
    let p2 = provider(&server, Arc::new(MemorySecretStore::new()));
    assert!(!p2.probe_model(&ProbeRequest::default()).await.unwrap().auth_ok);
}
