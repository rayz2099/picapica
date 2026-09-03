use crate::app::App;
use crate::config::Config;
use crate::error::{Error, Result};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct NsQuery {
    pub q: Option<String>,
    pub repo: Option<String>,
    pub ns: Option<String>,
    pub prefix: Option<String>,
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

#[derive(Deserialize)]
pub struct PruneQuery {
    pub dry_run: Option<bool>,
}

#[derive(Deserialize)]
pub struct TransferQuery {
    pub limit: Option<u32>,
}

pub async fn health(State(app): State<Arc<App>>) -> impl IntoResponse {
    let reload_error = app.reload_error().await;
    let cfg = app.snapshot().await;
    let storage = app.store.storage_status(cfg.cache_max_bytes);
    let ok = reload_error.is_none() && storage.ok;
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(serde_json::json!({
            "ok": ok,
            "config_watch": {
                "ok": reload_error.is_none(),
                "error": reload_error,
            },
            "storage": {
                "ok": storage.ok,
                "writable": storage.writable,
                "bytes": storage.bytes,
                "max_bytes": storage.max_bytes,
                "capacity_ok": storage.capacity_ok,
                "error": storage.error,
                "reconcile": storage.reconcile,
            }
        })),
    )
}

pub async fn get_config(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse> {
    let cfg = app.snapshot().await;
    app.check_token(bearer(&headers), &cfg)?;
    Ok(Json(cfg))
}

pub async fn put_config(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(cfg): Json<Config>,
) -> Result<impl IntoResponse> {
    let cur = app.snapshot().await;
    app.check_token(bearer(&headers), &cur)?;
    app.replace_config(cfg).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn stats(State(app): State<Arc<App>>, headers: HeaderMap) -> Result<impl IntoResponse> {
    let cfg = app.snapshot().await;
    app.check_token(bearer(&headers), &cfg)?;
    let (artifacts, bytes, refs) = app.store.stats()?;
    let metrics = app.metrics();
    let active_transfers = app.active_transfers();
    let last_prune = app.store.last_prune()?;
    Ok(Json(serde_json::json!({
        "artifacts": artifacts,
        "bytes": bytes,
        "refs": refs,
        "repos": cfg.repos.len(),
        "cache_hits": metrics.cache_hits,
        "cache_misses": metrics.cache_misses,
        "upstream_failures": metrics.upstream_failures,
        "bytes_served": metrics.bytes_served,
        "active_transfers": active_transfers,
        "last_prune": last_prune,
    })))
}

pub async fn status(State(app): State<Arc<App>>, headers: HeaderMap) -> Result<impl IntoResponse> {
    let cfg = app.snapshot().await;
    app.check_token(bearer(&headers), &cfg)?;
    let (artifacts, bytes, refs) = app.store.stats()?;
    let storage = app.store.storage_status(cfg.cache_max_bytes);
    let reload_error = app.reload_error().await;
    let repos: Vec<_> = cfg
        .repos
        .iter()
        .map(|repo| serde_json::json!({ "name": repo.name, "type": repo.kind }))
        .collect();
    Ok(Json(serde_json::json!({
        "ok": reload_error.is_none() && storage.ok,
        "listen": cfg.listen,
        "cache": cfg.cache,
        "cache_max_bytes": cfg.cache_max_bytes,
        "cache_ttl": cfg.cache_ttl,
        "repos": repos,
        "stats": { "artifacts": artifacts, "bytes": bytes, "refs": refs },
        "active_transfers": app.active_transfers(),
        "last_prune": app.store.last_prune()?,
        "config_watch": { "ok": reload_error.is_none(), "error": reload_error },
        "storage": storage,
    })))
}

pub async fn transfers(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<TransferQuery>,
) -> Result<impl IntoResponse> {
    let cfg = app.snapshot().await;
    app.check_token(bearer(&headers), &cfg)?;
    Ok(Json(app.transfers(query.limit.unwrap_or(100))))
}

pub async fn prune_dry_run(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse> {
    let cfg = app.snapshot().await;
    app.check_token(bearer(&headers), &cfg)?;
    Ok(Json(app.prune(true).await?))
}

pub async fn prune(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<PruneQuery>,
) -> Result<impl IntoResponse> {
    let cfg = app.snapshot().await;
    app.check_token(bearer(&headers), &cfg)?;
    Ok(Json(app.prune(query.dry_run.unwrap_or(false)).await?))
}

pub async fn get_probe(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse> {
    let cfg = app.snapshot().await;
    app.check_token(bearer(&headers), &cfg)?;
    let ranks = app.ranks.read().await.clone();
    Ok(Json(ranks))
}

pub async fn run_probe(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse> {
    let cfg = app.snapshot().await;
    app.check_token(bearer(&headers), &cfg)?;
    let ranks = app.probe_all().await?;
    Ok(Json(ranks))
}

pub async fn list_ns(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<NsQuery>,
) -> Result<impl IntoResponse> {
    let cfg = app.snapshot().await;
    app.check_token(bearer(&headers), &cfg)?;
    let mut rows = app.store.search(q.q.as_deref())?;
    if let Some(repo) = q.repo {
        rows.retain(|r| r.repo == repo);
    }
    Ok(Json(rows))
}

pub async fn list_tree(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<NsQuery>,
) -> Result<impl IntoResponse> {
    let cfg = app.snapshot().await;
    app.check_token(bearer(&headers), &cfg)?;
    let repo = q.repo.ok_or_else(|| Error::msg("缺少 repo"))?;
    if !cfg.cache {
        return Err(Error::msg("不允许：cache 已关闭，请求不走本地"));
    }
    let prefix = q.prefix.unwrap_or_default();
    let page = q.page.unwrap_or(1);
    let per_page = q.per_page.unwrap_or(50).clamp(1, 200);
    Ok(Json(app.store.tree(&repo, &prefix, page, per_page)?))
}

pub async fn delete_ns(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<NsQuery>,
) -> Result<impl IntoResponse> {
    let cfg = app.snapshot().await;
    app.check_token(bearer(&headers), &cfg)?;
    let repo = q.repo.ok_or_else(|| Error::msg("缺少 repo"))?;
    if q.prefix.is_some() {
        let prefix = q.prefix.unwrap_or_default();
        let n = app.store.delete_prefix(&repo, &prefix)?;
        return Ok((StatusCode::OK, Json(serde_json::json!({ "removed": n }))));
    }
    let ns = q.ns.ok_or_else(|| Error::msg("缺少 ns"))?;
    let n = app.store.delete_namespace(&repo, &ns)?;
    Ok((StatusCode::OK, Json(serde_json::json!({ "removed": n }))))
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
}
