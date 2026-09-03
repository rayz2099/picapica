use crate::error::{Error, Result};
use rusqlite::{params, Connection};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

mod maintenance;

pub use maintenance::{PruneReport, ReconcileReport, StorageStatus};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize)]
pub struct NsRow {
    pub repo: String,
    pub namespace: String,
    pub objects: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeEntry {
    pub name: String,
    pub namespace: String,
    pub leaf: bool,
    pub deeper: bool,
    pub objects: i64,
    pub bytes: i64,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreePage {
    pub repo: String,
    pub prefix: String,
    pub page: u32,
    pub per_page: u32,
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_ns: Option<TreeEntry>,
    pub entries: Vec<TreeEntry>,
}

pub struct Store {
    pub(super) db: Mutex<Connection>,
    pub(super) blob_root: PathBuf,
    pub(super) lifecycle: Mutex<()>,
    reconcile: ReconcileReport,
}

impl Store {
    pub fn open(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        std::fs::create_dir_all(data_dir.join("blobs"))?;
        let db_path = data_dir.join("meta.sqlite");
        let db = Connection::open(&db_path)?;
        db.execute_batch(
            "
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS artifacts (
                digest TEXT PRIMARY KEY,
                size INTEGER NOT NULL,
                refs INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                last_access INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS refs (
                repo TEXT NOT NULL,
                namespace TEXT NOT NULL,
                name TEXT NOT NULL,
                digest TEXT NOT NULL,
                PRIMARY KEY (repo, namespace, name)
            );
            CREATE TABLE IF NOT EXISTS maintenance (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            ",
        )?;
        ensure_column(
            &db,
            "artifacts",
            "last_access",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        let mut store = Self {
            db: Mutex::new(db),
            blob_root: data_dir.join("blobs"),
            lifecycle: Mutex::new(()),
            reconcile: ReconcileReport::default(),
        };
        store.reconcile = store.reconcile_storage()?;
        Ok(store)
    }

    /// why: digest 来自匿名数据面，先限定为规范 sha256，避免路径片段逃出缓存目录。
    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        let hex = digest_hex(digest)?;
        let prefix = &hex[..2];
        Ok(self.blob_root.join("sha256").join(prefix).join(hex))
    }

    pub fn has_digest(&self, digest: &str) -> Result<bool> {
        let exists = self.blob_path(digest)?.is_file();
        if exists {
            self.touch(digest)?;
        }
        Ok(exists)
    }

    pub fn get_tag(&self, repo: &str, name: &str, tag: &str) -> Result<Option<String>> {
        let db = self.db.lock().expect("db");
        let ns = image_ns(name);
        let mut stmt =
            db.prepare("SELECT digest FROM refs WHERE repo=?1 AND namespace=?2 AND name=?3")?;
        let key = format!("tag:{tag}");
        let g: Option<String> = stmt
            .query_row(params![repo, ns, key], |r| r.get(0))
            .optional()?;
        Ok(g)
    }

    pub fn put_tag(&self, repo: &str, name: &str, tag: &str, digest: &str) -> Result<()> {
        self.link(repo, &image_ns(name), &format!("tag:{tag}"), digest)
    }

    pub fn put_file(&self, repo: &str, path: &str, digest: &str) -> Result<()> {
        let ns = file_namespace(path);
        self.link(repo, &ns, path, digest)
    }

    pub fn get_file(&self, repo: &str, path: &str) -> Result<Option<String>> {
        let db = self.db.lock().expect("db");
        let ns = file_namespace(path);
        let mut stmt =
            db.prepare("SELECT digest FROM refs WHERE repo=?1 AND namespace=?2 AND name=?3")?;
        let g = stmt
            .query_row(params![repo, ns, path], |r| r.get(0))
            .optional()?;
        Ok(g)
    }

    fn link(&self, repo: &str, ns: &str, name: &str, digest: &str) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().expect("lifecycle");
        let path = self.blob_path(digest)?;
        let mut db = self.db.lock().expect("db");
        if !path.is_file() {
            return Err(Error::msg(format!("引用的制品不存在: {digest}")));
        }
        let tx = db.transaction()?;
        let now = now_secs();
        tx.execute(
            "INSERT INTO artifacts(digest,size,refs,created_at,last_access) VALUES(?1,0,0,?2,?2)
             ON CONFLICT(digest) DO UPDATE SET last_access=excluded.last_access",
            params![digest, now],
        )?;
        let old: Option<String> = tx
            .query_row(
                "SELECT digest FROM refs WHERE repo=?1 AND namespace=?2 AND name=?3",
                params![repo, ns, name],
                |r| r.get(0),
            )
            .optional()?;
        if old.as_deref() == Some(digest) {
            let refs: i64 = tx.query_row(
                "SELECT COUNT(*) FROM refs WHERE digest=?1",
                params![digest],
                |row| row.get(0),
            )?;
            tx.execute(
                "UPDATE artifacts SET refs=?2 WHERE digest=?1",
                params![digest, refs],
            )?;
            tx.commit()?;
            return Ok(());
        }
        tx.execute(
            "INSERT INTO refs(repo,namespace,name,digest) VALUES(?1,?2,?3,?4)
             ON CONFLICT(repo,namespace,name) DO UPDATE SET digest=excluded.digest",
            params![repo, ns, name, digest],
        )?;
        let new_refs: i64 = tx.query_row(
            "SELECT COUNT(*) FROM refs WHERE digest=?1",
            params![digest],
            |row| row.get(0),
        )?;
        tx.execute(
            "UPDATE artifacts SET refs=?2 WHERE digest=?1",
            params![digest, new_refs],
        )?;
        let obsolete = if let Some(prev) = old {
            let refs: i64 = tx.query_row(
                "SELECT COUNT(*) FROM refs WHERE digest=?1",
                params![prev],
                |row| row.get(0),
            )?;
            tx.execute(
                "UPDATE artifacts SET refs=?2 WHERE digest=?1",
                params![prev, refs],
            )?;
            if refs == 0 {
                tx.execute("DELETE FROM artifacts WHERE digest=?1", params![prev])?;
                Some(prev)
            } else {
                None
            }
        } else {
            None
        };
        tx.commit()?;
        if let Some(prev) = obsolete {
            self.remove_blob(&prev)?;
        }
        drop(db);
        Ok(())
    }

    pub fn set_size(&self, digest: &str, size: i64) -> Result<()> {
        digest_hex(digest)?;
        let _lifecycle = self.lifecycle.lock().expect("lifecycle");
        let db = self.db.lock().expect("db");
        db.execute(
            "INSERT INTO artifacts(digest,size,refs,created_at,last_access) VALUES(?1,?2,0,?3,?3)
             ON CONFLICT(digest) DO UPDATE SET size=excluded.size,last_access=excluded.last_access",
            params![digest, size, now_secs()],
        )?;
        Ok(())
    }

    /// why: LRU 必须随真实命中推进，不能用文件 mtime 推测访问行为。
    pub fn touch(&self, digest: &str) -> Result<()> {
        digest_hex(digest)?;
        let _lifecycle = self.lifecycle.lock().expect("lifecycle");
        let db = self.db.lock().expect("db");
        db.execute(
            "UPDATE artifacts SET last_access=?2 WHERE digest=?1",
            params![digest, now_secs()],
        )?;
        Ok(())
    }

    pub fn reconcile_report(&self) -> ReconcileReport {
        self.reconcile.clone()
    }

    pub fn stats(&self) -> Result<(i64, i64, i64)> {
        let db = self.db.lock().expect("db");
        let artifacts: i64 = db.query_row("SELECT COUNT(*) FROM artifacts", [], |r| r.get(0))?;
        let bytes: i64 = db.query_row("SELECT COALESCE(SUM(size),0) FROM artifacts", [], |r| {
            r.get(0)
        })?;
        let refs: i64 = db.query_row("SELECT COUNT(*) FROM refs", [], |r| r.get(0))?;
        Ok((artifacts, bytes, refs))
    }

    pub fn search(&self, q: Option<&str>) -> Result<Vec<NsRow>> {
        let db = self.db.lock().expect("db");
        let like = format!("%{}%", q.unwrap_or(""));
        let mut stmt = db.prepare(
            "SELECT r.repo, r.namespace, COUNT(*), COALESCE(SUM(a.size),0)
             FROM refs r LEFT JOIN artifacts a ON a.digest=r.digest
             WHERE r.namespace LIKE ?1 OR r.repo LIKE ?1
             GROUP BY r.repo, r.namespace
             ORDER BY r.repo, r.namespace",
        )?;
        let rows = stmt.query_map(params![like], |row| {
            Ok(NsRow {
                repo: row.get(0)?,
                namespace: row.get(1)?,
                objects: row.get(2)?,
                bytes: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 去掉命名空间引用；制品引用归零后删文件。
    pub fn delete_namespace(&self, repo: &str, ns: &str) -> Result<u64> {
        let _lifecycle = self.lifecycle.lock().expect("lifecycle");
        self.delete_namespace_locked(repo, ns)
    }

    pub(super) fn delete_namespace_locked(&self, repo: &str, ns: &str) -> Result<u64> {
        let db = self.db.lock().expect("db");
        let digests: std::collections::HashSet<String> = {
            let mut stmt = db.prepare("SELECT digest FROM refs WHERE repo=?1 AND namespace=?2")?;
            let x = stmt.query_map(params![repo, ns], |r| r.get(0))?;
            x.collect::<std::result::Result<_, _>>()?
        };
        db.execute(
            "DELETE FROM refs WHERE repo=?1 AND namespace=?2",
            params![repo, ns],
        )?;
        let mut removed = 0u64;
        for d in digests {
            let refs: i64 = db.query_row(
                "SELECT COUNT(*) FROM refs WHERE digest=?1",
                params![d],
                |r| r.get(0),
            )?;
            db.execute(
                "UPDATE artifacts SET refs=?2 WHERE digest=?1",
                params![d, refs],
            )?;
            if refs == 0 {
                let p = self.blob_path(&d)?;
                if p.exists() {
                    std::fs::remove_file(&p)?;
                }
                db.execute("DELETE FROM artifacts WHERE digest=?1", params![d])?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    pub fn namespace_count(&self, repo: &str) -> Result<i64> {
        let db = self.db.lock().expect("db");
        let n: i64 = db.query_row(
            "SELECT COUNT(DISTINCT namespace) FROM refs WHERE repo=?1",
            params![repo],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    pub fn tags(&self, repo: &str, ns: &str) -> Result<Vec<String>> {
        let db = self.db.lock().expect("db");
        let mut stmt = db.prepare(
            "SELECT name FROM refs WHERE repo=?1 AND namespace=?2 AND name LIKE 'tag:%' AND name NOT LIKE 'tag:sha256:%' ORDER BY name",
        )?;
        let rows = stmt.query_map(params![repo, ns], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            let name = r?;
            out.push(name.trim_start_matches("tag:").to_string());
        }
        Ok(out)
    }

    /// why: 树按 / 展开命名空间，分页的是当前层子节点，不是底层制品。
    pub fn tree(&self, repo: &str, prefix: &str, page: u32, per_page: u32) -> Result<TreePage> {
        let rows = self.search(None)?;
        let mine: Vec<NsRow> = rows.into_iter().filter(|r| r.repo == repo).collect();
        let mut page_out = fold_tree(repo, &mine, prefix, page, per_page);
        if let Some(self_ns) = page_out.self_ns.as_mut() {
            self_ns.tags = self.tags(repo, &self_ns.namespace)?;
        }
        for e in &mut page_out.entries {
            if e.leaf {
                e.tags = self.tags(repo, &e.namespace)?;
            }
        }
        Ok(page_out)
    }

    /// why: 全选删除当前节点下全部命名空间，不只当前页。
    pub fn delete_prefix(&self, repo: &str, prefix: &str) -> Result<u64> {
        let _lifecycle = self.lifecycle.lock().expect("lifecycle");
        let rows = self.search(None)?;
        let mut nss: Vec<String> = rows
            .into_iter()
            .filter(|r| r.repo == repo)
            .map(|r| r.namespace)
            .filter(|ns| ns_under_prefix(ns, prefix))
            .collect();
        nss.sort();
        nss.dedup();
        let mut removed = 0u64;
        for ns in nss {
            removed += self.delete_namespace_locked(repo, &ns)?;
        }
        Ok(removed)
    }

    pub async fn write_blob(&self, bytes: &[u8]) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
        self.persist_bytes(&digest, bytes).await?;
        Ok(digest)
    }

    pub async fn persist_bytes(&self, digest: &str, bytes: &[u8]) -> Result<()> {
        verify_digest(digest, bytes)?;
        let path = self.blob_path(digest)?;
        if path.exists() {
            self.set_size(digest, bytes.len() as i64)?;
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let dir = path
            .parent()
            .ok_or_else(|| Error::msg("制品路径没有父目录"))?;
        let tmp = unique_tmp(dir, "part")?;
        let mut cleanup = PartGuard::new(tmp.clone());
        let mut f = tokio::fs::File::create(&tmp).await?;
        f.write_all(bytes).await?;
        f.flush().await?;
        if path.exists() {
            tokio::fs::remove_file(&tmp).await?;
        } else {
            tokio::fs::rename(&tmp, &path).await?;
        }
        cleanup.disarm();
        self.set_size(digest, bytes.len() as i64)?;
        Ok(())
    }

    pub async fn persist_stream<S>(
        &self,
        expected: Option<&str>,
        mut stream: S,
    ) -> Result<(String, u64)>
    where
        S: futures_util::Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Unpin,
    {
        use futures_util::StreamExt;
        if let Some(digest) = expected {
            digest_hex(digest)?;
        }
        let tmp_dir = self.blob_root.join("tmp");
        tokio::fs::create_dir_all(&tmp_dir).await?;
        let tmp = unique_tmp(&tmp_dir, "up")?;
        let mut cleanup = PartGuard::new(tmp.clone());
        let mut f = tokio::fs::File::create(&tmp).await?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = stream.next().await {
            let c = chunk.map_err(|e| Error::msg(format!("读上游失败: {e}")))?;
            hasher.update(&c);
            size += c.len() as u64;
            f.write_all(&c).await?;
        }
        f.flush().await?;
        drop(f);
        let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
        if let Some(exp) = expected {
            if exp != digest {
                return Err(Error::msg(format!(
                    "digest mismatch: want {exp} got {digest}"
                )));
            }
        }
        let dest = self.blob_path(&digest)?;
        if let Some(dir) = dest.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        if dest.exists() {
            tokio::fs::remove_file(&tmp).await?;
        } else {
            tokio::fs::rename(&tmp, &dest).await?;
        }
        cleanup.disarm();
        self.set_size(&digest, size as i64)?;
        Ok((digest, size))
    }

    /// why: 引用事务提交后再删物理文件，删除失败最多留下无引用孤儿，不会留下悬空引用。
    fn remove_blob(&self, digest: &str) -> Result<()> {
        let path = self.blob_path(digest)?;
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// why: future 取消时 async 清理不会执行，Drop 保证临时文件最终被回收。
struct PartGuard {
    path: PathBuf,
    armed: bool,
}

impl PartGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PartGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Err(e) = std::fs::remove_file(&self.path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::error!(path = %self.path.display(), error = %e, "清理临时文件失败");
                }
            }
        }
    }
}

/// why: 只接受规范的小写 sha256，保证同一摘要只有一个缓存路径与数据库键。
fn digest_hex(digest: &str) -> Result<&str> {
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| Error::msg("digest 必须使用 sha256"))?;
    let valid = hex.len() == 64
        && hex
            .as_bytes()
            .iter()
            .all(|c| c.is_ascii_digit() || matches!(c, b'a'..=b'f'));
    if !valid {
        return Err(Error::msg("sha256 digest 必须是 64 位小写十六进制"));
    }
    Ok(hex)
}

fn verify_digest(expected: &str, bytes: &[u8]) -> Result<()> {
    digest_hex(expected)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
    if actual != expected {
        return Err(Error::msg(format!(
            "digest mismatch: want {expected} got {actual}"
        )));
    }
    Ok(())
}

fn ns_under_prefix(ns: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    ns == prefix || ns.starts_with(&format!("{prefix}/"))
}

fn fold_tree(repo: &str, rows: &[NsRow], prefix: &str, page: u32, per_page: u32) -> TreePage {
    use std::collections::BTreeMap;
    struct Agg {
        objects: i64,
        bytes: i64,
        leaf: bool,
        deeper: bool,
    }
    let mut self_ns = None;
    let mut kids: BTreeMap<String, Agg> = BTreeMap::new();
    for row in rows {
        if !ns_under_prefix(&row.namespace, prefix) {
            continue;
        }
        if row.namespace == prefix {
            self_ns = Some(TreeEntry {
                name: prefix.rsplit('/').next().unwrap_or(prefix).to_string(),
                namespace: prefix.to_string(),
                leaf: true,
                deeper: false,
                objects: row.objects,
                bytes: row.bytes,
                tags: Vec::new(),
            });
            continue;
        }
        let rest = if prefix.is_empty() {
            row.namespace.as_str()
        } else {
            row.namespace
                .strip_prefix(&format!("{prefix}/"))
                .unwrap_or(&row.namespace)
        };
        let mut segs = rest.split('/');
        let Some(head) = segs.next() else {
            continue;
        };
        let deeper = segs.next().is_some();
        let e = kids.entry(head.to_string()).or_insert(Agg {
            objects: 0,
            bytes: 0,
            leaf: false,
            deeper: false,
        });
        e.objects += row.objects;
        e.bytes += row.bytes;
        if deeper {
            e.deeper = true;
        } else {
            e.leaf = true;
        }
    }
    if let Some(self_ns) = self_ns.as_mut() {
        self_ns.deeper = !kids.is_empty();
    }
    let names: Vec<String> = kids.keys().cloned().collect();
    let total = names.len();
    let per = per_page.max(1) as usize;
    let page = page.max(1);
    let start = (page as usize - 1).saturating_mul(per);
    let slice = if start >= names.len() {
        &[][..]
    } else {
        let end = (start + per).min(names.len());
        &names[start..end]
    };
    let entries = slice
        .iter()
        .map(|name| {
            let agg = &kids[name];
            let namespace = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            TreeEntry {
                name: name.clone(),
                namespace,
                leaf: agg.leaf,
                deeper: agg.deeper,
                objects: agg.objects,
                bytes: agg.bytes,
                tags: Vec::new(),
            }
        })
        .collect();
    TreePage {
        repo: repo.to_string(),
        prefix: prefix.to_string(),
        page,
        per_page: per as u32,
        total,
        self_ns,
        entries,
    }
}

fn image_ns(name: &str) -> String {
    name.to_string()
}

pub fn file_namespace(path: &str) -> String {
    let p = path.trim_start_matches('/');
    p.split('/').take(3).collect::<Vec<_>>().join("/")
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn ensure_column(db: &Connection, table: &str, column: &str, sql_type: &str) -> Result<()> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = db.prepare(&sql)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(());
        }
    }
    db.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {sql_type}"),
        [],
    )?;
    Ok(())
}

/// why: 不同制品会并发下载，临时文件名必须在单实例进程内唯一。
fn unique_tmp(dir: &Path, prefix: &str) -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::msg(format!("系统时间早于 UNIX_EPOCH: {e}")))?
        .as_nanos();
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    Ok(dir.join(format!("{prefix}-{nanos}-{seq}.part")))
}

trait OptionalExt<T> {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/store-tests");
            std::fs::create_dir_all(&root).expect("创建测试根目录");
            let path = unique_tmp(&root, "case").expect("生成测试目录");
            std::fs::create_dir_all(&path).expect("创建测试目录");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn row(ns: &str, objects: i64, bytes: i64) -> NsRow {
        NsRow {
            repo: "docker".into(),
            namespace: ns.into(),
            objects,
            bytes,
        }
    }

    #[test]
    fn new_store_does_not_persist_transfers() {
        let dir = TestDir::new();
        let store = Store::open(&dir.0).expect("打开存储");
        let db = store.db.lock().expect("db");
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='transfers'",
                [],
                |row| row.get(0),
            )
            .expect("检查传输表");
        assert_eq!(count, 0);
    }

    #[test]
    fn tree_splits_variable_depth() {
        let rows = vec![
            row("library/busybox", 2, 10),
            row("library/redis", 3, 20),
            row("grafana/grafana", 1, 5),
        ];
        let page = fold_tree("docker", &rows, "", 1, 50);
        assert_eq!(page.total, 2);
        assert_eq!(page.entries[0].name, "grafana");
        assert!(page.entries[0].deeper || page.entries[0].leaf);
        assert_eq!(page.entries[1].name, "library");
        assert!(page.entries[1].deeper);
        assert!(!page.entries[1].leaf);

        let lib = fold_tree("docker", &rows, "library", 1, 50);
        assert_eq!(lib.total, 2);
        assert!(lib.entries.iter().all(|e| e.leaf && !e.deeper));
        assert_eq!(lib.entries[0].namespace, "library/busybox");
    }

    #[test]
    fn tree_paginates_children() {
        let rows: Vec<NsRow> = (0..3)
            .map(|i| row(&format!("library/img{i}"), 1, 1))
            .collect();
        let page = fold_tree("docker", &rows, "library", 2, 1);
        assert_eq!(page.total, 3);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].name, "img1");
    }

    #[test]
    fn digest_path_rejects_non_canonical_input() {
        let dir = TestDir::new();
        let store = Store::open(&dir.0).expect("打开存储");
        let attacks = [
            "../../etc/passwd",
            "sha256:../../etc/passwd",
            "sha256:/etc/passwd",
            "sha256:abcd",
            "sha512:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ];
        for digest in attacks {
            assert!(store.blob_path(digest).is_err(), "应拒绝 {digest}");
        }
    }

    #[tokio::test]
    async fn persist_bytes_rejects_digest_mismatch() {
        let dir = TestDir::new();
        let store = Store::open(&dir.0).expect("打开存储");
        let mut hasher = Sha256::new();
        hasher.update(b"expected");
        let digest = format!("sha256:{}", hex::encode(hasher.finalize()));

        let err = store
            .persist_bytes(&digest, b"tampered")
            .await
            .expect_err("摘要不一致必须失败");

        assert!(err.to_string().contains("digest mismatch"));
        assert!(!store.has_digest(&digest).expect("检查缓存"));
        assert_eq!(store.stats().expect("读取统计"), (0, 0, 0));
    }

    #[tokio::test]
    async fn cancelled_stream_removes_part_file() {
        use futures_util::StreamExt;
        let dir = TestDir::new();
        let store = std::sync::Arc::new(Store::open(&dir.0).expect("打开存储"));
        let stream = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(
            "partial",
        ))])
        .chain(futures_util::stream::pending());
        let task_store = store.clone();
        let task = tokio::spawn(async move { task_store.persist_stream(None, stream).await });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        task.abort();
        let _ = task.await;

        let tmp = dir.0.join("blobs/tmp");
        let entries = std::fs::read_dir(tmp).expect("读临时目录").count();
        assert_eq!(entries, 0);
    }

    #[tokio::test]
    async fn replacing_last_reference_removes_blob() {
        let dir = TestDir::new();
        let store = Store::open(&dir.0).expect("打开存储");
        let old = store.write_blob(b"old").await.expect("写旧制品");
        let new = store.write_blob(b"new").await.expect("写新制品");
        store
            .put_tag("docker-a", "library/a", "latest", &old)
            .expect("链接第一份引用");
        store
            .put_tag("docker-b", "library/b", "latest", &old)
            .expect("链接第二份引用");

        store
            .put_tag("docker-a", "library/a", "latest", &new)
            .expect("替换第一份引用");
        assert!(store.has_digest(&old).expect("检查共享制品"));

        store
            .put_tag("docker-b", "library/b", "latest", &new)
            .expect("替换最后一份引用");
        assert!(!store.has_digest(&old).expect("检查旧制品"));
        assert!(store.has_digest(&new).expect("检查新制品"));
        assert_eq!(store.stats().expect("读取统计"), (1, 3, 2));
    }

    #[test]
    fn link_rejects_missing_blob() {
        let dir = TestDir::new();
        let store = Store::open(&dir.0).expect("打开存储");
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        let err = store
            .put_tag("docker", "library/a", "latest", digest)
            .expect_err("不存在的文件不能建立引用");

        assert!(err.to_string().contains("制品不存在"));
        assert_eq!(store.stats().expect("读取统计"), (0, 0, 0));
    }
}
