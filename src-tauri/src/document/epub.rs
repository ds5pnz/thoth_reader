//! EPUB extraction orchestration (spec §2.2): validate + sanitize into a staging directory,
//! atomically publish it as `derived/<extractionId>`, create sections, then extract the
//! current/next spine first and the rest in the background, publishing per section.

use crate::commands::core::AppCore;
use crate::document::session::Session;
use crate::domain::{canonical_json::canonical_sha256, derive_extraction_id, derive_section_id, new_uuid, versions};
use crate::epub_security::{self, EpubLimits};
use crate::error::{AppError, AppResult};
use crate::storage::repo::{documents, extraction, misc};
use std::sync::Arc;

fn versions_json() -> serde_json::Value {
    serde_json::json!({
        "epubExtractor": versions::EPUB_EXTRACTOR,
        "epubSanitizer": versions::EPUB_SANITIZER,
        "canonicalRevision": epub_security::canonical_revision(),
    })
}

pub async fn ensure_extraction(core: Arc<AppCore>, s: Arc<Session>, doc: documents::DocumentRow) -> AppResult<()> {
    let versions = versions_json();
    let config = serde_json::json!({});
    let ex_id = derive_extraction_id(&doc.id, &versions, &config);
    let config_hash = canonical_sha256(&serde_json::json!({"versions": versions, "config": config}));
    let derived = core.paths.derived_dir(&ex_id);
    let state = extraction::ensure_extraction(&core.db.writer, &ex_id, &doc.id, &config_hash, &versions).await?;
    let package_ready = derived.join("package.opf").exists();

    if !package_ready {
        core.db.ensure_writable()?;
        let staging = core.paths.derived.join(format!(".staging-{}", new_uuid()));
        let src = s.managed_file.clone();
        let st2 = staging.clone();
        let built = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&st2)?;
            epub_security::build_safe_package(&src, &st2, &EpubLimits::default())
        })
        .await
        .map_err(|_| AppError::internal("EPUB 처리 작업이 중단되었습니다."))?;
        let pkg = match built {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                extraction::set_extraction_state(&core.db.writer, &ex_id, "failed").await?;
                documents::set_state(&core.db.writer, &doc.id, "failed").await?;
                documents::merge_metadata(&core.db.writer, &doc.id, &serde_json::json!({"error": e.code.as_str()})).await?;
                return Err(e);
            }
        };
        if derived.exists() {
            let _ = std::fs::remove_dir_all(&derived);
        }
        std::fs::rename(&staging, &derived)?;
        let resources: serde_json::Map<String, serde_json::Value> =
            pkg.resources.iter().map(|r| (r.resource_id.clone(), serde_json::json!(r.safe_href))).collect();
        let mut meta = serde_json::json!({
            "title": pkg.title,
            "toc": pkg.toc,
            "resources": resources,
            "restrictions": pkg.restrictions,
            "canonicalRevision": pkg.canonical_revision,
        });
        if doc.metadata.get("sourceLanguage").is_none() {
            if let Some(l) = pkg.language.as_deref().map(|l| l.trim()).filter(|l| crate::commands::core::check_lang(l).is_ok()) {
                meta["sourceLanguage"] = serde_json::json!(l);
            }
        }
        documents::merge_metadata(&core.db.writer, &doc.id, &meta).await?;
        for it in &pkg.spine {
            let loc = format!("spine:{}:{}", it.index, it.safe_href);
            let sid = derive_section_id(&ex_id, it.index, &loc);
            extraction::ensure_section(
                &core.db.writer,
                &sid,
                &ex_id,
                it.index,
                &serde_json::json!({"kind": "epub", "spineIndex": it.index, "href": it.safe_href, "idref": it.idref, "linear": it.linear}),
            )
            .await?;
        }
    }

    let doc_state = if state == "ready" { "ready" } else if state == "partial" { "partial" } else { "indexing" };
    extraction::activate(&core.db.writer, &doc.id, &ex_id, doc_state).await?;
    *s.extraction_id.write().unwrap() = Some(ex_id.clone());
    *s.derived_dir.write().unwrap() = Some(derived.clone());
    if let Some(src) = documents::get(&core.db.reader, &doc.id).await?.and_then(|d| d.metadata.get("sourceLanguage").and_then(|v| v.as_str()).map(|s| s.to_string())) {
        s.state.lock().unwrap().source_language = Some(src);
    }

    if state != "ready" && state != "partial" {
        // Restore spine first, then following spines, then the rest.
        let restore_spine = misc::position_get(&core.db.reader, &doc.id)
            .await?
            .and_then(|(a, _, _)| match a.location {
                Some(crate::domain::SourceLocation::Epub { spine_index, .. }) => Some(spine_index as i64),
                _ => None,
            })
            .unwrap_or(0);
        let core2 = core.clone();
        let s2 = s.clone();
        tokio::spawn(async move {
            if let Err(e) = index_sections(core2, s2, ex_id, doc.id.clone(), restore_spine).await {
                tracing::warn!(target: "epub", "indexing stopped: {:?}", e.code);
            }
        });
    }
    Ok(())
}

async fn index_sections(core: Arc<AppCore>, s: Arc<Session>, ex_id: String, doc_id: String, first: i64) -> AppResult<()> {
    let derived = core.paths.derived_dir(&ex_id);
    let revision = epub_security::canonical_revision();
    let mut sections = extraction::list_sections(&core.db.reader, &ex_id).await?;
    sections.sort_by_key(|sec| if sec.ordinal >= first { sec.ordinal - first } else { 1_000_000 + sec.ordinal });
    let total = sections.len() as i64;
    for sec in sections.into_iter().filter(|x| x.state != "ready" && x.state != "failed") {
        let href = sec.locator.get("href").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let idref = sec.locator.get("idref").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let path = crate::document::paths::safe_join(&derived, &href);
        let (ex2, sid, rev) = (ex_id.clone(), sec.id.clone(), revision.clone());
        let ordinal = sec.ordinal as u32;
        let res = match path {
            Some(p) => tokio::task::spawn_blocking(move || {
                let xhtml = std::fs::read_to_string(&p).map_err(|_| AppError::internal("섹션 파일을 읽을 수 없습니다."))?;
                epub_security::extract_blocks(&xhtml, ordinal, &idref, &href, &rev, &ex2, &sid)
            })
            .await
            .map_err(|_| AppError::internal("추출 작업 중단"))?,
            None => Err(AppError::invalid("섹션 경로가 올바르지 않습니다.")),
        };
        match res {
            Ok(blocks) => {
                if let Err(e) = extraction::publish_section_direct(&core.db.writer, &ex_id, &sec.id, &blocks).await {
                    tracing::warn!(target: "epub", "section publish failed: {:?}", e.code);
                    extraction::mark_section_failed(&core.db.writer, &sec.id).await?;
                }
            }
            Err(_) => extraction::mark_section_failed(&core.db.writer, &sec.id).await?,
        }
        let (ready, failed): (i64, i64) = sqlx::query_as("select coalesce(sum(state='ready'),0), coalesce(sum(state='failed'),0) from sections where extraction_id = ?")
            .bind(&ex_id)
            .fetch_one(&core.db.reader)
            .await?;
        s.emit("index_progress", None, None, serde_json::json!({"ready": ready, "failed": failed, "total": total, "sectionId": sec.id, "extractionId": ex_id}));
    }
    let (ready, failed): (i64, i64) = sqlx::query_as("select coalesce(sum(state='ready'),0), coalesce(sum(state='failed'),0) from sections where extraction_id = ?")
        .bind(&ex_id)
        .fetch_one(&core.db.reader)
        .await?;
    let st = if failed > 0 { "partial" } else { "ready" };
    if ready + failed == total {
        extraction::set_extraction_state(&core.db.writer, &ex_id, st).await?;
        documents::set_state(&core.db.writer, &doc_id, st).await?;
    }
    Ok(())
}
