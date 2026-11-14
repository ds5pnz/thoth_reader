// Explicit app-command permissions (spec §8.4): registered commands are not implicitly allowed.
const COMMANDS: &[&str] = &[
    "library_list", "import_pick", "document_import", "document_open", "document_close", "document_delete",
    "library_thumb_begin", "library_cover_set",
    "sections_list", "blocks_get", "extraction_begin", "blocks_commit_batch", "extraction_finalize_section",
    "open_external", "reader_save_position", "bookmark_put", "bookmark_delete", "bookmark_list", "annotation_put", "annotation_delete", "annotation_list", "search_query",
    "settings_get", "settings_patch", "translation_save_edit", "translation_apply_revision", "translation_history",
    "block_set_preference", "reader_set_viewport", "translation_request", "translation_cancel", "reader_set_mode",
    "translation_pause", "reader_switch_profile", "reader_get_snapshot", "provider_save", "provider_list",
    "provider_probe", "provider_new_cache_epoch", "profile_create", "cache_invalidate", "storage_usage",
    // debug-only handlers (not compiled into release; the permission alone grants nothing)
    "debug_selftest_files", "debug_log",
];

fn main() {
    // sqlx::migrate! embeds the directory at compile time; rebuild when a migration is added.
    println!("cargo:rerun-if-changed=migrations");
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
