use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Docker,
    Ubuntu,
    Fedora,
}

/// 单个回源地址。proxy=direct 直连，default 走全局出站。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upstream {
    pub url: String,
    #[serde(default = "default_proxy")]
    pub proxy: String,
}

fn default_proxy() -> String {
    "default".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Kind,
    pub upstreams: Vec<Upstream>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub listen: String,
    pub data_dir: PathBuf,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default = "default_probe_interval")]
    pub probe_interval: String,
    #[serde(default)]
    pub api_keys: Vec<String>,
    pub repos: Vec<Repo>,
}

fn default_probe_interval() -> String {
    "10m".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| Error::msg(format!("读配置 {} 失败: {e}", path.display())))?;
        let cfg: Config = serde_yaml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// 缺省时从旁边的 example 复制，避免代码里写死仓库名。
    pub fn load_or_seed(path: &Path) -> Result<Self> {
        if !path.exists() {
            let example = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("config.example.yaml");
            if !example.exists() {
                return Err(Error::msg(format!(
                    "没有 {}，也没有 {}",
                    path.display(),
                    example.display()
                )));
            }
            std::fs::copy(&example, path)?;
            tracing::info!(from = %example.display(), to = %path.display(), "seeded config");
        }
        Self::load(path)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let yaml = serde_yaml::to_string(self)?;
        let tmp = path.with_extension("yaml.tmp");
        std::fs::write(&tmp, yaml)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.api_keys.is_empty() {
            return Err(Error::msg("api_keys 不能为空"));
        }
        if self.repos.is_empty() {
            return Err(Error::msg("至少配置一个仓库"));
        }
        let mut names = std::collections::HashSet::new();
        for repo in &self.repos {
            if repo.name.is_empty() || repo.name.contains('/') {
                return Err(Error::msg(format!("仓库名非法: {}", repo.name)));
            }
            if !names.insert(repo.name.clone()) {
                return Err(Error::msg(format!("仓库名重复: {}", repo.name)));
            }
            if repo.upstreams.is_empty() {
                return Err(Error::msg(format!("仓库 {} 没有上游", repo.name)));
            }
            for up in &repo.upstreams {
                url::Url::parse(&up.url)
                    .map_err(|_| Error::msg(format!("上游 URL 非法: {}", up.url)))?;
            }
        }
        Ok(())
    }

    pub fn repo(&self, name: &str) -> Result<&Repo> {
        self.repos
            .iter()
            .find(|r| r.name == name || r.aliases.iter().any(|a| a == name))
            .ok_or_else(|| Error::msg(format!("仓库不存在: {name}")))
    }

    pub fn repo_by_host(&self, host: &str) -> Result<&Repo> {
        let host = host.split(':').next().unwrap_or(host);
        self.repos
            .iter()
            .find(|r| r.aliases.iter().any(|a| a == host))
            .ok_or_else(|| Error::msg(format!("Host {host} 没有对应仓库")))
    }

    pub fn probe_interval_std(&self) -> std::time::Duration {
        parse_interval(&self.probe_interval)
    }
}

/// 只认 30s / 10m / 1h 这种运维常用写法。
fn parse_interval(s: &str) -> std::time::Duration {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n.parse().unwrap_or(10);
    match unit {
        "s" => std::time::Duration::from_secs(n),
        "h" => std::time::Duration::from_secs(n * 3600),
        _ => std::time::Duration::from_secs(n * 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_yaml_parses() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config.example.yaml");
        let cfg = Config::load(&p).expect("example");
        assert!(cfg.repos.iter().any(|r| r.name == "docker"));
        assert!(cfg.repos.iter().any(|r| r.upstreams.len() > 1));
    }
}
