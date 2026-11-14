//! Consistent DB snapshots via `VACUUM INTO` (equivalent to the Online Backup API for a
//! WAL database: it reads one transaction-consistent snapshot). Never file-copy an open WAL DB.

use crate::error::AppResult;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

pub async fn snapshot(pool: &SqlitePool, dir: &Path, label: &str) -> AppResult<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = format!("thoth-{}-{}.sqlite", crate::domain::now_ms(), label);
    let target = dir.join(&name);
    let tmp = dir.join(format!("{name}.partial"));
    let _ = std::fs::remove_file(&tmp);
    // Path is app-controlled (no user input) but still bind it as a parameter.
    sqlx::query("VACUUM INTO ?").bind(tmp.to_string_lossy().to_string()).execute(pool).await?;
    std::fs::rename(&tmp, &target)?;
    Ok(target)
}

/// Keep the newest `keep` routine backups. Recovery backups (label `recovery`) are kept
/// until the user confirms.
pub fn prune(dir: &Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut routine: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            n.starts_with("thoth-") && n.ends_with(".sqlite") && !n.contains("-recovery")
        })
        .collect();
    routine.sort();
    if routine.len() > keep {
        for p in &routine[..routine.len() - keep] {
            let _ = std::fs::remove_file(p);
        }
    }
    // Leftover partial snapshots from an interrupted run.
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("partial") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

pub fn mark_latest_as_recovery(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut v: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with("-pre-migration.sqlite"))
        .collect();
    v.sort();
    if let Some(last) = v.last() {
        let s = last.to_string_lossy().replace("-pre-migration.sqlite", "-recovery.sqlite");
        let _ = std::fs::rename(last, s);
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn snapshot_is_openable_and_pruned() {
        let (db, dir) = crate::storage::test_db().await;
        let b = dir.path().join("backups");
        for _ in 0..5 {
            super::snapshot(&db.writer, &b, "manual").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        }
        super::prune(&b, 3);
        let n = std::fs::read_dir(&b).unwrap().count();
        assert_eq!(n, 3);
    }
}
