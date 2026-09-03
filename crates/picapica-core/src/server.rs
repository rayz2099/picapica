use crate::admin;
use crate::app::App;
use crate::config::Kind;
use crate::error::{Error, Result};
use crate::httpfs;
use crate::oci;
use crate::web;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{CONTENT_TYPE, HOST};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;

pub async fn serve(config_path: PathBuf) -> Result<()> {
    let cfg = crate::config::Config::load_or_seed(&config_path)?;
    let _log_guard = crate::logging::install(&cfg)?;
    let app = App::boot(config_path)?;
    let interval = app.snapshot().await.probe_interval_std()?;
    let listen = app.snapshot().await.listen.clone();
    let log_dir = app.snapshot().await.data_dir.join("logs");
    let log_retain = app.snapshot().await.log_retain_std()?;
    // why: 启动前执行一次治理，容量超限不会带病进入服务态。
    app.prune(false).await?;
    let cancel = CancellationToken::new();

    let probe_app = app.clone();
    let probe_cancel = cancel.clone();
    let probe_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = probe_cancel.cancelled() => break,
                result = probe_app.probe_all() => {
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "周期测速失败");
                    }
                }
            }
            tokio::select! {
                _ = probe_cancel.cancelled() => break,
                _ = tokio::time::sleep(interval.max(Duration::from_secs(30))) => {}
            }
        }
    });

    let watch_app = app.clone();
    let watch_cancel = cancel.clone();
    let watch_task = tokio::spawn(async move {
        if let Err(e) = watch_config(watch_app.clone(), watch_cancel).await {
            watch_app.set_reload_error(Some(e.to_string())).await;
            tracing::error!(error = %e, "配置监视退出");
        }
    });

    let prune_app = app.clone();
    let prune_cancel = cancel.clone();
    let prune_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = prune_cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(300)) => {
                    if let Err(error) = prune_app.prune(false).await {
                        tracing::error!(%error, "周期缓存治理失败");
                    }
                }
            }
        }
    });

    let log_cancel = cancel.clone();
    let log_task = tokio::spawn(async move {
        let Some(retain) = log_retain else {
            return;
        };
        loop {
            tokio::select! {
                _ = log_cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(3600)) => {
                    if let Err(error) = crate::logging::prune_logs(&log_dir, "picapica", retain) {
                        tracing::error!(%error, "周期日志清理失败");
                    }
                }
            }
        }
    });

    let router = Router::new()
        .route("/api/health", get(admin::health))
        .route("/api/config", get(admin::get_config).put(admin::put_config))
        .route("/api/stats", get(admin::stats))
        .route("/api/status", get(admin::status))
        .route("/api/transfers", get(admin::transfers))
        .route(
            "/api/cache/prune",
            get(admin::prune_dry_run).post(admin::prune),
        )
        .route("/api/probe", get(admin::get_probe).post(admin::run_probe))
        .route(
            "/api/namespaces",
            get(admin::list_ns).delete(admin::delete_ns),
        )
        .route("/api/tree", get(admin::list_tree))
        .route("/", get(web::index))
        .route("/ui", get(web::index))
        .route("/ui/", get(web::index))
        .route("/ui/{file}", get(web::asset))
        .route("/favicon.ico", get(web::favicon))
        .route("/favicon.svg", get(web::favicon))
        .fallback(data_plane)
        .layer(TraceLayer::new_for_http())
        .with_state(app);

    let listener = TcpListener::bind(&listen)
        .await
        .map_err(|e| Error::msg(format!("绑定 {listen} 失败: {e}")))?;
    tracing::info!(%listen, "picapica serve");
    let result = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(cancel.clone()))
    .await
    .map_err(|e| Error::msg(format!("server: {e}")));
    cancel.cancel();
    for task in [probe_task, watch_task, prune_task, log_task] {
        task.await
            .map_err(|error| Error::msg(format!("后台任务退出异常: {error}")))?;
    }
    result?;
    Ok(())
}

async fn data_plane(
    State(app): State<Arc<App>>,
    ConnectInfo(_addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response {
    match dispatch(app, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn dispatch(app: Arc<App>, req: Request<Body>) -> Result<Response> {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let path = req.uri().path().to_string();

    if path == "/v2" || path == "/v2/" {
        // why: Docker 探测发生在镜像名之前，不能要求代理域名预先绑定仓库。
        return Ok(([(CONTENT_TYPE, "application/json")], "{}").into_response());
    }
    if let Some(rest) = path.strip_prefix("/v2/") {
        let (first, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let cfg = app.snapshot().await;
        let path_repo = cfg
            .repos
            .iter()
            .any(|repo| repo.name == first || repo.aliases.iter().any(|alias| alias == first));
        if path_repo {
            // why: Portless 等统一域名无法参与仓库 Host 别名，首段显式选择仓库。
            let repo = cfg.repo(first)?.clone();
            if repo.kind != Kind::Docker {
                return Err(Error::msg(format!("仓库 {first} 不是 docker 类型")));
            }
            let oci_path = format!("v2/{tail}");
            return oci::handle(app, repo, &oci_path, req).await;
        }
        let repo = cfg.repo_by_host(&host)?.clone();
        let oci_path = path.trim_start_matches('/');
        return oci::handle(app, repo, oci_path, req).await;
    }

    let mut segs = path.trim_start_matches('/').splitn(2, '/');
    let first = segs.next().unwrap_or("");
    let rest = segs.next().unwrap_or("");
    if first.is_empty() {
        return Err(Error::msg("空路径"));
    }
    let repo = app.repo(first).await?;
    match repo.kind {
        Kind::Docker => oci::handle(app, repo, rest, req).await,
        Kind::Ubuntu | Kind::Fedora => httpfs::handle(app, repo, rest, req).await,
    }
}

async fn watch_config(app: Arc<App>, cancel: CancellationToken) -> Result<()> {
    use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let path = app.config_path.clone();
    let dir = watch_dir(&path);
    let target = path.clone();
    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                let relevant = matches!(
                    ev.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                ) && ev.paths.iter().any(|changed| same_file(changed, &target));
                if relevant {
                    let _ = tx.blocking_send(());
                }
            }
        },
        notify::Config::default(),
    )
    .map_err(|e| Error::msg(format!("watch: {e}")))?;
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|e| Error::msg(format!("watch {}: {e}", dir.display())))?;
    loop {
        let changed = tokio::select! {
            _ = cancel.cancelled() => break,
            changed = rx.recv() => changed,
        };
        if changed.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        while rx.try_recv().is_ok() {}
        match app.reload().await {
            Ok(()) => tracing::info!("配置已 reload"),
            Err(e) => tracing::error!(error = %e, "配置 reload 失败，保持旧配置"),
        }
    }
    Ok(())
}

async fn shutdown_signal(cancel: CancellationToken) {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("注册 SIGTERM 失败");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    cancel.cancel();
}

/// why: 配置保存用原子 rename，监视父目录才能在 inode 被替换后继续收到事件。
fn watch_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn same_file(changed: &Path, target: &Path) -> bool {
    changed.file_name() == target.file_name()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_watcher_tracks_parent_and_ignores_temp_file() {
        let path = Path::new("etc/picapica/config.yaml");
        assert_eq!(watch_dir(path), Path::new("etc/picapica"));
        assert!(same_file(Path::new("etc/picapica/config.yaml"), path));
        assert!(!same_file(Path::new("etc/picapica/config.yaml.tmp"), path));
    }
}
