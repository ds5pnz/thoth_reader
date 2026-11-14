//! Deterministic scheduler tests with the mock provider (spec §10.1, T11–T13, AC03, AC05).

use super::service::*;
use crate::document::import::Format;
use crate::document::session::{OpenParams, ReadingMode, Session, SessionManager};
use crate::document::sink::VecSink;
use crate::domain::*;
use crate::providers::mock::{MockConfig, MockProvider, MockStep};
use crate::providers::{ProviderError, TranslationProvider};
use crate::security::keychain::MemorySecretStore;
use crate::storage::repo::{extraction, translations};
use crate::storage::{test_db, Db};
use std::sync::Arc;
use std::time::Duration;

struct Fx {
    db: Db,
    svc: Arc<TranslationService>,
    mock: Arc<MockProvider>,
    sessions: SessionManager,
    s: Arc<Session>,
    sink: Arc<VecSink>,
    blocks: Vec<Block>,
    _d: tempfile::TempDir,
}

async fn fixture(n_blocks: u32, cfg: MockConfig) -> Fx {
    let (db, d) = test_db().await;
    let (doc, ex, sec, _) = crate::storage::repo::tests::seed_doc(&db).await;
    let blocks: Vec<Block> = (0..n_blocks)
        .map(|i| {
            let raw = format!("Paragraph {i}: the quick brown fox jumps over the lazy dog.");
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
                content: vec![],
                location: SourceLocation::Pdf { page_index: 0, view_box: vec![0.0, 0.0, 612.0, 792.0], rotation: 0, rects: vec![], item_ranges: vec![] },
                quality: Quality { level: QualityLevel::High, reasons: vec![] },
                translation_policy: TranslationPolicy::Auto,
                mapping: BlockMapping::default(),
                roles: vec![],
            }
        })
        .collect();
    extraction::publish_section_direct(&db.writer, &ex, &sec, &blocks).await.unwrap();
    extraction::activate(&db.writer, &doc, &ex, "ready").await.unwrap();
    let prov = translations::ProviderRow {
        id: new_uuid(),
        kind: "openai_compatible".into(),
        label: "mock".into(),
        endpoint: "https://mock.example/v1".into(),
        account_namespace: "default".into(),
        keychain_ref: None,
        key_configured: false,
        capabilities: serde_json::json!({"concurrency": 2, "probed": cfg.capabilities}),
        enabled: true,
        cache_epoch: 0,
        created_at: now_ms(),
    };
    translations::provider_insert(&db.writer, &prov).await.unwrap();
    let profile = super::prepare::create_profile(&db, &prov, "mock-model", "en", "ko", Some(0.2), None, None, Some(8192), Some("sha256:mock")).await.unwrap();
    let mock = Arc::new(MockProvider::new(cfg));
    let m2 = mock.clone();
    let factory: ProviderFactory = Arc::new(move |_c, _s| Ok(m2.clone() as Arc<dyn TranslationProvider>));
    let svc = TranslationService::new(db.clone(), Arc::new(MemorySecretStore::new()), factory).with_jitter(Arc::new(|| 0.0));
    svc.start().await.unwrap();
    let sessions = SessionManager::default();
    let sink = Arc::new(VecSink::default());
    let s = sessions.open(OpenParams {
        document_id: doc,
        format: Format::Pdf,
        window_label: "main".into(),
        managed_file: "/x".into(),
        sink: sink.clone(),
        target_language: "ko".into(),
        source_language: Some("en".into()),
        profile_id: Some(profile.id.clone()),
    });
    *s.extraction_id.write().unwrap() = Some(ex);
    Fx { db, svc, mock, sessions, s, sink, blocks, _d: d }
}

async fn wait_until<F: Fn() -> bool>(f: F) {
    for _ in 0..400 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

async fn generated_text(db: &Db, block: &str) -> Option<String> {
    let h = translations::head_get(&db.reader, block, "ko").await.unwrap();
    match h.generated_revision_id {
        Some(r) => translations::revision_get(&db.reader, &r).await.unwrap().map(|r| r.text),
        None => None,
    }
}

/// T11: 100 identical visible reports -> one job / one HTTP call per key; force bypasses cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t11_dedupe_and_force() {
    let f = fixture(2, MockConfig { first_chunk_delay: Duration::from_millis(30), ..Default::default() }).await;
    let id = f.blocks[0].id.clone();
    for seq in 1..=100u64 {
        f.svc.set_viewport(&f.s, f.s.generation(), seq, vec![id.clone()], true).await.unwrap();
    }
    let svc = f.svc.clone();
    wait_until(|| svc.queue_stats()["waiting"] == 0 && svc.queue_stats()["running"] == 0 && f.mock.calls() >= 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // block 0 visible + block 1 prefetched (P2) => 2 calls, never more.
    assert_eq!(f.mock.calls(), 2);
    let n: i64 = sqlx::query_scalar("select count(*) from translation_jobs where block_id = ?").bind(&id).fetch_one(&f.db.reader).await.unwrap();
    assert_eq!(n, 1);
    // cache hit: no new HTTP
    let r = f.svc.request(&f.s, &id, false).await.unwrap();
    assert_eq!(r["kind"], "cache_hit");
    assert_eq!(f.mock.calls(), 2);
    // force: bypass the cache, new revision, history kept
    f.svc.request(&f.s, &id, true).await.unwrap();
    let mock = f.mock.clone();
    wait_until(|| mock.calls() == 3).await;
    let db = f.db.clone();
    let idc = id.clone();
    for _ in 0..200 {
        let (hist, _) = translations::history(&db.reader, &idc, "ko", None, 50).await.unwrap();
        if hist.len() == 2 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("force did not create a second revision");
}

/// AC03: original mode / pause => no automatic requests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn original_mode_stops_auto() {
    let f = fixture(3, MockConfig::default()).await;
    f.svc.set_mode(&f.s, ReadingMode::Original).await.unwrap();
    let r = f.svc.set_viewport(&f.s, f.s.generation(), 1, vec![f.blocks[0].id.clone()], true).await.unwrap();
    assert_eq!(r["blockedReason"], "original_mode");
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(f.mock.calls(), 0);
    // explicit request still works
    f.svc.request(&f.s, &f.blocks[0].id, false).await.unwrap();
    let m = f.mock.clone();
    wait_until(|| m.calls() == 1).await;
}

/// AC03: prefetch is limited to the next 3 blocks in reading direction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefetch_next_three() {
    let f = fixture(10, MockConfig { first_chunk_delay: Duration::from_millis(5), ..Default::default() }).await;
    // Single slot so the recorded order is the dispatch order.
    sqlx::query("update providers set capabilities_json = json_set(capabilities_json, '$.concurrency', 1)").execute(&f.db.writer).await.unwrap();
    f.svc.set_viewport(&f.s, f.s.generation(), 1, vec![f.blocks[0].id.clone()], true).await.unwrap();
    let svc = f.svc.clone();
    wait_until(|| f.mock.calls() >= 4 && svc.queue_stats()["running"] == 0 && svc.queue_stats()["waiting"] == 0).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(f.mock.calls(), 4);
    let order: Vec<String> = f.mock.requests().iter().map(|r| r.block_id.clone()).collect();
    assert_eq!(order[0], f.blocks[0].id, "visible block first");
}

/// T12: a completion that arrives after a generation bump is not shown to the new UI and,
/// if cancel committed first, never enters cache/revisions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t12_late_completion_after_generation_bump() {
    let f = fixture(1, MockConfig { first_chunk_delay: Duration::from_millis(150), ..Default::default() }).await;
    let id = f.blocks[0].id.clone();
    let g = f.s.generation();
    f.svc.set_viewport(&f.s, g, 1, vec![id.clone()], true).await.unwrap();
    let m = f.mock.clone();
    wait_until(|| m.calls() == 1).await;
    let g2 = f.svc.switch_profile(&f.s, None, None, None).await.unwrap();
    assert!(g2 > g);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(generated_text(&f.db, &id).await.is_none(), "cancelled-first result must not be stored");
    let n: i64 = sqlx::query_scalar("select count(*) from translation_cache").fetch_one(&f.db.reader).await.unwrap();
    assert_eq!(n, 0);
    let ev = f.sink.0.lock().unwrap();
    assert!(!ev.iter().any(|e| e.generation == g2 && e.kind == "translation_committed"));
}

/// T13: manual save racing a model completion keeps the manual pin; candidate + history kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t13_manual_vs_completion() {
    let f = fixture(1, MockConfig { first_chunk_delay: Duration::from_millis(40), ..Default::default() }).await;
    let b = f.blocks[0].clone();
    f.svc.request(&f.s, &b.id, false).await.unwrap();
    translations::save_manual(&f.db.writer, &b.id, "ko", &b.source_hash, "직접 수정", 0, "rq").await.unwrap();
    let db = f.db.clone();
    let bid = b.id.clone();
    for _ in 0..300 {
        let h = translations::head_get(&db.reader, &bid, "ko").await.unwrap();
        if h.generated_revision_id.is_some() {
            assert!(h.manual_revision_id.is_some());
            let (hist, _) = translations::history(&db.reader, &bid, "ko", None, 50).await.unwrap();
            assert_eq!(hist.len(), 2);
            let d = f.svc.display_for_blocks(&f.s, std::slice::from_ref(&b)).await.unwrap();
            assert_eq!(d[0].status, "manual");
            assert_eq!(d[0].shown.as_ref().unwrap().text, "직접 수정");
            assert!(d[0].has_candidate);
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("completion never committed");
}

/// T10: 401 stops the provider; 429 waits then retries; invalid output is not retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t10_error_policies() {
    let f = fixture(3, MockConfig::default()).await;
    f.mock.script([MockStep::Fail(ProviderError::RateLimited { retry_after: Some(Duration::from_millis(50)) }), MockStep::Succeed { text: None }]);
    f.svc.request(&f.s, &f.blocks[0].id, false).await.unwrap();
    let db = f.db.clone();
    let b0 = f.blocks[0].id.clone();
    for _ in 0..300 {
        if generated_text(&db, &b0).await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(generated_text(&f.db, &b0).await.is_some(), "429 then success");
    assert_eq!(f.mock.calls(), 2);

    f.mock.script([MockStep::Succeed { text: Some("{\"segments\":[]}".into()) }]);
    f.svc.request(&f.s, &f.blocks[1].id, false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let st: String = sqlx::query_scalar("select state || ':' || coalesce(error_code,'') from translation_jobs where block_id = ?").bind(&f.blocks[1].id).fetch_one(&f.db.reader).await.unwrap();
    assert_eq!(st, "FAILED:INVALID_OUTPUT");
    assert_eq!(f.mock.calls(), 3, "invalid output must not be retried");

    f.mock.script([MockStep::Fail(ProviderError::Auth { status: 401 })]);
    f.svc.request(&f.s, &f.blocks[2].id, false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let e = f.svc.request(&f.s, &f.blocks[2].id, true).await.unwrap_err();
    assert_eq!(e.code, crate::error::ErrorCode::AuthFailed);
    let _ = &f.sessions;
}

/// AC05: cancel vs completion race repeated 1,000 times — invariants hold every time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac05_cancel_completion_race_1000() {
    let (db, _d) = test_db().await;
    let (doc, ex, sec, blocks) = crate::storage::repo::tests::seed_doc(&db).await;
    extraction::publish_section_direct(&db.writer, &ex, &sec, &blocks).await.unwrap();
    extraction::activate(&db.writer, &doc, &ex, "ready").await.unwrap();
    let prov = translations::ProviderRow {
        id: new_uuid(), kind: "ollama".into(), label: "l".into(), endpoint: "http://127.0.0.1:11434".into(),
        account_namespace: "n".into(), keychain_ref: None, key_configured: false, capabilities: serde_json::json!({}),
        enabled: true, cache_epoch: 0, created_at: now_ms(),
    };
    translations::provider_insert(&db.writer, &prov).await.unwrap();
    let prof = super::prepare::create_profile(&db, &prov, "m", "en", "ko", None, None, None, None, None).await.unwrap();
    let b = &blocks[0];
    let mut committed = 0;
    for i in 0..1000 {
        let key = format!("k{i}");
        let (job, _) = translations::job_insert_or_merge(&db.writer, &b.id, &prof.id, &key, &format!("r{i}"), "s", i, 1, &serde_json::json!({})).await.unwrap();
        translations::job_transition(&db.writer, &job.id, "QUEUED", "RUNNING", None, None, true, None).await.unwrap();
        let db1 = db.clone();
        let db2 = db.clone();
        let jid = job.id.clone();
        let jid2 = job.id.clone();
        let bid = b.id.clone();
        let sh = b.source_hash.clone();
        let pid = prof.id.clone();
        let k2 = key.clone();
        let c = tokio::spawn(async move { translations::job_cancel(&db1.writer, &jid, "GENERATION").await.unwrap() });
        let m = tokio::spawn(async move {
            translations::commit_generated(&db2.writer, translations::CommitGenerated {
                job_id: &jid2, block_id: &bid, target_language: "ko", source_hash: &sh, profile_id: &pid, cache_key: &k2,
                descriptor: &serde_json::json!({}), text: "t", metadata: &serde_json::json!({}), is_current_profile: false, force: false,
            })
            .await
        });
        let cancelled = c.await.unwrap();
        let done = m.await.unwrap();
        let state: String = sqlx::query_scalar("select state from translation_jobs where id = ?").bind(&job.id).fetch_one(&db.reader).await.unwrap();
        let cache: i64 = sqlx::query_scalar("select count(*) from translation_cache where cache_key = ?").bind(&key).fetch_one(&db.reader).await.unwrap();
        match (cancelled.is_some(), done.is_ok()) {
            (true, false) => {
                assert_eq!(state, "CANCELLED");
                assert_eq!(cache, 0, "cancel won: no cache pointer");
            }
            (false, true) => {
                assert_eq!(state, "COMPLETED");
                assert_eq!(cache, 1);
                committed += 1;
            }
            other => panic!("invariant violated at {i}: {other:?} state={state}"),
        }
    }
    let revs: i64 = sqlx::query_scalar("select count(*) from translation_revisions").fetch_one(&db.reader).await.unwrap();
    assert_eq!(revs, committed);
    crate::storage::integrity_check(&db.writer).await.unwrap();
}

/// AC05: manual save vs automatic completion 1,000 times — manual pin never lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac05_manual_vs_auto_race_1000() {
    let (db, _d) = test_db().await;
    let (doc, ex, sec, blocks) = crate::storage::repo::tests::seed_doc(&db).await;
    extraction::publish_section_direct(&db.writer, &ex, &sec, &blocks).await.unwrap();
    extraction::activate(&db.writer, &doc, &ex, "ready").await.unwrap();
    let prov = translations::ProviderRow {
        id: new_uuid(), kind: "ollama".into(), label: "l".into(), endpoint: "http://127.0.0.1:11434".into(),
        account_namespace: "n".into(), keychain_ref: None, key_configured: false, capabilities: serde_json::json!({}),
        enabled: true, cache_epoch: 0, created_at: now_ms(),
    };
    translations::provider_insert(&db.writer, &prov).await.unwrap();
    let prof = super::prepare::create_profile(&db, &prov, "m", "en", "ko", None, None, None, None, None).await.unwrap();
    let b = blocks[0].clone();
    let mut manual_saves = 0;
    for i in 0..1000 {
        let key = format!("m{i}");
        let (job, _) = translations::job_insert_or_merge(&db.writer, &b.id, &prof.id, &key, &format!("q{i}"), "s", 0, 1, &serde_json::json!({})).await.unwrap();
        translations::job_transition(&db.writer, &job.id, "QUEUED", "RUNNING", None, None, true, None).await.unwrap();
        let ver = translations::head_get(&db.reader, &b.id, "ko").await.unwrap().version;
        let (db1, db2) = (db.clone(), db.clone());
        let (b1, b2) = (b.clone(), b.clone());
        let jid = job.id.clone();
        let pid = prof.id.clone();
        let save = tokio::spawn(async move { translations::save_manual(&db1.writer, &b1.id, "ko", &b1.source_hash, &format!("manual {i}"), ver, "x").await });
        let comp = tokio::spawn(async move {
            translations::commit_generated(&db2.writer, translations::CommitGenerated {
                job_id: &jid, block_id: &b2.id, target_language: "ko", source_hash: &b2.source_hash, profile_id: &pid, cache_key: &key,
                descriptor: &serde_json::json!({}), text: "auto", metadata: &serde_json::json!({}), is_current_profile: true, force: false,
            })
            .await
        });
        let s = save.await.unwrap();
        comp.await.unwrap().unwrap();
        if s.is_ok() {
            manual_saves += 1;
        }
        let h = translations::head_get(&db.reader, &b.id, "ko").await.unwrap();
        if manual_saves > 0 {
            let m = translations::revision_get(&db.reader, h.manual_revision_id.as_ref().unwrap()).await.unwrap().unwrap();
            assert!(m.origin == "manual", "manual pointer must stay manual at {i}");
        }
    }
    assert!(manual_saves > 0);
    let revs: i64 = sqlx::query_scalar("select count(*) from translation_revisions").fetch_one(&db.reader).await.unwrap();
    assert_eq!(revs, 1000 + manual_saves);
}

/// T14: the app dies while a job streams. After restart the uncommitted preview is gone,
/// the job is CANCELLED/INTERRUPTED (not re-run), and the saved manual edit is restored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t14_crash_during_stream_then_restart() {
    let f = fixture(2, MockConfig::default()).await;
    let b = f.blocks[0].clone();
    translations::save_manual(&f.db.writer, &b.id, "ko", &b.source_hash, "저장된 수정본", 0, "rq").await.unwrap();
    // Block 1 starts streaming and never completes (simulated crash mid-stream).
    f.mock.script([MockStep::FailAfterChunks { chunks: 1, error: ProviderError::Disconnected { maybe_sent: true } }, MockStep::Hang]);
    f.svc.request(&f.s, &f.blocks[1].id, false).await.unwrap();
    f.svc.request(&f.s, &b.id, true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let path = f.db.path.clone();
    let backups = path.parent().unwrap().parent().unwrap().join("backups");
    // "Kill": no shutdown, no cancel. Re-open the same DB file as a new process would.
    let db2 = Db::open(&path, &backups).await.unwrap();
    let svc2 = TranslationService::new(db2.clone(), Arc::new(MemorySecretStore::new()), default_factory());
    svc2.start().await.unwrap();
    let active: i64 = sqlx::query_scalar("select count(*) from translation_jobs where state in ('QUEUED','RUNNING','STREAMING','RETRY_WAIT')").fetch_one(&db2.reader).await.unwrap();
    assert_eq!(active, 0, "no job survives a restart as active");
    let interrupted: i64 = sqlx::query_scalar("select count(*) from translation_jobs where state = 'CANCELLED' and error_code = 'INTERRUPTED'").fetch_one(&db2.reader).await.unwrap();
    assert!(interrupted >= 1);
    // No partial stream became a revision or cache entry.
    let generated: i64 = sqlx::query_scalar("select count(*) from translation_revisions where origin = 'generated'").fetch_one(&db2.reader).await.unwrap();
    assert_eq!(generated, 0);
    let cache: i64 = sqlx::query_scalar("select count(*) from translation_cache").fetch_one(&db2.reader).await.unwrap();
    assert_eq!(cache, 0);
    // The committed manual edit is still the head.
    let h = translations::head_get(&db2.reader, &b.id, "ko").await.unwrap();
    let m = translations::revision_get(&db2.reader, h.manual_revision_id.as_ref().unwrap()).await.unwrap().unwrap();
    assert_eq!(m.text, "저장된 수정본");
    crate::storage::integrity_check(&db2.writer).await.unwrap();
}
