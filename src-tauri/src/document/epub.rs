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

#[cfg(test)]
mod corpus {
    //! Local corpus harness: `THOTH_CORPUS=/path cargo test --lib epub_corpus -- --ignored --nocapture`.
    //! Reads the user's EPUBs in place (never copied into the repo); the report holds statistics only.
    use crate::domain::{derive_section_id, TranslationPolicy};
    use crate::epub_security::{self, EpubLimits};
    use std::collections::BTreeMap;

    #[test]
    #[ignore]
    fn epub_corpus() {
        let Ok(root) = std::env::var("THOTH_CORPUS") else { return };
        let mut files: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x.eq_ignore_ascii_case("epub")).unwrap_or(false)).collect();
        files.sort();
        let mut rows = vec![];
        for (i, f) in files.iter().enumerate() {
            let label = format!("epub-{:02}", i + 1);
            let out = tempfile::tempdir().unwrap();
            let t0 = std::time::Instant::now();
            let mut row = serde_json::json!({"label": label, "bytes": std::fs::metadata(f).map(|m| m.len()).unwrap_or(0)});
            match epub_security::build_safe_package(f, out.path(), &EpubLimits::default()) {
                Err(e) => {
                    row["error"] = serde_json::json!(format!("{} {}", e.code.as_str(), e.details.get("reason").map(|v| v.to_string()).unwrap_or_default()));
                }
                Ok(pkg) => {
                    let build_ms = t0.elapsed().as_millis();
                    let (mut blocks, mut failed, mut chars, mut auto_chars) = (0usize, 0usize, 0usize, 0usize);
                    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
                    let mut policies: BTreeMap<String, usize> = BTreeMap::new();
                    let mut bad_cfi = 0usize;
                    for it in &pkg.spine {
                        let xhtml = std::fs::read_to_string(out.path().join(&it.safe_href)).unwrap_or_default();
                        let sid = derive_section_id("corpus", it.index, &it.safe_href);
                        match epub_security::extract_blocks(&xhtml, it.index, &it.idref, &it.safe_href, &pkg.canonical_revision, "corpus", &sid) {
                            Ok(bs) => {
                                for b in &bs {
                                    blocks += 1;
                                    *kinds.entry(b.kind.as_str().into()).or_default() += 1;
                                    *policies.entry(b.translation_policy.as_str().into()).or_default() += 1;
                                    chars += b.source_text.chars().count();
                                    if b.translation_policy == TranslationPolicy::Auto {
                                        auto_chars += b.source_text.chars().count();
                                    }
                                    if let crate::domain::SourceLocation::Epub { cfi_range, .. } = &b.location {
                                        if !cfi_range.starts_with("epubcfi(/6/") || cfi_range.matches(',').count() != 2 {
                                            bad_cfi += 1;
                                        }
                                    }
                                }
                            }
                            Err(_) => failed += 1,
                        }
                    }
                    let restrictions: Vec<String> = pkg.restrictions.iter().map(|r| r.split(':').next().unwrap_or("").to_string()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
                    row["spine"] = serde_json::json!(pkg.spine.len());
                    row["tocEntries"] = serde_json::json!(pkg.toc.len());
                    row["resources"] = serde_json::json!(pkg.resources.len());
                    row["restrictions"] = serde_json::json!(restrictions);
                    row["restrictionCount"] = serde_json::json!(pkg.restrictions.len());
                    row["failedSections"] = serde_json::json!(failed);
                    row["blocks"] = serde_json::json!(blocks);
                    row["kinds"] = serde_json::json!(kinds);
                    row["policies"] = serde_json::json!(policies);
                    row["badCfi"] = serde_json::json!(bad_cfi);
                    row["autoCoverage"] = serde_json::json!(if chars > 0 { (auto_chars as f64 / chars as f64 * 1000.0).round() / 1000.0 } else { 0.0 });
                    row["language"] = serde_json::json!(pkg.language);
                    row["buildMs"] = serde_json::json!(build_ms);
                    row["totalMs"] = serde_json::json!(t0.elapsed().as_millis());
                }
            }
            println!("{row}");
            rows.push(row);
        }
        let out = std::env::var("THOTH_CORPUS_REPORT").unwrap_or_else(|_| "../reports/corpus-epub.json".into());
        let _ = std::fs::create_dir_all(std::path::Path::new(&out).parent().unwrap());
        std::fs::write(&out, serde_json::to_string_pretty(&serde_json::json!({"documents": rows})).unwrap()).unwrap();
    }
}
