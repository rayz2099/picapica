use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
    /// why: 关闭则请求不读不写本地，只转发上游。缺省 true，兼容旧 YAML。
    #[serde(default = "default_cache")]
    pub cache: bool,
    /// why: 缺省不限容量，升级旧配置不会意外驱逐已有缓存。
    #[serde(default)]
    pub cache_max_bytes: Option<u64>,
    /// why: 运维使用与 probe_interval 一致的时长语法，避免裸数字单位歧义。
    #[serde(default)]
    pub cache_ttl: Option<String>,
    /// why: 复制地址和 Docker HTTPS 样例用对外域名；host 运行时并进 docker 别名。
    #[serde(default)]
    pub public_url: Option<String>,
    /// why: 运行日志级别走 YAML，实验室不用再记 RUST_LOG。
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// why: 只留最近一段时间的文件日志，磁盘不被历史输出占满。
    #[serde(default)]
    pub log_retain: Option<String>,
    #[serde(default)]
    pub api_keys: Vec<String>,
    pub repos: Vec<Repo>,
}

fn default_probe_interval() -> String {
    "10m".into()
}

fn default_cache() -> bool {
    true
}

fn default_log_level() -> String {
    "info".into()
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
        parse_interval(&self.probe_interval)?;
        if self
            .cache_max_bytes
            .is_some_and(|value| value == 0 || value > i64::MAX as u64)
        {
            return Err(Error::msg("cache_max_bytes 必须在 1..=i64::MAX 范围"));
        }
        if let Some(ttl) = self.cache_ttl.as_deref() {
            parse_named_interval(ttl, "cache_ttl")?;
        }
        crate::logging::parse_log_level(&self.log_level)?;
        if let Some(retain) = self.log_retain.as_deref() {
            parse_named_interval(retain, "log_retain")?;
        }
        if let Some(proxy_url) = self.proxy_url.as_deref() {
            validate_proxy_url(proxy_url, "proxy_url")?;
        }

        // why: 名称和别名共用同一路由空间，提前拒绝歧义，避免配置顺序决定命中仓库。
        let mut routes = HashSet::new();
        for repo in &self.repos {
            if !valid_route_key(&repo.name) {
                return Err(Error::msg(format!("仓库名非法: {}", repo.name)));
            }
            if !routes.insert(repo.name.clone()) {
                return Err(Error::msg(format!("仓库名或别名冲突: {}", repo.name)));
            }
            for alias in &repo.aliases {
                if !valid_route_key(alias) {
                    return Err(Error::msg(format!(
                        "仓库 {} 的别名非法: {alias}",
                        repo.name
                    )));
                }
                if !routes.insert(alias.clone()) {
                    return Err(Error::msg(format!("仓库名或别名冲突: {alias}")));
                }
            }
            if repo.upstreams.is_empty() {
                return Err(Error::msg(format!("仓库 {} 没有上游", repo.name)));
            }
            for up in &repo.upstreams {
                validate_http_url(&up.url, "上游 URL")?;
                validate_upstream_proxy(&up.proxy)?;
            }
        }
        if let Some(raw) = self.public_url.as_deref() {
            let raw = raw.trim();
            if !raw.is_empty() {
                validate_http_url(raw, "public_url")?;
            }
        }
        Ok(())
    }

    /// why: 样例和 Host 路由用 hostname，不把端口写进 docker 别名。
    pub fn public_host(&self) -> Option<String> {
        let raw = self.public_url.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        url::Url::parse(raw)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
    }

    pub fn repo(&self, name: &str) -> Result<&Repo> {
        let extra = self.public_host();
        self.repos
            .iter()
            .find(|r| {
                r.name == name
                    || r.aliases.iter().any(|a| a == name)
                    || (r.kind == Kind::Docker && extra.as_deref() == Some(name))
            })
            .ok_or_else(|| Error::msg(format!("仓库不存在: {name}")))
    }

    pub fn repo_by_host(&self, host: &str) -> Result<&Repo> {
        let host = host.split(':').next().unwrap_or(host);
        let extra = self.public_host();
        self.repos
            .iter()
            .find(|r| {
                r.aliases.iter().any(|a| a == host)
                    || (r.kind == Kind::Docker && extra.as_deref() == Some(host))
            })
            .ok_or_else(|| Error::msg(format!("Host {host} 没有对应仓库")))
    }

    pub fn probe_interval_std(&self) -> Result<std::time::Duration> {
        parse_interval(&self.probe_interval)
    }

    pub fn cache_ttl_std(&self) -> Result<Option<std::time::Duration>> {
        self.cache_ttl
            .as_deref()
            .map(|ttl| parse_named_interval(ttl, "cache_ttl"))
            .transpose()
    }

    pub fn log_retain_std(&self) -> Result<Option<std::time::Duration>> {
        self.log_retain
            .as_deref()
            .map(|retain| parse_named_interval(retain, "log_retain"))
            .transpose()
    }
}

/// 只认 30s / 10m / 1h / 1d 这种运维常用写法。
fn parse_interval(s: &str) -> Result<std::time::Duration> {
    parse_named_interval(s, "probe_interval")
}

fn parse_named_interval(s: &str, field: &str) -> Result<std::time::Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: u64 = n
        .parse()
        .map_err(|_| Error::msg(format!("{field} 非法: {s}")))?;
    if n == 0 {
        return Err(Error::msg(format!("{field} 非法: 必须大于 0")));
    }
    let factor = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(Error::msg(format!("{field} 非法: {s}"))),
    };
    let secs = n
        .checked_mul(factor)
        .ok_or_else(|| Error::msg(format!("{field} 过大: {s}")))?;
    if secs > i64::MAX as u64 {
        return Err(Error::msg(format!("{field} 过大: {s}")));
    }
    Ok(std::time::Duration::from_secs(secs))
}

fn valid_route_key(raw: &str) -> bool {
    !raw.is_empty()
        && raw
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

/// why: 文件源和 OCI 都只实现 HTTP，配置期拒绝运行时必然失败的协议。
fn validate_http_url(raw: &str, field: &str) -> Result<()> {
    let parsed = url::Url::parse(raw).map_err(|_| Error::msg(format!("{field} 非法: {raw}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(Error::msg(format!("{field} 非法: {raw}")));
    }
    Ok(())
}

fn validate_upstream_proxy(raw: &str) -> Result<()> {
    if matches!(raw, "" | "default" | "direct" | "none") {
        return Ok(());
    }
    validate_proxy_url(raw, "上游 proxy")
}

fn validate_proxy_url(raw: &str, field: &str) -> Result<()> {
    if raw.is_empty() {
        return Ok(());
    }
    let parsed = url::Url::parse(raw).map_err(|_| Error::msg(format!("{field} 非法: {raw}")))?;
    if !matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h")
        || parsed.host_str().is_none()
    {
        return Err(Error::msg(format!("{field} 非法: {raw}")));
    }
    Ok(())
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
        assert!(cfg.cache);
    }

    #[test]
    fn public_host_from_https_url() {
        let mut cfg =
            Config::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config.example.yaml"))
                .expect("example");
        cfg.public_url = Some("https://pica.example.com".into());
        assert_eq!(cfg.public_host().as_deref(), Some("pica.example.com"));
        assert!(cfg.repo("pica.example.com").is_ok());
    }

    fn example() -> Config {
        Config::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config.example.yaml"))
            .expect("example")
    }

    #[test]
    fn rejects_route_conflicts_and_illegal_aliases() {
        let mut cfg = example();
        cfg.repos[1].aliases.push("docker".into());
        assert!(cfg.validate().unwrap_err().to_string().contains("冲突"));

        let mut cfg = example();
        cfg.repos[0].aliases.push("bad/alias".into());
        assert!(cfg.validate().unwrap_err().to_string().contains("别名非法"));
    }

    #[test]
    fn rejects_unknown_or_malformed_proxies() {
        let mut cfg = example();
        cfg.repos[0].upstreams[0].proxy = "automatic".into();
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("proxy 非法"));

        let mut cfg = example();
        cfg.proxy_url = Some("ftp://proxy.example.com".into());
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("proxy_url 非法"));
    }

    #[test]
    fn rejects_invalid_probe_interval() {
        let mut cfg = example();
        cfg.probe_interval = "often".into();
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("probe_interval 非法"));
    }

    #[test]
    fn accepts_log_level_and_day_retain() {
        let mut cfg = example();
        cfg.log_level = "warn".into();
        cfg.log_retain = Some("1d".into());
        cfg.validate().expect("warn + 1d");
        assert_eq!(
            cfg.log_retain_std().expect("retain").expect("set"),
            std::time::Duration::from_secs(86400)
        );
    }

    #[test]
    fn rejects_unknown_log_level() {
        let mut cfg = example();
        cfg.log_level = "verbose".into();
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("log_level 非法"));
    }

    #[test]
    fn rejects_cache_limits_outside_sqlite_range() {
        let mut cfg = example();
        cfg.cache_max_bytes = Some(i64::MAX as u64 + 1);
        assert!(cfg
            .validate()
            .expect_err("容量超出 SQLite 范围")
            .to_string()
            .contains("i64::MAX"));

        let mut cfg = example();
        cfg.cache_ttl = Some(format!("{}h", u64::MAX));
        assert!(cfg.validate().is_err());
    }
}
