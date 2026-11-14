use crate::domain::now_ms;
use crate::error::AppResult;
use serde::Serialize;
use sqlx::{Row, SqliteConnection, SqlitePool};
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DocumentRow {
    pub id: String,
    pub sha256: String,
    pub format: String,
    pub title: String,
    /// Relative to the app data root. Never sent to the UI.
    #[serde(skip)]
    #[ts(skip)]
    pub managed_path: String,
    pub byte_size: i64,
    pub state: String,
    pub active_extraction_id: Option<String>,
    #[ts(type = "Record<string, unknown>")]
    pub metadata: serde_json::Value,
    pub created_at: i64,
    pub last_opened_at: Option<i64>,
}

fn from_row(r: &sqlx::sqlite::SqliteRow) -> DocumentRow {
    DocumentRow {
        id: r.get("id"),
        sha256: r.get("sha256"),
        format: r.get("format"),
        title: r.get("title"),
        managed_path: r.get("managed_path"),
        byte_size: r.get("byte_size"),
        state: r.get("state"),
        active_extraction_id: r.get("active_extraction_id"),
        metadata: serde_json::from_str(r.get::<String, _>("metadata_json").as_str()).unwrap_or_default(),
        created_at: r.get("created_at"),
        last_opened_at: r.get("last_opened_at"),
    }
}

pub async fn insert(conn: &mut SqliteConnection, d: &DocumentRow) -> AppResult<()> {
    sqlx::query(
        "insert into documents(id, sha256, format, title, managed_path, byte_size, state, active_extraction_id, metadata_json, created_at, last_opened_at)
         values (?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&d.id)
    .bind(&d.sha256)
    .bind(&d.format)
    .bind(&d.title)
    .bind(&d.managed_path)
    .bind(d.byte_size)
    .bind(&d.state)
    .bind(&d.active_extraction_id)
    .bind(d.metadata.to_string())
    .bind(d.created_at)
    .bind(d.last_opened_at)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn get(pool: &SqlitePool, id: &str) -> AppResult<Option<DocumentRow>> {
    Ok(sqlx::query("select * from documents where id = ?").bind(id).fetch_optional(pool).await?.map(|r| from_row(&r)))
}

pub async fn by_sha(pool: &SqlitePool, sha: &str) -> AppResult<Option<DocumentRow>> {
    Ok(sqlx::query("select * from documents where sha256 = ?").bind(sha).fetch_optional(pool).await?.map(|r| from_row(&r)))
}

/// Recent first. Cursor = "<sortKey>:<id>".
pub async fn list(pool: &SqlitePool, cursor: Option<&str>, limit: u32) -> AppResult<(Vec<DocumentRow>, Option<String>)> {
    let limit = limit.clamp(1, 100) as i64;
    let (ck, cid) = match cursor.and_then(|c| c.split_once(':')) {
        Some((k, id)) => (k.parse::<i64>().unwrap_or(i64::MAX), id.to_string()),
        None => (i64::MAX, String::new()),
    };
    let rows = sqlx::query(
        "select *, coalesce(last_opened_at, created_at) as sk from documents
         where (coalesce(last_opened_at, created_at) < ?) or (coalesce(last_opened_at, created_at) = ? and id > ?)
         order by sk desc, id asc limit ?",
    )
    .bind(ck)
    .bind(ck)
    .bind(&cid)
    .bind(limit + 1)
    .fetch_all(pool)
    .await?;
    let mut docs: Vec<DocumentRow> = rows.iter().map(from_row).collect();
    let next = if docs.len() as i64 > limit {
        docs.truncate(limit as usize);
        docs.last().map(|d| format!("{}:{}", d.last_opened_at.unwrap_or(d.created_at), d.id))
    } else {
        None
    };
    Ok((docs, next))
}

pub async fn set_state(pool: &SqlitePool, id: &str, state: &str) -> AppResult<()> {
    sqlx::query("update documents set state = ? where id = ?").bind(state).bind(id).execute(pool).await?;
    Ok(())
}

pub async fn merge_metadata(pool: &SqlitePool, id: &str, patch: &serde_json::Value) -> AppResult<()> {
    sqlx::query("update documents set metadata_json = json_patch(metadata_json, ?) where id = ?")
        .bind(patch.to_string())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn touch_opened(pool: &SqlitePool, id: &str) -> AppResult<()> {
    sqlx::query("update documents set last_opened_at = ? where id = ?").bind(now_ms()).bind(id).execute(pool).await?;
    Ok(())
}
