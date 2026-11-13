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

pub fn run() {
    diagnostics::init_logging();
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running Thoth Reader");
}
