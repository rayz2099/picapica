use crate::error::{Error, Result};
use reqwest::redirect::Policy;
use reqwest::{Client, Proxy};
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

/// 按上游的 proxy 字段挑 Client：直连 / 全局出站 / 单独代理。
pub struct Egress {
    direct: Client,
    via_default: Client,
    extra: RwLock<HashMap<String, Client>>,
}

impl Egress {
    pub fn new(proxy_url: Option<&str>) -> Result<Self> {
        let direct = builder().build()?;
        let via_default = match proxy_url {
            Some(u) if !u.is_empty() => builder()
                .proxy(Proxy::all(u).map_err(|e| Error::msg(format!("proxy_url 非法: {e}")))?)
                .build()?,
            _ => builder().build()?,
        };
        Ok(Self {
            direct,
            via_default,
            extra: RwLock::new(HashMap::new()),
        })
    }

    pub fn client(&self, mode: &str) -> Result<Client> {
        match mode {
            "" | "default" => Ok(self.via_default.clone()),
            "direct" | "none" => Ok(self.direct.clone()),
            other
                if other.starts_with("http://")
                    || other.starts_with("https://")
                    || other.starts_with("socks5://")
                    || other.starts_with("socks5h://") =>
            {
                {
                    let g = self.extra.read().expect("egress lock");
                    if let Some(c) = g.get(other) {
                        return Ok(c.clone());
                    }
                }
                let c = builder()
                    .proxy(
                        Proxy::all(other)
                            .map_err(|e| Error::msg(format!("上游 proxy 非法: {e}")))?,
                    )
                    .build()?;
                self.extra
                    .write()
                    .expect("egress lock")
                    .insert(other.to_string(), c.clone());
                Ok(c)
            }
            other => Err(Error::msg(format!("不支持的 proxy: {other}"))),
        }
    }
}

fn builder() -> reqwest::ClientBuilder {
    Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(10))
        // why: connect_timeout 不覆盖响应头和响应体，卡死的上游会阻断后续节点切换。
        .read_timeout(Duration::from_secs(60))
        .timeout(Duration::from_secs(30 * 60))
        .pool_idle_timeout(Duration::from_secs(30))
        .user_agent("picapica/0.1")
}
