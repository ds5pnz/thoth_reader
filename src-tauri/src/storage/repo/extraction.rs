//! Extractions, sections, staged batches and immutable blocks (spec §9.2).

use crate::domain::{
    canonical_json::canonical_sha256, is_valid_id, utf16, Block, BlockKind, Quality, SafeContent, SourceLocation,
    TranslationPolicy,
};
use crate::error::{AppError, AppResult};
use serde::Serialize;
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::collections::{HashMap, HashSet};
use ts_rs::TS;

pub const MAX_BLOCK_TEXT_BYTES: usize = 128 * 1024;
pub const MAX_BATCH_BLOCKS: usize = 100;
pub const MAX_BATCH_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SectionRow {
    pub id: String,
    pub extraction_id: String,
    pub ordinal: i64,
    #[ts(type = "Record<string, unknown>")]
    pub locator: serde_json::Value,
    pub state: String,
    pub block_count: i64,
}

/// Checksum over `[id, sourceHash]` pairs in order; identical in TS and Rust (no floats).
pub fn blocks_checksum(blocks: &[Block]) -> String {
    let v: Vec<serde_json::Value> =
        blocks.iter().map(|b| serde_json::json!([b.id, b.source_hash])).collect();
    canonical_sha256(&serde_json::Value::Array(v))
}

pub async fn ensure_extraction(
    pool: &SqlitePool,
    id: &str,
    document_id: &str,
    config_hash: &str,
    versions: &serde_json::Value,
) -> AppResult<String> {
    sqlx::query(
        "insert into extractions(id, document_id, config_hash, versions_json, state, created_at)
         values (?,?,?,?, 'staging', ?) on conflict(document_id, config_hash) do nothing",
    )
    .bind(id)
    .bind(document_id)
    .bind(config_hash)
    .bind(versions.to_string())
    .bind(crate::domain::now_ms())
    .execute(pool)
    .await?;
    let state: String = sqlx::query_scalar("select state from extractions where document_id = ? and config_hash = ?")
        .bind(document_id)
        .bind(config_hash)
        .fetch_one(pool)
        .await?;
    Ok(state)
}

pub async fn extraction_document(pool: &SqlitePool, extraction_id: &str) -> AppResult<Option<String>> {
    Ok(sqlx::query_scalar("select document_id from extractions where id = ?")
        .bind(extraction_id)
        .fetch_optional(pool)
        .await?)
}

pub async fn ensure_section(
    pool: &SqlitePool,
    id: &str,
    extraction_id: &str,
    ordinal: u32,
    locator: &serde_json::Value,
) -> AppResult<String> {
    sqlx::query(
        "insert into sections(id, extraction_id, ordinal, locator_json, state) values (?,?,?,?, 'pending')
         on conflict(id) do nothing",
    )
    .bind(id)
    .bind(extraction_id)
    .bind(ordinal as i64)
    .bind(locator.to_string())
    .execute(pool)
    .await?;
    let row = sqlx::query("select extraction_id, state from sections where id = ?").bind(id).fetch_one(pool).await?;
    let ex: String = row.get(0);
    if ex != extraction_id {
        return Err(AppError::invalid("section이 extraction에 속하지 않습니다."));
    }
    Ok(row.get(1))
}

pub async fn mark_section_failed(pool: &SqlitePool, section_id: &str) -> AppResult<()> {
    sqlx::query("update sections set state = 'failed' where id = ? and state <> 'ready'")
        .bind(section_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Validate one block against its section/extraction and size/shape limits.
pub fn validate_block(b: &Block, extraction_id: &str, section_id: &str) -> AppResult<()> {
    if !is_valid_id(&b.id) || b.extraction_id != extraction_id || b.section_id != section_id {
        return Err(AppError::invalid("block 식별자가 올바르지 않습니다."));
    }
    if let Some(p) = &b.parent_id {
        if !is_valid_id(p) || p == &b.id {
            return Err(AppError::invalid("parent block이 올바르지 않습니다."));
        }
    }
    if b.raw_text.len() > MAX_BLOCK_TEXT_BYTES || b.source_text.len() > MAX_BLOCK_TEXT_BYTES {
        return Err(AppError::limit("블록 본문이 128KiB를 넘습니다."));
    }
    if crate::domain::sha256_hex_str(&b.source_text) != b.source_hash {
        return Err(AppError::invalid("sourceHash가 원문과 일치하지 않습니다."));
    }
    let raw_len = utf16::len_u16(&b.raw_text);
    match &b.location {
        SourceLocation::Pdf { rects, view_box, .. } => {
            if view_box.len() != 4 || rects.len() > 4096 || rects.iter().any(|r| !(r.x.is_finite() && r.y.is_finite() && r.width.is_finite() && r.height.is_finite())) {
                return Err(AppError::invalid("PDF 위치 정보가 올바르지 않습니다."));
            }
        }
        SourceLocation::Epub { start_u16, end_u16, cfi_range, .. } => {
            if start_u16 > end_u16 || *end_u16 > raw_len.max(*end_u16) || cfi_range.len() > 4096 || !cfi_range.starts_with("epubcfi(") {
                return Err(AppError::invalid("EPUB 위치 정보가 올바르지 않습니다."));
            }
        }
    }
    for t in &b.mapping.text_nodes {
        if t.raw_start_u16.saturating_add(t.raw_len_u16) > raw_len {
            return Err(AppError::invalid("텍스트 매핑 범위가 올바르지 않습니다."));
        }
    }
    Ok(())
}

/// Parents must live in the same section and the parent graph must be acyclic.
pub fn validate_parents(blocks: &[Block]) -> AppResult<()> {
    let ids: HashMap<&str, Option<&str>> = blocks.iter().map(|b| (b.id.as_str(), b.parent_id.as_deref())).collect();
    if ids.len() != blocks.len() {
        return Err(AppError::invalid("중복된 block id가 있습니다."));
    }
    for b in blocks {
        let mut seen = HashSet::new();
        let mut cur = b.parent_id.as_deref();
        while let Some(p) = cur {
            if !seen.insert(p) {
                return Err(AppError::invalid("parent 관계에 순환이 있습니다."));
            }
            match ids.get(p) {
                Some(next) => cur = *next,
                None => return Err(AppError::invalid("parent block이 같은 section에 없습니다.")),
            }
        }
    }
    let mut ordinals = HashSet::new();
    for b in blocks {
        if !ordinals.insert(b.ordinal) {
            return Err(AppError::invalid("중복된 ordinal이 있습니다."));
        }
    }
    Ok(())
}

/// Idempotent staging of one batch. Same index + checksum => existing success; different => reject.
pub async fn stage_batch(
    pool: &SqlitePool,
    extraction_id: &str,
    section_id: &str,
    batch_index: u32,
    blocks: &[Block],
) -> AppResult<(String, usize)> {
    if blocks.len() > MAX_BATCH_BLOCKS {
        return Err(AppError::limit("batch는 최대 100개 블록입니다."));
    }
    let payload = serde_json::to_string(blocks)?;
    if payload.len() > MAX_BATCH_BYTES {
        return Err(AppError::limit("batch는 최대 1MiB입니다."));
    }
    for b in blocks {
        validate_block(b, extraction_id, section_id)?;
    }
    let checksum = blocks_checksum(blocks);
    let state: Option<String> = sqlx::query_scalar("select state from sections where id = ? and extraction_id = ?")
        .bind(section_id)
        .bind(extraction_id)
        .fetch_optional(pool)
        .await?;
    match state.as_deref() {
        None => return Err(AppError::not_found("section")),
        Some("ready") => return Err(AppError::conflict("이미 공개된 section입니다.")),
        _ => {}
    }
    let existing: Option<String> =
        sqlx::query_scalar("select checksum from extraction_batches where section_id = ? and batch_index = ?")
            .bind(section_id)
            .bind(batch_index as i64)
            .fetch_optional(pool)
            .await?;
    if let Some(cs) = existing {
        return if cs == checksum { Ok((cs, blocks.len())) } else { Err(AppError::conflict("같은 batch에 다른 내용이 전송되었습니다.")) };
    }
    let mut tx = pool.begin().await?;
    sqlx::query("insert into extraction_batches(extraction_id, section_id, batch_index, checksum, payload_json) values (?,?,?,?,?)")
        .bind(extraction_id)
        .bind(section_id)
        .bind(batch_index as i64)
        .bind(&checksum)
        .bind(&payload)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update sections set state = 'staging' where id = ? and state = 'pending'")
        .bind(section_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((checksum, blocks.len()))
}

async fn insert_blocks(conn: &mut SqliteConnection, blocks: &[Block], document_id: &str) -> AppResult<()> {
    // parents first (ordinal order guarantees parents precede children only if extractor emits so;
    // sort topologically to be safe)
    let mut ordered: Vec<&Block> = Vec::with_capacity(blocks.len());
    let mut placed: HashSet<&str> = HashSet::new();
    let mut remaining: Vec<&Block> = blocks.iter().collect();
    while !remaining.is_empty() {
        let before = remaining.len();
        remaining.retain(|b| {
            let ok = b.parent_id.as_deref().map(|p| placed.contains(p)).unwrap_or(true);
            if ok {
                ordered.push(b);
                placed.insert(b.id.as_str());
            }
            !ok
        });
        if remaining.len() == before {
            return Err(AppError::invalid("parent 관계를 해석할 수 없습니다."));
        }
    }
    for b in ordered {
        sqlx::query(
            "insert into blocks(id, extraction_id, section_id, parent_id, ordinal, kind, raw_text, source_text, source_hash,
              content_json, location_json, mapping_json, quality_json, translation_policy)
             values (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&b.id)
        .bind(&b.extraction_id)
        .bind(&b.section_id)
        .bind(&b.parent_id)
        .bind(b.ordinal as i64)
        .bind(b.kind.as_str())
        .bind(&b.raw_text)
        .bind(&b.source_text)
        .bind(&b.source_hash)
        .bind(serde_json::to_string(&b.content)?)
        .bind(serde_json::to_string(&b.location)?)
        .bind(serde_json::to_string(&serde_json::json!({"offsets": b.mapping.offsets, "textNodes": b.mapping.text_nodes, "roles": b.roles}))?)
        .bind(serde_json::to_string(&b.quality)?)
        .bind(b.translation_policy.as_str())
        .execute(&mut *conn)
        .await?;
        if !b.source_text.is_empty() {
            sqlx::query(
                "insert into search_entries(document_id, block_id, scope, language, revision_id, text) values (?,?, 'source', 'und', null, ?)
                 on conflict(block_id, scope, language) do nothing",
            )
            .bind(document_id)
            .bind(&b.id)
            .bind(&b.source_text)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// Validate staged batches and publish the section atomically.
pub async fn finalize_section(
    pool: &SqlitePool,
    extraction_id: &str,
    section_id: &str,
    expected_count: u32,
    checksum: &str,
) -> AppResult<u32> {
    let document_id = extraction_document(pool, extraction_id).await?.ok_or_else(|| AppError::not_found("extraction"))?;
    let mut tx = pool.begin().await?;
    let row = sqlx::query("select state, checksum, block_count from sections where id = ? and extraction_id = ?")
        .bind(section_id)
        .bind(extraction_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::not_found("section"))?;
    let state: String = row.get(0);
    if state == "ready" {
        let cs: Option<String> = row.get(1);
        let n: i64 = row.get(2);
        return if cs.as_deref() == Some(checksum) && n == expected_count as i64 { Ok(n as u32) } else { Err(AppError::conflict("이미 다른 내용으로 공개된 section입니다.")) };
    }
    let batches = sqlx::query("select batch_index, payload_json from extraction_batches where section_id = ? and extraction_id = ? order by batch_index")
        .bind(section_id)
        .bind(extraction_id)
        .fetch_all(&mut *tx)
        .await?;
    let mut blocks: Vec<Block> = Vec::new();
    for (i, b) in batches.iter().enumerate() {
        let idx: i64 = b.get(0);
        if idx != i as i64 {
            return Err(AppError::invalid("batch 순서에 누락이 있습니다."));
        }
        let mut part: Vec<Block> = serde_json::from_str(b.get::<String, _>(1).as_str())?;
        blocks.append(&mut part);
    }
    if blocks.len() as u32 != expected_count || blocks_checksum(&blocks) != checksum {
        return Err(AppError::invalid("section 블록 수량 또는 checksum이 일치하지 않습니다."));
    }
    validate_parents(&blocks)?;
    insert_blocks(&mut tx, &blocks, &document_id).await?;
    sqlx::query("update sections set state = 'ready', checksum = ?, block_count = ? where id = ?")
        .bind(checksum)
        .bind(blocks.len() as i64)
        .bind(section_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("delete from extraction_batches where section_id = ?").bind(section_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(blocks.len() as u32)
}

/// Rust-side extraction (EPUB): validate + publish a whole section in one transaction.
pub async fn publish_section_direct(
    pool: &SqlitePool,
    extraction_id: &str,
    section_id: &str,
    blocks: &[Block],
) -> AppResult<u32> {
    for b in blocks {
        validate_block(b, extraction_id, section_id)?;
    }
    validate_parents(blocks)?;
    let document_id = extraction_document(pool, extraction_id).await?.ok_or_else(|| AppError::not_found("extraction"))?;
    let checksum = blocks_checksum(blocks);
    let mut tx = pool.begin().await?;
    let state: String = sqlx::query_scalar("select state from sections where id = ?").bind(section_id).fetch_one(&mut *tx).await?;
    if state == "ready" {
        return Ok(blocks.len() as u32);
    }
    insert_blocks(&mut tx, blocks, &document_id).await?;
    sqlx::query("update sections set state = 'ready', checksum = ?, block_count = ? where id = ?")
        .bind(&checksum)
        .bind(blocks.len() as i64)
        .bind(section_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(blocks.len() as u32)
}

pub async fn set_extraction_state(pool: &SqlitePool, extraction_id: &str, state: &str) -> AppResult<()> {
    sqlx::query("update extractions set state = ? where id = ?").bind(state).bind(extraction_id).execute(pool).await?;
    Ok(())
}

/// Short transaction switching the document's active extraction pointer.
pub async fn activate(pool: &SqlitePool, document_id: &str, extraction_id: &str, doc_state: &str) -> AppResult<()> {
    let owner = extraction_document(pool, extraction_id).await?;
    if owner.as_deref() != Some(document_id) {
        return Err(AppError::invalid("extraction이 문서에 속하지 않습니다."));
    }
    sqlx::query("update documents set active_extraction_id = ?, state = ? where id = ?")
        .bind(extraction_id)
        .bind(doc_state)
        .bind(document_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_sections(pool: &SqlitePool, extraction_id: &str) -> AppResult<Vec<SectionRow>> {
    let rows = sqlx::query("select id, extraction_id, ordinal, locator_json, state, block_count from sections where extraction_id = ? order by ordinal")
        .bind(extraction_id)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| SectionRow {
            id: r.get(0),
            extraction_id: r.get(1),
            ordinal: r.get(2),
            locator: serde_json::from_str(r.get::<String, _>(3).as_str()).unwrap_or_default(),
            state: r.get(4),
            block_count: r.get(5),
        })
        .collect())
}

pub fn block_from_row(r: &sqlx::sqlite::SqliteRow) -> AppResult<Block> {
    let mapping: serde_json::Value = serde_json::from_str(r.get::<String, _>("mapping_json").as_str())?;
    let kind: String = r.get("kind");
    let policy: String = r.get("translation_policy");
    Ok(Block {
        id: r.get("id"),
        extraction_id: r.get("extraction_id"),
        section_id: r.get("section_id"),
        parent_id: r.get("parent_id"),
        ordinal: r.get::<i64, _>("ordinal") as u32,
        kind: BlockKind::parse(&kind).unwrap_or(BlockKind::Unknown),
        raw_text: r.get("raw_text"),
        source_text: r.get("source_text"),
        source_hash: r.get("source_hash"),
        content: serde_json::from_str::<SafeContent>(r.get::<String, _>("content_json").as_str())?,
        location: serde_json::from_str::<SourceLocation>(r.get::<String, _>("location_json").as_str())?,
        quality: serde_json::from_str::<Quality>(r.get::<String, _>("quality_json").as_str())?,
        translation_policy: TranslationPolicy::parse(&policy).unwrap_or(TranslationPolicy::Preserve),
        mapping: crate::domain::BlockMapping {
            offsets: serde_json::from_value(mapping.get("offsets").cloned().unwrap_or_default()).unwrap_or_default(),
            text_nodes: serde_json::from_value(mapping.get("textNodes").cloned().unwrap_or_default()).unwrap_or_default(),
        },
        roles: serde_json::from_value(mapping.get("roles").cloned().unwrap_or_default()).unwrap_or_default(),
    })
}

/// Blocks of a *published* section, paginated by ordinal.
pub async fn blocks_of_section(pool: &SqlitePool, extraction_id: &str, section_id: &str, after_ordinal: Option<u32>, limit: u32) -> AppResult<Vec<Block>> {
    let rows = sqlx::query(
        "select b.* from blocks b join sections s on s.id = b.section_id
         where b.extraction_id = ? and b.section_id = ? and s.state = 'ready' and b.ordinal > ?
         order by b.ordinal limit ?",
    )
    .bind(extraction_id)
    .bind(section_id)
    .bind(after_ordinal.map(|o| o as i64).unwrap_or(-1))
    .bind(limit.clamp(1, 2000) as i64)
    .fetch_all(pool)
    .await?;
    rows.iter().map(block_from_row).collect()
}

pub async fn get_block(pool: &SqlitePool, id: &str) -> AppResult<Option<Block>> {
    match sqlx::query("select * from blocks where id = ?").bind(id).fetch_optional(pool).await? {
        Some(r) => Ok(Some(block_from_row(&r)?)),
        None => Ok(None),
    }
}

/// The block plus its document, asserting it belongs to the given active extraction.
pub async fn get_block_in_extraction(pool: &SqlitePool, id: &str, extraction_id: &str) -> AppResult<Block> {
    let b = get_block(pool, id).await?.ok_or_else(|| AppError::not_found("블록"))?;
    if b.extraction_id != extraction_id {
        return Err(AppError::invalid("블록이 현재 문서에 속하지 않습니다."));
    }
    Ok(b)
}

/// Previous leaf block in reading order (for context) and the nearest preceding heading.
pub async fn context_blocks(pool: &SqlitePool, b: &Block) -> AppResult<(Option<Block>, Option<Block>)> {
    let prev = sqlx::query(
        "select * from blocks where section_id = ? and ordinal < ? and kind in ('paragraph','list_item','quote','caption')
         order by ordinal desc limit 1",
    )
    .bind(&b.section_id)
    .bind(b.ordinal as i64)
    .fetch_optional(pool)
    .await?;
    let heading = sqlx::query("select * from blocks where section_id = ? and ordinal < ? and kind = 'heading' order by ordinal desc limit 1")
        .bind(&b.section_id)
        .bind(b.ordinal as i64)
        .fetch_optional(pool)
        .await?;
    Ok((prev.map(|r| block_from_row(&r)).transpose()?, heading.map(|r| block_from_row(&r)).transpose()?))
}

/// Next N translatable leaves after `b` in reading order, crossing sections (for prefetch).
pub async fn next_blocks(pool: &SqlitePool, b: &Block, n: u32) -> AppResult<Vec<Block>> {
    let rows = sqlx::query(
        "select b.* from blocks b join sections s on s.id = b.section_id
         join sections cur on cur.id = ?
         where b.extraction_id = ? and s.state = 'ready'
           and (s.ordinal > cur.ordinal or (s.ordinal = cur.ordinal and b.ordinal > ?))
           and b.translation_policy = 'auto'
           and not exists (select 1 from blocks c where c.parent_id = b.id)
         order by s.ordinal, b.ordinal limit ?",
    )
    .bind(&b.section_id)
    .bind(&b.extraction_id)
    .bind(b.ordinal as i64)
    .bind(n as i64)
    .fetch_all(pool)
    .await?;
    rows.iter().map(block_from_row).collect()
}

pub async fn is_leaf(pool: &SqlitePool, block_id: &str) -> AppResult<bool> {
    let n: i64 = sqlx::query_scalar("select count(*) from blocks where parent_id = ?").bind(block_id).fetch_one(pool).await?;
    Ok(n == 0)
}
