pub mod commands;
pub mod diagnostics;
pub mod document;
pub mod domain;
pub mod epub_security;
pub mod error;
pub mod providers;
pub mod security;
pub mod storage;
pub mod translation;

use commands::core::AppCore;
use std::sync::Arc;
use tauri::{Emitter, Manager};

const DEV_ORIGIN: &str = "http://localhost:1420";

fn allowed_origins() -> Vec<&'static str> {
    let mut v = vec!["tauri://localhost", "http://tauri.localhost"];
    if cfg!(debug_assertions) {
        v.push(DEV_ORIGIN);
    }
    v
}

/// Only the bundled app (or the dev server in debug) may be shown in the main webview.
fn navigation_allowed(url: &url::Url) -> bool {
    match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" if url.host_str() == Some("tauri.localhost") => true,
        "http" | "ws" if cfg!(debug_assertions) => url.host_str() == Some("localhost") && url.port() == Some(1420),
        "about" => matches!(url.as_str(), "about:blank" | "about:srcdoc"),
        // epub.js renders sanitized chapters into blob: iframes created by the app itself.
        "blob" => url::Url::parse(url.path()).map(|inner| navigation_allowed(&inner) && inner.scheme() != "blob").unwrap_or(false),
        _ => false,
    }
}

macro_rules! app_handlers {
    ($($extra:path),*) => {
        tauri::generate_handler![
            commands::document::library_list,
            commands::document::import_pick,
            commands::document::document_import,
            commands::document::document_open,
            commands::document::document_close,
            commands::document::document_delete,
            commands::document::library_thumb_begin,
            commands::document::library_cover_set,
            commands::document::sections_list,
            commands::document::blocks_get,
            commands::document::extraction_begin,
            commands::document::blocks_commit_batch,
            commands::document::extraction_finalize_section,
            commands::document::open_external,
            commands::reader::reader_save_position,
            commands::reader::bookmark_put,
            commands::reader::bookmark_delete,
            commands::reader::bookmark_list,
            commands::reader::annotation_put,
            commands::reader::annotation_delete,
            commands::reader::annotation_list,
            commands::reader::search_query,
            commands::reader::settings_get,
            commands::reader::settings_patch,
            commands::reader::translation_save_edit,
            commands::reader::translation_apply_revision,
            commands::reader::translation_history,
            commands::reader::block_set_preference,
            commands::translation::reader_set_viewport,
            commands::translation::translation_request,
            commands::translation::translation_cancel,
            commands::translation::translate_selection,
            commands::translation::translation_batch_start,
            commands::translation::translation_batch_cancel,
            commands::translation::translation_batch_status,
            commands::export::export_translation,
            commands::pdf_out::export_pdf_begin,
            commands::pdf_out::export_pdf_page,
            commands::pdf_out::export_pdf_end,
            commands::translation::reader_set_mode,
            commands::translation::translation_pause,
            commands::translation::reader_switch_profile,
            commands::translation::reader_get_snapshot,
            commands::translation::provider_save,
            commands::translation::provider_list,
            commands::translation::provider_probe,
            commands::translation::provider_new_cache_epoch,
            commands::translation::profile_create,
            commands::translation::cache_invalidate,
            commands::translation::storage_usage,
            $($extra),*
        ]
    };
}

#[cfg(debug_assertions)]
fn handlers() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
    app_handlers!(commands::debug::debug_selftest_files, commands::debug::debug_log)
}

#[cfg(not(debug_assertions))]
fn handlers() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
    app_handlers!()
}

pub fn run() {
    diagnostics::init_logging();
    let mut builder = tauri::Builder::default();
    // The debug self-test uses its own scratch data dir, so it may run next to a normal
    // instance; every other launch takes the single-instance lock.
    let selftest = cfg!(debug_assertions) && std::env::var("THOTH_SELFTEST").is_ok() && std::env::var("THOTH_DATA_DIR").is_ok();
    if !selftest {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_focus();
            }
        }));
    }
    let builder = builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .register_asynchronous_uri_scheme_protocol("thoth-doc", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            std::thread::spawn(move || {
                let Some(core) = app.try_state::<Arc<AppCore>>() else {
                    let _ = responder.respond(tauri::http::Response::builder().status(503).body(Vec::new()).unwrap());
                    return;
                };
                let method = request.method().as_str().to_string();
                let range = request.headers().get("range").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
                let origin = request.headers().get("origin").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
                let resp = document::asset::handle_with_library(&core.sessions, &core.library_assets, &core.paths, &method, request.uri().path(), range.as_deref(), origin.as_deref(), &core.allowed_origins);
                let mut b = tauri::http::Response::builder().status(resp.status);
                for (k, v) in resp.headers {
                    b = b.header(k, v);
                }
                let _ = responder.respond(b.body(resp.body).unwrap());
            });
        })
        .setup(|app| {
            #[allow(unused_mut)]
            let mut root = app.path().app_data_dir().map_err(|e| format!("app data dir: {e}"))?;
            #[cfg(debug_assertions)]
            if let Ok(d) = std::env::var("THOTH_DATA_DIR") {
                root = std::path::PathBuf::from(d); // debug-only scratch library for self-tests
            }
            let paths = document::paths::AppPaths::new(root)?;
            let db = tauri::async_runtime::block_on(storage::Db::open(&paths.db_file(), &paths.backups)).map_err(|e| e.message)?;
            if let Some(r) = &db.read_only_reason {
                tracing::warn!("database is read-only: {r}");
            }
            tauri::async_runtime::block_on(document::import::recover(&db, &paths)).map_err(|e| e.message)?;
            let translation = translation::service::TranslationService::new(db.clone(), security::keychain::keychain_store(), translation::service::default_factory());
            tauri::async_runtime::block_on(translation.start()).map_err(|e| e.message)?;
            let core = Arc::new(AppCore {
                db,
                paths,
                sessions: Default::default(),
                imports: Default::default(),
                translation,
                allowed_origins: allowed_origins(),
                library_assets: Default::default(),
                epub_prepare: Default::default(),
                import_lock: Default::default(),
                pdf_exports: Default::default(),
            });
            app.manage(core.clone());
            tauri::async_runtime::spawn(document::epub::backfill_library(core.clone()));

            #[allow(unused_mut)]
            let mut wb = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()));
            #[cfg(debug_assertions)]
            if std::env::var("THOTH_SELFTEST").is_ok() {
                // Self-test runs while the window may be occluded; keep timers running.
                wb = wb.background_throttling(tauri::utils::config::BackgroundThrottlingPolicy::Disabled);
            }
            wb
                .title("Thoth Reader")
                .inner_size(1280.0, 840.0)
                .min_inner_size(900.0, 600.0)
                .on_navigation(|url| navigation_allowed(url))
                .on_new_window(|_url, _features| tauri::webview::NewWindowResponse::Deny)
                .build()?;

            // Idle WAL checkpoints and receipt pruning.
            let core_bg = core.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                    core_bg.db.checkpoint().await;
                    let _ = storage::repo::misc::receipts_prune(&core_bg.db.writer, 7 * 24 * 3600 * 1000).await;
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Dropped paths are only accepted here and turned into single-use tokens.
            if let tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) = event {
                if let Some(core) = window.try_state::<Arc<AppCore>>() {
                    let cands: Vec<_> = paths.iter().take(50).filter_map(|p| core.imports.register(p.clone())).collect();
                    let _ = window.emit_to(window.label(), "import-candidates", cands);
                }
            }
        })
        .invoke_handler(handlers());

    let app = builder.build(tauri::generate_context!()).expect("error while building Thoth Reader");
    app.run(|handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            // Stop feeding work, let position/edit commits finish, cancel requests (≤3s).
            if let Some(core) = handle.try_state::<Arc<AppCore>>() {
                let core = core.inner().clone();
                tauri::async_runtime::block_on(async move {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                        core.translation.shutdown().await;
                        core.db.checkpoint().await;
                    })
                    .await;
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn navigation_policy() {
        let ok = |s: &str| super::navigation_allowed(&url::Url::parse(s).unwrap());
        assert!(ok("tauri://localhost/index.html"));
        assert!(!ok("https://evil.example/"));
        assert!(!ok("file:///etc/passwd"));
        assert!(!ok("thoth-doc://localhost/x/pdf"));
        assert!(!ok("javascript:alert(1)"));
        assert!(ok("about:srcdoc"));
        assert!(ok("blob:tauri://localhost/2b1e-uuid"));
        assert!(!ok("blob:https://evil.example/2b1e-uuid"));
        assert!(!ok("blob:blob:tauri://localhost/x"));
    }
}
