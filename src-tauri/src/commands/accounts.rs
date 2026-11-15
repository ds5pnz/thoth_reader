//! ChatGPT sign-in and Codex connection commands.
//!
//! These create and maintain the two account-backed provider kinds. Tokens never cross the
//! IPC boundary: the web view only learns the e-mail address and whether plan usage is on.

use std::sync::{Arc, Mutex, OnceLock};

use tauri::State;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

use super::core::*;
use crate::domain::{new_uuid, now_ms, sha256_hex_str};
use crate::error::{AppError, AppResult, ErrorCode};
use crate::providers::{chatgpt, codex, ProviderKind};
use crate::security::chatgpt_auth::{self, Credential, Endpoints};
use crate::storage::repo::translations;

type Core<'a> = State<'a, Arc<AppCore>>;

fn pending_sign_in() -> &'static Mutex<Option<CancellationToken>> {
    static P: OnceLock<Mutex<Option<CancellationToken>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// This installation's opaque host id, created before the first sign-in and kept.
fn host_id(core: &AppCore) -> AppResult<String> {
    let path = core.paths.root.join("chatgpt_host_id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if s.starts_with("urn:uuid:") && s.len() < 80 {
            return Ok(s);
        }
    }
    let id = chatgpt_auth::new_host_id();
    std::fs::write(&path, &id).map_err(|_| AppError::internal("기기 식별자를 저장할 수 없습니다."))?;
    Ok(id)
}

fn account_caps(base: &serde_json::Value, variant: &str, account: serde_json::Value) -> serde_json::Value {
    let mut caps = if base.is_object() { base.clone() } else { serde_json::json!({}) };
    let o = caps.as_object_mut().expect("object");
    o.insert("variant".into(), serde_json::json!(variant));
    o.entry("allowPrivateHttp").or_insert(serde_json::json!(false));
    o.entry("concurrency").or_insert(serde_json::json!(2));
    o.insert("account".into(), account);
    caps
}

fn chatgpt_account(c: &Credential) -> serde_json::Value {
    serde_json::json!({"email": c.email, "signedIn": c.signed_in(), "planEnabled": c.plan_enabled()})
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChatgptSignInArgs {
    /// Sign in again to this ChatGPT provider; `None` adds a new account.
    pub provider_id: Option<String>,
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChatgptSignInResult {
    pub provider: translations::ProviderRow,
    /// False when the user signed in but did not allow plan usage.
    pub plan_enabled: bool,
    /// True for the first sign-in of this account here (show the welcome note once).
    pub new_registration: bool,
}

/// Opens the browser for "Continue with ChatGPT" and waits for the user to finish there.
#[tauri::command]
pub async fn chatgpt_sign_in(core: Core<'_>, app: tauri::AppHandle, meta: CommandMeta, args: ChatgptSignInArgs) -> Result<Wire, ()> {
    let r = async {
        let existing = match &args.provider_id {
            Some(id) => {
                check_id(id, "providerId")?;
                let row = translations::provider_get(&core.db.reader, id).await?.ok_or_else(|| AppError::not_found("Provider"))?;
                if ProviderKind::of_row(&row.kind, &row.capabilities) != Some(ProviderKind::Chatgpt) {
                    return Err(AppError::invalid("ChatGPT Provider가 아닙니다."));
                }
                Some(row)
            }
            None => None,
        };
        let secrets = core.translation.secrets.clone();
        let saved = match existing.as_ref().and_then(|r| r.keychain_ref.clone()) {
            Some(kref) => chatgpt_auth::load(secrets.as_ref(), &kref)?,
            None => None,
        };
        let host = saved.as_ref().map(|c| c.ext_agent_host_id.clone()).filter(|h| !h.is_empty()).map(Ok).unwrap_or_else(|| host_id(&core))?;
        // one attempt at a time: a new one replaces the one still waiting
        let cancel = CancellationToken::new();
        if let Some(prev) = pending_sign_in().lock().unwrap().replace(cancel.clone()) {
            prev.cancel();
        }
        let open = |url: &str| {
            use tauri_plugin_opener::OpenerExt;
            app.opener().open_url(url, None::<&str>).map_err(|_| AppError::internal("브라우저를 열 수 없습니다."))
        };
        let signed = chatgpt_auth::sign_in(&Endpoints::default(), &host, saved.as_ref(), open, &cancel).await;
        {
            let mut p = pending_sign_in().lock().unwrap();
            if p.as_ref().is_some_and(|c| c.is_cancelled() == cancel.is_cancelled()) {
                *p = None;
            }
        }
        let signed = signed?;
        let cred = signed.credential;
        let plan_enabled = cred.plan_enabled();
        let provider = match existing {
            Some(row) => {
                let kref = row.keychain_ref.clone().unwrap_or_else(new_uuid);
                chatgpt_auth::save(secrets.as_ref(), &kref, &cred)?;
                let caps = account_caps(&row.capabilities, "chatgpt", chatgpt_account(&cred));
                translations::provider_update_mutable(&core.db.writer, &row.id, None, Some(plan_enabled), Some(Some(kref.as_str())), Some(&caps)).await?;
                core.translation.invalidate_provider(&row.id);
                translations::provider_get(&core.db.reader, &row.id).await?.ok_or_else(|| AppError::not_found("Provider"))?
            }
            None => {
                let kref = new_uuid();
                chatgpt_auth::save(secrets.as_ref(), &kref, &cred)?;
                let ep = crate::security::endpoint::normalize_endpoint(chatgpt::ENDPOINT, false)?;
                let row = translations::ProviderRow {
                    id: new_uuid(),
                    kind: ProviderKind::Chatgpt.column().into(),
                    label: match cred.email.as_deref() {
                        Some(e) if !e.is_empty() => format!("ChatGPT ({e})").chars().take(80).collect(),
                        _ => "ChatGPT".into(),
                    },
                    endpoint: ep.identity_string.clone(),
                    // one cache namespace per registration (account + workspace)
                    account_namespace: format!("chatgpt-{}", &sha256_hex_str(&format!("{}|{}", cred.subject, cred.client_id))[..16]),
                    key_configured: true,
                    keychain_ref: Some(kref),
                    capabilities: account_caps(&serde_json::json!({}), "chatgpt", chatgpt_account(&cred)),
                    // The user just agreed, in the browser, to send requests on their plan.
                    enabled: plan_enabled,
                    cache_epoch: 0,
                    created_at: now_ms(),
                };
                translations::provider_insert(&core.db.writer, &row).await?;
                row
            }
        };
        Ok(ChatgptSignInResult { provider, plan_enabled, new_registration: signed.new_registration })
    }
    .await;
    Ok(respond(&meta, r))
}

#[tauri::command]
pub async fn chatgpt_sign_in_cancel(meta: CommandMeta) -> Result<Wire, ()> {
    if let Some(c) = pending_sign_in().lock().unwrap().take() {
        c.cancel();
    }
    Ok(respond(&meta, Ok(serde_json::json!({}))))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AccountProviderArgs {
    pub provider_id: String,
}

/// Ends the renewable session and clears the tokens. The registration stays for a later sign-in.
#[tauri::command]
pub async fn chatgpt_sign_out(core: Core<'_>, meta: CommandMeta, args: AccountProviderArgs) -> Result<Wire, ()> {
    let r = async {
        check_id(&args.provider_id, "providerId")?;
        let row = translations::provider_get(&core.db.reader, &args.provider_id).await?.ok_or_else(|| AppError::not_found("Provider"))?;
        if ProviderKind::of_row(&row.kind, &row.capabilities) != Some(ProviderKind::Chatgpt) {
            return Err(AppError::invalid("ChatGPT Provider가 아닙니다."));
        }
        let secrets = core.translation.secrets.clone();
        // stop requests first
        core.translation.invalidate_provider(&row.id);
        let mut revoked = true;
        let mut account = serde_json::json!({"email": row.capabilities.pointer("/account/email"), "signedIn": false, "planEnabled": false});
        if let Some(kref) = row.keychain_ref.as_deref() {
            if let Some(cred) = chatgpt_auth::load(secrets.as_ref(), kref)? {
                revoked = chatgpt_auth::revoke(&Endpoints::default(), &cred).await;
                let cleared = cred.cleared();
                chatgpt_auth::save(secrets.as_ref(), kref, &cleared)?;
                account = chatgpt_account(&cleared);
            }
        }
        let caps = account_caps(&row.capabilities, "chatgpt", account);
        translations::provider_update_mutable(&core.db.writer, &row.id, None, Some(false), None, Some(&caps)).await?;
        core.translation.invalidate_provider(&row.id);
        let provider = translations::provider_get(&core.db.reader, &row.id).await?.ok_or_else(|| AppError::not_found("Provider"))?;
        Ok(serde_json::json!({"provider": provider, "revoked": revoked}))
    }
    .await;
    Ok(respond(&meta, r))
}

/// Whether Codex is installed and signed in on this Mac (no prompt is sent).
#[tauri::command]
pub async fn codex_status(meta: CommandMeta) -> Result<Wire, ()> {
    Ok(respond(&meta, Ok(codex::status().await)))
}

/// Adds the Codex provider (once). It starts disabled: enabling shows the same notice as
/// any provider that sends text off this Mac.
#[tauri::command]
pub async fn codex_connect(core: Core<'_>, meta: CommandMeta) -> Result<Wire, ()> {
    let r = async {
        let st = codex::status().await;
        if !st.installed {
            return Err(AppError::new(ErrorCode::ProviderUnavailable, "Codex를 찾을 수 없습니다. Codex를 설치한 뒤 다시 시도해 주세요."));
        }
        if !st.signed_in {
            return Err(AppError::new(ErrorCode::AuthFailed, "Codex에 로그인되어 있지 않습니다. 터미널에서 codex login을 실행한 뒤 다시 시도해 주세요."));
        }
        let account = serde_json::json!({"email": st.email, "signedIn": true, "plan": st.plan, "type": st.account_type});
        let all = translations::provider_list(&core.db.reader).await?;
        if let Some(row) = all.into_iter().find(|p| !crate::commands::translation::is_removed(p) && ProviderKind::of_row(&p.kind, &p.capabilities) == Some(ProviderKind::Codex)) {
            let caps = account_caps(&row.capabilities, "codex", account);
            translations::provider_update_mutable(&core.db.writer, &row.id, None, None, None, Some(&caps)).await?;
            core.translation.invalidate_provider(&row.id);
            return translations::provider_get(&core.db.reader, &row.id).await?.ok_or_else(|| AppError::not_found("Provider"));
        }
        let ep = crate::security::endpoint::normalize_endpoint(codex::ENDPOINT_LABEL, false)?;
        let row = translations::ProviderRow {
            id: new_uuid(),
            kind: ProviderKind::Codex.column().into(),
            label: "Codex".into(),
            endpoint: ep.identity_string.clone(),
            account_namespace: "codex-local".into(),
            key_configured: false,
            keychain_ref: None,
            capabilities: account_caps(&serde_json::json!({}), "codex", account),
            enabled: false,
            cache_epoch: 0,
            created_at: now_ms(),
        };
        translations::provider_insert(&core.db.writer, &row).await?;
        Ok(row)
    }
    .await;
    Ok(respond(&meta, r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_caps_keep_existing_settings_and_mark_the_variant() {
        let base = serde_json::json!({"concurrency": 3, "probed": {"streaming": true}});
        let caps = account_caps(&base, "chatgpt", serde_json::json!({"email": "a@example.com", "signedIn": true, "planEnabled": true}));
        assert_eq!(caps["variant"], "chatgpt");
        assert_eq!(caps["concurrency"], 3);
        assert_eq!(caps["probed"]["streaming"], true);
        assert_eq!(caps["account"]["email"], "a@example.com");
        assert_eq!(ProviderKind::of_row("openai_compatible", &caps), Some(ProviderKind::Chatgpt));
        let fresh = account_caps(&serde_json::json!(null), "codex", serde_json::json!({}));
        assert_eq!(fresh["concurrency"], 2);
        assert_eq!(ProviderKind::of_row("openai_compatible", &fresh), Some(ProviderKind::Codex));
        assert_eq!(ProviderKind::of_row("openai_compatible", &serde_json::json!({})), Some(ProviderKind::OpenaiCompatible));
        assert_eq!(ProviderKind::Chatgpt.column(), "openai_compatible");
    }
}
