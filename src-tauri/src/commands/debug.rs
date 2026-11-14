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
    Ok(respond(&meta, Ok(serde_json::json!({"candidates": cands, "provider": provider, "model": model, "slowMs": slow_ms, "scenario": std::env::var("THOTH_SELFTEST_SCENARIO").ok(), "page": std::env::var("THOTH_SELFTEST_PAGE").ok().and_then(|v| v.parse::<u32>().ok()), "only": std::env::var("THOTH_SELFTEST_ONLY").ok()}))))
}

#[tauri::command]
pub async fn debug_log(meta: CommandMeta, level: String, message: String) -> Result<Wire, ()> {
    let msg: String = message.chars().take(200_000).collect();
    println!("[webview:{level}] {msg}");
    Ok(respond(&meta, Ok(serde_json::json!({}))))
}

/// Save what the web view shows as a PNG (documentation screenshots). WebKit renders the
/// snapshot itself, so no screen-recording permission is involved. Files go only into
/// `THOTH_SELFTEST_EXPORT_DIR`.
#[tauri::command]
pub async fn debug_snapshot(window: tauri::WebviewWindow, meta: CommandMeta, name: String) -> Result<Wire, ()> {
    let Some(dir) = std::env::var_os("THOTH_SELFTEST_EXPORT_DIR") else {
        return Ok(respond(&meta, Ok(serde_json::json!({"saved": false}))));
    };
    let safe: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(60).collect();
    let path = std::path::PathBuf::from(dir).join(format!("{safe}.png"));
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    let tx = std::sync::Mutex::new(Some(tx));
    let started = window.with_webview(move |wv| snapshot::take(wv.inner(), path, move |ok| {
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(ok);
        }
    }));
    let why = match (&started, tokio::time::timeout(std::time::Duration::from_secs(20), rx).await) {
        (Err(_), _) => "no_webview",
        (_, Err(_)) => "timeout",
        (_, Ok(Err(_))) => "dropped",
        (_, Ok(Ok(false))) => "no_image",
        (_, Ok(Ok(true))) => "",
    };
    Ok(respond(&meta, Ok(serde_json::json!({"saved": why.is_empty(), "why": why}))))
}

#[cfg(target_os = "macos")]
mod snapshot {
    use block2::RcBlock;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{class, msg_send};
    use std::path::PathBuf;

    pub fn take(webview: *mut std::ffi::c_void, path: PathBuf, done: impl Fn(bool) + 'static) {
        let wk = webview as *mut AnyObject;
        let block = RcBlock::new(move |image: *mut AnyObject, _error: *mut AnyObject| {
            done(!image.is_null() && unsafe { write_png(image, &path) });
        });
        let none: *mut AnyObject = std::ptr::null_mut();
        unsafe {
            let _: () = msg_send![wk, takeSnapshotWithConfiguration: none, completionHandler: &*block];
        }
    }

    unsafe fn write_png(image: *mut AnyObject, path: &std::path::Path) -> bool {
        let Ok(cpath) = std::ffi::CString::new(path.to_string_lossy().as_bytes()) else { return false };
        let tiff: *mut AnyObject = msg_send![image, TIFFRepresentation];
        if tiff.is_null() {
            return false;
        }
        let rep: *mut AnyObject = msg_send![class!(NSBitmapImageRep), imageRepWithData: tiff];
        if rep.is_null() {
            return false;
        }
        let props: *mut AnyObject = msg_send![class!(NSDictionary), dictionary];
        // NSBitmapImageFileTypePNG = 4
        let png: *mut AnyObject = msg_send![rep, representationUsingType: 4usize, properties: props];
        if png.is_null() {
            return false;
        }
        let ns_path: *mut AnyObject = msg_send![class!(NSString), stringWithUTF8String: cpath.as_ptr()];
        let ok: Bool = msg_send![png, writeToFile: ns_path, atomically: true];
        ok.as_bool()
    }
}

#[cfg(not(target_os = "macos"))]
mod snapshot {
    pub fn take(_webview: *mut std::ffi::c_void, _path: std::path::PathBuf, done: impl Fn(bool) + 'static) {
        done(false);
    }
}
