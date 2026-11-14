//! Providers, profiles, revisions, heads, cache pointers, jobs (spec §7, §9.2).

use crate::domain::{new_uuid, now_ms};
use crate::error::{AppError, AppResult, ErrorCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection, SqlitePool};
use ts_rs::TS;

// ---------------------------------------------------------------- providers

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ProviderRow {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub endpoint: String,
    pub account_namespace: String,
    #[serde(skip)]
    #[ts(skip)]
    pub keychain_ref: Option<String>,
    pub key_configured: bool,
    #[ts(type = "Record<string, unknown>")]
    pub capabilities: serde_json::Value,
    pub enabled: bool,
    pub cache_epoch: i64,
    pub created_at: i64,
}

fn provider_from_row(r: &sqlx::sqlite::SqliteRow) -> ProviderRow {
    let kref: Option<String> = r.get("keychain_ref");
    ProviderRow {
        id: r.get("id"),
        kind: r.get("kind"),
        label: r.get("label"),
        endpoint: r.get("endpoint"),
        account_namespace: r.get("account_namespace"),
        key_configured: kref.is_some(),
        keychain_ref: kref,
        capabilities: serde_json::from_str(r.get::<String, _>("capabilities_json").as_str()).unwrap_or_default(),
        enabled: r.get::<i64, _>("enabled") == 1,
        cache_epoch: r.get("cache_epoch"),
        created_at: r.get("created_at"),
    }
}

pub async fn provider_insert(pool: &SqlitePool, p: &ProviderRow) -> AppResult<()> {
    sqlx::query(
        "insert into providers(id, kind, label, endpoint, account_namespace, keychain_ref, capabilities_json, enabled, cache_epoch, created_at)
         values (?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&p.id)
    .bind(&p.kind)
    .bind(&p.label)
    .bind(&p.endpoint)
    .bind(&p.account_namespace)
    .bind(&p.keychain_ref)
    .bind(p.capabilities.to_string())
    .bind(p.enabled as i64)
    .bind(p.cache_epoch)
    .bind(p.created_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn provider_get(pool: &SqlitePool, id: &str) -> AppResult<Option<ProviderRow>> {
    Ok(sqlx::query("select * from providers where id = ?").bind(id).fetch_optional(pool).await?.map(|r| provider_from_row(&r)))
}

pub async fn provider_list(pool: &SqlitePool) -> AppResult<Vec<ProviderRow>> {
    Ok(sqlx::query("select * from providers order by created_at").fetch_all(pool).await?.iter().map(provider_from_row).collect())
}

/// Only label / enabled / keychain ref / capabilities / cache epoch may change in place.
pub async fn provider_update_mutable(
    pool: &SqlitePool,
    id: &str,
    label: Option<&str>,
    enabled: Option<bool>,
    keychain_ref: Option<Option<&str>>,
    capabilities: Option<&serde_json::Value>,
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    if let Some(l) = label {
        sqlx::query("update providers set label = ? where id = ?").bind(l).bind(id).execute(&mut *tx).await?;
    }
    if let Some(e) = enabled {
        sqlx::query("update providers set enabled = ? where id = ?").bind(e as i64).bind(id).execute(&mut *tx).await?;
    }
    if let Some(k) = keychain_ref {
        sqlx::query("update providers set keychain_ref = ? where id = ?").bind(k).bind(id).execute(&mut *tx).await?;
    }
    if let Some(c) = capabilities {
        sqlx::query("update providers set capabilities_json = ? where id = ?").bind(c.to_string()).bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn provider_bump_epoch(pool: &SqlitePool, id: &str) -> AppResult<i64> {
    Ok(sqlx::query_scalar("update providers set cache_epoch = cache_epoch + 1 where id = ? returning cache_epoch")
        .bind(id)
        .fetch_one(pool)
        .await?)
}

// ---------------------------------------------------------------- profiles

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ProfileRow {
    pub id: String,
    pub provider_id: String,
    pub profile_hash: String,
    pub source_language: String,
    pub target_language: String,
    pub model_id: String,
    #[ts(type = "Record<string, unknown>")]
    pub snapshot: serde_json::Value,
    pub created_at: i64,
}

fn profile_from_row(r: &sqlx::sqlite::SqliteRow) -> ProfileRow {
    ProfileRow {
        id: r.get("id"),
        provider_id: r.get("provider_id"),
        profile_hash: r.get("profile_hash"),
        source_language: r.get("source_language"),
        target_language: r.get("target_language"),
        model_id: r.get("model_id"),
        snapshot: serde_json::from_str(r.get::<String, _>("snapshot_json").as_str()).unwrap_or_default(),
        created_at: r.get("created_at"),
    }
}

/// Profiles are immutable; identical effective settings reuse the same row.
pub async fn profile_get_or_create(pool: &SqlitePool, p: &ProfileRow) -> AppResult<ProfileRow> {
    sqlx::query(
        "insert into translation_profiles(id, provider_id, profile_hash, source_language, target_language, model_id, snapshot_json, created_at)
         values (?,?,?,?,?,?,?,?) on conflict(profile_hash) do nothing",
    )
    .bind(&p.id)
    .bind(&p.provider_id)
    .bind(&p.profile_hash)
    .bind(&p.source_language)
    .bind(&p.target_language)
    .bind(&p.model_id)
    .bind(p.snapshot.to_string())
    .bind(p.created_at)
    .execute(pool)
    .await?;
    let r = sqlx::query("select * from translation_profiles where profile_hash = ?").bind(&p.profile_hash).fetch_one(pool).await?;
    Ok(profile_from_row(&r))
}

pub async fn profile_get(pool: &SqlitePool, id: &str) -> AppResult<Option<ProfileRow>> {
    Ok(sqlx::query("select * from translation_profiles where id = ?").bind(id).fetch_optional(pool).await?.map(|r| profile_from_row(&r)))
}

// ---------------------------------------------------------------- block preferences

pub async fn preference_get(pool: &SqlitePool, block_id: &str) -> AppResult<(Option<String>, i64)> {
    let r = sqlx::query("select translation_override, version from block_preferences where block_id = ?")
        .bind(block_id)
        .fetch_optional(pool)
        .await?;
    Ok(r.map(|r| (r.get(0), r.get(1))).unwrap_or((None, 0)))
}

pub async fn preference_set(pool: &SqlitePool, block_id: &str, value: Option<&str>, expected: i64) -> AppResult<i64> {
    let mut tx = pool.begin().await?;
    sqlx::query("insert into block_preferences(block_id, translation_override, version, updated_at) values (?, null, 0, ?) on conflict(block_id) do nothing")
        .bind(block_id)
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
    let n = sqlx::query("update block_preferences set translation_override = ?, version = version + 1, updated_at = ? where block_id = ? and version = ?")
        .bind(value)
        .bind(now_ms())
        .bind(block_id)
        .bind(expected)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n != 1 {
        tx.rollback().await?;
        return Err(AppError::conflict("다른 곳에서 설정이 변경되었습니다."));
    }
    tx.commit().await?;
    Ok(expected + 1)
}

// ---------------------------------------------------------------- revisions & heads

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RevisionRow {
    pub id: String,
    pub block_id: String,
    pub target_language: String,
    pub source_hash: String,
    pub origin: String,
    pub profile_id: Option<String>,
    pub parent_revision_id: Option<String>,
    pub cache_key: Option<String>,
    pub text: String,
    #[ts(type = "Record<string, unknown>")]
    pub metadata: serde_json::Value,
    pub created_at: i64,
}

fn revision_from_row(r: &sqlx::sqlite::SqliteRow) -> RevisionRow {
    RevisionRow {
        id: r.get("id"),
        block_id: r.get("block_id"),
        target_language: r.get("target_language"),
        source_hash: r.get("source_hash"),
        origin: r.get("origin"),
        profile_id: r.get("profile_id"),
        parent_revision_id: r.get("parent_revision_id"),
        cache_key: r.get("cache_key"),
        text: r.get("text"),
        metadata: serde_json::from_str(r.get::<String, _>("metadata_json").as_str()).unwrap_or_default(),
        created_at: r.get("created_at"),
    }
}

#[derive(Debug, Clone, Default, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct HeadRow {
    pub block_id: String,
    pub target_language: String,
    pub manual_revision_id: Option<String>,
    pub generated_revision_id: Option<String>,
    pub version: i64,
}

pub async fn revision_get(pool: &SqlitePool, id: &str) -> AppResult<Option<RevisionRow>> {
    Ok(sqlx::query("select * from translation_revisions where id = ?").bind(id).fetch_optional(pool).await?.map(|r| revision_from_row(&r)))
}

pub async fn head_get(conn: impl sqlx::SqliteExecutor<'_>, block_id: &str, lang: &str) -> AppResult<HeadRow> {
    let r = sqlx::query("select manual_revision_id, generated_revision_id, version from translation_heads where block_id = ? and target_language = ?")
        .bind(block_id)
        .bind(lang)
        .fetch_optional(conn)
        .await?;
    Ok(match r {
        Some(r) => HeadRow { block_id: block_id.into(), target_language: lang.into(), manual_revision_id: r.get(0), generated_revision_id: r.get(1), version: r.get(2) },
        None => HeadRow { block_id: block_id.into(), target_language: lang.into(), ..Default::default() },
    })
}

async fn head_ensure(conn: &mut SqliteConnection, block_id: &str, lang: &str) -> AppResult<()> {
    sqlx::query("insert into translation_heads(block_id, target_language, version, updated_at) values (?,?,0,?) on conflict do nothing")
        .bind(block_id)
        .bind(lang)
        .bind(now_ms())
        .execute(conn)
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn revision_insert(
    conn: &mut SqliteConnection,
    block_id: &str,
    lang: &str,
    source_hash: &str,
    origin: &str,
    profile_id: Option<&str>,
    parent: Option<&str>,
    cache_key: Option<&str>,
    text: &str,
    metadata: &serde_json::Value,
) -> AppResult<String> {
    let id = new_uuid();
    sqlx::query(
        "insert into translation_revisions(id, block_id, target_language, source_hash, origin, profile_id, parent_revision_id, cache_key, text, metadata_json, created_at)
         values (?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(block_id)
    .bind(lang)
    .bind(source_hash)
    .bind(origin)
    .bind(profile_id)
    .bind(parent)
    .bind(cache_key)
    .bind(text)
    .bind(metadata.to_string())
    .bind(now_ms())
    .execute(conn)
    .await?;
    Ok(id)
}

/// Re-index the selected stored translation (manual head, else generated head).
async fn reindex_translation(conn: &mut SqliteConnection, block_id: &str, lang: &str) -> AppResult<()> {
    let head = head_get(&mut *conn, block_id, lang).await?;
    let sel = head.manual_revision_id.or(head.generated_revision_id);
    match sel {
        Some(rev) => {
            let text: String = sqlx::query_scalar("select text from translation_revisions where id = ?").bind(&rev).fetch_one(&mut *conn).await?;
            use unicode_normalization::UnicodeNormalization;
            let text: String = text.nfc().collect();
            sqlx::query(
                "insert into search_entries(document_id, block_id, scope, language, revision_id, text)
                 select e.document_id, ?, 'translation', ?, ?, ? from blocks b join extractions e on e.id = b.extraction_id where b.id = ?
                 on conflict(block_id, scope, language) do update set revision_id = excluded.revision_id, text = excluded.text",
            )
            .bind(block_id)
            .bind(lang)
            .bind(&rev)
            .bind(&text)
            .bind(block_id)
            .execute(&mut *conn)
            .await?;
        }
        None => {
            sqlx::query("delete from search_entries where block_id = ? and scope = 'translation' and language = ?")
                .bind(block_id)
                .bind(lang)
                .execute(&mut *conn)
                .await?;
        }
    }
    Ok(())
}

/// Manual edit: new manual revision + head CAS in one transaction (spec §7.5, §9.2-5).
pub async fn save_manual(
    pool: &SqlitePool,
    block_id: &str,
    lang: &str,
    source_hash: &str,
    text: &str,
    expected_version: i64,
    request_id: &str,
) -> AppResult<(String, i64)> {
    let mut tx = pool.begin().await?;
    head_ensure(&mut tx, block_id, lang).await?;
    let head = head_get(&mut *tx, block_id, lang).await?;
    if head.version != expected_version {
        tx.rollback().await?;
        return Err(AppError::conflict("다른 변경이 먼저 저장되었습니다. 입력 내용은 유지됩니다.").with("currentVersion", head.version));
    }
    let parent = head.manual_revision_id.clone().or(head.generated_revision_id.clone());
    let rev = revision_insert(&mut tx, block_id, lang, source_hash, "manual", None, parent.as_deref(), None, text, &serde_json::json!({"requestId": request_id})).await?;
    let n = sqlx::query("update translation_heads set manual_revision_id = ?, version = version + 1, updated_at = ? where block_id = ? and target_language = ? and version = ?")
        .bind(&rev)
        .bind(now_ms())
        .bind(block_id)
        .bind(lang)
        .bind(expected_version)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n != 1 {
        tx.rollback().await?;
        return Err(AppError::conflict("다른 변경이 먼저 저장되었습니다."));
    }
    reindex_translation(&mut tx, block_id, lang).await?;
    tx.commit().await?;
    Ok((rev, expected_version + 1))
}

/// Apply a revision: generated => release manual pin and select it; manual/restore => create a
/// new `restore` revision and pin it. Never mutates history rows.
pub async fn apply_revision(pool: &SqlitePool, revision_id: &str, current_source_hash: &str, expected_version: i64) -> AppResult<(String, i64)> {
    let rev = revision_get(pool, revision_id).await?.ok_or_else(|| AppError::not_found("번역 이력"))?;
    if rev.source_hash != current_source_hash {
        return Err(AppError::invalid("원문이 다른 번역은 적용할 수 없습니다."));
    }
    let mut tx = pool.begin().await?;
    head_ensure(&mut tx, &rev.block_id, &rev.target_language).await?;
    let head = head_get(&mut *tx, &rev.block_id, &rev.target_language).await?;
    if head.version != expected_version {
        tx.rollback().await?;
        return Err(AppError::conflict("다른 변경이 먼저 저장되었습니다.").with("currentVersion", head.version));
    }
    let applied_id;
    let n = if rev.origin == "generated" {
        applied_id = rev.id.clone();
        sqlx::query("update translation_heads set manual_revision_id = null, generated_revision_id = ?, version = version + 1, updated_at = ? where block_id = ? and target_language = ? and version = ?")
            .bind(&rev.id)
            .bind(now_ms())
            .bind(&rev.block_id)
            .bind(&rev.target_language)
            .bind(expected_version)
            .execute(&mut *tx)
            .await?
            .rows_affected()
    } else {
        let parent = head.manual_revision_id.clone().or(head.generated_revision_id.clone());
        applied_id = revision_insert(&mut tx, &rev.block_id, &rev.target_language, &rev.source_hash, "restore", None, parent.as_deref(), None, &rev.text, &serde_json::json!({"restoredFrom": rev.id})).await?;
        sqlx::query("update translation_heads set manual_revision_id = ?, version = version + 1, updated_at = ? where block_id = ? and target_language = ? and version = ?")
            .bind(&applied_id)
            .bind(now_ms())
            .bind(&rev.block_id)
            .bind(&rev.target_language)
            .bind(expected_version)
            .execute(&mut *tx)
            .await?
            .rows_affected()
    };
    if n != 1 {
        tx.rollback().await?;
        return Err(AppError::conflict("다른 변경이 먼저 저장되었습니다."));
    }
    reindex_translation(&mut tx, &rev.block_id, &rev.target_language).await?;
    tx.commit().await?;
    Ok((applied_id, expected_version + 1))
}

pub async fn history(pool: &SqlitePool, block_id: &str, lang: &str, cursor: Option<&str>, limit: u32) -> AppResult<(Vec<RevisionRow>, Option<String>)> {
    let limit = limit.clamp(1, 50) as i64;
    let (cts, cid) = match cursor.and_then(|c| c.split_once(':')) {
        Some((t, id)) => (t.parse::<i64>().unwrap_or(i64::MAX), id.to_string()),
        None => (i64::MAX, String::new()),
    };
    let rows = sqlx::query(
        "select * from translation_revisions where block_id = ? and target_language = ?
         and (created_at < ? or (created_at = ? and id > ?)) order by created_at desc, id asc limit ?",
    )
    .bind(block_id)
    .bind(lang)
    .bind(cts)
    .bind(cts)
    .bind(&cid)
    .bind(limit + 1)
    .fetch_all(pool)
    .await?;
    let mut v: Vec<RevisionRow> = rows.iter().map(revision_from_row).collect();
    let next = if v.len() as i64 > limit {
        v.truncate(limit as usize);
        v.last().map(|r| format!("{}:{}", r.created_at, r.id))
    } else {
        None
    };
    Ok((v, next))
}

// ---------------------------------------------------------------- cache

pub async fn cache_lookup(pool: &SqlitePool, cache_key: &str) -> AppResult<Option<RevisionRow>> {
    let r = sqlx::query("select r.* from translation_cache c join translation_revisions r on r.id = c.revision_id where c.cache_key = ? and r.origin = 'generated'")
        .bind(cache_key)
        .fetch_optional(pool)
        .await?;
    Ok(r.map(|r| revision_from_row(&r)))
}

pub async fn cache_touch(pool: &SqlitePool, cache_key: &str) -> AppResult<()> {
    sqlx::query("update translation_cache set last_accessed_at = ? where cache_key = ?").bind(now_ms()).bind(cache_key).execute(pool).await?;
    Ok(())
}

/// Cache hit for the current profile: select it as the generated candidate (never touches manual).
pub async fn select_generated_candidate(pool: &SqlitePool, block_id: &str, lang: &str, revision_id: &str) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    head_ensure(&mut tx, block_id, lang).await?;
    let n = sqlx::query(
        "update translation_heads set generated_revision_id = ?, version = version + 1, updated_at = ?
         where block_id = ? and target_language = ? and coalesce(generated_revision_id, '') <> ?",
    )
    .bind(revision_id)
    .bind(now_ms())
    .bind(block_id)
    .bind(lang)
    .bind(revision_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n > 0 {
        reindex_translation(&mut tx, block_id, lang).await?;
    }
    tx.commit().await?;
    Ok(n > 0)
}

pub async fn cache_invalidate_document(pool: &SqlitePool, document_id: &str) -> AppResult<u64> {
    Ok(sqlx::query("delete from translation_cache where block_id in (select b.id from blocks b join extractions e on e.id = b.extraction_id where e.document_id = ?)")
        .bind(document_id)
        .execute(pool)
        .await?
        .rows_affected())
}

pub async fn cache_invalidate_profile(pool: &SqlitePool, profile_id: &str) -> AppResult<u64> {
    Ok(sqlx::query("delete from translation_cache where revision_id in (select id from translation_revisions where profile_id = ?)")
        .bind(profile_id)
        .execute(pool)
        .await?
        .rows_affected())
}

// ---------------------------------------------------------------- jobs

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct JobRow {
    pub id: String,
    pub block_id: String,
    pub profile_id: String,
    pub cache_key: String,
    pub request_id: String,
    pub session_id: String,
    pub generation: i64,
    pub priority: i64,
    pub state: String,
    pub attempt_count: i64,
    pub last_part_index: i64,
    pub next_attempt_at: Option<i64>,
    pub result_revision_id: Option<String>,
    pub error_code: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn job_from_row(r: &sqlx::sqlite::SqliteRow) -> JobRow {
    JobRow {
        id: r.get("id"),
        block_id: r.get("block_id"),
        profile_id: r.get("profile_id"),
        cache_key: r.get("cache_key"),
        request_id: r.get("request_id"),
        session_id: r.get("session_id"),
        generation: r.get("generation"),
        priority: r.get("priority"),
        state: r.get("state"),
        attempt_count: r.get("attempt_count"),
        last_part_index: r.get("last_part_index"),
        next_attempt_at: r.get("next_attempt_at"),
        result_revision_id: r.get("result_revision_id"),
        error_code: r.get("error_code"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

pub const ACTIVE_STATES: [&str; 4] = ["QUEUED", "RUNNING", "STREAMING", "RETRY_WAIT"];

pub fn transition_allowed(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("QUEUED", "RUNNING")
            | ("RUNNING", "STREAMING")
            | ("RUNNING", "COMPLETED")
            | ("STREAMING", "COMPLETED")
            | ("RUNNING", "RETRY_WAIT")
            | ("STREAMING", "RETRY_WAIT")
            | ("RETRY_WAIT", "QUEUED")
            | ("QUEUED", "CANCELLED")
            | ("RUNNING", "CANCELLED")
            | ("STREAMING", "CANCELLED")
            | ("RETRY_WAIT", "CANCELLED")
            | ("RUNNING", "FAILED")
            | ("STREAMING", "FAILED")
            | ("RETRY_WAIT", "FAILED")
    )
}

/// Insert a job or merge into the active job for the same cache key (promoting priority).
/// Returns (job, created).
#[allow(clippy::too_many_arguments)]
pub async fn job_insert_or_merge(
    pool: &SqlitePool,
    block_id: &str,
    profile_id: &str,
    cache_key: &str,
    request_id: &str,
    session_id: &str,
    generation: i64,
    priority: i64,
    snapshot: &serde_json::Value,
) -> AppResult<(JobRow, bool)> {
    let mut tx = pool.begin().await?;
    if let Some(r) = sqlx::query("select * from translation_jobs where cache_key = ? and state in ('QUEUED','RUNNING','STREAMING','RETRY_WAIT')")
        .bind(cache_key)
        .fetch_optional(&mut *tx)
        .await?
    {
        let mut j = job_from_row(&r);
        if priority < j.priority || j.session_id != session_id || j.generation != generation {
            sqlx::query("update translation_jobs set priority = min(priority, ?), session_id = ?, generation = ?, updated_at = ? where id = ?")
                .bind(priority)
                .bind(session_id)
                .bind(generation)
                .bind(now_ms())
                .bind(&j.id)
                .execute(&mut *tx)
                .await?;
            j.priority = j.priority.min(priority);
            j.session_id = session_id.into();
            j.generation = generation;
        }
        tx.commit().await?;
        return Ok((j, false));
    }
    let id = new_uuid();
    let now = now_ms();
    sqlx::query(
        "insert into translation_jobs(id, block_id, profile_id, cache_key, request_id, session_id, generation, priority, state, request_snapshot_json, created_at, updated_at)
         values (?,?,?,?,?,?,?,?, 'QUEUED', ?,?,?)",
    )
    .bind(&id)
    .bind(block_id)
    .bind(profile_id)
    .bind(cache_key)
    .bind(request_id)
    .bind(session_id)
    .bind(generation)
    .bind(priority)
    .bind(snapshot.to_string())
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let r = sqlx::query("select * from translation_jobs where id = ?").bind(&id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok((job_from_row(&r), true))
}

pub async fn job_get(pool: &SqlitePool, id: &str) -> AppResult<Option<JobRow>> {
    Ok(sqlx::query("select * from translation_jobs where id = ?").bind(id).fetch_optional(pool).await?.map(|r| job_from_row(&r)))
}

pub async fn job_snapshot(pool: &SqlitePool, id: &str) -> AppResult<serde_json::Value> {
    let s: String = sqlx::query_scalar("select request_snapshot_json from translation_jobs where id = ?").bind(id).fetch_one(pool).await?;
    Ok(serde_json::from_str(&s)?)
}

/// Guarded state transition. Returns false if the job is no longer in `from` (lost race).
pub async fn job_transition(
    pool: &SqlitePool,
    id: &str,
    from: &str,
    to: &str,
    error_code: Option<&str>,
    next_attempt_at: Option<i64>,
    bump_attempt: bool,
    last_part_index: Option<i64>,
) -> AppResult<bool> {
    if !transition_allowed(from, to) {
        return Err(AppError::internal(format!("invalid job transition {from}->{to}")));
    }
    let n = sqlx::query(
        "update translation_jobs set state = ?, error_code = coalesce(?, error_code), next_attempt_at = ?,
           attempt_count = attempt_count + ?, last_part_index = coalesce(?, last_part_index), updated_at = ?
         where id = ? and state = ?",
    )
    .bind(to)
    .bind(error_code)
    .bind(next_attempt_at)
    .bind(bump_attempt as i64)
    .bind(last_part_index)
    .bind(now_ms())
    .bind(id)
    .bind(from)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Cancel any non-terminal job. Returns the state it was cancelled from, if any.
pub async fn job_cancel(pool: &SqlitePool, id: &str, reason: &str) -> AppResult<Option<String>> {
    let mut tx = pool.begin().await?;
    let prev: Option<String> = sqlx::query_scalar(
        "select state from translation_jobs where id = ? and state in ('QUEUED','RUNNING','STREAMING','RETRY_WAIT')",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if prev.is_some() {
        sqlx::query("update translation_jobs set state = 'CANCELLED', error_code = ?, next_attempt_at = null, updated_at = ? where id = ?")
            .bind(reason)
            .bind(now_ms())
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(prev)
}

/// App restart: every non-terminal job becomes CANCELLED/INTERRUPTED (spec §7.3).
pub async fn jobs_interrupt_all(pool: &SqlitePool) -> AppResult<u64> {
    Ok(sqlx::query("update translation_jobs set state = 'CANCELLED', error_code = 'INTERRUPTED', next_attempt_at = null, updated_at = ? where state in ('QUEUED','RUNNING','STREAMING','RETRY_WAIT')")
        .bind(now_ms())
        .execute(pool)
        .await?
        .rows_affected())
}

pub async fn jobs_active_for_blocks(pool: &SqlitePool, block_ids: &[String]) -> AppResult<Vec<JobRow>> {
    if block_ids.is_empty() {
        return Ok(vec![]);
    }
    let placeholders = vec!["?"; block_ids.len()].join(",");
    let sql = format!("select * from translation_jobs where block_id in ({placeholders}) and state in ('QUEUED','RUNNING','STREAMING','RETRY_WAIT')");
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.clone()));
    for b in block_ids {
        q = q.bind(b);
    }
    Ok(q.fetch_all(pool).await?.iter().map(job_from_row).collect())
}

pub async fn last_failed_job(pool: &SqlitePool, block_id: &str, profile_id: &str) -> AppResult<Option<JobRow>> {
    Ok(sqlx::query("select * from translation_jobs where block_id = ? and profile_id = ? order by created_at desc limit 1")
        .bind(block_id)
        .bind(profile_id)
        .fetch_optional(pool)
        .await?
        .map(|r| job_from_row(&r))
        .filter(|j| j.state == "FAILED"))
}

pub struct CommitGenerated<'a> {
    pub job_id: &'a str,
    pub block_id: &'a str,
    pub target_language: &'a str,
    pub source_hash: &'a str,
    pub profile_id: &'a str,
    pub cache_key: &'a str,
    pub descriptor: &'a serde_json::Value,
    pub text: &'a str,
    pub metadata: &'a serde_json::Value,
    /// Whether this job's profile is still the current one for its session.
    pub is_current_profile: bool,
    /// Force re-translation: always move the cache pointer.
    pub force: bool,
}

/// Completion transaction (spec §9.2-4): job COMPLETED, new revision, cache pointer, head CAS,
/// search index. Aborts with CANCELLED if a cancel committed first.
pub async fn commit_generated(pool: &SqlitePool, c: CommitGenerated<'_>) -> AppResult<(String, HeadRow)> {
    let mut tx = pool.begin().await?;
    let state: Option<String> = sqlx::query_scalar("select state from translation_jobs where id = ?").bind(c.job_id).fetch_optional(&mut *tx).await?;
    match state.as_deref() {
        Some("RUNNING") | Some("STREAMING") => {}
        _ => {
            tx.rollback().await?;
            return Err(AppError::new(ErrorCode::Cancelled, "번역 작업이 취소되었습니다."));
        }
    }
    let current_source: Option<String> = sqlx::query_scalar("select source_hash from blocks where id = ?").bind(c.block_id).fetch_optional(&mut *tx).await?;
    let profile_lang: Option<String> = sqlx::query_scalar("select target_language from translation_profiles where id = ?").bind(c.profile_id).fetch_optional(&mut *tx).await?;
    if current_source.as_deref() != Some(c.source_hash) || profile_lang.as_deref() != Some(c.target_language) {
        tx.rollback().await?;
        return Err(AppError::invalid("원문 또는 대상 언어가 일치하지 않습니다."));
    }
    head_ensure(&mut tx, c.block_id, c.target_language).await?;
    let head = head_get(&mut *tx, c.block_id, c.target_language).await?;
    let parent = head.generated_revision_id.clone();
    let rev = revision_insert(&mut tx, c.block_id, c.target_language, c.source_hash, "generated", Some(c.profile_id), parent.as_deref(), Some(c.cache_key), c.text, c.metadata).await?;
    sqlx::query(
        "insert into translation_cache(cache_key, block_id, target_language, revision_id, descriptor_json, created_at, last_accessed_at)
         values (?,?,?,?,?,?,?)
         on conflict(cache_key) do update set revision_id = excluded.revision_id, last_accessed_at = excluded.last_accessed_at",
    )
    .bind(c.cache_key)
    .bind(c.block_id)
    .bind(c.target_language)
    .bind(&rev)
    .bind(c.descriptor.to_string())
    .bind(now_ms())
    .bind(now_ms())
    .execute(&mut *tx)
    .await?;
    let _ = c.force;
    // Generated candidate never replaces manual; stale-profile results never override a newer one.
    if c.is_current_profile || head.generated_revision_id.is_none() {
        sqlx::query("update translation_heads set generated_revision_id = ?, version = version + 1, updated_at = ? where block_id = ? and target_language = ? and version = ?")
            .bind(&rev)
            .bind(now_ms())
            .bind(c.block_id)
            .bind(c.target_language)
            .bind(head.version)
            .execute(&mut *tx)
            .await?;
    }
    reindex_translation(&mut tx, c.block_id, c.target_language).await?;
    sqlx::query("update translation_jobs set state = 'COMPLETED', result_revision_id = ?, next_attempt_at = null, updated_at = ? where id = ?")
        .bind(&rev)
        .bind(now_ms())
        .bind(c.job_id)
        .execute(&mut *tx)
        .await?;
    let new_head = head_get(&mut *tx, c.block_id, c.target_language).await?;
    tx.commit().await?;
    Ok((rev, new_head))
}

/// Display summary for many blocks at once.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DisplayRevision {
    pub revision_id: String,
    pub text: String,
    pub origin: String,
    pub profile_id: Option<String>,
    pub cache_key: Option<String>,
    /// e.g. "script:cyrillic" or "language" when the output looks wrong; a hint, not a failure.
    pub warning: Option<String>,
}

pub fn warning_of(metadata: &serde_json::Value) -> Option<String> {
    if let Some(s) = metadata.get("scriptWarning").and_then(|v| v.as_str()) {
        return Some(format!("script:{s}"));
    }
    if metadata.get("languageWarning").and_then(|v| v.as_bool()) == Some(true) {
        return Some("language".into());
    }
    None
}

pub async fn heads_with_text(pool: &SqlitePool, block_ids: &[String], lang: &str) -> AppResult<Vec<(HeadRow, Option<DisplayRevision>, Option<DisplayRevision>)>> {
    let mut out = Vec::with_capacity(block_ids.len());
    for chunk in block_ids.chunks(200) {
        let ph = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "select h.block_id, h.manual_revision_id, h.generated_revision_id, h.version,
                    m.text as mtext, m.origin as morigin, g.text as gtext, g.profile_id as gprofile, g.cache_key as gkey, g.metadata_json as gmeta
             from translation_heads h
             left join translation_revisions m on m.id = h.manual_revision_id
             left join translation_revisions g on g.id = h.generated_revision_id
             where h.target_language = ? and h.block_id in ({ph})"
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.clone())).bind(lang);
        for b in chunk {
            q = q.bind(b);
        }
        for r in q.fetch_all(pool).await? {
            let head = HeadRow { block_id: r.get(0), target_language: lang.into(), manual_revision_id: r.get(1), generated_revision_id: r.get(2), version: r.get(3) };
            let manual = head.manual_revision_id.clone().map(|id| DisplayRevision { revision_id: id, text: r.get("mtext"), origin: r.get("morigin"), profile_id: None, cache_key: None, warning: None });
            let generated = head.generated_revision_id.clone().map(|id| DisplayRevision { revision_id: id, text: r.get("gtext"), origin: "generated".into(), profile_id: r.get("gprofile"), cache_key: r.get("gkey"), warning: r.get::<Option<String>, _>("gmeta").and_then(|m| serde_json::from_str::<serde_json::Value>(&m).ok()).and_then(|m| warning_of(&m)) });
            out.push((head, manual, generated));
        }
    }
    Ok(out)
}
