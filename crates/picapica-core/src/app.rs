use crate::config::{Config, Repo};
use crate::egress::Egress;
use crate::error::{Error, Result};
use crate::probe::{self, ProbeRow};
use crate::store::Store;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub struct App {
    pub config_path: PathBuf,
    pub config: RwLock<Config>,
    pub store: Store,
    pub egress: RwLock<Egress>,
    pub ranks: RwLock<HashMap<String, Vec<ProbeRow>>>,
    pub inflight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl App {
    pub fn boot(config_path: PathBuf) -> Result<Arc<Self>> {
        let cfg = Config::load_or_seed(&config_path)?;
        let store = Store::open(&cfg.data_dir)?;
        let egress = Egress::new(cfg.proxy_url.as_deref())?;
        Ok(Arc::new(Self {
            config_path,
            config: RwLock::new(cfg),
            store,
            egress: RwLock::new(egress),
            ranks: RwLock::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
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
        let cfg = Config::load(&self.config_path)?;
        let egress = Egress::new(cfg.proxy_url.as_deref())?;
        *self.egress.write().await = egress;
        *self.config.write().await = cfg;
        Ok(())
    }

    pub async fn replace_config(&self, cfg: Config) -> Result<()> {
        cfg.validate()?;
        cfg.save(&self.config_path)?;
        let egress = Egress::new(cfg.proxy_url.as_deref())?;
        *self.egress.write().await = egress;
        *self.config.write().await = cfg;
        Ok(())
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
        g.entry(digest.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
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
}
