use crate::config::{Config, Repo};
use crate::egress::Egress;
use crate::error::{Error, Result};
use crate::probe::{self, ProbeRow};
use crate::store::{PruneReport, Store};
pub use crate::transfers::TransferGuard;
use crate::transfers::{TransferRegistry, TransferRow};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use tokio::sync::{Mutex, RwLock};

pub struct App {
    pub config_path: PathBuf,
    pub config: RwLock<Config>,
    pub store: Store,
    pub egress: RwLock<Egress>,
    pub ranks: RwLock<HashMap<String, Vec<ProbeRow>>>,
    pub inflight: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    tag_hints: StdMutex<HashMap<String, Vec<String>>>,
    transfers: TransferRegistry,
    reload_error: RwLock<Option<String>>,
    metrics: Metrics,
}

#[derive(Default)]
struct Metrics {
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    upstream_failures: AtomicU64,
    bytes_served: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub upstream_failures: u64,
    pub bytes_served: u64,
}

fn hint_key(repo: &str, name: &str, digest: &str) -> String {
    format!("{repo}\0{name}\0{digest}")
}

impl App {
    pub fn boot(config_path: PathBuf) -> Result<Arc<Self>> {
        let cfg = Config::load_or_seed(&config_path)?;
        let store = Store::open(&cfg.data_dir)?;
        let egress = prepare_egress(&cfg)?;
        Ok(Arc::new(Self {
            config_path,
            config: RwLock::new(cfg),
            store,
            egress: RwLock::new(egress),
            ranks: RwLock::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            tag_hints: StdMutex::new(HashMap::new()),
            transfers: TransferRegistry::default(),
            reload_error: RwLock::new(None),
            metrics: Metrics::default(),
        }))
    }

    pub async fn snapshot(&self) -> Config {
        self.config.read().await.clone()
    }

    pub async fn repo(&self, name: &str) -> Result<Repo> {
        self.config.read().await.repo(name).cloned()
    }

    pub async fn repo_by_host(&self, host: &str) -> Result<Repo> {
        self.config.read().await.repo_by_host(host).cloned()
    }

    pub async fn ranks_for(&self, repo: &str) -> Option<Vec<ProbeRow>> {
        self.ranks.read().await.get(repo).cloned()
    }

    pub async fn reload(&self) -> Result<()> {
        let result = self.reload_inner().await;
        match &result {
            Ok(()) => self.set_reload_error(None).await,
            Err(e) => self.set_reload_error(Some(e.to_string())).await,
        }
        result
    }

    pub async fn replace_config(&self, cfg: Config) -> Result<()> {
        cfg.validate()?;
        let old = self.snapshot().await;
        ensure_hot_reloadable(&old, &cfg)?;
        self.ensure_repos_removable(&old, &cfg)?;
        // why: 出站构建可能失败，必须在写事实源前完成，避免磁盘配置和运行态分裂。
        let egress = prepare_egress(&cfg)?;
        cfg.save(&self.config_path)?;
        *self.egress.write().await = egress;
        *self.config.write().await = cfg;
        self.set_reload_error(None).await;
        Ok(())
    }

    async fn reload_inner(&self) -> Result<()> {
        let cfg = Config::load(&self.config_path)?;
        let old = self.snapshot().await;
        ensure_hot_reloadable(&old, &cfg)?;
        self.ensure_repos_removable(&old, &cfg)?;
        let egress = prepare_egress(&cfg)?;
        *self.egress.write().await = egress;
        *self.config.write().await = cfg;
        Ok(())
    }

    pub async fn reload_error(&self) -> Option<String> {
        self.reload_error.read().await.clone()
    }

    pub async fn set_reload_error(&self, error: Option<String>) {
        *self.reload_error.write().await = error;
    }

    /// why: 手改 YAML 与控制 API 必须遵守同一删除约束，避免缓存变成无入口孤儿。
    fn ensure_repos_removable(&self, old: &Config, new: &Config) -> Result<()> {
        for repo in &old.repos {
            if new
                .repos
                .iter()
                .any(|candidate| candidate.name == repo.name)
            {
                continue;
            }
            let count = self.store.namespace_count(&repo.name)?;
            if count > 0 {
                return Err(Error::msg(format!(
                    "不允许删除仓库 {}：还有 {count} 个缓存命名空间",
                    repo.name
                )));
            }
        }
        Ok(())
    }

    pub async fn caching(&self) -> bool {
        self.config.read().await.cache
    }

    pub async fn probe_all(&self) -> Result<HashMap<String, Vec<ProbeRow>>> {
        let cfg = self.snapshot().await;
        let egress = self.egress.read().await;
        let mut map = HashMap::new();
        for repo in &cfg.repos {
            let rows = probe::probe_repo(&egress, repo).await?;
            tracing::info!(repo = %repo.name, ?rows, "probe");
            map.insert(repo.name.clone(), rows);
        }
        drop(egress);
        *self.ranks.write().await = map.clone();
        Ok(map)
    }

    pub async fn digest_lock(&self, digest: &str) -> Arc<Mutex<()>> {
        let mut g = self.inflight.lock().await;
        // why: digest 基数没有上限，弱引用让已完成下载的互斥锁可回收。
        g.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = g.get(digest).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        let weak = Arc::downgrade(&lock);
        g.insert(digest.to_string(), weak);
        lock
    }

    /// why: containerd 先 HEAD tag 再按 digest 拉 manifest，必须把可变指针记下来，落地后才能写回人类 tag。
    pub fn note_tag(&self, repo: &str, name: &str, reference: &str, digest: &str) -> Result<()> {
        if reference.starts_with("sha256:") {
            return Ok(());
        }
        if self.store.has_digest(digest)? {
            return self.store.put_tag(repo, name, reference, digest);
        }
        let key = hint_key(repo, name, digest);
        let mut hints = self.tag_hints.lock().expect("tag_hints");
        if hints.len() >= 4096 {
            hints.clear();
        }
        let tags = hints.entry(key).or_default();
        if !tags.iter().any(|tag| tag == reference) {
            tags.push(reference.to_string());
        }
        Ok(())
    }

    /// why: digest 引用负责计数和树节点；同一摘要上若有 HEAD 记下的 tag，要一并写成可变指针。
    pub fn bind_digest(&self, repo: &str, name: &str, digest: &str) -> Result<()> {
        self.store.put_tag(repo, name, digest, digest)?;
        let key = hint_key(repo, name, digest);
        let tags = self
            .tag_hints
            .lock()
            .expect("tag_hints")
            .remove(&key)
            .unwrap_or_default();
        for tag in tags {
            self.store.put_tag(repo, name, &tag, digest)?;
        }
        Ok(())
    }

    pub fn check_token(&self, presented: Option<&str>, cfg: &Config) -> Result<()> {
        let Some(raw) = presented else {
            return Err(Error::msg("未授权：缺少令牌"));
        };
        let token = raw.strip_prefix("Bearer ").unwrap_or(raw);
        if cfg.api_keys.iter().any(|k| k == token) {
            Ok(())
        } else {
            Err(Error::msg("未授权：令牌不匹配"))
        }
    }

    pub fn record_cache_hit(&self) {
        self.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cache_miss(&self) {
        self.metrics.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_upstream_failure(&self) {
        self.metrics
            .upstream_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_bytes_served(&self, bytes: u64) {
        self.metrics
            .bytes_served
            .fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn metrics(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            cache_hits: self.metrics.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.metrics.cache_misses.load(Ordering::Relaxed),
            upstream_failures: self.metrics.upstream_failures.load(Ordering::Relaxed),
            bytes_served: self.metrics.bytes_served.load(Ordering::Relaxed),
        }
    }

    pub fn begin_transfer(
        self: &Arc<Self>,
        repo: &str,
        namespace: &str,
        name: &str,
        upstream: &str,
        total: Option<u64>,
    ) -> Result<TransferGuard> {
        self.transfers.begin(repo, namespace, name, upstream, total)
    }

    pub fn transfers(&self, limit: u32) -> Vec<TransferRow> {
        self.transfers.list(limit)
    }

    pub fn active_transfers(&self) -> usize {
        self.transfers.active_count()
    }

    pub async fn prune(&self, dry_run: bool) -> Result<PruneReport> {
        let cfg = self.snapshot().await;
        let ttl = cfg.cache_ttl_std()?;
        if dry_run {
            return self.store.prune(true, cfg.cache_max_bytes, ttl);
        }
        self.transfers
            .run_if_idle(|| self.store.prune(false, cfg.cache_max_bytes, ttl))
    }
}

/// why: 监听地址、存储句柄和测速调度在启动时固定，在线伪更新会误导控制面。
fn ensure_hot_reloadable(old: &Config, new: &Config) -> Result<()> {
    let mut changed = Vec::new();
    if old.listen != new.listen {
        changed.push("listen");
    }
    if old.data_dir != new.data_dir {
        changed.push("data_dir");
    }
    if old.probe_interval != new.probe_interval {
        changed.push("probe_interval");
    }
    if old.log_level != new.log_level {
        changed.push("log_level");
    }
    if old.log_retain != new.log_retain {
        changed.push("log_retain");
    }
    if changed.is_empty() {
        return Ok(());
    }
    Err(Error::msg(format!(
        "不允许在线修改启动字段 {}，需重启 picapica",
        changed.join("、")
    )))
}

fn prepare_egress(cfg: &Config) -> Result<Egress> {
    let egress = Egress::new(cfg.proxy_url.as_deref())?;
    // why: 单仓库代理原本延迟创建，PUT 必须在落盘前穷尽所有构建失败点。
    for repo in &cfg.repos {
        for upstream in &repo.upstreams {
            egress.client(&upstream.proxy)?;
        }
    }
    Ok(egress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/picapica-tests")
                .join(format!("app-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create test dir");
            Self(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn config(dir: &TestDir) -> Config {
        Config {
            listen: "127.0.0.1:8080".into(),
            data_dir: dir.0.join("data"),
            proxy_url: None,
            probe_interval: "10m".into(),
            cache: true,
            cache_max_bytes: None,
            cache_ttl: None,
            public_url: None,
            log_level: "info".into(),
            log_retain: None,
            api_keys: vec!["test".into()],
            repos: vec![Repo {
                name: "docker".into(),
                kind: crate::config::Kind::Docker,
                upstreams: vec![crate::config::Upstream {
                    url: "https://registry.example.com".into(),
                    proxy: "direct".into(),
                }],
                aliases: vec!["localhost".into()],
                username: None,
                password: None,
            }],
        }
    }

    #[tokio::test]
    async fn put_rejects_startup_fields_without_overwriting_file() {
        let dir = TestDir::new();
        let path = dir.0.join("config.yaml");
        let initial = config(&dir);
        initial.save(&path).expect("save initial");
        let app = App::boot(path.clone()).expect("boot");

        let mut next = initial.clone();
        next.listen = "127.0.0.1:9090".into();
        let err = app.replace_config(next).await.unwrap_err().to_string();

        assert!(err.contains("listen"));
        assert!(err.contains("需重启"));
        assert_eq!(
            Config::load(&path).expect("disk config").listen,
            initial.listen
        );
        assert_eq!(app.snapshot().await.listen, initial.listen);
    }

    #[tokio::test]
    async fn reload_rejects_startup_fields_and_records_health_error() {
        let dir = TestDir::new();
        let path = dir.0.join("config.yaml");
        let initial = config(&dir);
        initial.save(&path).expect("save initial");
        let app = App::boot(path.clone()).expect("boot");

        let mut next = initial.clone();
        next.probe_interval = "1h".into();
        next.save(&path).expect("save external config");
        let err = app.reload().await.unwrap_err().to_string();

        assert!(err.contains("probe_interval"));
        assert_eq!(app.snapshot().await.probe_interval, initial.probe_interval);
        assert_eq!(app.reload_error().await.as_deref(), Some(err.as_str()));
    }

    #[tokio::test]
    async fn invalid_proxy_does_not_overwrite_file() {
        let dir = TestDir::new();
        let path = dir.0.join("config.yaml");
        let initial = config(&dir);
        initial.save(&path).expect("save initial");
        let app = App::boot(path.clone()).expect("boot");

        let mut next = initial.clone();
        next.proxy_url = Some("ftp://proxy.example.com".into());
        assert!(app.replace_config(next).await.is_err());

        assert!(Config::load(&path)
            .expect("disk config")
            .proxy_url
            .is_none());
    }

    #[tokio::test]
    async fn reload_rejects_removing_repo_with_cached_namespace() {
        let dir = TestDir::new();
        let path = dir.0.join("config.yaml");
        let initial = config(&dir);
        initial.save(&path).expect("save initial");
        let app = App::boot(path.clone()).expect("boot");
        let digest = app.store.write_blob(b"cached").await.expect("write blob");
        app.store
            .put_tag("docker", "library/demo", "latest", &digest)
            .expect("link cache");

        let mut next = initial;
        next.repos[0].name = "replacement".into();
        next.repos[0].aliases.clear();
        next.save(&path).expect("save external config");

        let err = app.reload().await.expect_err("cached repo must remain");
        assert!(err.to_string().contains("缓存命名空间"));
        assert_eq!(app.snapshot().await.repos[0].name, "docker");
    }

    #[tokio::test]
    async fn digest_locks_are_shared_only_while_in_use() {
        let dir = TestDir::new();
        let path = dir.0.join("config.yaml");
        config(&dir).save(&path).expect("save initial");
        let app = App::boot(path).expect("boot");

        let first = app.digest_lock("sha256:a").await;
        let shared = app.digest_lock("sha256:a").await;
        assert!(Arc::ptr_eq(&first, &shared));
        drop(first);
        drop(shared);

        let _next = app.digest_lock("sha256:b").await;
        let locks = app.inflight.lock().await;
        assert!(!locks.contains_key("sha256:a"));
        assert!(locks.contains_key("sha256:b"));
    }

    #[tokio::test]
    async fn note_tag_writes_immediately_when_digest_exists() {
        let dir = TestDir::new();
        let path = dir.0.join("config.yaml");
        config(&dir).save(&path).expect("save initial");
        let app = App::boot(path).expect("boot");
        let digest = app.store.write_blob(b"manifest").await.expect("写制品");
        app.note_tag("docker", "library/postgres", "16", &digest)
            .expect("补 tag");
        assert_eq!(
            app.store
                .tags("docker", "library/postgres")
                .expect("读 tag"),
            vec!["16".to_string()]
        );
    }

    #[tokio::test]
    async fn bind_digest_applies_hint_after_blob_arrives() {
        use sha2::{Digest, Sha256};
        let dir = TestDir::new();
        let path = dir.0.join("config.yaml");
        config(&dir).save(&path).expect("save initial");
        let app = App::boot(path).expect("boot");
        let bytes = b"manifest-body";
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
        app.note_tag("docker", "library/postgres", "latest", &digest)
            .expect("记下 tag");
        assert!(app
            .store
            .tags("docker", "library/postgres")
            .expect("读 tag")
            .is_empty());
        app.store
            .persist_bytes(&digest, bytes)
            .await
            .expect("落下制品");
        app.bind_digest("docker", "library/postgres", &digest)
            .expect("绑定 digest");
        assert_eq!(
            app.store
                .tags("docker", "library/postgres")
                .expect("读 tag"),
            vec!["latest".to_string()]
        );
    }
}
