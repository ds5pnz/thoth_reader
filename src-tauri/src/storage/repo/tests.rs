//! SQL integration tests on a real temporary SQLite (spec §10.1).
use super::*;
use crate::domain::*;
use crate::storage::test_db;

pub(crate) async fn seed_doc(db: &crate::storage::Db) -> (String, String, String, Vec<Block>) {
    let doc = documents::DocumentRow {
        id: new_uuid(),
        sha256: sha256_hex(new_uuid().as_bytes()),
        format: "epub".into(),
        title: "t".into(),
        managed_path: format!("documents/{}.epub", new_uuid()),
        byte_size: 1,
        state: "indexing".into(),
        active_extraction_id: None,
        metadata: serde_json::json!({}),
        created_at: now_ms(),
        last_opened_at: None,
    };
    let mut c = db.writer.acquire().await.unwrap();
    documents::insert(&mut c, &doc).await.unwrap();
    drop(c);
    let ex = derive_extraction_id(&doc.id, &serde_json::json!({"x":1}), &serde_json::json!({}));
    extraction::ensure_extraction(&db.writer, &ex, &doc.id, "cfg", &serde_json::json!({})).await.unwrap();
    let sec = derive_section_id(&ex, 0, "spine:0");
    extraction::ensure_section(&db.writer, &sec, &ex, 0, &serde_json::json!({"spine":0})).await.unwrap();
    let blocks: Vec<Block> = (0..3)
        .map(|i| {
            let raw = format!("Hello world sentence number {i}. Same text.");
            let src = normalize_source_text(&raw, BlockKind::Paragraph);
            Block {
                id: derive_block_id(&ex, "spine:0", &format!("/4/{}", (i + 1) * 2), BlockKind::Paragraph, 0),
                extraction_id: ex.clone(),
                section_id: sec.clone(),
                parent_id: None,
                ordinal: i,
                kind: BlockKind::Paragraph,
                source_hash: sha256_hex_str(&src),
                raw_text: raw,
                source_text: src,
                content: vec![SafeNode::Text { v: "x".into() }],
                location: SourceLocation::Epub {
                    spine_index: 0,
                    href: "x/0.xhtml".into(),
                    canonical_revision: "r".into(),
                    cfi_range: "epubcfi(/6/2!/4/2,/1:0,/1:5)".into(),
                    text_path: vec![4, 2],
                    start_u16: 0,
                    end_u16: 5,
                    quote: TextQuote { exact: "x".into(), prefix: "".into(), suffix: "".into() },
                },
                quality: Quality { level: QualityLevel::High, reasons: vec![] },
                translation_policy: TranslationPolicy::Auto,
                mapping: BlockMapping::default(),
                roles: vec![],
            }
        })
        .collect();
    (doc.id, ex, sec, blocks)
}

#[tokio::test]
async fn staging_is_idempotent_and_finalize_publishes() {
    let (db, _d) = test_db().await;
    let (doc, ex, sec, blocks) = seed_doc(&db).await;
    let (cs1, _) = extraction::stage_batch(&db.writer, &ex, &sec, 0, &blocks[..2]).await.unwrap();
    let (cs1b, _) = extraction::stage_batch(&db.writer, &ex, &sec, 0, &blocks[..2]).await.unwrap();
    assert_eq!(cs1, cs1b);
    assert!(extraction::stage_batch(&db.writer, &ex, &sec, 0, &blocks[1..]).await.is_err());
    extraction::stage_batch(&db.writer, &ex, &sec, 1, &blocks[2..]).await.unwrap();
    let total = extraction::blocks_checksum(&blocks);
    assert!(extraction::finalize_section(&db.writer, &ex, &sec, 2, &total).await.is_err());
    assert_eq!(extraction::finalize_section(&db.writer, &ex, &sec, 3, &total).await.unwrap(), 3);
    // finalize replay is idempotent
    assert_eq!(extraction::finalize_section(&db.writer, &ex, &sec, 3, &total).await.unwrap(), 3);
    let got = extraction::blocks_of_section(&db.reader, &ex, &sec, None, 100).await.unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[1], blocks[1]);
    extraction::activate(&db.writer, &doc, &ex, "ready").await.unwrap();
    let (hits, _, fts) = misc::search(&db.reader, &doc, &ex, "sentence number 1", "all", None, 0, 50).await.unwrap();
    assert!(fts);
    assert_eq!(hits.len(), 1);
    let (hits2, _, fts2) = misc::search(&db.reader, &doc, &ex, "He", "all", None, 0, 50).await.unwrap();
    assert!(!fts2);
    assert_eq!(hits2.len(), 3);
}

#[tokio::test]
async fn bad_source_hash_rejected() {
    let (db, _d) = test_db().await;
    let (_doc, ex, sec, mut blocks) = seed_doc(&db).await;
    blocks[0].source_hash = "00".into();
    assert!(extraction::stage_batch(&db.writer, &ex, &sec, 0, &blocks).await.is_err());
}

async fn seed_profile(db: &crate::storage::Db) -> translations::ProfileRow {
    let p = translations::ProviderRow {
        id: new_uuid(),
        kind: "ollama".into(),
        label: "local".into(),
        endpoint: "http://127.0.0.1:11434".into(),
        account_namespace: "local-default".into(),
        keychain_ref: None,
        key_configured: false,
        capabilities: serde_json::json!({}),
        enabled: true,
        cache_epoch: 0,
        created_at: now_ms(),
    };
    translations::provider_insert(&db.writer, &p).await.unwrap();
    translations::profile_get_or_create(&db.writer, &translations::ProfileRow {
        id: new_uuid(),
        provider_id: p.id.clone(),
        profile_hash: sha256_hex(new_uuid().as_bytes()),
        source_language: "en".into(),
        target_language: "ko".into(),
        model_id: "m".into(),
        snapshot: serde_json::json!({}),
        created_at: now_ms(),
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn manual_wins_over_generated_and_cas_conflicts() {
    let (db, _d) = test_db().await;
    let (doc, ex, sec, blocks) = seed_doc(&db).await;
    extraction::publish_section_direct(&db.writer, &ex, &sec, &blocks).await.unwrap();
    extraction::activate(&db.writer, &doc, &ex, "ready").await.unwrap();
    let prof = seed_profile(&db).await;
    let b = &blocks[0];
    // Manual save at version 0
    let (_m, v1) = translations::save_manual(&db.writer, &b.id, "ko", &b.source_hash, "수동", 0, "r1").await.unwrap();
    assert_eq!(v1, 1);
    // stale version conflicts
    assert!(translations::save_manual(&db.writer, &b.id, "ko", &b.source_hash, "x", 0, "r2").await.is_err());
    // A model completion arrives afterwards: manual pointer must stay.
    let (job, created) = translations::job_insert_or_merge(&db.writer, &b.id, &prof.id, "key1", "req1", "s", 0, 1, &serde_json::json!({})).await.unwrap();
    assert!(created);
    let (job2, created2) = translations::job_insert_or_merge(&db.writer, &b.id, &prof.id, "key1", "req2", "s", 0, 0, &serde_json::json!({})).await.unwrap();
    assert!(!created2);
    assert_eq!(job2.id, job.id);
    assert_eq!(job2.priority, 0);
    assert!(translations::job_transition(&db.writer, &job.id, "QUEUED", "RUNNING", None, None, true, None).await.unwrap());
    let (rev, head) = translations::commit_generated(&db.writer, translations::CommitGenerated {
        job_id: &job.id, block_id: &b.id, target_language: "ko", source_hash: &b.source_hash, profile_id: &prof.id,
        cache_key: "key1", descriptor: &serde_json::json!({}), text: "생성", metadata: &serde_json::json!({}), is_current_profile: true, force: false,
    }).await.unwrap();
    assert!(head.manual_revision_id.is_some());
    assert_eq!(head.generated_revision_id.as_deref(), Some(rev.as_str()));
    // search indexes the manual head
    let (hits, _, _) = misc::search(&db.reader, &doc, &ex, "수동", "translation", Some("ko"), 0, 50).await.unwrap();
    assert_eq!(hits.len(), 1);
    // apply generated => manual released
    let (_a, v3) = translations::apply_revision(&db.writer, &rev, &b.source_hash, head.version).await.unwrap();
    let h = translations::head_get(&db.reader, &b.id, "ko").await.unwrap();
    assert!(h.manual_revision_id.is_none());
    assert_eq!(h.version, v3);
    let (hist, _) = translations::history(&db.reader, &b.id, "ko", None, 50).await.unwrap();
    assert_eq!(hist.len(), 2);
    assert!(translations::cache_lookup(&db.reader, "key1").await.unwrap().is_some());
}

#[tokio::test]
async fn cancel_before_completion_discards_result() {
    let (db, _d) = test_db().await;
    let (doc, ex, sec, blocks) = seed_doc(&db).await;
    extraction::publish_section_direct(&db.writer, &ex, &sec, &blocks).await.unwrap();
    extraction::activate(&db.writer, &doc, &ex, "ready").await.unwrap();
    let prof = seed_profile(&db).await;
    let b = &blocks[1];
    let (job, _) = translations::job_insert_or_merge(&db.writer, &b.id, &prof.id, "k", "rq", "s", 0, 1, &serde_json::json!({})).await.unwrap();
    translations::job_transition(&db.writer, &job.id, "QUEUED", "RUNNING", None, None, true, None).await.unwrap();
    assert_eq!(translations::job_cancel(&db.writer, &job.id, "GENERATION").await.unwrap().as_deref(), Some("RUNNING"));
    let r = translations::commit_generated(&db.writer, translations::CommitGenerated {
        job_id: &job.id, block_id: &b.id, target_language: "ko", source_hash: &b.source_hash, profile_id: &prof.id,
        cache_key: "k", descriptor: &serde_json::json!({}), text: "늦음", metadata: &serde_json::json!({}), is_current_profile: true, force: false,
    }).await;
    assert!(r.is_err());
    assert!(translations::cache_lookup(&db.reader, "k").await.unwrap().is_none());
    // a new job with the same key may be created after the cancel
    let (_j, created) = translations::job_insert_or_merge(&db.writer, &b.id, &prof.id, "k", "rq2", "s", 1, 1, &serde_json::json!({})).await.unwrap();
    assert!(created);
}

#[tokio::test]
async fn receipts_replay_and_conflict() {
    let (db, _d) = test_db().await;
    assert!(matches!(misc::receipt_check(&db.reader, "r", "c", "h").await.unwrap(), misc::Receipt::New));
    misc::receipt_store(&db.writer, "r", "c", "h", &serde_json::json!({"ok":1})).await.unwrap();
    assert!(matches!(misc::receipt_check(&db.reader, "r", "c", "h").await.unwrap(), misc::Receipt::Replay(_)));
    assert!(misc::receipt_check(&db.reader, "r", "c", "other").await.is_err());
}

#[tokio::test]
async fn delete_document_removes_everything() {
    let (db, _d) = test_db().await;
    let (doc, ex, sec, blocks) = seed_doc(&db).await;
    extraction::publish_section_direct(&db.writer, &ex, &sec, &blocks).await.unwrap();
    extraction::activate(&db.writer, &doc, &ex, "ready").await.unwrap();
    let prof = seed_profile(&db).await;
    let b = &blocks[0];
    translations::save_manual(&db.writer, &b.id, "ko", &b.source_hash, "a", 0, "r").await.unwrap();
    translations::save_manual(&db.writer, &b.id, "ko", &b.source_hash, "b", 1, "r2").await.unwrap();
    let (job, _) = translations::job_insert_or_merge(&db.writer, &b.id, &prof.id, "k", "rq", "s", 0, 1, &serde_json::json!({})).await.unwrap();
    let _ = job;
    misc::delete_document_rows(&db.writer, &doc).await.unwrap();
    for t in ["documents", "blocks", "translation_revisions", "translation_heads", "search_entries", "translation_jobs"] {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from {t}"))).fetch_one(&db.reader).await.unwrap();
        assert_eq!(n, 0, "{t}");
    }
    crate::storage::integrity_check(&db.writer).await.unwrap();
}
