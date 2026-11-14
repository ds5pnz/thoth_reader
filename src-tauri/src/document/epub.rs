//! EPUB extraction orchestration (filled in when epub_security lands).
use crate::commands::core::AppCore;
use crate::document::session::Session;
use crate::error::{AppError, AppResult};
use crate::storage::repo::documents::DocumentRow;
use std::sync::Arc;

pub async fn ensure_extraction(_core: Arc<AppCore>, _s: Arc<Session>, _doc: DocumentRow) -> AppResult<()> {
    Err(AppError::internal("EPUB pending"))
}
