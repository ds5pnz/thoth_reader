//! SQLite persistence (spec §9). One writer pool (1 connection) + up to 3 readers.
//! WAL + synchronous=FULL, foreign_keys=ON, busy_timeout=5000.

pub mod backup;
pub mod repo;

use crate::error::{AppError, AppResult, ErrorCode};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous};
use sqlx::Row;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

pub const MIN_SQLITE_VERSION: (u32, u32, u32) = (3, 51, 3);

#[derive(Clone)]
pub struct Db {
    pub writer: SqlitePool,
    pub reader: SqlitePool,
    pub path: PathBuf,
    /// Set when the DB schema is newer than this app or migration failed: no writes allowed.
    pub read_only_reason: Option<String>,
}

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

fn connect_options(path: &Path, read_only: bool) -> AppResult<SqliteConnectOptions> {
    let url = format!("sqlite://{}", path.to_string_lossy());
    Ok(SqliteConnectOptions::from_str(&url)
        .map_err(|_| AppError::internal("DB 경로가 올바르지 않습니다."))?
        .create_if_missing(!read_only)
        .read_only(read_only)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(5000))
        .pragma("trusted_schema", "OFF")
        .statement_cache_capacity(256))
}

pub fn parse_version(v: &str) -> (u32, u32, u32) {
    let mut it = v.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

impl Db {
    /// Open (or create) the DB, verify the bundled SQLite build, back up and migrate.
    pub async fn open(path: &Path, backups_dir: &Path) -> AppResult<Db> {
        Self::open_with(path, backups_dir, &MIGRATOR).await
    }

    /// Same as `open` with an explicit migrator (fault-injection tests).
    pub async fn open_with(path: &Path, backups_dir: &Path, migrator: &sqlx::migrate::Migrator) -> AppResult<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(backups_dir)?;
        let existed = path.exists();
        let writer = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(connect_options(path, false)?)
            .await
            .map_err(|e| {
                tracing::error!("db open failed: {}", crate::diagnostics::redact(&e.to_string()));
                AppError::new(ErrorCode::MigrationFailed, "데이터베이스를 열 수 없습니다.")
            })?;

        verify_sqlite_build(&writer).await?;

        let mut read_only_reason = None;
        match pending_state(&writer, migrator).await? {
            SchemaState::Newer => {
                read_only_reason = Some("DB가 더 새로운 앱 버전에서 생성되었습니다. 호환되는 버전으로 열어 주세요.".into());
            }
            SchemaState::Pending if existed => {
                // A migration that fails on every launch must not add a full copy each time.
                if !backup::has_recovery(backups_dir) {
                    backup::snapshot(&writer, backups_dir, "pre-migration").await?;
                }
                if let Err(e) = migrator.run(&writer).await {
                    backup::mark_latest_as_recovery(backups_dir);
                    tracing::error!("migration failed: {}", crate::diagnostics::redact(&e.to_string()));
                    read_only_reason = Some("데이터베이스 업데이트에 실패했습니다. 백업에서 복원할 수 있습니다.".into());
                }
            }
            SchemaState::Pending => {
                migrator.run(&writer).await.map_err(|e| {
                    tracing::error!("migration failed: {}", crate::diagnostics::redact(&e.to_string()));
                    AppError::new(ErrorCode::MigrationFailed, "데이터베이스 초기화에 실패했습니다.")
                })?;
            }
            SchemaState::Current => {}
        }
        if read_only_reason.is_none() {
            integrity_check(&writer).await?;
        }

        let reader = SqlitePoolOptions::new()
            .max_connections(3)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(connect_options(path, false)?.pragma("query_only", "ON"))
            .await?;
        backup::prune(backups_dir, 3);
        // Read-only mode is enforced by SQLite, not by each caller remembering to check.
        let writer = if read_only_reason.is_some() {
            writer.close().await;
            SqlitePoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(30))
                .connect_with(connect_options(path, false)?.pragma("query_only", "ON"))
                .await?
        } else {
            writer
        };
        Ok(Db { writer, reader, path: path.to_path_buf(), read_only_reason })
    }

    pub fn ensure_writable(&self) -> AppResult<()> {
        match &self.read_only_reason {
            Some(r) => Err(AppError::new(ErrorCode::MigrationFailed, r.clone())),
            None => Ok(()),
        }
    }

    /// Opportunistic WAL checkpoint while idle.
    pub async fn checkpoint(&self) {
        let _ = sqlx::query("PRAGMA wal_checkpoint(PASSIVE)").execute(&self.writer).await;
    }
}

async fn verify_sqlite_build(pool: &SqlitePool) -> AppResult<()> {
    let v: String = sqlx::query_scalar("select sqlite_version()").fetch_one(pool).await?;
    if parse_version(&v) < MIN_SQLITE_VERSION {
        return Err(AppError::new(ErrorCode::MigrationFailed, format!("SQLite {v}는 지원하지 않습니다(3.51.3 이상 필요).")));
    }
    // JSON functions and the FTS5 trigram tokenizer must be compiled in.
    sqlx::query("select json_valid('{}')").execute(pool).await?;
    let fts: i64 = sqlx::query_scalar("select sqlite_compileoption_used('ENABLE_FTS5')").fetch_one(pool).await?;
    if fts != 1 {
        return Err(AppError::new(ErrorCode::MigrationFailed, "SQLite FTS5가 빌드에 포함되지 않았습니다."));
    }
    Ok(())
}

enum SchemaState {
    Current,
    Pending,
    Newer,
}

async fn pending_state(pool: &SqlitePool, migrator: &sqlx::migrate::Migrator) -> AppResult<SchemaState> {
    let has_table: i64 = sqlx::query_scalar(
        "select count(*) from sqlite_master where type='table' and name='_sqlx_migrations'",
    )
    .fetch_one(pool)
    .await?;
    if has_table == 0 {
        return Ok(SchemaState::Pending);
    }
    let rows = sqlx::query("select version, checksum from _sqlx_migrations where success = 1")
        .fetch_all(pool)
        .await?;
    let known: std::collections::HashMap<i64, Vec<u8>> = migrator
        .iter()
        .map(|m| (m.version, m.checksum.to_vec()))
        .collect();
    let mut applied = 0usize;
    for r in rows {
        let v: i64 = r.get(0);
        let cs: Vec<u8> = r.get(1);
        match known.get(&v) {
            None => return Ok(SchemaState::Newer),
            Some(k) if *k != cs => {
                return Err(AppError::new(ErrorCode::MigrationFailed, "마이그레이션 checksum이 일치하지 않습니다."))
            }
            _ => applied += 1,
        }
    }
    Ok(if applied < known.len() { SchemaState::Pending } else { SchemaState::Current })
}

pub async fn integrity_check(pool: &SqlitePool) -> AppResult<()> {
    let qc: String = sqlx::query_scalar("PRAGMA quick_check").fetch_one(pool).await?;
    if qc != "ok" {
        return Err(AppError::new(ErrorCode::MigrationFailed, "데이터베이스 무결성 검사에 실패했습니다."));
    }
    let fk = sqlx::query("PRAGMA foreign_key_check").fetch_all(pool).await?;
    if !fk.is_empty() {
        return Err(AppError::new(ErrorCode::MigrationFailed, "데이터베이스 참조 무결성 검사에 실패했습니다."));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) async fn test_db() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("database/thoth.sqlite"), &dir.path().join("backups")).await.unwrap();
    (db, dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn opens_migrates_and_reopens() {
        let (db, dir) = test_db().await;
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode").fetch_one(&db.writer).await.unwrap();
        assert_eq!(mode, "wal");
        let sync: i64 = sqlx::query_scalar("PRAGMA synchronous").fetch_one(&db.writer).await.unwrap();
        assert_eq!(sync, 2); // FULL
        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys").fetch_one(&db.writer).await.unwrap();
        assert_eq!(fk, 1);
        let n: i64 = sqlx::query_scalar("select count(*) from sqlite_master where name='search_fts'")
            .fetch_one(&db.reader).await.unwrap();
        assert_eq!(n, 1);
        drop(db);
        let db2 = Db::open(&dir.path().join("database/thoth.sqlite"), &dir.path().join("backups")).await.unwrap();
        assert!(db2.read_only_reason.is_none());
    }

    #[tokio::test]
    async fn newer_schema_is_read_only() {
        let (db, dir) = test_db().await;
        sqlx::query("insert into _sqlx_migrations(version, description, installed_on, success, checksum, execution_time) values (9999, 'future', CURRENT_TIMESTAMP, 1, x'00', 0)")
            .execute(&db.writer).await.unwrap();
        drop(db);
        let db2 = Db::open(&dir.path().join("database/thoth.sqlite"), &dir.path().join("backups")).await.unwrap();
        assert!(db2.read_only_reason.is_some());
        assert!(db2.ensure_writable().is_err());
    }

    #[tokio::test]
    async fn reader_pool_is_query_only() {
        let (db, _d) = test_db().await;
        let r = sqlx::query("insert into settings(key, value_json, updated_at) values ('x','1',0)")
            .execute(&db.reader).await;
        assert!(r.is_err());
    }

    /// T16: a migration that fails midway must leave existing data intact, keep a recovery
    /// backup, and put the app in read-only mode instead of starting queues/edits.
    #[tokio::test]
    async fn failed_migration_keeps_data_and_recovery_backup() {
        let (db, dir) = test_db().await;
        sqlx::query("insert into settings(key, value_json, updated_at) values ('theme','\"dark\"',0)").execute(&db.writer).await.unwrap();
        drop(db);
        // Migrator = real migrations + a broken 9000 (second statement fails).
        let mdir = dir.path().join("mig");
        std::fs::create_dir_all(&mdir).unwrap();
        for f in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations")).unwrap().flatten() {
            std::fs::copy(f.path(), mdir.join(f.file_name())).unwrap();
        }
        std::fs::write(mdir.join("9000_broken.sql"), "CREATE TABLE half_done(x);\nINSERT INTO no_such_table VALUES (1);").unwrap();
        let broken = sqlx::migrate::Migrator::new(mdir.clone()).await.unwrap();
        let p = dir.path().join("database/thoth.sqlite");
        let b = dir.path().join("backups");
        let db2 = Db::open_with(&p, &b, &broken).await.unwrap();
        assert!(db2.read_only_reason.is_some());
        assert!(db2.ensure_writable().is_err());
        // Existing data readable; the failed migration's partial work was rolled back.
        let v: String = sqlx::query_scalar("select value_json from settings where key='theme'").fetch_one(&db2.reader).await.unwrap();
        assert_eq!(v, "\"dark\"");
        let half: i64 = sqlx::query_scalar("select count(*) from sqlite_master where name='half_done'").fetch_one(&db2.reader).await.unwrap();
        assert_eq!(half, 0);
        integrity_check(&db2.writer).await.unwrap();
        let rec: Vec<_> = std::fs::read_dir(&b).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("-recovery")).collect();
        assert_eq!(rec.len(), 1, "recovery backup kept");
        // Pruning never removes the recovery backup.
        backup::prune(&b, 0);
        assert!(rec[0].path().exists());
        drop(db2);
        // The correct app version opens the same DB normally afterwards.
        let db3 = Db::open(&p, &b).await.unwrap();
        assert!(db3.read_only_reason.is_none());
    }

    /// T16: a migration checksum that differs from the recorded one is refused.
    #[tokio::test]
    async fn checksum_mismatch_is_refused() {
        let (db, dir) = test_db().await;
        sqlx::query("update _sqlx_migrations set checksum = x'00' where version = 1").execute(&db.writer).await.unwrap();
        drop(db);
        let r = Db::open(&dir.path().join("database/thoth.sqlite"), &dir.path().join("backups")).await;
        assert!(r.is_err());
    }
}
