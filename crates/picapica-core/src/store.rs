use crate::error::{Error, Result};
use rusqlite::{params, Connection};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Serialize)]
pub struct NsRow {
    pub repo: String,
    pub namespace: String,
    pub objects: i64,
    pub bytes: i64,
}

pub struct Store {
    db: Mutex<Connection>,
    blob_root: PathBuf,
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
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS refs (
                repo TEXT NOT NULL,
                namespace TEXT NOT NULL,
                name TEXT NOT NULL,
                digest TEXT NOT NULL,
                PRIMARY KEY (repo, namespace, name)
            );
            ",
        )?;
        Ok(Self {
            db: Mutex::new(db),
            blob_root: data_dir.join("blobs"),
        })
    }

    pub fn blob_path(&self, digest: &str) -> PathBuf {
        let hex = digest.rsplit(':').next().unwrap_or(digest);
        let prefix = hex.get(..2).unwrap_or("00");
        self.blob_root.join("sha256").join(prefix).join(hex)
    }

    pub fn has_digest(&self, digest: &str) -> bool {
        self.blob_path(digest).is_file()
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
        let ns = file_ns(path);
        self.link(repo, &ns, path, digest)
    }

    pub fn get_file(&self, repo: &str, path: &str) -> Result<Option<String>> {
        let db = self.db.lock().expect("db");
        let ns = file_ns(path);
        let mut stmt =
            db.prepare("SELECT digest FROM refs WHERE repo=?1 AND namespace=?2 AND name=?3")?;
        let g = stmt
            .query_row(params![repo, ns, path], |r| r.get(0))
            .optional()?;
        Ok(g)
    }

    fn link(&self, repo: &str, ns: &str, name: &str, digest: &str) -> Result<()> {
        let db = self.db.lock().expect("db");
        let now = now_secs();
        db.execute(
            "INSERT INTO artifacts(digest,size,refs,created_at) VALUES(?1,0,0,?2)
             ON CONFLICT(digest) DO NOTHING",
            params![digest, now],
        )?;
        let old: Option<String> = db
            .query_row(
                "SELECT digest FROM refs WHERE repo=?1 AND namespace=?2 AND name=?3",
                params![repo, ns, name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prev) = old {
            if prev != digest {
                db.execute(
                    "UPDATE artifacts SET refs = MAX(refs-1,0) WHERE digest=?1",
                    params![prev],
                )?;
            }
        }
        db.execute(
            "INSERT INTO refs(repo,namespace,name,digest) VALUES(?1,?2,?3,?4)
             ON CONFLICT(repo,namespace,name) DO UPDATE SET digest=excluded.digest",
            params![repo, ns, name, digest],
        )?;
        db.execute(
            "UPDATE artifacts SET refs = refs+1 WHERE digest=?1",
            params![digest],
        )?;
        Ok(())
    }

    pub fn set_size(&self, digest: &str, size: i64) -> Result<()> {
        let db = self.db.lock().expect("db");
        db.execute(
            "UPDATE artifacts SET size=?2 WHERE digest=?1",
            params![digest, size],
        )?;
        Ok(())
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
        let db = self.db.lock().expect("db");
        let digests: Vec<String> = {
            let mut stmt = db.prepare("SELECT digest FROM refs WHERE repo=?1 AND namespace=?2")?;
            let x = stmt.query_map(params![repo, ns], |r| r.get(0))?;
            x.collect::<std::result::Result<Vec<_>, _>>()?
        };
        db.execute(
            "DELETE FROM refs WHERE repo=?1 AND namespace=?2",
            params![repo, ns],
        )?;
        let mut removed = 0u64;
        for d in digests {
            db.execute(
                "UPDATE artifacts SET refs = MAX(refs-1,0) WHERE digest=?1",
                params![d],
            )?;
            let refs: i64 = db.query_row(
                "SELECT refs FROM artifacts WHERE digest=?1",
                params![d],
                |r| r.get(0),
            )?;
            if refs == 0 {
                let p = self.blob_path(&d);
                if p.exists() {
                    let _ = std::fs::remove_file(&p);
                }
                db.execute("DELETE FROM artifacts WHERE digest=?1", params![d])?;
                removed += 1;
            }
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
        let path = self.blob_path(digest);
        if path.exists() {
            self.set_size(digest, bytes.len() as i64)?;
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let tmp = path.with_extension("part");
        let mut f = tokio::fs::File::create(&tmp).await?;
        f.write_all(bytes).await?;
        f.flush().await?;
        tokio::fs::rename(&tmp, &path).await?;
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
        let tmp_dir = self.blob_root.join("tmp");
        tokio::fs::create_dir_all(&tmp_dir).await?;
        let tmp = tmp_dir.join(format!("up-{}", now_secs()));
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
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(Error::msg(format!(
                    "digest mismatch: want {exp} got {digest}"
                )));
            }
        }
        let dest = self.blob_path(&digest);
        if let Some(dir) = dest.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        if dest.exists() {
            let _ = tokio::fs::remove_file(&tmp).await;
        } else {
            tokio::fs::rename(&tmp, &dest).await?;
        }
        self.set_size(&digest, size as i64)?;
        Ok((digest, size))
    }
}

fn image_ns(name: &str) -> String {
    name.to_string()
}

fn file_ns(path: &str) -> String {
    let p = path.trim_start_matches('/');
    p.split('/').take(3).collect::<Vec<_>>().join("/")
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
