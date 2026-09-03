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
}

pub async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
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
    Ok(Json(serde_json::json!({
        "artifacts": artifacts,
        "bytes": bytes,
        "refs": refs,
        "repos": cfg.repos.len(),
    })))
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

pub async fn delete_ns(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<NsQuery>,
) -> Result<impl IntoResponse> {
    let cfg = app.snapshot().await;
    app.check_token(bearer(&headers), &cfg)?;
    let repo = q.repo.ok_or_else(|| Error::msg("缺少 repo"))?;
    let ns = q.ns.ok_or_else(|| Error::msg("缺少 ns"))?;
    let n = app.store.delete_namespace(&repo, &ns)?;
    Ok((StatusCode::OK, Json(serde_json::json!({ "removed": n }))))
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
}
