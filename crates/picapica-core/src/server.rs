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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

pub async fn serve(config_path: PathBuf) -> Result<()> {
    let app = App::boot(config_path)?;
    let interval = app.snapshot().await.probe_interval_std();
    let listen = app.snapshot().await.listen.clone();

    let probe_app = app.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = probe_app.probe_all().await {
                tracing::warn!(error = %e, "周期测速失败");
            }
            tokio::time::sleep(interval.max(Duration::from_secs(30))).await;
        }
    });

    let watch_app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = watch_config(watch_app).await {
            tracing::error!(error = %e, "配置监视退出");
        }
    });

    let router = Router::new()
        .route("/api/health", get(admin::health))
        .route("/api/config", get(admin::get_config).put(admin::put_config))
        .route("/api/stats", get(admin::stats))
        .route("/api/probe", get(admin::get_probe).post(admin::run_probe))
        .route(
            "/api/namespaces",
            get(admin::list_ns).delete(admin::delete_ns),
        )
        .route("/", get(web::index))
        .route("/ui", get(web::index))
        .route("/ui/", get(web::index))
        .route("/favicon.ico", get(web::favicon))
        .route("/favicon.svg", get(web::favicon))
        .fallback(data_plane)
        .layer(TraceLayer::new_for_http())
        .with_state(app);

    let listener = TcpListener::bind(&listen)
        .await
        .map_err(|e| Error::msg(format!("绑定 {listen} 失败: {e}")))?;
    tracing::info!(%listen, "picapica serve");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(|e| Error::msg(format!("server: {e}")))?;
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

async fn watch_config(app: Arc<App>) -> Result<()> {
    use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let path = app.config_path.clone();
    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if matches!(
                    ev.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                ) {
                    let _ = tx.blocking_send(());
                }
            }
        },
        notify::Config::default(),
    )
    .map_err(|e| Error::msg(format!("watch: {e}")))?;
    watcher
        .watch(&path, RecursiveMode::NonRecursive)
        .map_err(|e| Error::msg(format!("watch {}: {e}", path.display())))?;
    while rx.recv().await.is_some() {
        tokio::time::sleep(Duration::from_millis(300)).await;
        while rx.try_recv().is_ok() {}
        match app.reload().await {
            Ok(()) => tracing::info!("配置已 reload"),
            Err(e) => tracing::error!(error = %e, "配置 reload 失败，保持旧配置"),
        }
    }
    Ok(())
}
