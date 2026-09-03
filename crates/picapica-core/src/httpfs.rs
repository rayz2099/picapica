use crate::app::App;
use crate::config::Repo;
use crate::error::{Error, Result};
use crate::httpx;
use crate::probe::order_urls;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// Ubuntu / Fedora 都是带签名索引的 HTTP 文件树，同一套穿透。
pub async fn handle(app: Arc<App>, repo: Repo, rest: &str, req: Request<Body>) -> Result<Response> {
    let path = rest.trim_start_matches('/');
    if path.is_empty() {
        return Ok((StatusCode::OK, "picapica httpfs\n").into_response());
    }
    let volatile = is_volatile(path);
    if !volatile {
        if let Some(digest) = app.store.get_file(&repo.name, path)? {
            if app.store.has_digest(&digest) {
                return serve_blob(&app, &digest).await;
            }
        }
    }
    let ranked = app.ranks_for(&repo.name).await;
    let mut last_err = None;
    for (base, proxy) in order_urls(&repo, ranked.as_deref()) {
        match fetch_one(&app, &repo, &base, &proxy, path, req.headers(), volatile).await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                tracing::warn!(upstream = %base, error = %e, "httpfs 上游失败");
                last_err = Some(e);
            }
        }
    }
    if let Some(digest) = app.store.get_file(&repo.name, path)? {
        if app.store.has_digest(&digest) {
            return serve_blob(&app, &digest).await;
        }
    }
    Err(last_err.unwrap_or_else(|| Error::msg(format!("文件不存在: {path}"))))
}

async fn fetch_one(
    app: &App,
    repo: &Repo,
    base: &str,
    proxy: &str,
    path: &str,
    inbound: &HeaderMap,
    volatile: bool,
) -> Result<Response> {
    let client = app.egress.read().await.client(proxy)?;
    let url = format!("{}/{}", base.trim_end_matches('/'), path);
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(a) = inbound.get(header::IF_NONE_MATCH) {
        if let Ok(v) = reqwest::header::HeaderValue::from_bytes(a.as_bytes()) {
            headers.insert(reqwest::header::IF_NONE_MATCH, v);
        }
    }
    let resp = httpx::get_follow(&client, &url, headers).await?;
    if resp.status() == StatusCode::NOT_MODIFIED {
        if let Some(digest) = app.store.get_file(&repo.name, path)? {
            return serve_blob(app, &digest).await;
        }
    }
    if resp.status() == StatusCode::NOT_FOUND {
        return Err(Error::msg(format!("不存在 {path}")));
    }
    if !resp.status().is_success() {
        return Err(Error::msg(format!("{} HTTP {}", path, resp.status())));
    }
    let ct = httpx::header_str(&resp, "content-type")
        .unwrap_or_else(|| "application/octet-stream".into());
    let (digest, _) = app.store.persist_stream(None, resp.bytes_stream()).await?;
    app.store.put_file(&repo.name, path, &digest)?;
    let _ = volatile;
    serve_blob_ct(app, &digest, &ct).await
}

async fn serve_blob(app: &App, digest: &str) -> Result<Response> {
    serve_blob_ct(app, digest, "application/octet-stream").await
}

async fn serve_blob_ct(app: &App, digest: &str, ct: &str) -> Result<Response> {
    let path = app.store.blob_path(digest);
    let data = tokio::fs::read(&path).await?;
    Ok((
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            header::HeaderValue::from_str(ct)
                .unwrap_or(header::HeaderValue::from_static("application/octet-stream")),
        )],
        data,
    )
        .into_response())
}

fn is_volatile(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    matches!(
        base,
        "InRelease"
            | "Release"
            | "Release.gpg"
            | "Packages"
            | "Sources"
            | "repomd.xml"
            | "repomd.xml.asc"
    ) || base.starts_with("Packages.")
        || base.starts_with("Sources.")
        || (path.contains("/repodata/") && base.ends_with(".xml.gz"))
        || base.ends_with(".db")
        || base.ends_with(".files")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inrelease_is_volatile() {
        assert!(is_volatile("dists/jammy/InRelease"));
        assert!(!is_volatile("pool/main/a/apt/apt_2.4.0_amd64.deb"));
    }
}
