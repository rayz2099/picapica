use super::{digest_hex, now_secs, unique_tmp, Store};
use crate::error::{Error, Result};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

const ORPHAN_GRACE_SECS: i64 = 60;

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReconcileReport {
    pub parts_removed: u64,
    pub orphans_found: u64,
    pub missing_records_removed: u64,
    pub refs_repaired: u64,
    pub finished_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageStatus {
    pub ok: bool,
    pub writable: bool,
    pub bytes: i64,
    pub max_bytes: Option<u64>,
    pub capacity_ok: bool,
    pub error: Option<String>,
    pub reconcile: ReconcileReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneReport {
    pub dry_run: bool,
    pub namespaces: u64,
    pub artifacts: u64,
    pub bytes_before: i64,
    pub bytes_reclaimable: i64,
    pub bytes_after: i64,
    pub limit_satisfied: bool,
    pub reason: Option<String>,
    pub finished_at: i64,
}

#[derive(Clone)]
struct RefRow {
    repo: String,
    namespace: String,
    digest: String,
}

#[derive(Clone)]
struct ArtifactRow {
    size: i64,
    last_access: i64,
}

impl Store {
    /// why: 启动时先修复元数据计数并登记磁盘孤儿，避免运行态基于脏账做删除决策。
    pub(super) fn reconcile_storage(&self) -> Result<ReconcileReport> {
        let parts_removed = clean_parts(&self.blob_root)?;
        let files = scan_blobs(&self.blob_root)?;
        let mut db = self.db.lock().expect("db");
        let tx = db.transaction()?;
        let artifacts = {
            let mut stmt = tx.prepare("SELECT digest,refs FROM artifacts")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut missing_records_removed = 0u64;
        for (digest, refs) in artifacts {
            digest_hex(&digest)?;
            if files.contains_key(&digest) {
                continue;
            }
            if refs > 0 {
                return Err(Error::msg(format!("引用制品文件缺失: {digest}")));
            }
            tx.execute("DELETE FROM artifacts WHERE digest=?1", params![digest])?;
            missing_records_removed += 1;
        }

        let now = now_secs();
        let mut orphans_found = 0u64;
        for (digest, (size, modified)) in &files {
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM artifacts WHERE digest=?1",
                params![digest],
                |row| row.get(0),
            )?;
            if exists == 0 {
                tx.execute(
                    "INSERT INTO artifacts(digest,size,refs,created_at,last_access) VALUES(?1,?2,0,?3,?3)",
                    params![digest, *size, *modified],
                )?;
                orphans_found += 1;
            } else {
                tx.execute(
                    "UPDATE artifacts SET size=?2,last_access=CASE WHEN last_access=0 THEN created_at ELSE last_access END WHERE digest=?1",
                    params![digest, *size],
                )?;
            }
        }

        let dangling: Option<String> = tx
            .query_row(
                "SELECT r.digest FROM refs r LEFT JOIN artifacts a ON a.digest=r.digest WHERE a.digest IS NULL LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(digest) = dangling {
            return Err(Error::msg(format!("引用指向不存在的制品: {digest}")));
        }

        let refs_repaired = tx.execute(
            "UPDATE artifacts SET refs=(SELECT COUNT(*) FROM refs WHERE refs.digest=artifacts.digest)",
            [],
        )? as u64;
        tx.commit()?;
        Ok(ReconcileReport {
            parts_removed,
            orphans_found,
            missing_records_removed,
            refs_repaired,
            finished_at: now,
        })
    }

    /// why: TTL/LRU 以命名空间为驱逐单元，才能保持 tag、manifest、layer 的业务引用一致。
    pub fn prune(
        &self,
        dry_run: bool,
        max_bytes: Option<u64>,
        ttl: Option<Duration>,
    ) -> Result<PruneReport> {
        if max_bytes.is_some_and(|value| value > i64::MAX as u64) {
            return Err(Error::msg("cache_max_bytes 超出 i64::MAX"));
        }
        if ttl.is_some_and(|value| value.as_secs() > i64::MAX as u64) {
            return Err(Error::msg("cache_ttl 超出 i64::MAX 秒"));
        }
        let _lifecycle = self.lifecycle.lock().expect("lifecycle");
        let (artifacts, refs, bytes_before) = self.prune_snapshot()?;
        let mut ns_access: HashMap<(String, String), i64> = HashMap::new();
        for row in &refs {
            let access = artifacts
                .get(&row.digest)
                .map(|artifact| artifact.last_access)
                .unwrap_or_default();
            ns_access
                .entry((row.repo.clone(), row.namespace.clone()))
                .and_modify(|current| *current = (*current).max(access))
                .or_insert(access);
        }
        let mut ordered: Vec<_> = ns_access.into_iter().collect();
        ordered.sort_by_key(|((repo, ns), access)| (*access, repo.clone(), ns.clone()));
        let cutoff = ttl.map(|value| now_secs().saturating_sub(value.as_secs() as i64));
        let mut selected = HashSet::new();
        if let Some(cutoff) = cutoff {
            for (ns, access) in &ordered {
                if *access <= cutoff {
                    selected.insert(ns.clone());
                }
            }
        }
        let referenced: HashSet<_> = refs.iter().map(|row| row.digest.as_str()).collect();
        let recent_cutoff = now_secs().saturating_sub(ORPHAN_GRACE_SECS);
        let protected: HashSet<_> = artifacts
            .iter()
            .filter(|(digest, row)| {
                !referenced.contains(digest.as_str()) && row.last_access > recent_cutoff
            })
            .map(|(digest, _)| digest.clone())
            .collect();
        let mut reclaim = calculate_reclaimable(&artifacts, &refs, &selected, &protected);
        if let Some(limit) = max_bytes {
            for (ns, _) in &ordered {
                if bytes_before.saturating_sub(reclaim.1) <= limit as i64 {
                    break;
                }
                selected.insert(ns.clone());
                reclaim = calculate_reclaimable(&artifacts, &refs, &selected, &protected);
            }
        }
        let projected = bytes_before.saturating_sub(reclaim.1);
        let limit_satisfied = max_bytes.is_none_or(|limit| projected <= limit as i64);
        let reason = (!limit_satisfied)
            .then(|| "可驱逐命名空间不足，近期落盘制品仍在安全保护期".to_string());
        if !dry_run {
            let mut namespaces: Vec<_> = selected.iter().cloned().collect();
            namespaces.sort();
            for (repo, ns) in namespaces {
                self.delete_namespace_locked(&repo, &ns)?;
            }
            self.remove_orphan_artifacts()?;
        }
        let bytes_after = if dry_run { projected } else { self.stats()?.1 };
        let report = PruneReport {
            dry_run,
            namespaces: selected.len() as u64,
            artifacts: reclaim.0,
            bytes_before,
            bytes_reclaimable: reclaim.1,
            bytes_after,
            limit_satisfied,
            reason,
            finished_at: now_secs(),
        };
        if !dry_run {
            self.save_last_prune(&report)?;
        }
        Ok(report)
    }

    pub fn last_prune(&self) -> Result<Option<PruneReport>> {
        let db = self.db.lock().expect("db");
        let raw: Option<String> = db
            .query_row(
                "SELECT value FROM maintenance WHERE key='last_prune'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|value| serde_json::from_str(&value).map_err(Error::from))
            .transpose()
    }

    pub fn storage_status(&self, max_bytes: Option<u64>) -> StorageStatus {
        let result = self.check_writable().and_then(|_| self.stats());
        match result {
            Ok((_, bytes, _)) => {
                let capacity_ok = max_bytes.is_none_or(|max| bytes <= max as i64);
                StorageStatus {
                    ok: capacity_ok,
                    writable: true,
                    bytes,
                    max_bytes,
                    capacity_ok,
                    error: (!capacity_ok).then(|| "缓存用量已超过 cache_max_bytes".into()),
                    reconcile: self.reconcile.clone(),
                }
            }
            Err(error) => StorageStatus {
                ok: false,
                writable: false,
                bytes: 0,
                max_bytes,
                capacity_ok: false,
                error: Some(error.to_string()),
                reconcile: self.reconcile.clone(),
            },
        }
    }

    fn prune_snapshot(&self) -> Result<(HashMap<String, ArtifactRow>, Vec<RefRow>, i64)> {
        let db = self.db.lock().expect("db");
        let artifacts = {
            let mut stmt = db.prepare("SELECT digest,size,last_access FROM artifacts")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    ArtifactRow {
                        size: row.get(1)?,
                        last_access: row.get(2)?,
                    },
                ))
            })?;
            rows.collect::<std::result::Result<HashMap<_, _>, _>>()?
        };
        let refs = {
            let mut stmt = db.prepare("SELECT repo,namespace,digest FROM refs")?;
            let rows = stmt.query_map([], |row| {
                Ok(RefRow {
                    repo: row.get(0)?,
                    namespace: row.get(1)?,
                    digest: row.get(2)?,
                })
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let bytes = artifacts.values().map(|artifact| artifact.size).sum();
        Ok((artifacts, refs, bytes))
    }

    fn remove_orphan_artifacts(&self) -> Result<()> {
        let db = self.db.lock().expect("db");
        let cutoff = now_secs().saturating_sub(ORPHAN_GRACE_SECS);
        let digests = {
            let mut stmt = db.prepare("SELECT digest FROM artifacts WHERE refs=0")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for digest in digests {
            let refs: i64 = db.query_row(
                "SELECT COUNT(*) FROM refs WHERE digest=?1",
                params![digest],
                |row| row.get(0),
            )?;
            let access: i64 = db.query_row(
                "SELECT last_access FROM artifacts WHERE digest=?1",
                params![digest],
                |row| row.get(0),
            )?;
            if refs > 0 || access > cutoff {
                continue;
            }
            self.remove_blob(&digest)?;
            db.execute(
                "DELETE FROM artifacts WHERE digest=?1 AND refs=0",
                params![digest],
            )?;
        }
        Ok(())
    }

    fn save_last_prune(&self, report: &PruneReport) -> Result<()> {
        let raw = serde_json::to_string(report)?;
        let db = self.db.lock().expect("db");
        db.execute(
            "INSERT INTO maintenance(key,value) VALUES('last_prune',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![raw],
        )?;
        Ok(())
    }

    fn check_writable(&self) -> Result<()> {
        let dir = self.blob_root.join("tmp");
        std::fs::create_dir_all(&dir)?;
        let path = unique_tmp(&dir, "health")?;
        std::fs::write(&path, b"health")?;
        std::fs::remove_file(path)?;
        Ok(())
    }
}

fn calculate_reclaimable(
    artifacts: &HashMap<String, ArtifactRow>,
    refs: &[RefRow],
    selected: &HashSet<(String, String)>,
    protected: &HashSet<String>,
) -> (u64, i64) {
    let mut retained = HashSet::new();
    for row in refs {
        if !selected.contains(&(row.repo.clone(), row.namespace.clone())) {
            retained.insert(row.digest.as_str());
        }
    }
    retained.extend(protected.iter().map(String::as_str));
    let reclaimable: Vec<_> = artifacts
        .iter()
        .filter(|(digest, _)| !retained.contains(digest.as_str()))
        .collect();
    (
        reclaimable.len() as u64,
        reclaimable.iter().map(|(_, row)| row.size).sum(),
    )
}

fn clean_parts(root: &Path) -> Result<u64> {
    let mut removed = 0u64;
    for path in collect_files(root)? {
        if path.extension().is_some_and(|ext| ext == "part") {
            std::fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn scan_blobs(root: &Path) -> Result<HashMap<String, (i64, i64)>> {
    let sha_root = root.join("sha256");
    if !sha_root.exists() {
        return Ok(HashMap::new());
    }
    let mut out = HashMap::new();
    for path in collect_files(&sha_root)? {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| Error::msg(format!("非 UTF-8 制品路径: {}", path.display())))?;
        let digest = format!("sha256:{name}");
        digest_hex(&digest)?;
        let expected_prefix = &name[..2];
        let actual_prefix = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|value| value.to_str());
        if actual_prefix != Some(expected_prefix) {
            return Err(Error::msg(format!(
                "制品目录与摘要不匹配: {}",
                path.display()
            )));
        }
        let metadata = path.metadata()?;
        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| Error::msg(format!("制品修改时间非法: {error}")))?
            .as_secs() as i64;
        out.insert(digest, (metadata.len() as i64, modified));
    }
    Ok(out)
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut dirs = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(Error::msg(format!(
                    "存储目录不允许符号链接: {}",
                    entry.path().display()
                )));
            }
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
            }
        }
    }
    Ok(files)
}

trait OptionalExt<T> {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/maintenance-tests")
                .join(format!("{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&path).expect("创建测试目录");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn startup_cleans_part_and_registers_orphan() {
        let dir = TestDir::new();
        let tmp = dir.0.join("blobs/tmp/old.part");
        std::fs::create_dir_all(tmp.parent().expect("临时目录")).expect("创建临时目录");
        std::fs::write(&tmp, b"partial").expect("写临时文件");
        let body = b"orphan";
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(body)));
        let hex = digest.strip_prefix("sha256:").expect("摘要前缀");
        let blob = dir.0.join("blobs/sha256").join(&hex[..2]).join(hex);
        std::fs::create_dir_all(blob.parent().expect("制品目录")).expect("创建制品目录");
        std::fs::write(&blob, body).expect("写制品");

        let store = Store::open(&dir.0).expect("打开存储");

        assert!(!tmp.exists());
        assert_eq!(store.reconcile_report().parts_removed, 1);
        assert_eq!(store.reconcile_report().orphans_found, 1);
        assert_eq!(store.stats().expect("统计"), (1, body.len() as i64, 0));
    }

    #[tokio::test]
    async fn prune_lru_keeps_shared_artifact() {
        let dir = TestDir::new();
        let store = Store::open(&dir.0).expect("打开存储");
        let shared = store.write_blob(b"shared").await.expect("写共享制品");
        let only_a = store.write_blob(b"aaa").await.expect("写 A 制品");
        let only_b = store.write_blob(b"bbbb").await.expect("写 B 制品");
        store
            .put_tag("docker", "a", "shared", &shared)
            .expect("链接 A");
        store
            .put_tag("docker", "a", "only", &only_a)
            .expect("链接 A");
        store
            .put_tag("docker", "b", "shared", &shared)
            .expect("链接 B");
        store
            .put_tag("docker", "b", "only", &only_b)
            .expect("链接 B");

        let report = store.prune(false, Some(10), None).expect("驱逐");

        assert!(report.limit_satisfied);
        assert_eq!(report.namespaces, 1);
        assert!(!store.has_digest(&only_a).expect("检查 A"));
        assert!(store.has_digest(&shared).expect("检查共享制品"));
        assert!(store.has_digest(&only_b).expect("检查 B"));
        assert_eq!(store.stats().expect("统计").1, 10);
    }

    #[tokio::test]
    async fn reconcile_rejects_missing_referenced_blob() {
        let dir = TestDir::new();
        let store = Store::open(&dir.0).expect("打开存储");
        let digest = store.write_blob(b"lost").await.expect("写制品");
        store
            .put_tag("docker", "a", "latest", &digest)
            .expect("链接");
        std::fs::remove_file(store.blob_path(&digest).expect("制品路径")).expect("删制品");
        drop(store);

        let error = Store::open(&dir.0).err().expect("启动必须失败");
        assert!(error.to_string().contains("引用制品文件缺失"));
    }
}
