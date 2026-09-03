use crate::app::App;
use crate::config::Repo;
use crate::error::{Error, Result};
use crate::httpx;
use crate::probe::order_urls;
use crate::store::file_namespace;
use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// Ubuntu / Fedora 都是带签名索引的 HTTP 文件树，同一套穿透。
pub async fn handle(app: Arc<App>, repo: Repo, rest: &str, req: Request<Body>) -> Result<Response> {
    let path = rest.trim_start_matches('/');
    if path.is_empty() {
        return Ok((StatusCode::OK, "picapica httpfs\n").into_response());
    }
    let method = req.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return Ok(StatusCode::METHOD_NOT_ALLOWED.into_response());
    }
    let caching = app.caching().await;
    let volatile = is_volatile(path);
    if caching && !volatile {
        if let Some(digest) = app.store.get_file(&repo.name, path)? {
            if app.store.has_digest(&digest)? {
                app.record_cache_hit();
                return serve_blob(&app, &digest, &method, req.headers()).await;
            }
        }
        app.record_cache_miss();
    }
    let ranked = app.ranks_for(&repo.name).await;
    let upstreams = order_urls(&repo, ranked.as_deref());
    let transfer = if method == Method::GET {
        upstreams.first().map(|(base, _)| {
            let namespace = file_namespace(path);
            app.begin_transfer(&repo.name, &namespace, path, base, None)
                .map(httpx::TransferProgress::new)
        })
    } else {
        None
    }
    .transpose()?;
    let mut last_err = None;
    let fetch = Fetch {
        app: &app,
        repo: &repo,
        path,
        method: &method,
        inbound: req.headers(),
        caching,
        transfer: transfer.as_ref(),
    };
    for (base, proxy) in upstreams {
        match fetch.one(&base, &proxy).await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                app.record_upstream_failure();
                tracing::warn!(upstream = %base, error = %e, "httpfs 上游失败");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| Error::msg(format!("文件不存在: {path}"))))
}

/// why: 回源的请求上下文在节点切换时保持不变，只替换上游地址和出站。
struct Fetch<'a> {
    app: &'a Arc<App>,
    repo: &'a Repo,
    path: &'a str,
    method: &'a Method,
    inbound: &'a HeaderMap,
    caching: bool,
    transfer: Option<&'a httpx::TransferProgress>,
}

impl Fetch<'_> {
    async fn one(&self, base: &str, proxy: &str) -> Result<Response> {
        let client = self.app.egress.read().await.client(proxy)?;
        let url = format!("{}/{}", base.trim_end_matches('/'), self.path);
        let mut headers = reqwest::header::HeaderMap::new();
        for name in [
            header::IF_NONE_MATCH,
            header::IF_MODIFIED_SINCE,
            header::IF_MATCH,
            header::IF_UNMODIFIED_SINCE,
            header::RANGE,
            header::IF_RANGE,
        ] {
            if let Some(value) = self.inbound.get(&name) {
                if let Ok(value) = reqwest::header::HeaderValue::from_bytes(value.as_bytes()) {
                    headers.insert(name, value);
                }
            }
        }
        let resp = if self.method == Method::HEAD {
            httpx::head_follow(&client, &url, headers).await?
        } else {
            httpx::get_follow(&client, &url, headers).await?
        };
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(Error::msg(format!("不存在 {}", self.path)));
        }
        if should_try_next(status) {
            return Err(Error::msg(format!("{} HTTP {}", self.path, status)));
        }
        if status == StatusCode::NOT_MODIFIED {
            return httpx::into_axum(resp);
        }
        let partial = self.inbound.contains_key(header::RANGE);
        if self.method == Method::GET && status.is_success() {
            let transfer = self
                .transfer
                .ok_or_else(|| Error::msg("HTTP 文件下载缺少进度守卫"))?;
            transfer.start_attempt(base, resp.content_length())?;
        }
        if !self.caching || self.method == Method::HEAD || partial || !status.is_success() {
            if self.method == Method::GET && status.is_success() {
                if let Some(size) = resp.content_length() {
                    self.app.record_bytes_served(size);
                }
                let transfer = self
                    .transfer
                    .ok_or_else(|| Error::msg("HTTP 文件下载缺少进度守卫"))?;
                return httpx::into_axum_tracked(resp, transfer.clone());
            }
            return httpx::into_axum(resp);
        }
        let ct = httpx::header_str(&resp, "content-type")
            .unwrap_or_else(|| "application/octet-stream".into());
        let transfer = self
            .transfer
            .ok_or_else(|| Error::msg("HTTP 文件下载缺少进度守卫"))?;
        let stream = transfer.track(resp.bytes_stream());
        let persisted = self.app.store.persist_stream(None, stream).await;
        let (digest, size) = match persisted {
            Ok(value) => value,
            Err(e) => return Err(e),
        };
        transfer.flush(size)?;
        let linked = self.app.store.put_file(&self.repo.name, self.path, &digest);
        linked?;
        transfer.finish()?;
        serve_blob_ct(self.app, &digest, &ct, self.method, self.inbound).await
    }
}

/// why: 授权、限流、缺失和服务端错误可能只影响单个镜像；请求条件失败则必须原样返回。
fn should_try_next(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::NOT_FOUND
                | reqwest::StatusCode::REQUEST_TIMEOUT
                | reqwest::StatusCode::TOO_EARLY
                | reqwest::StatusCode::TOO_MANY_REQUESTS
        )
}

async fn serve_blob(
    app: &App,
    digest: &str,
    method: &Method,
    inbound: &HeaderMap,
) -> Result<Response> {
    serve_blob_ct(app, digest, "application/octet-stream", method, inbound).await
}

async fn serve_blob_ct(
    app: &App,
    digest: &str,
    ct: &str,
    method: &Method,
    inbound: &HeaderMap,
) -> Result<Response> {
    let path = app.store.blob_path(digest)?;
    let total = tokio::fs::metadata(&path).await?.len();
    let content_type = header::HeaderValue::from_str(ct)
        .unwrap_or(header::HeaderValue::from_static("application/octet-stream"));

    if method == Method::HEAD {
        return response(StatusCode::OK, content_type, total, None, Body::empty());
    }

    // why: 当前索引未保存 ETag/Last-Modified，无法证明 If-Range 命中时必须返回完整实体。
    let range = if inbound.contains_key(header::IF_RANGE) {
        None
    } else {
        inbound.get(header::RANGE)
    };
    if let Some(value) = range {
        let range = value
            .to_str()
            .ok()
            .and_then(|value| httpx::parse_byte_range(value, total));
        let Some((start, end)) = range else {
            let content_range = header::HeaderValue::from_str(&format!("bytes */{total}"))
                .map_err(|e| Error::msg(format!("Content-Range 非法: {e}")))?;
            let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            resp.headers_mut()
                .insert(header::CONTENT_RANGE, content_range);
            return Ok(resp);
        };
        let size = end - start + 1;
        let body = httpx::file_body(&path, start, size).await?;
        app.record_bytes_served(size);
        let content_range = header::HeaderValue::from_str(&format!("bytes {start}-{end}/{total}"))
            .map_err(|e| Error::msg(format!("Content-Range 非法: {e}")))?;
        return response(
            StatusCode::PARTIAL_CONTENT,
            content_type,
            size,
            Some(content_range),
            body,
        );
    }

    let body = httpx::file_body(&path, 0, total).await?;
    app.record_bytes_served(total);
    response(StatusCode::OK, content_type, total, None, body)
}

fn response(
    status: StatusCode,
    content_type: header::HeaderValue,
    size: u64,
    content_range: Option<header::HeaderValue>,
    body: Body,
) -> Result<Response> {
    let content_length = header::HeaderValue::from_str(&size.to_string())
        .map_err(|e| Error::msg(format!("Content-Length 非法: {e}")))?;
    let mut resp = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, content_length)
        .header(header::ACCEPT_RANGES, "bytes");
    if let Some(value) = content_range {
        resp = resp.header(header::CONTENT_RANGE, value);
    }
    resp.body(body)
        .map_err(|e| Error::msg(format!("构造 HTTP 响应失败: {e}")))
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

    #[test]
    fn failed_upstreams_allow_next_attempt() {
        assert!(should_try_next(reqwest::StatusCode::BAD_GATEWAY));
        assert!(should_try_next(reqwest::StatusCode::UNAUTHORIZED));
        assert!(should_try_next(reqwest::StatusCode::FORBIDDEN));
        assert!(should_try_next(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(!should_try_next(reqwest::StatusCode::NOT_MODIFIED));
        assert!(!should_try_next(reqwest::StatusCode::PRECONDITION_FAILED));
        assert!(!should_try_next(reqwest::StatusCode::RANGE_NOT_SATISFIABLE));
    }
}
