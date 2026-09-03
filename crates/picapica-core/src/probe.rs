use crate::config::{Kind, Repo};
use crate::egress::Egress;
use crate::error::Result;
use serde::Serialize;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct ProbeRow {
    pub url: String,
    pub proxy: String,
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
            proxy: proxy.into(),
            ok: true,
            rtt_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
        Err(e) => ProbeRow {
            url: url.into(),
            proxy: proxy.into(),
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

/// 按测速名次返回所有上游；why: 探测失败不代表数据请求必然失败。
pub fn order_urls(repo: &Repo, ranked: Option<&[ProbeRow]>) -> Vec<(String, String)> {
    if let Some(rows) = ranked {
        let mut ordered = Vec::with_capacity(repo.upstreams.len());
        let mut claimed = vec![false; repo.upstreams.len()];
        let mut row_indices = Vec::with_capacity(rows.len());

        // why: 同一 URL 允许配不同出站，按出现次序匹配才不会丢节点。
        for row in rows {
            let index = repo
                .upstreams
                .iter()
                .enumerate()
                .find(|(index, up)| !claimed[*index] && up.url == row.url && up.proxy == row.proxy)
                .map(|(index, _)| index);
            if let Some(index) = index {
                claimed[index] = true;
            }
            row_indices.push(index);
        }

        for (row, index) in rows.iter().zip(&row_indices) {
            if row.ok {
                if let Some(index) = index {
                    let up = &repo.upstreams[*index];
                    ordered.push((up.url.clone(), up.proxy.clone()));
                }
            }
        }

        // why: 旧探测结果可能比热加载后的配置少节点，未探测节点优先于已知失败节点。
        for (index, up) in repo.upstreams.iter().enumerate() {
            if !claimed[index] {
                ordered.push((up.url.clone(), up.proxy.clone()));
            }
        }

        for (row, index) in rows.iter().zip(row_indices) {
            if !row.ok {
                if let Some(index) = index {
                    let up = &repo.upstreams[index];
                    ordered.push((up.url.clone(), up.proxy.clone()));
                }
            }
        }

        return ordered;
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
        let ranked = vec![
            ProbeRow {
                url: "http://a".into(),
                proxy: "direct".into(),
                ok: false,
                rtt_ms: None,
                error: Some("x".into()),
            },
            ProbeRow {
                url: "http://b".into(),
                proxy: "default".into(),
                ok: false,
                rtt_ms: None,
                error: Some("x".into()),
            },
        ];
        let o = order_urls(&repo, Some(&ranked));
        assert_eq!(o[0].0, "http://a");
        assert_eq!(o[1].0, "http://b");
    }

    #[test]
    fn order_keeps_failed_upstreams_after_healthy_and_unprobed() {
        let repo = Repo {
            name: "u".into(),
            kind: Kind::Ubuntu,
            aliases: vec![],
            username: None,
            password: None,
            upstreams: vec![
                Upstream {
                    url: "http://failed".into(),
                    proxy: "direct".into(),
                },
                Upstream {
                    url: "http://new".into(),
                    proxy: "default".into(),
                },
                Upstream {
                    url: "http://fast".into(),
                    proxy: "direct".into(),
                },
            ],
        };
        let ranked = vec![
            ProbeRow {
                url: "http://fast".into(),
                proxy: "direct".into(),
                ok: true,
                rtt_ms: Some(1),
                error: None,
            },
            ProbeRow {
                url: "http://failed".into(),
                proxy: "direct".into(),
                ok: false,
                rtt_ms: None,
                error: Some("timeout".into()),
            },
        ];

        let ordered = order_urls(&repo, Some(&ranked));

        assert_eq!(
            ordered
                .iter()
                .map(|item| item.0.as_str())
                .collect::<Vec<_>>(),
            vec!["http://fast", "http://new", "http://failed"]
        );
    }

    #[test]
    fn order_distinguishes_same_url_by_proxy() {
        let repo = Repo {
            name: "u".into(),
            kind: Kind::Ubuntu,
            aliases: vec![],
            username: None,
            password: None,
            upstreams: vec![
                Upstream {
                    url: "http://mirror".into(),
                    proxy: "direct".into(),
                },
                Upstream {
                    url: "http://mirror".into(),
                    proxy: "default".into(),
                },
            ],
        };
        let ranked = vec![
            ProbeRow {
                url: "http://mirror".into(),
                proxy: "default".into(),
                ok: true,
                rtt_ms: Some(1),
                error: None,
            },
            ProbeRow {
                url: "http://mirror".into(),
                proxy: "direct".into(),
                ok: false,
                rtt_ms: None,
                error: Some("timeout".into()),
            },
        ];

        let ordered = order_urls(&repo, Some(&ranked));

        assert_eq!(ordered[0].1, "default");
        assert_eq!(ordered[1].1, "direct");
    }
}
