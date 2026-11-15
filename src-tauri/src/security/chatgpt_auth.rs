//! Sign in with ChatGPT for open-source clients (ChatGPT plan usage).
//!
//! Follows OpenAI's "Sign in with ChatGPT → ChatGPT plan usage" flow for locally run apps:
//!
//! * first sign-in registers a client (`client_id=dynamic_agent_client`) and returns the
//!   issued client id in the loopback callback; later sign-ins reuse the issued id;
//! * authorization code + PKCE (S256), fresh `state` and `nonce` per attempt, no client secret;
//! * the ID token is verified against OpenAI's JWKS (RS256), issuer, audience (the issued
//!   client id), expiry and nonce before anything is stored;
//! * plan usage is enabled only when the granted scopes contain `chatgpt.tokens.use.direct`;
//! * credentials live in the macOS Keychain (one record per provider), never in the database,
//!   logs or URLs; refreshes are serialized because the refresh token rotates.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult, ErrorCode};
use crate::security::keychain::SecretStore;

pub const AGENT_NAME: &str = "Thoth Reader";
pub const RESOURCE: &str = "https://api.openai.com/v1";
pub const SCOPES: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
pub const REGISTRATION_CLIENT: &str = "dynamic_agent_client";
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const USAGE_SETTINGS_URL: &str = "https://chatgpt.com/settings/usage";
/// Preferred loopback port; any free port is used when it is taken (only the port may vary).
const PREFERRED_PORT: u16 = 1455;
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh this long before the access token expires.
const REFRESH_MARGIN_SECS: u64 = 300;

/// Where the flow talks to. Production values by default; tests point it at a local server.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub issuer: String,
    pub authorize: String,
    pub token: String,
    pub discovery: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            issuer: "https://auth.openai.com".into(),
            authorize: "https://auth.openai.com/api/accounts/authorize".into(),
            token: "https://auth.openai.com/api/accounts/oauth/token".into(),
            discovery: "https://auth.openai.com/.well-known/openid-configuration".into(),
        }
    }
}

/// One registration: the issued client, the verified identity and its tokens.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    pub email: Option<String>,
    pub issuer: String,
    pub subject: String,
    pub client_id: String,
    pub ext_agent_host_id: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_in: u64,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Unix seconds when the token response was received.
    #[serde(default)]
    pub saved_at: u64,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // never print tokens
        f.debug_struct("Credential").field("client_id", &self.client_id).field("signed_in", &self.signed_in()).finish_non_exhaustive()
    }
}

impl Credential {
    pub fn signed_in(&self) -> bool {
        !self.access_token.is_empty() && !self.refresh_token.is_empty()
    }
    pub fn plan_enabled(&self) -> bool {
        self.signed_in() && self.scopes.iter().any(|s| s == PLAN_SCOPE)
    }
    fn needs_refresh(&self, now: u64) -> bool {
        now + REFRESH_MARGIN_SECS >= self.saved_at.saturating_add(self.expires_in)
    }
    /// Signed out: tokens gone, the account/client mapping and host id stay for the next sign-in.
    pub fn cleared(&self) -> Self {
        Self { id_token: String::new(), access_token: String::new(), refresh_token: String::new(), expires_in: 0, scopes: vec![], ..self.clone() }
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn load(secrets: &dyn SecretStore, keychain_ref: &str) -> AppResult<Option<Credential>> {
    match secrets.read(keychain_ref)? {
        Some(s) => Ok(serde_json::from_str(s.expose()).ok()),
        None => Ok(None),
    }
}

pub fn save(secrets: &dyn SecretStore, keychain_ref: &str, c: &Credential) -> AppResult<()> {
    let json = serde_json::to_string(c).map_err(|_| AppError::internal("자격 증명을 저장할 수 없습니다."))?;
    secrets.store(keychain_ref, &json)
}

// ------------------------------------------------------------------ authorization request

fn random_token(bytes: usize) -> String {
    use rand::RngCore;
    let mut b = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A new opaque host id (`urn:uuid:…`); persisted by the caller before the first sign-in.
pub fn new_host_id() -> String {
    format!("urn:uuid:{}", uuid::Uuid::new_v4())
}

pub struct Attempt {
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    pub redirect_uri: String,
    /// Issued client id when signing in again; `None` for a first registration.
    pub saved_client: Option<String>,
}

/// The authorization URL for this attempt. `saved` selects reauthorization of that account.
pub fn authorize_url(ep: &Endpoints, a: &Attempt, host_id: &str, saved: Option<&Credential>) -> AppResult<url::Url> {
    let mut u = url::Url::parse(&ep.authorize).map_err(|_| AppError::internal("인증 주소가 올바르지 않습니다."))?;
    {
        let mut q = u.query_pairs_mut();
        match &a.saved_client {
            Some(id) => {
                q.append_pair("client_id", id);
            }
            None => {
                q.append_pair("client_id", REGISTRATION_CLIENT);
                q.append_pair("agent_name_hint", AGENT_NAME);
            }
        }
        q.append_pair("ext_agent_host_id", host_id);
        if let Some(c) = saved.filter(|_| a.saved_client.is_some()) {
            if !c.id_token.is_empty() {
                q.append_pair("id_token_hint", &c.id_token);
            }
            if let Some(e) = c.email.as_deref().filter(|e| !e.is_empty()) {
                q.append_pair("login_hint", e);
            }
        }
        q.append_pair("response_type", "code");
        q.append_pair("redirect_uri", &a.redirect_uri);
        q.append_pair("scope", SCOPES);
        q.append_pair("resource", RESOURCE);
        q.append_pair("state", &a.state);
        q.append_pair("nonce", &a.nonce);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("code_challenge", &pkce_challenge(&a.verifier));
    }
    Ok(u)
}

// ------------------------------------------------------------------ loopback callback

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Callback {
    pub code: Option<String>,
    pub state: Option<String>,
    pub client_id: Option<String>,
    pub error: Option<String>,
}

/// Parse the request line of the browser's redirect ("GET /auth/callback?… HTTP/1.1").
pub fn parse_callback(request_line: &str) -> Option<Callback> {
    let mut parts = request_line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?;
    let u = url::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if u.path() != CALLBACK_PATH {
        return None;
    }
    let mut cb = Callback::default();
    for (k, v) in u.query_pairs() {
        match k.as_ref() {
            "code" => cb.code = Some(v.into_owned()),
            "state" => cb.state = Some(v.into_owned()),
            "client_id" => cb.client_id = Some(v.into_owned()),
            "error" => cb.error = Some(v.into_owned()),
            _ => {}
        }
    }
    Some(cb)
}

const DONE_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>Thoth Reader</title><body style=\"font:16px -apple-system,sans-serif;padding:48px;text-align:center\"><h2>Thoth Reader</h2><p>로그인 처리가 끝났습니다. 이 창을 닫고 앱으로 돌아가 주세요.</p><p>You can close this window and return to the app.</p></body>";

async fn bind_listener() -> AppResult<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(("127.0.0.1", PREFERRED_PORT)).await {
        Ok(l) => Ok(l),
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.map_err(|_| AppError::internal("로그인 응답을 받을 포트를 열 수 없습니다.")),
    }
}

/// Wait for the browser to come back to the loopback callback. Other requests (favicon,
/// probes) are answered with 404 and ignored.
async fn wait_callback(listener: tokio::net::TcpListener, cancel: &CancellationToken) -> AppResult<Callback> {
    let deadline = tokio::time::Instant::now() + SIGN_IN_TIMEOUT;
    loop {
        let (mut sock, _) = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AppError::new(ErrorCode::Cancelled, "로그인을 취소했습니다.")),
            _ = tokio::time::sleep_until(deadline) => return Err(AppError::new(ErrorCode::ProviderUnavailable, "로그인 대기 시간이 지났습니다. 다시 시도해 주세요.")),
            r = listener.accept() => r.map_err(|_| AppError::internal("로그인 응답을 받지 못했습니다."))?,
        };
        let mut buf = vec![0u8; 16 * 1024];
        let mut n = 0;
        // read the request head (bounded, short timeout)
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while n < buf.len() {
                match sock.read(&mut buf[n..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(k) => {
                        n += k;
                        if buf[..n].windows(2).any(|w| w == b"\r\n") {
                            break;
                        }
                    }
                }
            }
        })
        .await;
        let head = String::from_utf8_lossy(&buf[..n]);
        let line = head.lines().next().unwrap_or("");
        match parse_callback(line) {
            Some(cb) => {
                let body = DONE_PAGE.as_bytes();
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.shutdown().await;
                return Ok(cb);
            }
            None => {
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                let _ = sock.shutdown().await;
            }
        }
    }
}

// ------------------------------------------------------------------ token endpoint

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TokenError {
    /// The grant / refresh token can no longer be used: sign in again.
    Unusable,
    /// The client registration is wrong.
    InvalidClient,
    /// Network or server trouble: keep the credentials and try later.
    Transient,
}

fn classify_token_error(status: u16, body: &str) -> TokenError {
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").map(|e| e.as_str().map(str::to_string).or_else(|| e.get("code").and_then(|c| c.as_str()).map(str::to_string))))
        .flatten()
        .unwrap_or_default();
    match code.as_str() {
        "invalid_grant" | "invalid_refresh_token" | "token_expired" | "refresh_token_expired" | "refresh_token_invalidated" | "refresh_token_reused" => TokenError::Unusable,
        "invalid_client" => TokenError::InvalidClient,
        _ if status >= 500 || status == 429 => TokenError::Transient,
        _ if status == 400 || status == 401 => TokenError::Unusable,
        _ => TokenError::Transient,
    }
}

fn http_client() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| AppError::internal("HTTP 클라이언트를 만들 수 없습니다."))
}

async fn post_form(http: &reqwest::Client, url: &str, form: &[(&str, &str)]) -> Result<(u16, String), TokenError> {
    let body: String = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form.iter().copied()).finish();
    let resp = http
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|_| TokenError::Transient)?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|_| TokenError::Transient)?;
    Ok((status, text))
}

// ------------------------------------------------------------------ ID token

#[derive(Deserialize)]
struct IdClaims {
    iss: String,
    sub: String,
    exp: u64,
    nonce: Option<String>,
    email: Option<String>,
    aud: serde_json::Value,
}

#[derive(Deserialize)]
struct Jwk {
    kid: Option<String>,
    kty: String,
    n: Option<String>,
    e: Option<String>,
}

fn b64(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

/// Verify an RS256 ID token against `jwks` and the expected issuer / audience / nonce.
pub fn verify_id_token(token: &str, jwks: &serde_json::Value, issuer: &str, audience: &str, nonce: Option<&str>, now: u64) -> AppResult<(String, Option<String>)> {
    let bad = || AppError::new(ErrorCode::AuthFailed, "로그인 응답을 확인할 수 없습니다. 다시 시도해 주세요.");
    let mut parts = token.split('.');
    let (h, p, s) = (parts.next().ok_or_else(bad)?, parts.next().ok_or_else(bad)?, parts.next().ok_or_else(bad)?);
    if parts.next().is_some() {
        return Err(bad());
    }
    let header: serde_json::Value = serde_json::from_slice(&b64(h).ok_or_else(bad)?).map_err(|_| bad())?;
    if header.get("alg").and_then(|a| a.as_str()) != Some("RS256") {
        return Err(bad());
    }
    let kid = header.get("kid").and_then(|k| k.as_str());
    let keys: Vec<Jwk> = serde_json::from_value(jwks.get("keys").cloned().unwrap_or_default()).map_err(|_| bad())?;
    let sig = b64(s).ok_or_else(bad)?;
    let signed = format!("{h}.{p}");
    let verified = keys.iter().filter(|k| k.kty == "RSA" && (kid.is_none() || k.kid.as_deref() == kid)).any(|k| {
        let (Some(n), Some(e)) = (k.n.as_deref().and_then(b64), k.e.as_deref().and_then(b64)) else { return false };
        ring::signature::RsaPublicKeyComponents { n, e }.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &sig).is_ok()
    });
    if !verified {
        return Err(bad());
    }
    let c: IdClaims = serde_json::from_slice(&b64(p).ok_or_else(bad)?).map_err(|_| bad())?;
    let aud_ok = match &c.aud {
        serde_json::Value::String(a) => a == audience,
        serde_json::Value::Array(a) => a.iter().any(|v| v.as_str() == Some(audience)),
        _ => false,
    };
    if c.iss != issuer || !aud_ok || c.exp <= now || c.sub.is_empty() {
        return Err(bad());
    }
    if let Some(n) = nonce {
        if c.nonce.as_deref() != Some(n) {
            return Err(bad());
        }
    }
    Ok((c.sub, c.email))
}

async fn fetch_jwks(http: &reqwest::Client, ep: &Endpoints) -> AppResult<(serde_json::Value, Option<String>)> {
    let fail = || AppError::new(ErrorCode::ProviderUnavailable, "OpenAI 인증 서버에 연결할 수 없습니다.");
    let disc: serde_json::Value = http.get(&ep.discovery).send().await.map_err(|_| fail())?.json().await.map_err(|_| fail())?;
    let jwks_uri = disc.get("jwks_uri").and_then(|v| v.as_str()).ok_or_else(fail)?;
    let jwks: serde_json::Value = http.get(jwks_uri).send().await.map_err(|_| fail())?.json().await.map_err(|_| fail())?;
    Ok((jwks, disc.get("revocation_endpoint").and_then(|v| v.as_str()).map(str::to_string)))
}

// ------------------------------------------------------------------ flows

pub struct SignedIn {
    pub credential: Credential,
    /// True when this attempt registered a new client (first sign-in for the account here).
    pub new_registration: bool,
}

/// Run one sign-in attempt: open the browser, wait for the callback, exchange the code and
/// validate the result. `saved` reauthorizes that registration; `None` registers a new one.
pub async fn sign_in(
    ep: &Endpoints,
    host_id: &str,
    saved: Option<&Credential>,
    open_browser: impl FnOnce(&str) -> AppResult<()>,
    cancel: &CancellationToken,
) -> AppResult<SignedIn> {
    let http = http_client()?;
    let listener = bind_listener().await?;
    let port = listener.local_addr().map_err(|_| AppError::internal("로그인 응답 포트를 확인할 수 없습니다."))?.port();
    let attempt = Attempt {
        state: random_token(32),
        nonce: random_token(32),
        verifier: random_token(48),
        redirect_uri: format!("http://127.0.0.1:{port}{CALLBACK_PATH}"),
        saved_client: saved.map(|c| c.client_id.clone()).filter(|c| !c.is_empty()),
    };
    let url = authorize_url(ep, &attempt, host_id, saved)?;
    open_browser(url.as_str())?;
    let cb = wait_callback(listener, cancel).await?;
    finish_sign_in(ep, &http, &attempt, host_id, saved, cb).await
}

/// Everything after the callback (separate so it can be tested without a browser).
pub async fn finish_sign_in(ep: &Endpoints, http: &reqwest::Client, attempt: &Attempt, host_id: &str, saved: Option<&Credential>, cb: Callback) -> AppResult<SignedIn> {
    if cb.state.as_deref() != Some(attempt.state.as_str()) {
        return Err(AppError::new(ErrorCode::AuthFailed, "로그인 응답이 이번 요청과 맞지 않습니다. 다시 시도해 주세요."));
    }
    if let Some(e) = &cb.error {
        return Err(if e == "access_denied" {
            AppError::new(ErrorCode::Cancelled, "ChatGPT에서 사용 동의를 하지 않았습니다.")
        } else {
            AppError::new(ErrorCode::AuthFailed, "로그인에 실패했습니다. 다시 시도해 주세요.")
        });
    }
    let code = cb.code.ok_or_else(|| AppError::new(ErrorCode::AuthFailed, "로그인 응답에 인증 코드가 없습니다."))?;
    let client_id = match (&attempt.saved_client, cb.client_id) {
        // reauthorization: the callback may omit the id, but must not change it
        (Some(saved_id), Some(got)) if &got != saved_id => return Err(AppError::new(ErrorCode::AuthFailed, "로그인 응답의 앱 등록 정보가 저장된 것과 다릅니다.")),
        (Some(saved_id), _) => saved_id.clone(),
        (None, Some(got)) if got != REGISTRATION_CLIENT && !got.is_empty() => got,
        (None, _) => return Err(AppError::new(ErrorCode::AuthFailed, "앱 등록이 끝나지 않았습니다. 다시 시도해 주세요.")),
    };
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id.as_str()),
        ("code", code.as_str()),
        ("code_verifier", attempt.verifier.as_str()),
        ("redirect_uri", attempt.redirect_uri.as_str()),
        ("resource", RESOURCE),
    ];
    let (status, body) = post_form(http, &ep.token, &form).await.map_err(|_| AppError::new(ErrorCode::ProviderUnavailable, "OpenAI 인증 서버에 연결할 수 없습니다."))?;
    if status != 200 {
        return Err(match classify_token_error(status, &body) {
            TokenError::Transient => AppError::new(ErrorCode::ProviderUnavailable, "OpenAI 인증 서버가 응답하지 않습니다. 잠시 후 다시 시도해 주세요."),
            _ => AppError::new(ErrorCode::AuthFailed, "로그인을 완료하지 못했습니다. 다시 시도해 주세요."),
        });
    }
    let tr: TokenResponse = serde_json::from_str(&body).map_err(|_| AppError::new(ErrorCode::AuthFailed, "로그인 응답 형식이 올바르지 않습니다."))?;
    let id_token = tr.id_token.ok_or_else(|| AppError::new(ErrorCode::AuthFailed, "로그인 응답에 신원 정보가 없습니다."))?;
    let (jwks, _) = fetch_jwks(http, ep).await?;
    let (subject, email) = verify_id_token(&id_token, &jwks, &ep.issuer, &client_id, Some(&attempt.nonce), now_secs())?;
    if let Some(prev) = saved.filter(|p| !p.subject.is_empty()) {
        if prev.subject != subject {
            return Err(AppError::new(ErrorCode::AuthFailed, "다른 ChatGPT 계정으로 로그인했습니다. 같은 계정으로 다시 로그인하거나 새 계정으로 추가해 주세요."));
        }
    }
    let credential = Credential {
        email,
        issuer: ep.issuer.clone(),
        subject,
        client_id,
        ext_agent_host_id: host_id.to_string(),
        id_token,
        access_token: tr.access_token,
        refresh_token: tr.refresh_token.unwrap_or_default(),
        expires_in: tr.expires_in.unwrap_or(3600),
        scopes: tr.scope.unwrap_or_default().split_whitespace().map(str::to_string).collect(),
        saved_at: now_secs(),
    };
    Ok(SignedIn { credential, new_registration: attempt.saved_client.is_none() })
}

/// Exchange the refresh token. The replacement tokens are returned together.
pub async fn refresh(ep: &Endpoints, http: &reqwest::Client, c: &Credential) -> Result<Credential, TokenError> {
    let form = [("grant_type", "refresh_token"), ("client_id", c.client_id.as_str()), ("refresh_token", c.refresh_token.as_str()), ("resource", RESOURCE)];
    let (status, body) = post_form(http, &ep.token, &form).await?;
    if status != 200 {
        return Err(classify_token_error(status, &body));
    }
    let tr: TokenResponse = serde_json::from_str(&body).map_err(|_| TokenError::Transient)?;
    let mut next = c.clone();
    next.access_token = tr.access_token;
    if let Some(r) = tr.refresh_token.filter(|r| !r.is_empty()) {
        next.refresh_token = r;
    }
    if let Some(i) = tr.id_token.filter(|i| !i.is_empty()) {
        next.id_token = i;
    }
    next.expires_in = tr.expires_in.unwrap_or(3600);
    if let Some(s) = tr.scope {
        next.scopes = s.split_whitespace().map(str::to_string).collect();
    }
    next.saved_at = now_secs();
    Ok(next)
}

/// End the renewable session. Returns false when the revocation could not be confirmed.
pub async fn revoke(ep: &Endpoints, c: &Credential) -> bool {
    if c.refresh_token.is_empty() {
        return true;
    }
    let Ok(http) = http_client() else { return false };
    let Ok((_, Some(endpoint))) = fetch_jwks(&http, ep).await else { return false };
    let form = [("token", c.refresh_token.as_str()), ("token_type_hint", "refresh_token"), ("client_id", c.client_id.as_str())];
    for delay in [0u64, 1, 3] {
        if delay > 0 {
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
        match post_form(&http, &endpoint, &form).await {
            Ok((200, _)) => return true,
            Ok((s, _)) if s < 500 => return false,
            _ => {}
        }
    }
    false
}

// ------------------------------------------------------------------ access token for requests

#[derive(Debug, PartialEq, Eq)]
pub enum AccessError {
    /// Not signed in, or the session can no longer be renewed.
    SignInRequired,
    /// Signed in, but plan usage was not granted.
    PlanNotEnabled,
    KeychainUnavailable,
    /// Could not reach the token endpoint; credentials are kept.
    Transient,
}

fn refresh_locks() -> &'static Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A valid access token for the registration stored under `keychain_ref`, refreshed when it
/// is about to expire. Refreshes for one registration run one at a time.
pub async fn access_token(ep: &Endpoints, secrets: Arc<dyn SecretStore>, keychain_ref: &str) -> Result<String, AccessError> {
    let lock = refresh_locks().lock().unwrap().entry(keychain_ref.to_string()).or_default().clone();
    let _guard = lock.lock().await;
    let (s2, r2) = (secrets.clone(), keychain_ref.to_string());
    let cred = tokio::task::spawn_blocking(move || load(s2.as_ref(), &r2)).await.map_err(|_| AccessError::KeychainUnavailable)?.map_err(|_| AccessError::KeychainUnavailable)?;
    let Some(cred) = cred.filter(Credential::signed_in) else { return Err(AccessError::SignInRequired) };
    if !cred.plan_enabled() {
        return Err(AccessError::PlanNotEnabled);
    }
    if !cred.needs_refresh(now_secs()) {
        return Ok(cred.access_token);
    }
    let http = http_client().map_err(|_| AccessError::Transient)?;
    match refresh(ep, &http, &cred).await {
        Ok(next) => {
            let token = next.access_token.clone();
            let (s2, r2) = (secrets.clone(), keychain_ref.to_string());
            tokio::task::spawn_blocking(move || save(s2.as_ref(), &r2, &next)).await.map_err(|_| AccessError::KeychainUnavailable)?.map_err(|_| AccessError::KeychainUnavailable)?;
            Ok(token)
        }
        Err(TokenError::Unusable) | Err(TokenError::InvalidClient) => {
            // clear the unusable tokens; the registration stays for the next sign-in
            let cleared = cred.cleared();
            let (s2, r2) = (secrets.clone(), keychain_ref.to_string());
            let _ = tokio::task::spawn_blocking(move || save(s2.as_ref(), &r2, &cleared)).await;
            Err(AccessError::SignInRequired)
        }
        // still usable for a moment? only if it has not actually expired
        Err(TokenError::Transient) if now_secs() < cred.saved_at.saturating_add(cred.expires_in) => Ok(cred.access_token),
        Err(TokenError::Transient) => Err(AccessError::Transient),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_example() {
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    fn attempt(saved: Option<&str>) -> Attempt {
        Attempt { state: "st".into(), nonce: "no".into(), verifier: "ver".into(), redirect_uri: "http://127.0.0.1:1455/auth/callback".into(), saved_client: saved.map(str::to_string) }
    }

    #[test]
    fn first_sign_in_registers_and_later_ones_reuse_the_issued_client() {
        let ep = Endpoints::default();
        let u = authorize_url(&ep, &attempt(None), "urn:uuid:h", None).unwrap();
        let q: HashMap<String, String> = u.query_pairs().into_owned().collect();
        assert_eq!(u.as_str().split('?').next().unwrap(), "https://auth.openai.com/api/accounts/authorize");
        assert_eq!(q["client_id"], "dynamic_agent_client");
        assert_eq!(q["agent_name_hint"], "Thoth Reader");
        assert_eq!(q["ext_agent_host_id"], "urn:uuid:h");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
        assert_eq!(q["resource"], "https://api.openai.com/v1");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], pkce_challenge("ver"));
        for s in ["openid", "profile", "email", "offline_access", "resource.invoke", "chatgpt.tokens.use.direct"] {
            assert!(q["scope"].split(' ').any(|x| x == s), "{s}");
        }
        assert!(!q.contains_key("id_token_hint"));

        let saved = Credential { email: Some("a@example.com".into()), issuer: ep.issuer.clone(), subject: "sub".into(), client_id: "oaiapp_1".into(), ext_agent_host_id: "urn:uuid:h".into(), id_token: "idt".into(), access_token: String::new(), refresh_token: String::new(), expires_in: 0, scopes: vec![], saved_at: 0 };
        let u = authorize_url(&ep, &attempt(Some("oaiapp_1")), "urn:uuid:h", Some(&saved)).unwrap();
        let q: HashMap<String, String> = u.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "oaiapp_1");
        assert!(!q.contains_key("agent_name_hint"), "the name hint is for registration only");
        assert_eq!(q["id_token_hint"], "idt");
        assert_eq!(q["login_hint"], "a@example.com");
    }

    #[test]
    fn callback_parsing() {
        let cb = parse_callback("GET /auth/callback?code=abc&scope=openid+email&state=st&client_id=oaiapp_9 HTTP/1.1").unwrap();
        assert_eq!(cb, Callback { code: Some("abc".into()), state: Some("st".into()), client_id: Some("oaiapp_9".into()), error: None });
        assert_eq!(parse_callback("GET /auth/callback?error=access_denied&state=st HTTP/1.1").unwrap().error.as_deref(), Some("access_denied"));
        assert!(parse_callback("GET /callback?code=abc HTTP/1.1").is_none(), "/callback is not /auth/callback");
        assert!(parse_callback("GET /favicon.ico HTTP/1.1").is_none());
        assert!(parse_callback("POST /auth/callback?code=abc HTTP/1.1").is_none());
    }

    #[test]
    fn token_errors_are_classified() {
        assert_eq!(classify_token_error(400, r#"{"error":"invalid_grant"}"#), TokenError::Unusable);
        assert_eq!(classify_token_error(400, r#"{"error":{"code":"refresh_token_reused"}}"#), TokenError::Unusable);
        assert_eq!(classify_token_error(401, r#"{"error":"invalid_client"}"#), TokenError::InvalidClient);
        assert_eq!(classify_token_error(503, "oops"), TokenError::Transient);
        assert_eq!(classify_token_error(429, "{}"), TokenError::Transient);
    }

    #[test]
    fn credential_state() {
        let mut c = Credential { email: None, issuer: "i".into(), subject: "s".into(), client_id: "c".into(), ext_agent_host_id: "h".into(), id_token: "t".into(), access_token: "a".into(), refresh_token: "r".into(), expires_in: 3600, scopes: vec!["openid".into()], saved_at: 1000 };
        assert!(c.signed_in() && !c.plan_enabled(), "a valid sign-in alone does not allow plan usage");
        c.scopes.push(PLAN_SCOPE.into());
        assert!(c.plan_enabled());
        assert!(!c.needs_refresh(1000) && c.needs_refresh(1000 + 3600 - 299) && c.needs_refresh(9999));
        let out = c.cleared();
        assert!(!out.signed_in() && out.id_token.is_empty() && out.client_id == "c" && out.subject == "s" && out.ext_agent_host_id == "h");
        assert!(!format!("{c:?}").contains("\"a\""), "tokens are never printed");
    }

    #[test]
    fn id_token_rejects_tampering_and_wrong_claims() {
        let jwks = serde_json::json!({"keys": []});
        assert!(verify_id_token("a.b", &jwks, "iss", "aud", None, 0).is_err());
        // unsigned / wrong algorithm
        let h = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let p = URL_SAFE_NO_PAD.encode(br#"{"iss":"iss","sub":"s","aud":"aud","exp":9999999999}"#);
        assert!(verify_id_token(&format!("{h}.{p}."), &jwks, "iss", "aud", None, 0).is_err());
        // RS256 claimed but no key verifies it
        let h = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","kid":"k"}"#);
        assert!(verify_id_token(&format!("{h}.{p}.{}", URL_SAFE_NO_PAD.encode(b"sig")), &jwks, "iss", "aud", None, 0).is_err());
    }

    #[test]
    fn host_id_is_an_opaque_uuid_urn() {
        let h = new_host_id();
        assert!(h.starts_with("urn:uuid:") && uuid::Uuid::parse_str(&h[9..]).is_ok());
        assert_ne!(h, new_host_id());
    }
}
