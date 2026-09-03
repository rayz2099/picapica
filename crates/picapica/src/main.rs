use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueHint};
use clap_complete::{generate, Shell};
use picapica_core::config::Config;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "picapica", about = "内网软件源代理")]
struct Cli {
    #[arg(
        short,
        long,
        default_value = "config.yaml",
        global = true,
        value_hint = ValueHint::FilePath
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 常驻：数据面 + 控制 API + WebUI
    Serve,
    /// 对所有仓库上游测速
    Probe,
    /// 制品占用
    Stats,
    /// 搜索或删除缓存命名空间
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
enum CacheCmd {
    /// 列出缓存命名空间
    Ls { query: Option<String> },
    /// 删除一个缓存命名空间的引用
    Rm { repo: String, namespace: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Completions { shell } = cli.command {
        let mut cmd = Cli::command();
        generate(shell, &mut cmd, "picapica", &mut io::stdout());
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "picapica=info,picapica_core=info".into()),
        )
        .init();
    match cli.command {
        Command::Completions { .. } => unreachable!("completions 已提前返回"),
        Command::Serve => picapica_core::serve(cli.config).await.map_err(into_any)?,
        Command::Probe => {
            let v = ctl(&cli.config, "/api/probe", "POST").await?;
            println!("{v}");
        }
        Command::Stats => {
            let v = ctl(&cli.config, "/api/stats", "GET").await?;
            println!("{v}");
        }
        Command::Cache { cmd } => match cmd {
            CacheCmd::Ls { query } => {
                let q = query.unwrap_or_default();
                let v = ctl(&cli.config, &format!("/api/namespaces?q={q}"), "GET").await?;
                println!("{v}");
            }
            CacheCmd::Rm { repo, namespace } => {
                let path = format!("/api/namespaces?repo={repo}&ns={namespace}");
                let v = ctl(&cli.config, &path, "DELETE").await?;
                println!("{v}");
            }
        },
    }
    Ok(())
}

async fn ctl(cfg_path: &Path, path: &str, method: &str) -> Result<serde_json::Value> {
    let cfg = Config::load(cfg_path).map_err(into_any)?;
    let addr = cfg.listen.replace("0.0.0.0", "127.0.0.1");
    let url = format!("http://{addr}{path}");
    let key = cfg.api_keys.first().context("api_keys 为空")?;
    let client = reqwest::Client::new();
    let mut req = match method {
        "POST" => client.post(&url),
        "DELETE" => client.delete(&url),
        _ => client.get(&url),
    };
    req = req.header("Authorization", format!("Bearer {key}"));
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("{status} {text}");
    }
    Ok(serde_json::from_str(&text).unwrap_or(serde_json::json!({ "raw": text })))
}

fn into_any(e: picapica_core::Error) -> anyhow::Error {
    anyhow::Error::msg(e.to_string())
}
