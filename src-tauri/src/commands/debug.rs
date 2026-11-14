//! Debug-only self-test hooks (spec §10.1: test tooling is never compiled into release).
//! THOTH_SELFTEST=<path>[:<path>...] registers those files as import candidates so the
//! in-webview self-test can drive the real WKWebView without a file dialog.

use super::core::*;
use std::sync::Arc;
use tauri::State;

#[tauri::command]
pub async fn debug_selftest_files(core: State<'_, Arc<AppCore>>, meta: CommandMeta) -> Result<Wire, ()> {
    let files = std::env::var("THOTH_SELFTEST").unwrap_or_default();
    let cands: Vec<_> = files
        .split(':')
        .filter(|s| !s.is_empty())
        .filter_map(|p| core.imports.register(std::path::PathBuf::from(p)))
        .collect();
    let provider = std::env::var("THOTH_SELFTEST_PROVIDER").ok();
    let model = std::env::var("THOTH_SELFTEST_MODEL").ok();
    let slow_ms = std::env::var("THOTH_SELFTEST_WAIT_MS").ok().and_then(|v| v.parse::<u64>().ok());
    Ok(respond(&meta, Ok(serde_json::json!({"candidates": cands, "provider": provider, "model": model, "slowMs": slow_ms, "scenario": std::env::var("THOTH_SELFTEST_SCENARIO").ok()}))))
}

#[tauri::command]
pub async fn debug_log(meta: CommandMeta, level: String, message: String) -> Result<Wire, ()> {
    let msg: String = message.chars().take(4000).collect();
    println!("[webview:{level}] {msg}");
    Ok(respond(&meta, Ok(serde_json::json!({}))))
}
