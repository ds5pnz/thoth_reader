//! App-managed storage layout under Application Support/<identifier> (spec §2.2).

use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub documents: PathBuf,
    pub derived: PathBuf,
    pub database: PathBuf,
    pub backups: PathBuf,
}

impl AppPaths {
    pub fn new(root: PathBuf) -> std::io::Result<Self> {
        let p = AppPaths {
            documents: root.join("documents"),
            derived: root.join("derived"),
            database: root.join("database"),
            backups: root.join("backups"),
            root,
        };
        for d in [&p.root, &p.documents, &p.derived, &p.database, &p.backups] {
            std::fs::create_dir_all(d)?;
        }
        Ok(p)
    }
    pub fn db_file(&self) -> PathBuf {
        self.database.join("thoth.sqlite")
    }
    /// Resolve an app-relative path, refusing anything that escapes the root.
    pub fn resolve(&self, rel: &str) -> Option<PathBuf> {
        let rp = Path::new(rel);
        if rp.is_absolute() || rp.components().any(|c| !matches!(c, Component::Normal(_))) {
            return None;
        }
        Some(self.root.join(rp))
    }
    pub fn derived_dir(&self, extraction_id: &str) -> PathBuf {
        self.derived.join(extraction_id)
    }
}

/// Resolve `rel` under `base` and verify (after canonicalization) that the result
/// stays inside `base` and is not reached through a symlink.
pub fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
    let rp = Path::new(rel);
    if rel.is_empty() || rel.contains('\0') || rel.contains('\\') || rp.is_absolute() {
        return None;
    }
    if rp.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    let joined = base.join(rp);
    let canon_base = base.canonicalize().ok()?;
    let canon = joined.canonicalize().ok()?;
    if !canon.starts_with(&canon_base) {
        return None;
    }
    // reject symlinks anywhere along the relative part
    let mut cur = canon_base.clone();
    for c in rp.components() {
        cur.push(c);
        if std::fs::symlink_metadata(&cur).ok()?.file_type().is_symlink() {
            return None;
        }
    }
    Some(canon)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn safe_join_rejects_escape_and_symlink() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a")).unwrap();
        std::fs::write(d.path().join("a/f.txt"), b"x").unwrap();
        std::fs::write(d.path().join("secret"), b"s").unwrap();
        assert!(safe_join(&d.path().join("a"), "f.txt").is_some());
        assert!(safe_join(&d.path().join("a"), "../secret").is_none());
        assert!(safe_join(&d.path().join("a"), "/etc/passwd").is_none());
        assert!(safe_join(&d.path().join("a"), "x\\..\\secret").is_none());
        std::os::unix::fs::symlink(d.path().join("secret"), d.path().join("a/link")).unwrap();
        assert!(safe_join(&d.path().join("a"), "link").is_none());
    }
}
