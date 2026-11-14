//! Positions, bookmarks, settings, IPC receipts, search, file journal, document deletion.

use crate::domain::{new_uuid, now_ms, ReaderAnchor};
use crate::error::{AppError, AppResult};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use ts_rs::TS;

// ---------------------------------------------------------------- positions

pub async fn position_get(pool: &SqlitePool, document_id: &str) -> AppResult<Option<(ReaderAnchor, serde_json::Value, i64)>> {
    let r = sqlx::query("select anchor_json, view_json, version from reading_positions where document_id = ?")
        .bind(document_id)
        .fetch_optional(pool)
        .await?;
    Ok(match r {
        Some(r) => {
            let a: ReaderAnchor = serde_json::from_str(r.get::<String, _>(0).as_str())?;
            Some((a, serde_json::from_str(r.get::<String, _>(1).as_str()).unwrap_or_default(), r.get(2)))
        }
        None => None,
    })
}

/// Last-writer-wins guarded by version; a stale version returns CONFLICT with the current one.
pub async fn position_save(pool: &SqlitePool, document_id: &str, anchor: &ReaderAnchor, view: &serde_json::Value, expected: i64) -> AppResult<i64> {
    let mut tx = pool.begin().await?;
    sqlx::query("insert into reading_positions(document_id, anchor_json, view_json, version, updated_at) values (?,?,?,0,?) on conflict do nothing")
        .bind(document_id)
        .bind(serde_json::to_string(anchor)?)
        .bind(view.to_string())
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
    let n = sqlx::query("update reading_positions set anchor_json = ?, view_json = ?, version = version + 1, updated_at = ? where document_id = ? and version = ?")
        .bind(serde_json::to_string(anchor)?)
        .bind(view.to_string())
        .bind(now_ms())
        .bind(document_id)
        .bind(expected)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n != 1 {
        let cur: i64 = sqlx::query_scalar("select version from reading_positions where document_id = ?").bind(document_id).fetch_one(&mut *tx).await?;
        tx.rollback().await?;
        return Err(AppError::conflict("위치가 다른 곳에서 갱신되었습니다.").with("currentVersion", cur));
    }
    tx.commit().await?;
    Ok(expected + 1)
}

// ---------------------------------------------------------------- bookmarks

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BookmarkRow {
    pub id: String,
    pub document_id: String,
    pub anchor: ReaderAnchor,
    pub label: String,
    pub created_at: i64,
}

pub async fn bookmark_put(pool: &SqlitePool, id: Option<&str>, document_id: &str, anchor: &ReaderAnchor, label: &str) -> AppResult<BookmarkRow> {
    let id = id.map(|s| s.to_string()).unwrap_or_else(new_uuid);
    let now = now_ms();
    sqlx::query(
        "insert into bookmarks(id, document_id, anchor_json, label, created_at) values (?,?,?,?,?)
         on conflict(id) do update set label = excluded.label, anchor_json = excluded.anchor_json
         where bookmarks.document_id = excluded.document_id",
    )
    .bind(&id)
    .bind(document_id)
    .bind(serde_json::to_string(anchor)?)
    .bind(label)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(BookmarkRow { id, document_id: document_id.into(), anchor: anchor.clone(), label: label.into(), created_at: now })
}

pub async fn bookmark_delete(pool: &SqlitePool, document_id: &str, id: &str) -> AppResult<bool> {
    Ok(sqlx::query("delete from bookmarks where id = ? and document_id = ?").bind(id).bind(document_id).execute(pool).await?.rows_affected() == 1)
}

pub async fn bookmark_list(pool: &SqlitePool, document_id: &str) -> AppResult<Vec<BookmarkRow>> {
    let rows = sqlx::query("select * from bookmarks where document_id = ? order by created_at").bind(document_id).fetch_all(pool).await?;
    rows.iter()
        .map(|r| {
            Ok(BookmarkRow {
                id: r.get("id"),
                document_id: r.get("document_id"),
                anchor: serde_json::from_str(r.get::<String, _>("anchor_json").as_str())?,
                label: r.get("label"),
                created_at: r.get("created_at"),
            })
        })
        .collect()
}

// ---------------------------------------------------------------- annotations

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AnnotationRow {
    pub id: String,
    pub document_id: String,
    /// Block in the *current* extraction (re-resolved by source hash when ids changed);
    /// `None` when the paragraph no longer exists.
    pub block_id: Option<String>,
    pub section_id: Option<String>,
    pub side: String,
    pub target_language: Option<String>,
    pub quote: String,
    pub occurrence: u32,
    pub color: String,
    pub note: String,
    #[ts(type = "Record<string, unknown>")]
    pub location: serde_json::Value,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
}

pub struct AnnotationInput<'a> {
    pub id: Option<&'a str>,
    pub document_id: &'a str,
    pub block_id: &'a str,
    pub side: &'a str,
    pub target_language: Option<&'a str>,
    pub quote: &'a str,
    pub occurrence: u32,
    pub color: &'a str,
    pub note: &'a str,
}

/// Create, or (with `id`) change colour / note of, an annotation. The block must belong to
/// the document; its source hash and location are recorded for later re-anchoring.
pub async fn annotation_put(pool: &SqlitePool, a: AnnotationInput<'_>) -> AppResult<String> {
    let now = now_ms();
    if let Some(id) = a.id {
        let n = sqlx::query("update annotations set color = ?, note = ?, updated_at = ? where id = ? and document_id = ?")
            .bind(a.color)
            .bind(a.note)
            .bind(now)
            .bind(id)
            .bind(a.document_id)
            .execute(pool)
            .await?
            .rows_affected();
        return if n == 1 { Ok(id.to_string()) } else { Err(AppError::not_found("메모")) };
    }
    let row: Option<(String, String)> = sqlx::query_as(
        "select b.source_hash, b.location_json from blocks b join extractions e on e.id = b.extraction_id where b.id = ? and e.document_id = ?",
    )
    .bind(a.block_id)
    .bind(a.document_id)
    .fetch_optional(pool)
    .await?;
    let Some((source_hash, location)) = row else { return Err(AppError::not_found("문단")) };
    let id = new_uuid();
    sqlx::query(
        "insert into annotations(id, document_id, block_id, source_hash, side, target_language, quote, occurrence, color, note, location_json, created_at, updated_at)
         values (?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(a.document_id)
    .bind(a.block_id)
    .bind(source_hash)
    .bind(a.side)
    .bind(a.target_language)
    .bind(a.quote)
    .bind(a.occurrence as i64)
    .bind(a.color)
    .bind(a.note)
    .bind(location)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn annotation_delete(pool: &SqlitePool, document_id: &str, id: &str) -> AppResult<bool> {
    Ok(sqlx::query("delete from annotations where id = ? and document_id = ?").bind(id).bind(document_id).execute(pool).await?.rows_affected() == 1)
}

/// All annotations of a document, each resolved to a block of the active extraction: the
/// original block when it is still there, else the first block with the same source text.
pub async fn annotation_list(pool: &SqlitePool, document_id: &str) -> AppResult<Vec<AnnotationRow>> {
    let rows = sqlx::query(
        "select a.*,
                coalesce(
                  (select b.id from blocks b where b.id = a.block_id and b.extraction_id = d.active_extraction_id),
                  (select b.id from blocks b where b.extraction_id = d.active_extraction_id and b.source_hash = a.source_hash order by b.ordinal limit 1)
                ) as cur_block
         from annotations a join documents d on d.id = a.document_id
         where a.document_id = ? order by a.created_at",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let block_id: Option<String> = r.get("cur_block");
        let section_id: Option<String> = match &block_id {
            Some(b) => sqlx::query_scalar("select section_id from blocks where id = ?").bind(b).fetch_optional(pool).await?,
            None => None,
        };
        out.push(AnnotationRow {
            id: r.get("id"),
            document_id: r.get("document_id"),
            block_id,
            section_id,
            side: r.get("side"),
            target_language: r.get("target_language"),
            quote: r.get("quote"),
            occurrence: r.get::<i64, _>("occurrence") as u32,
            color: r.get("color"),
            note: r.get("note"),
            location: serde_json::from_str(r.get::<String, _>("location_json").as_str()).unwrap_or(serde_json::Value::Null),
            created_at: r.get("created_at"),
            updated_at: r.get("updated_at"),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- settings

pub async fn setting_get(pool: &SqlitePool, key: &str) -> AppResult<Option<(serde_json::Value, i64)>> {
    let r = sqlx::query("select value_json, version from settings where key = ?").bind(key).fetch_optional(pool).await?;
    Ok(r.map(|r| (serde_json::from_str(r.get::<String, _>(0).as_str()).unwrap_or_default(), r.get(1))))
}

pub async fn settings_all(pool: &SqlitePool) -> AppResult<Vec<(String, serde_json::Value, i64)>> {
    let rows = sqlx::query("select key, value_json, version from settings").fetch_all(pool).await?;
    Ok(rows.iter().map(|r| (r.get(0), serde_json::from_str(r.get::<String, _>(1).as_str()).unwrap_or_default(), r.get(2))).collect())
}

pub async fn setting_put(pool: &SqlitePool, key: &str, value: &serde_json::Value, expected: Option<i64>) -> AppResult<i64> {
    let mut tx = pool.begin().await?;
    let cur: Option<i64> = sqlx::query_scalar("select version from settings where key = ?").bind(key).fetch_optional(&mut *tx).await?;
    if let (Some(e), Some(c)) = (expected, cur) {
        if e != c {
            tx.rollback().await?;
            return Err(AppError::conflict("설정이 다른 곳에서 변경되었습니다.").with("currentVersion", c));
        }
    }
    let v = cur.map(|c| c + 1).unwrap_or(0);
    sqlx::query("insert into settings(key, value_json, version, updated_at) values (?,?,?,?) on conflict(key) do update set value_json = excluded.value_json, version = excluded.version, updated_at = excluded.updated_at")
        .bind(key)
        .bind(value.to_string())
        .bind(v)
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(v)
}

// ---------------------------------------------------------------- IPC receipts

pub enum Receipt {
    New,
    Replay(serde_json::Value),
}

/// Same requestId + same payload => replay stored result; different payload => CONFLICT.
pub async fn receipt_check(pool: &SqlitePool, request_id: &str, command: &str, payload_hash: &str) -> AppResult<Receipt> {
    let r = sqlx::query("select command, payload_hash, result_json from ipc_receipts where request_id = ?")
        .bind(request_id)
        .fetch_optional(pool)
        .await?;
    match r {
        None => Ok(Receipt::New),
        Some(r) => {
            let c: String = r.get(0);
            let h: String = r.get(1);
            if c != command || h != payload_hash {
                return Err(AppError::conflict("같은 requestId에 다른 요청이 전송되었습니다."));
            }
            Ok(Receipt::Replay(serde_json::from_str(r.get::<String, _>(2).as_str())?))
        }
    }
}

pub async fn receipt_store(pool: &SqlitePool, request_id: &str, command: &str, payload_hash: &str, result: &serde_json::Value) -> AppResult<()> {
    sqlx::query("insert into ipc_receipts(request_id, command, payload_hash, result_json, created_at) values (?,?,?,?,?) on conflict(request_id) do nothing")
        .bind(request_id)
        .bind(command)
        .bind(payload_hash)
        .bind(result.to_string())
        .bind(now_ms())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn receipts_prune(pool: &SqlitePool, older_than_ms: i64) -> AppResult<()> {
    sqlx::query("delete from ipc_receipts where created_at < ?").bind(now_ms() - older_than_ms).execute(pool).await?;
    Ok(())
}

// ---------------------------------------------------------------- search

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SearchHit {
    pub block_id: String,
    pub section_id: String,
    pub section_ordinal: i64,
    pub block_ordinal: i64,
    pub scope: String,
    pub language: String,
    /// UTF-16 offsets of the first match within `text`.
    pub match_start_u16: u32,
    pub match_end_u16: u32,
    pub snippet: String,
    pub snippet_match_start_u16: u32,
    pub snippet_match_end_u16: u32,
}

/// Escape a user query into a single FTS5 string token (never raw FTS syntax).
pub fn fts_phrase(q: &str) -> String {
    format!("\"{}\"", q.replace('"', "\"\""))
}

fn find_u16(hay: &str, needle: &str) -> Option<(u32, u32)> {
    let hl = hay.to_lowercase();
    let nl = needle.to_lowercase();
    // lowercase may change lengths for some scripts; fall back to exact search.
    let (h, n) = if hl.len() == hay.len() && nl.len() == needle.len() { (hl.as_str(), nl.as_str()) } else { (hay, needle) };
    let b = h.find(n)?;
    let s = crate::domain::utf16::byte_to_u16(hay, b)?;
    Some((s, s + crate::domain::utf16::len_u16(needle)))
}

fn snippet(text: &str, start: u32, end: u32) -> (String, u32, u32) {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut s = start.saturating_sub(40) as usize;
    let mut e = ((end + 60) as usize).min(units.len());
    // don't split surrogates
    while s > 0 && (0xDC00..0xE000).contains(&units[s]) {
        s -= 1;
    }
    while e < units.len() && (0xDC00..0xE000).contains(&units[e]) {
        e += 1;
    }
    let snip = String::from_utf16_lossy(&units[s..e]);
    (snip, start - s as u32, end - s as u32)
}

/// Search within one document's active extraction. ≥3 chars => trigram FTS, otherwise
/// bounded substring scan with bound parameters (spec §9.2).
pub async fn search(
    pool: &SqlitePool,
    document_id: &str,
    extraction_id: &str,
    query: &str,
    scope: &str,
    language: Option<&str>,
    offset: u32,
    limit: u32,
) -> AppResult<(Vec<SearchHit>, Option<u32>, bool)> {
    use unicode_normalization::UnicodeNormalization;
    let q: String = query.nfc().collect::<String>().trim().to_string();
    if q.is_empty() || q.chars().count() > 256 {
        return Err(AppError::invalid("검색어는 1~256자여야 합니다."));
    }
    let limit = limit.clamp(1, 50) as i64;
    let use_fts = q.chars().count() >= 3;
    let scope_filter = match scope {
        "source" => "e.scope = 'source'",
        "translation" => "e.scope = 'translation'",
        _ => "1=1",
    };
    let lang_filter = if language.is_some() { "and (e.scope = 'source' or e.language = ?)" } else { "" };
    let base = format!(
        "select e.block_id, b.section_id, s.ordinal as so, b.ordinal as bo, e.scope, e.language, e.text
         from search_entries e join blocks b on b.id = e.block_id join sections s on s.id = b.section_id
         where e.document_id = ? and b.extraction_id = ? and {scope_filter} {lang_filter}"
    );
    let sql = if use_fts {
        format!("{base} and e.id in (select rowid from search_fts where search_fts match ?) order by s.ordinal, b.ordinal, e.scope limit ? offset ?")
    } else {
        format!("{base} and instr(lower(e.text), lower(?)) > 0 order by s.ordinal, b.ordinal, e.scope limit ? offset ?")
    };
    let mut qq = sqlx::query(sqlx::AssertSqlSafe(sql.clone())).bind(document_id).bind(extraction_id);
    if let Some(l) = language {
        qq = qq.bind(l);
    }
    qq = qq.bind(if use_fts { fts_phrase(&q) } else { q.clone() }).bind(limit + 1).bind(offset as i64);
    let rows = qq.fetch_all(pool).await?;
    let mut hits = Vec::new();
    for r in rows.iter().take(limit as usize) {
        let text: String = r.get("text");
        let (ms, me) = find_u16(&text, &q).unwrap_or((0, 0));
        let (snip, ss, se) = snippet(&text, ms, me);
        hits.push(SearchHit {
            block_id: r.get(0),
            section_id: r.get(1),
            section_ordinal: r.get(2),
            block_ordinal: r.get(3),
            scope: r.get(4),
            language: r.get(5),
            match_start_u16: ms,
            match_end_u16: me,
            snippet: snip,
            snippet_match_start_u16: ss,
            snippet_match_end_u16: se,
        });
    }
    let next = if rows.len() as i64 > limit { Some(offset + limit as u32) } else { None };
    Ok((hits, next, use_fts))
}

// ---------------------------------------------------------------- file operations journal

pub async fn fileop_begin(pool: &SqlitePool, document_id: &str, kind: &str, manifest: &serde_json::Value) -> AppResult<String> {
    let id = new_uuid();
    sqlx::query("insert into file_operations(id, document_id, kind, state, manifest_json, created_at, updated_at) values (?,?,?, 'planned', ?,?,?)")
        .bind(&id)
        .bind(document_id)
        .bind(kind)
        .bind(manifest.to_string())
        .bind(now_ms())
        .bind(now_ms())
        .execute(pool)
        .await?;
    Ok(id)
}

pub async fn fileop_set_state(pool: &SqlitePool, id: &str, state: &str) -> AppResult<()> {
    sqlx::query("update file_operations set state = ?, updated_at = ? where id = ?").bind(state).bind(now_ms()).bind(id).execute(pool).await?;
    Ok(())
}

pub async fn fileops_unfinished(pool: &SqlitePool) -> AppResult<Vec<(String, String, String, String, serde_json::Value)>> {
    let rows = sqlx::query("select id, document_id, kind, state, manifest_json from file_operations where state not in ('complete','failed')")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), serde_json::from_str(r.get::<String, _>(4).as_str()).unwrap_or_default())).collect())
}

/// Remove every row of a document inside one transaction, in FK order (spec §9.3).
pub async fn delete_document_rows(pool: &SqlitePool, document_id: &str) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    let blocks_sub = "select b.id from blocks b join extractions e on e.id = b.extraction_id where e.document_id = ?";
    for sql in [
        format!("delete from search_entries where document_id = ?"),
        format!("delete from translation_jobs where block_id in ({blocks_sub})"),
        format!("delete from translation_cache where block_id in ({blocks_sub})"),
        format!("delete from translation_heads where block_id in ({blocks_sub})"),
        // revisions reference each other via parent; null the parents first.
        format!("update translation_revisions set parent_revision_id = null where block_id in ({blocks_sub})"),
        format!("delete from translation_revisions where block_id in ({blocks_sub})"),
        format!("delete from block_preferences where block_id in ({blocks_sub})"),
        format!("update blocks set parent_id = null where extraction_id in (select id from extractions where document_id = ?)"),
        "update documents set active_extraction_id = null where id = ?".to_string(),
        "delete from extractions where document_id = ?".to_string(),
        "delete from reading_positions where document_id = ?".to_string(),
        "delete from bookmarks where document_id = ?".to_string(),
        "delete from documents where id = ?".to_string(),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql.clone())).bind(document_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Reading progress (0..1) of a position: its section's place in the book plus the block's
/// place in that section. Stored on the document so the library can show it without a query
/// per book.
pub async fn progress_update(pool: &SqlitePool, document_id: &str, block_id: Option<&str>) -> AppResult<Option<f64>> {
    let Some(block_id) = block_id else { return Ok(None) };
    let row: Option<(i64, i64, i64, i64)> = sqlx::query_as(
        "select s.ordinal, b.ordinal,
                (select count(*) from blocks x where x.section_id = b.section_id),
                (select count(*) from sections y where y.extraction_id = b.extraction_id)
         from blocks b join sections s on s.id = b.section_id
         join extractions e on e.id = b.extraction_id
         where b.id = ? and e.document_id = ?",
    )
    .bind(block_id)
    .bind(document_id)
    .fetch_optional(pool)
    .await?;
    let Some((section, block, blocks, sections)) = row else { return Ok(None) };
    if sections <= 0 {
        return Ok(None);
    }
    let p = ((section as f64 + block as f64 / blocks.max(1) as f64) / sections as f64).clamp(0.0, 1.0);
    let p = (p * 1000.0).round() / 1000.0;
    sqlx::query("update documents set metadata_json = json_set(metadata_json, '$.progress', ?) where id = ?").bind(p).bind(document_id).execute(pool).await?;
    Ok(Some(p))
}
