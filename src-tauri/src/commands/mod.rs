//! Typed IPC commands (spec §8.2). No arbitrary SQL/HTTP/Keychain-read/file commands exist.

pub mod core;
pub mod document;
pub mod reader;
pub mod translation;

/// Every app command; build.rs generates `allow-<name>` permissions for exactly these.
pub const COMMANDS: &[&str] = &[
    "library_list", "import_pick", "document_import", "document_open", "document_close", "document_delete",
    "sections_list", "blocks_get", "extraction_begin", "blocks_commit_batch", "extraction_finalize_section",
    "open_external", "reader_save_position", "bookmark_put", "bookmark_delete", "bookmark_list", "search_query",
    "settings_get", "settings_patch", "translation_save_edit", "translation_apply_revision", "translation_history",
    "block_set_preference", "reader_set_viewport", "translation_request", "translation_cancel", "reader_set_mode",
    "translation_pause", "reader_switch_profile", "reader_get_snapshot", "provider_save", "provider_list",
    "provider_probe", "provider_new_cache_epoch", "profile_create", "cache_invalidate", "storage_usage",
];
