use anyhow::{Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueHint};
use clap_complete::{generate, Shell};
use picapica_core::config::Config;
use reqwest::{Method, Url};
use serde_json::Value;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "picapica", version, about = "内网软件源代理")]
struct Cli {
    #[arg(short, long, default_value = "config.yaml", global = true, value_hint = ValueHint::FilePath)]
    config: PathBuf,
    /// 远端控制面根 URL；显式指定时必须同时传 --token（health 除外）
    #[arg(long, global = true)]
    url: Option<Url>,
    /// 控制面令牌；未指定 --url 时默认取本地配置第一项
    #[arg(long, global = true)]
    token: Option<String>,
    /// 管理请求总超时（秒）
    #[arg(long, global = true, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 常驻：数据面 + 控制 API + WebUI
    Serve,
    /// 查看健康状态（无需令牌）
    Health,
    /// 查看运行状态与缓存统计
    Status,
    /// 查看最近传输进度
    Transfers {
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=500))]
        limit: u32,
    },
    /// 上游测速
    Probe {
        #[command(subcommand)]
        cmd: Option<ProbeCmd>,
    },
    /// 配置校验、查看与应用
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// 制品占用（兼容旧命令）
    Stats,
    /// 浏览、删除与清理缓存
    Cache {
        #[command(subcommand)]
        cmd: CacheCmd,
    },
    /// 生成 shell 补全脚本
    Completions {
        /// fish / bash / zsh / powershell / elvish
        shell: Shell,
    },
}

#[derive(Subcommand)]
enum ProbeCmd {
    /// 执行一次测速
    Run,
    /// 查看最近一次测速结果
    Results,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// 校验本地 YAML，不修改运行实例
    Validate {
        #[arg(value_hint = ValueHint::FilePath)]
        file: Option<PathBuf>,
    },
    /// 查看运行实例当前配置
    Show,
    /// 校验并应用本地 YAML
    Apply {
        #[arg(value_hint = ValueHint::FilePath)]
        file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// 搜索缓存命名空间
    Ls { query: Option<String> },
    /// 浏览仓库缓存树
    Tree(TreeArgs),
    /// 删除一个缓存命名空间的引用；带 --tag 则只删该版本指针
    Rm {
        repo: String,
        namespace: String,
        /// why: 只摘一个 tag/版本，避免 rm 命名空间把镜像层一起清掉。
        #[arg(long)]
        tag: Option<String>,
    },
    /// 前缀级缓存操作
    Prefix {
        #[command(subcommand)]
        cmd: PrefixCmd,
    },
    /// 清理无引用、过期或超容量制品
    Prune {
        /// 仅展示清理计划
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum PrefixCmd {
    /// 删除一个前缀下的缓存引用
    Rm { repo: String, prefix: String },
}

#[derive(Args)]
struct TreeArgs {
    repo: String,
    #[arg(default_value = "")]
    prefix: String,
    #[arg(long, default_value_t = 1)]
    page: u32,
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=200))]
    per_page: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Completions { shell } = &cli.command {
        let mut cmd = Cli::command();
        generate(*shell, &mut cmd, "picapica", &mut io::stdout());
        return Ok(());
    }
    if !matches!(cli.command, Command::Serve) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "picapica=info,picapica_core=info".into()),
            )
            .init();
    }
    match &cli.command {
        Command::Completions { .. } => unreachable!("completions 已提前返回"),
        Command::Serve => picapica_core::serve(cli.config.clone())
            .await
            .map_err(into_any)?,
        Command::Health => {
            print_json(ctl(&cli, "/api/health", Method::GET, &[], None, false).await?)
        }
        Command::Status => {
            print_json(ctl(&cli, "/api/status", Method::GET, &[], None, true).await?)
        }
        Command::Transfers { limit } => {
            let limit = limit.to_string();
            print_json(
                ctl(
                    &cli,
                    "/api/transfers",
                    Method::GET,
                    &[("limit", limit.as_str())],
                    None,
                    true,
                )
                .await?,
            );
        }
        Command::Stats => print_json(ctl(&cli, "/api/stats", Method::GET, &[], None, true).await?),
        Command::Probe { cmd } => {
            let method = match cmd.as_ref().unwrap_or(&ProbeCmd::Run) {
                ProbeCmd::Run => Method::POST,
                ProbeCmd::Results => Method::GET,
            };
            print_json(ctl(&cli, "/api/probe", method, &[], None, true).await?);
        }
        Command::Config { cmd } => match cmd {
            ConfigCmd::Validate { file } => {
                let path = file.as_deref().unwrap_or(&cli.config);
                Config::load(path).map_err(into_any)?;
                println!("ok {}", path.display());
            }
            ConfigCmd::Show => {
                print_json(ctl(&cli, "/api/config", Method::GET, &[], None, true).await?)
            }
            ConfigCmd::Apply { file } => {
                let path = file.as_deref().unwrap_or(&cli.config);
                let cfg = Config::load(path).map_err(into_any)?;
                let body = serde_json::to_value(cfg)?;
                print_json(ctl(&cli, "/api/config", Method::PUT, &[], Some(body), true).await?);
            }
        },
        Command::Cache { cmd } => match cmd {
            CacheCmd::Ls { query } => {
                let query = query.as_deref().unwrap_or_default();
                print_json(
                    ctl(
                        &cli,
                        "/api/namespaces",
                        Method::GET,
                        &[("q", query)],
                        None,
                        true,
                    )
                    .await?,
                );
            }
            CacheCmd::Tree(args) => {
                let page = args.page.to_string();
                let per_page = args.per_page.to_string();
                print_json(
                    ctl(
                        &cli,
                        "/api/tree",
                        Method::GET,
                        &[
                            ("repo", args.repo.as_str()),
                            ("prefix", args.prefix.as_str()),
                            ("page", page.as_str()),
                            ("per_page", per_page.as_str()),
                        ],
                        None,
                        true,
                    )
                    .await?,
                );
            }
            CacheCmd::Rm {
                repo,
                namespace,
                tag,
            } => {
                let mut query = vec![("repo", repo.as_str()), ("ns", namespace.as_str())];
                if let Some(tag) = tag {
                    query.push(("tag", tag.as_str()));
                }
                print_json(ctl(&cli, "/api/namespaces", Method::DELETE, &query, None, true).await?);
            }
            CacheCmd::Prefix { cmd } => match cmd {
                PrefixCmd::Rm { repo, prefix } => {
                    print_json(
                        ctl(
                            &cli,
                            "/api/namespaces",
                            Method::DELETE,
                            &[("repo", repo.as_str()), ("prefix", prefix.as_str())],
                            None,
                            true,
                        )
                        .await?,
                    );
                }
            },
            CacheCmd::Prune { dry_run } => {
                print_json(
                    ctl(
                        &cli,
                        "/api/cache/prune",
                        Method::POST,
                        &[("dry_run", if *dry_run { "true" } else { "false" })],
                        None,
                        true,
                    )
                    .await?,
                );
            }
        },
    }
    Ok(())
}

/// why: URL、鉴权、超时和错误解析统一，避免各管理命令产生不同的远端语义。
async fn ctl(
    cli: &Cli,
    path: &str,
    method: Method,
    query: &[(&str, &str)],
    body: Option<Value>,
    auth: bool,
) -> Result<Value> {
    let (base, token) = endpoint(cli, auth)?;
    let mut url = base.join(path.trim_start_matches('/'))?;
    url.query_pairs_mut().extend_pairs(query.iter().copied());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(cli.timeout))
        .build()?;
    let mut req = client.request(method, url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("{status} {text}");
    }
    serde_json::from_str(&text).context("控制面返回的不是 JSON")
}

fn endpoint(cli: &Cli, auth: bool) -> Result<(Url, Option<String>)> {
    if let Some(base) = &cli.url {
        if auth {
            let token = cli.token.clone().context("远端管理必须显式传 --token")?;
            return Ok((base_url(base.clone()), Some(token)));
        }
        return Ok((base_url(base.clone()), cli.token.clone()));
    }
    let cfg = Config::load(&cli.config).map_err(into_any)?;
    let addr = cfg.listen.replace("0.0.0.0", "127.0.0.1");
    let base = Url::parse(&format!("http://{addr}/"))?;
    let token = if auth {
        Some(
            cli.token
                .clone()
                .or_else(|| cfg.api_keys.first().cloned())
                .context("api_keys 为空")?,
        )
    } else {
        cli.token.clone()
    };
    Ok((base, token))
}

fn base_url(mut url: Url) -> Url {
    url.set_query(None);
    url.set_fragment(None);
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url
}

fn print_json(value: Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(&value).expect("JSON Value 必可序列化")
    );
}

fn into_any(e: picapica_core::Error) -> anyhow::Error {
    anyhow::Error::msg(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_tree_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn query_values_are_percent_encoded() {
        let mut url = Url::parse("http://127.0.0.1:8080/api/tree").expect("URL");
        url.query_pairs_mut()
            .extend_pairs([("repo", "a&b"), ("prefix", "x y/中")]);
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:8080/api/tree?repo=a%26b&prefix=x+y%2F%E4%B8%AD"
        );
    }
}
