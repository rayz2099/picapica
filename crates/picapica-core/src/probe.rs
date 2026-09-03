use crate::config::{Kind, Repo};
use crate::egress::Egress;
use crate::error::Result;
use serde::Serialize;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct ProbeRow {
    pub url: String,
    pub ok: bool,
    pub rtt_ms: Option<u64>,
    pub error: Option<String>,
}

/// 对一组上游做 RTT 探测。401 对 Docker /v2/ 算通，因为 Hub 匿名 ping 就是这样。
pub async fn probe_repo(egress: &Egress, repo: &Repo) -> Result<Vec<ProbeRow>> {
    let mut rows = Vec::new();
    for up in &repo.upstreams {
        rows.push(probe_one(egress, repo.kind, up.url.as_str(), up.proxy.as_str()).await);
    }
    rows.sort_by(|a, b| match (a.ok, b.ok, a.rtt_ms, b.rtt_ms) {
        (true, false, _, _) => std::cmp::Ordering::Less,
        (false, true, _, _) => std::cmp::Ordering::Greater,
        (true, true, Some(x), Some(y)) => x.cmp(&y),
        _ => std::cmp::Ordering::Equal,
    });
    Ok(rows)
}

async fn probe_one(egress: &Egress, kind: Kind, url: &str, proxy: &str) -> ProbeRow {
    let start = Instant::now();
    match do_probe(egress, kind, url, proxy).await {
        Ok(()) => ProbeRow {
            url: url.into(),
            ok: true,
            rtt_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
        Err(e) => ProbeRow {
            url: url.into(),
            ok: false,
            rtt_ms: None,
            error: Some(e.to_string()),
        },
    }
}

async fn do_probe(egress: &Egress, kind: Kind, url: &str, proxy: &str) -> Result<()> {
    let client = egress.client(proxy)?;
    let target = match kind {
        Kind::Docker => format!("{}/v2/", url.trim_end_matches('/')),
        _ => format!("{}/", url.trim_end_matches('/')),
    };
    let resp = client
        .get(&target)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;
    let code = resp.status().as_u16();
    if kind == Kind::Docker && (code == 200 || code == 401) {
        return Ok(());
    }
    if code < 500 {
        return Ok(());
    }
    Err(crate::error::Error::msg(format!("HTTP {code}")))
}

/// 按测速名次返回可尝试的上游 URL 列表；全挂则退回配置顺序。
pub fn order_urls(repo: &Repo, ranked: Option<&[ProbeRow]>) -> Vec<(String, String)> {
    if let Some(rows) = ranked {
        let ok: Vec<(String, String)> = rows
            .iter()
            .filter(|r| r.ok)
            .filter_map(|r| {
                repo.upstreams
                    .iter()
                    .find(|u| u.url == r.url)
                    .map(|u| (u.url.clone(), u.proxy.clone()))
            })
            .collect();
        if !ok.is_empty() {
            return ok;
        }
    }
    repo.upstreams
        .iter()
        .map(|u| (u.url.clone(), u.proxy.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Upstream;

    #[test]
    fn order_falls_to_config_when_all_dead() {
        let repo = Repo {
            name: "u".into(),
            kind: Kind::Ubuntu,
            aliases: vec![],
            username: None,
            password: None,
            upstreams: vec![
                Upstream {
                    url: "http://a".into(),
                    proxy: "direct".into(),
                },
                Upstream {
                    url: "http://b".into(),
                    proxy: "default".into(),
                },
            ],
        };
        let ranked = vec![ProbeRow {
            url: "http://a".into(),
            ok: false,
            rtt_ms: None,
            error: Some("x".into()),
        }];
        let o = order_urls(&repo, Some(&ranked));
        assert_eq!(o[0].0, "http://a");
        assert_eq!(o[1].0, "http://b");
    }
}
