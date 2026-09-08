use crate::app::App;
use crate::config::Repo;
use crate::error::{Error, Result};
use crate::httpx;
use crate::probe::order_urls;
use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::TryStreamExt;
use reqwest::header::HeaderMap as ReqHeaders;
use std::sync::Arc;

mod response;
use response::*;

const ACCEPT: &str = "application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v1+json";

struct OciReq<'a> {
    base: &'a str,
    proxy: &'a str,
    name: &'a str,
    reference: &'a str,
    inbound: &'a HeaderMap,
    method: Method,
}

struct Upstream<'a> {
    base: &'a str,
    proxy: &'a str,
}

/// why: 多上游重试属于同一逻辑资源，只创建一个守卫才能保留实际累计流量。
fn begin_progress(
    app: &Arc<App>,
    repo: &Repo,
    namespace: &str,
    name: &str,
    upstreams: &[(String, String)],
) -> Result<httpx::TransferProgress> {
    let (base, _) = upstreams
        .first()
        .ok_or_else(|| Error::msg(format!("仓库 {} 没有上游", repo.name)))?;
    let guard = app.begin_transfer(&repo.name, namespace, name, base, None)?;
    Ok(httpx::TransferProgress::new(guard))
}

/// why: 多上游切换期间保留最后一个协议响应，全部失败后才能返回真实 OCI 状态。
enum Pulled<T> {
    Value(T),
    Rejected(reqwest::Response),
}

impl<T> Pulled<T> {
    fn map_rejected<U>(self) -> Pulled<U> {
        match self {
            Self::Rejected(resp) => Pulled::Rejected(resp),
            Self::Value(_) => unreachable!("只在 Rejected 分支转换类型"),
        }
    }
}

pub async fn handle(app: Arc<App>, repo: Repo, rest: &str, req: Request<Body>) -> Result<Response> {
    // why: 本产品只做 pull-through，写方法不能被误当作 GET 回源。
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return Ok(StatusCode::METHOD_NOT_ALLOWED.into_response());
    }
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() || rest == "v2" || rest == "v2/" {
        return Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            "{}",
        )
            .into_response());
    }
    let path = rest.strip_prefix("v2/").unwrap_or(rest);
    if let Some((name, reference)) = split_end(path, "/manifests/") {
        return manifest(app, repo, name, reference, req).await;
    }
    if let Some((name, digest)) = split_end(path, "/blobs/") {
        return blob(app, repo, name, digest, req).await;
    }
    Err(Error::msg("不支持的 OCI 路径"))
}

fn split_end<'a>(path: &'a str, mid: &str) -> Option<(&'a str, &'a str)> {
    let i = path.rfind(mid)?;
    Some((&path[..i], &path[i + mid.len()..]))
}

async fn manifest(
    app: Arc<App>,
    repo: Repo,
    name: &str,
    reference: &str,
    req: Request<Body>,
) -> Result<Response> {
    let name = normalize_name(name, &repo);
    let method = req.method().clone();
    let caching = app.caching().await;
    if reference.starts_with("sha256:") {
        app.store.blob_path(reference)?;
        if caching && app.store.has_digest(reference)? {
            app.record_cache_hit();
            app.bind_digest(&repo.name, &name, reference)?;
            if method == Method::HEAD {
                return serve_file_head(&app, reference, true).await;
            }
            return serve_file(&app, reference, true, req.headers()).await;
        }
        if caching {
            app.record_cache_miss();
        }
        if method == Method::HEAD {
            let meta = fetch_manifest_head(
                &app,
                &repo,
                &name,
                reference,
                Some(reference),
                req.headers(),
            )
            .await?;
            let Pulled::Value(meta) = meta else {
                return rejected_response(meta, "MANIFEST_UNKNOWN");
            };
            return head_response(&meta, Some(reference), true);
        }
        let body = fetch_manifest(&app, &repo, &name, reference, req.headers()).await?;
        let Pulled::Value(body) = body else {
            return rejected_response(body, "MANIFEST_UNKNOWN");
        };
        verify_body_digest(reference, &body)?;
        if caching {
            app.store.persist_bytes(reference, &body).await?;
            app.bind_digest(&repo.name, &name, reference)?;
        }
        return serve_bytes_digest(&app, body, reference, true);
    }
    if method == Method::HEAD {
        let meta = fetch_manifest_head(&app, &repo, &name, reference, None, req.headers()).await?;
        let Pulled::Value(meta) = meta else {
            return rejected_response(meta, "MANIFEST_UNKNOWN");
        };
        if caching {
            if let Some(digest) = meta.digest.as_deref() {
                app.note_tag(&repo.name, &name, reference, digest)?;
            }
        }
        return head_response(&meta, None, true);
    }
    if !caching {
        let pulled = fetch_tag_manifest(&app, &repo, &name, reference, req.headers()).await?;
        let Pulled::Value((digest, body)) = pulled else {
            return rejected_response(pulled, "MANIFEST_UNKNOWN");
        };
        return serve_bytes_digest(&app, body, &digest, true);
    }
    let pulled = revalidate(&app, &repo, &name, reference, req.headers()).await?;
    let Pulled::Value(digest) = pulled else {
        return rejected_response(pulled, "MANIFEST_UNKNOWN");
    };
    serve_file(&app, &digest, true, req.headers()).await
}

async fn blob(
    app: Arc<App>,
    repo: Repo,
    name: &str,
    digest: &str,
    req: Request<Body>,
) -> Result<Response> {
    let name = normalize_name(name, &repo);
    app.store.blob_path(digest)?;
    let caching = app.caching().await;
    if req.method() == Method::HEAD {
        if caching && app.store.has_digest(digest)? {
            app.record_cache_hit();
            app.bind_digest(&repo.name, &name, digest)?;
            return serve_file_head(&app, digest, false).await;
        }
        if caching {
            app.record_cache_miss();
        }
        let pulled = fetch_blob_head(&app, &repo, &name, digest, req.headers()).await?;
        let Pulled::Value(meta) = pulled else {
            return rejected_response(pulled, "BLOB_UNKNOWN");
        };
        return head_response(&meta, Some(digest), false);
    }
    if !caching {
        return fetch_blob_pass(&app, &repo, &name, digest, req.headers()).await;
    }
    if app.store.has_digest(digest)? {
        app.record_cache_hit();
        app.bind_digest(&repo.name, &name, digest)?;
        return serve_file(&app, digest, false, req.headers()).await;
    }
    app.record_cache_miss();
    // why: 单段响应不能证明完整摘要，Range miss 直接流式穿透且不发布缓存。
    if req.headers().contains_key(header::RANGE) {
        return fetch_blob_pass(&app, &repo, &name, digest, req.headers()).await;
    }
    let digest_lock = app.digest_lock(digest).await;
    let _g = digest_lock.lock().await;
    if app.store.has_digest(digest)? {
        app.bind_digest(&repo.name, &name, digest)?;
        return serve_file(&app, digest, false, req.headers()).await;
    }
    let pulled = fetch_blob(&app, &repo, &name, digest, req.headers()).await?;
    let Pulled::Value(()) = pulled else {
        return rejected_response(pulled, "BLOB_UNKNOWN");
    };
    serve_file(&app, digest, false, req.headers()).await
}

fn rejected_response<T>(pulled: Pulled<T>, missing_code: &'static str) -> Result<Response> {
    let Pulled::Rejected(resp) = pulled else {
        return Err(Error::msg("OCI 上游状态丢失"));
    };
    distribution_rejection(resp, missing_code)
}

/// why: 部分镜像返回 HTML 错误页；OCI 客户端要求常见错误使用 Distribution JSON 信封。
fn distribution_rejection(resp: reqwest::Response, missing_code: &'static str) -> Result<Response> {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let Some((code, message)) = distribution_error(status, missing_code) else {
        return httpx::into_axum(resp);
    };
    let body = serde_json::to_vec(&serde_json::json!({
        "errors": [{"code": code, "message": message, "detail": {}}]
    }))?;
    let challenge = resp.headers().get(header::WWW_AUTHENTICATE).cloned();
    let mut out = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.len())
        .header("docker-distribution-api-version", "registry/2.0");
    if let Some(value) = challenge {
        out = out.header(header::WWW_AUTHENTICATE, value);
    }
    out.body(Body::from(body))
        .map_err(|e| Error::msg(format!("构造 OCI 错误响应失败: {e}")))
}

fn distribution_error(
    status: StatusCode,
    missing_code: &'static str,
) -> Option<(&'static str, &'static str)> {
    match status {
        StatusCode::UNAUTHORIZED => Some(("UNAUTHORIZED", "authentication required")),
        StatusCode::FORBIDDEN => Some(("DENIED", "requested access to the resource is denied")),
        StatusCode::NOT_FOUND if missing_code == "BLOB_UNKNOWN" => {
            Some(("BLOB_UNKNOWN", "blob unknown to registry"))
        }
        StatusCode::NOT_FOUND => Some(("MANIFEST_UNKNOWN", "manifest unknown")),
        _ => None,
    }
}

async fn revalidate(
    app: &Arc<App>,
    repo: &Repo,
    name: &str,
    tag: &str,
    inbound: &HeaderMap,
) -> Result<Pulled<String>> {
    let ranked = app.ranks_for(&repo.name).await;
    let upstreams = order_urls(repo, ranked.as_deref());
    let transfer = begin_progress(app, repo, name, tag, &upstreams)?;
    let mut last_err = None;
    let mut rejected = None;
    for (base, proxy) in upstreams {
        let upstream = Upstream {
            base: &base,
            proxy: &proxy,
        };
        match refresh_tag(app, repo, upstream, name, tag, inbound, &transfer).await {
            Ok(Pulled::Value(digest)) => {
                transfer.finish()?;
                return Ok(Pulled::Value(digest));
            }
            Ok(Pulled::Rejected(resp)) => {
                app.record_upstream_failure();
                rejected = Some(resp);
            }
            Err(e) => {
                app.record_upstream_failure();
                tracing::warn!(upstream = %base, error = %e, "manifest 上游失败");
                last_err = Some(e);
            }
        }
    }
    if let Some(resp) = rejected {
        return Ok(Pulled::Rejected(resp));
    }
    Err(last_err.unwrap_or_else(|| Error::msg("所有上游都失败")))
}

/// why: tag 是可变指针，先用 HEAD 判定摘要，内容未变时避免重复下载 manifest。
async fn refresh_tag(
    app: &Arc<App>,
    repo: &Repo,
    upstream: Upstream<'_>,
    name: &str,
    tag: &str,
    inbound: &HeaderMap,
    transfer: &httpx::TransferProgress,
) -> Result<Pulled<String>> {
    let meta =
        pull_manifest_head(app, repo, upstream.base, upstream.proxy, name, tag, inbound).await?;
    let Pulled::Value(meta) = meta else {
        return Ok(meta.map_rejected());
    };
    let digest = meta
        .digest
        .ok_or_else(|| Error::msg("manifest HEAD 缺少 Docker-Content-Digest"))?;
    let local = app.store.get_tag(&repo.name, name, tag)?;
    let unchanged = local.as_deref() == Some(digest.as_str());
    if unchanged && app.store.has_digest(&digest)? {
        app.record_cache_hit();
        return Ok(Pulled::Value(digest));
    }
    app.record_cache_miss();
    let request = OciReq {
        base: upstream.base,
        proxy: upstream.proxy,
        name,
        reference: &digest,
        inbound,
        method: Method::GET,
    };
    let body = pull_manifest_bytes(app, repo, request, transfer).await?;
    let Pulled::Value(body) = body else {
        return Ok(body.map_rejected());
    };
    app.store.persist_bytes(&digest, &body).await?;
    app.store.put_tag(&repo.name, name, tag, &digest)?;
    Ok(Pulled::Value(digest))
}

async fn fetch_tag_manifest(
    app: &Arc<App>,
    repo: &Repo,
    name: &str,
    tag: &str,
    inbound: &HeaderMap,
) -> Result<Pulled<(String, bytes::Bytes)>> {
    let ranked = app.ranks_for(&repo.name).await;
    let upstreams = order_urls(repo, ranked.as_deref());
    let transfer = begin_progress(app, repo, name, tag, &upstreams)?;
    let mut last_err = None;
    let mut rejected = None;
    for (base, proxy) in upstreams {
        let result = async {
            let meta = pull_manifest_head(app, repo, &base, &proxy, name, tag, inbound).await?;
            let Pulled::Value(meta) = meta else {
                return Ok::<_, Error>(meta.map_rejected());
            };
            let digest = meta
                .digest
                .ok_or_else(|| Error::msg("manifest HEAD 缺少 Docker-Content-Digest"))?;
            let request = OciReq {
                base: &base,
                proxy: &proxy,
                name,
                reference: &digest,
                inbound,
                method: Method::GET,
            };
            let body = pull_manifest_bytes(app, repo, request, &transfer).await?;
            let Pulled::Value(body) = body else {
                return Ok::<_, Error>(body.map_rejected());
            };
            verify_body_digest(&digest, &body)?;
            Ok::<_, Error>(Pulled::Value((digest, body)))
        }
        .await;
        match result {
            Ok(Pulled::Value(found)) => {
                transfer.finish()?;
                return Ok(Pulled::Value(found));
            }
            Ok(Pulled::Rejected(resp)) => {
                app.record_upstream_failure();
                rejected = Some(resp);
            }
            Err(e) => {
                app.record_upstream_failure();
                tracing::warn!(upstream = %base, error = %e, "manifest 上游失败");
                last_err = Some(e);
            }
        }
    }
    if let Some(resp) = rejected {
        return Ok(Pulled::Rejected(resp));
    }
    Err(last_err.unwrap_or_else(|| Error::msg("所有上游都失败")))
}

async fn fetch_manifest(
    app: &Arc<App>,
    repo: &Repo,
    name: &str,
    reference: &str,
    inbound: &HeaderMap,
) -> Result<Pulled<bytes::Bytes>> {
    let ranked = app.ranks_for(&repo.name).await;
    let upstreams = order_urls(repo, ranked.as_deref());
    let transfer = begin_progress(app, repo, name, reference, &upstreams)?;
    let mut last_err = None;
    let mut rejected = None;
    for (base, proxy) in upstreams {
        let request = OciReq {
            base: &base,
            proxy: &proxy,
            name,
            reference,
            inbound,
            method: Method::GET,
        };
        match pull_manifest_bytes(app, repo, request, &transfer).await {
            Ok(Pulled::Value(body)) => {
                if let Err(error) = verify_body_digest(reference, &body) {
                    app.record_upstream_failure();
                    tracing::warn!(upstream = %base, error = %error, "manifest 校验失败");
                    last_err = Some(error);
                    continue;
                }
                transfer.finish()?;
                return Ok(Pulled::Value(body));
            }
            Ok(Pulled::Rejected(resp)) => {
                app.record_upstream_failure();
                rejected = Some(resp);
            }
            Err(e) => {
                app.record_upstream_failure();
                last_err = Some(e);
            }
        }
    }
    if let Some(resp) = rejected {
        return Ok(Pulled::Rejected(resp));
    }
    Err(last_err.unwrap_or_else(|| Error::msg("所有上游都失败")))
}

async fn fetch_manifest_head(
    app: &App,
    repo: &Repo,
    name: &str,
    reference: &str,
    expected: Option<&str>,
    inbound: &HeaderMap,
) -> Result<Pulled<HeadMeta>> {
    let ranked = app.ranks_for(&repo.name).await;
    let mut last_err = None;
    let mut rejected = None;
    for (base, proxy) in order_urls(repo, ranked.as_deref()) {
        let result = async {
            let meta =
                pull_manifest_head(app, repo, &base, &proxy, name, reference, inbound).await?;
            let Pulled::Value(meta) = meta else {
                return Ok::<_, Error>(meta.map_rejected());
            };
            if let Some(value) = expected {
                let digest = meta
                    .digest
                    .as_deref()
                    .ok_or_else(|| Error::msg("manifest HEAD 缺少 Docker-Content-Digest"))?;
                if value != digest {
                    return Err(Error::msg(format!(
                        "manifest digest mismatch: want {value} got {digest}"
                    )));
                }
            }
            Ok::<_, Error>(Pulled::Value(meta))
        }
        .await;
        match result {
            Ok(Pulled::Value(meta)) => return Ok(Pulled::Value(meta)),
            Ok(Pulled::Rejected(resp)) => {
                app.record_upstream_failure();
                rejected = Some(resp);
            }
            Err(e) => {
                app.record_upstream_failure();
                tracing::warn!(upstream = %base, error = %e, "manifest HEAD 上游失败");
                last_err = Some(e);
            }
        }
    }
    if let Some(resp) = rejected {
        return Ok(Pulled::Rejected(resp));
    }
    Err(last_err.unwrap_or_else(|| Error::msg("所有上游都失败")))
}

async fn fetch_blob(
    app: &Arc<App>,
    repo: &Repo,
    name: &str,
    digest: &str,
    inbound: &HeaderMap,
) -> Result<Pulled<()>> {
    let ranked = app.ranks_for(&repo.name).await;
    let upstreams = order_urls(repo, ranked.as_deref());
    let transfer = begin_progress(app, repo, name, digest, &upstreams)?;
    let mut last_err = None;
    let mut rejected = None;
    for (base, proxy) in upstreams {
        let request = OciReq {
            base: &base,
            proxy: &proxy,
            name,
            reference: digest,
            inbound,
            method: Method::GET,
        };
        match pull_blob(app, repo, request, &transfer).await {
            Ok(Pulled::Value(())) => {
                transfer.finish()?;
                return Ok(Pulled::Value(()));
            }
            Ok(Pulled::Rejected(resp)) => {
                app.record_upstream_failure();
                rejected = Some(resp);
            }
            Err(e) => {
                app.record_upstream_failure();
                tracing::warn!(upstream = %base, error = %e, "blob 上游失败");
                last_err = Some(e);
            }
        }
    }
    if let Some(resp) = rejected {
        return Ok(Pulled::Rejected(resp));
    }
    Err(last_err.unwrap_or_else(|| Error::msg("所有上游都失败")))
}

async fn fetch_blob_pass(
    app: &Arc<App>,
    repo: &Repo,
    name: &str,
    digest: &str,
    inbound: &HeaderMap,
) -> Result<Response> {
    let ranked = app.ranks_for(&repo.name).await;
    let upstreams = order_urls(repo, ranked.as_deref());
    let transfer = begin_progress(app, repo, name, digest, &upstreams)?;
    let mut last_err = None;
    let mut rejected = None;
    for (base, proxy) in upstreams {
        let request = OciReq {
            base: &base,
            proxy: &proxy,
            name,
            reference: digest,
            inbound,
            method: Method::GET,
        };
        match open_blob(app, repo, request).await {
            Ok(resp) if resp.status().is_success() => {
                if let Some(size) = resp.content_length() {
                    app.record_bytes_served(size);
                }
                transfer.start_attempt(&base, resp.content_length())?;
                return httpx::into_axum_tracked(resp, transfer);
            }
            Ok(resp) => {
                app.record_upstream_failure();
                rejected = Some(resp);
            }
            Err(e) => {
                app.record_upstream_failure();
                tracing::warn!(upstream = %base, error = %e, "blob 上游失败");
                last_err = Some(e);
            }
        }
    }
    if let Some(resp) = rejected {
        return distribution_rejection(resp, "BLOB_UNKNOWN");
    }
    Err(last_err.unwrap_or_else(|| Error::msg("所有上游都失败")))
}

async fn fetch_blob_head(
    app: &App,
    repo: &Repo,
    name: &str,
    digest: &str,
    inbound: &HeaderMap,
) -> Result<Pulled<HeadMeta>> {
    let ranked = app.ranks_for(&repo.name).await;
    let mut last_err = None;
    let mut rejected = None;
    for (base, proxy) in order_urls(repo, ranked.as_deref()) {
        let result = async {
            let request = OciReq {
                base: &base,
                proxy: &proxy,
                name,
                reference: digest,
                inbound,
                method: Method::HEAD,
            };
            let value = open_blob(app, repo, request).await?;
            if !value.status().is_success() {
                return Ok::<_, Error>(Pulled::Rejected(value));
            }
            let meta = response_head(&value)?;
            if let Some(actual) = meta.digest.as_deref() {
                if actual != digest {
                    return Err(Error::msg(format!(
                        "blob digest mismatch: want {digest} got {actual}"
                    )));
                }
            }
            Ok::<_, Error>(Pulled::Value(meta))
        }
        .await;
        match result {
            Ok(Pulled::Value(meta)) => return Ok(Pulled::Value(meta)),
            Ok(Pulled::Rejected(resp)) => {
                app.record_upstream_failure();
                rejected = Some(resp);
            }
            Err(e) => {
                app.record_upstream_failure();
                tracing::warn!(upstream = %base, error = %e, "blob HEAD 上游失败");
                last_err = Some(e);
            }
        }
    }
    if let Some(resp) = rejected {
        return Ok(Pulled::Rejected(resp));
    }
    Err(last_err.unwrap_or_else(|| Error::msg("所有上游都失败")))
}

async fn open_blob(app: &App, repo: &Repo, request: OciReq<'_>) -> Result<reqwest::Response> {
    let client = app.egress.read().await.client(request.proxy)?;
    let url = format!(
        "{}/v2/{}/blobs/{}",
        request.base.trim_end_matches('/'),
        request.name,
        request.reference
    );
    let mut h = oci_headers(request.inbound, false);
    if let Some(t) = token_for(&client, repo, request.base, request.proxy, request.name).await? {
        h.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {t}")).expect("token"),
        );
    }
    let resp = follow(&client, &request.method, &url, req_headers(&h)).await?;
    let resp = if resp.status() == StatusCode::UNAUTHORIZED {
        match challenge_token(&client, repo, &resp, request.name).await {
            Ok(t) => {
                let mut h2 = oci_headers(request.inbound, false);
                h2.insert(
                    header::AUTHORIZATION,
                    header::HeaderValue::from_str(&format!("Bearer {t}")).expect("token"),
                );
                follow(&client, &request.method, &url, req_headers(&h2)).await?
            }
            Err(e) => {
                tracing::warn!(error = %e, "blob 鉴权挑战无效");
                return Ok(resp);
            }
        }
    } else {
        resp
    };
    Ok(resp)
}

async fn pull_manifest_bytes(
    app: &Arc<App>,
    repo: &Repo,
    request: OciReq<'_>,
    transfer: &httpx::TransferProgress,
) -> Result<Pulled<bytes::Bytes>> {
    let base = request.base;
    let resp = open_manifest(app, repo, request).await?;
    if !resp.status().is_success() {
        return Ok(Pulled::Rejected(resp));
    }
    transfer.start_attempt(base, resp.content_length())?;
    let mut stream = transfer.track(resp.bytes_stream());
    let mut body = bytes::BytesMut::new();
    while let Some(chunk) = stream.try_next().await? {
        body.extend_from_slice(&chunk);
    }
    transfer.flush(body.len() as u64)?;
    Ok(Pulled::Value(body.freeze()))
}

async fn pull_manifest_head(
    app: &App,
    repo: &Repo,
    base: &str,
    proxy: &str,
    name: &str,
    reference: &str,
    inbound: &HeaderMap,
) -> Result<Pulled<HeadMeta>> {
    let request = OciReq {
        base,
        proxy,
        name,
        reference,
        inbound,
        method: Method::HEAD,
    };
    let resp = open_manifest(app, repo, request).await?;
    if !resp.status().is_success() {
        return Ok(Pulled::Rejected(resp));
    }
    let meta = response_head(&resp)?;
    let digest = meta
        .digest
        .as_deref()
        .ok_or_else(|| Error::msg("manifest HEAD 缺少 Docker-Content-Digest"))?;
    app.store.blob_path(digest)?;
    Ok(Pulled::Value(meta))
}

async fn open_manifest(app: &App, repo: &Repo, request: OciReq<'_>) -> Result<reqwest::Response> {
    let client = app.egress.read().await.client(request.proxy)?;
    let url = format!(
        "{}/v2/{}/manifests/{}",
        request.base.trim_end_matches('/'),
        request.name,
        request.reference
    );
    let headers = oci_headers(request.inbound, true);
    let token = token_for(&client, repo, request.base, request.proxy, request.name).await?;
    let mut h = headers;
    if let Some(t) = &token {
        h.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {t}")).expect("token"),
        );
    }
    let resp = follow(&client, &request.method, &url, req_headers(&h)).await?;
    if resp.status() == StatusCode::UNAUTHORIZED {
        match challenge_token(&client, repo, &resp, request.name).await {
            Ok(t) => {
                let mut h2 = oci_headers(request.inbound, true);
                h2.insert(
                    header::AUTHORIZATION,
                    header::HeaderValue::from_str(&format!("Bearer {t}")).expect("token"),
                );
                return follow(&client, &request.method, &url, req_headers(&h2)).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "manifest 鉴权挑战无效");
                return Ok(resp);
            }
        }
    }
    Ok(resp)
}

async fn pull_blob(
    app: &Arc<App>,
    repo: &Repo,
    request: OciReq<'_>,
    transfer: &httpx::TransferProgress,
) -> Result<Pulled<()>> {
    let base = request.base;
    let name = request.name;
    let digest = request.reference;
    let resp = open_blob(app, repo, request).await?;
    if !resp.status().is_success() {
        return Ok(Pulled::Rejected(resp));
    }
    let total = resp.content_length();
    transfer.start_attempt(base, total)?;
    let stream = transfer.track(resp.bytes_stream());
    let persisted = app.store.persist_stream(Some(digest), stream).await;
    let (got, size) = match persisted {
        Ok(value) => value,
        Err(e) => return Err(e),
    };
    transfer.flush(size)?;
    app.bind_digest(&repo.name, name, &got)?;
    Ok(Pulled::Value(()))
}

async fn token_for(
    client: &reqwest::Client,
    repo: &Repo,
    base: &str,
    _proxy: &str,
    name: &str,
) -> Result<Option<String>> {
    if let (Some(u), Some(p)) = (&repo.username, &repo.password) {
        let realm = format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{name}:pull"
        );
        if base.contains("docker.io") {
            return Ok(Some(fetch_token(client, &realm, Some((u, p))).await?));
        }
    }
    Ok(None)
}

async fn challenge_token(
    client: &reqwest::Client,
    repo: &Repo,
    resp: &reqwest::Response,
    name: &str,
) -> Result<String> {
    let www = httpx::header_str(resp, "www-authenticate").unwrap_or_default();
    let realm = parse_auth_param(&www, "realm")
        .ok_or_else(|| Error::msg(format!("无法解析 WWW-Authenticate: {www}")))?;
    let service = parse_auth_param(&www, "service").unwrap_or_default();
    let scope =
        parse_auth_param(&www, "scope").unwrap_or_else(|| format!("repository:{name}:pull"));
    let mut url = reqwest::Url::parse(&realm)?;
    {
        let mut q = url.query_pairs_mut();
        if !service.is_empty() {
            q.append_pair("service", &service);
        }
        q.append_pair("scope", &scope);
    }
    let cred = repo
        .username
        .as_ref()
        .zip(repo.password.as_ref())
        .map(|(u, p)| (u.as_str(), p.as_str()));
    fetch_token(client, url.as_str(), cred).await
}

async fn fetch_token(
    client: &reqwest::Client,
    url: &str,
    cred: Option<(&str, &str)>,
) -> Result<String> {
    let mut req = client.get(url);
    if let Some((u, p)) = cred {
        req = req.basic_auth(u, Some(p));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(Error::msg(format!("换 token 失败 HTTP {}", resp.status())));
    }
    let v: serde_json::Value = resp.json().await?;
    v.get("token")
        .or_else(|| v.get("access_token"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| Error::msg("token 响应没有 token 字段"))
}

fn parse_auth_param(header: &str, key: &str) -> Option<String> {
    let k = format!("{key}=");
    let i = header.find(&k)?;
    let rest = &header[i + k.len()..];
    if let Some(s) = rest.strip_prefix('"') {
        let end = s.find('"')?;
        Some(s[..end].to_string())
    } else {
        let end = rest.find(',').unwrap_or(rest.len());
        Some(rest[..end].trim().to_string())
    }
}

fn normalize_name(name: &str, repo: &Repo) -> String {
    if name.contains('/') {
        return name.to_string();
    }
    let hub = repo
        .upstreams
        .iter()
        .any(|u| u.url.contains("docker.io") || u.url.contains("daocloud"));
    if hub {
        format!("library/{name}")
    } else {
        name.to_string()
    }
}

fn oci_headers(inbound: &HeaderMap, manifest: bool) -> HeaderMap {
    let mut h = HeaderMap::new();
    if manifest {
        if let Some(a) = inbound.get(header::ACCEPT) {
            h.insert(header::ACCEPT, a.clone());
        } else {
            h.insert(header::ACCEPT, header::HeaderValue::from_static(ACCEPT));
        }
    } else {
        for name in [header::RANGE, header::IF_RANGE] {
            if let Some(value) = inbound.get(&name) {
                h.insert(name, value.clone());
            }
        }
    }
    h
}

fn req_headers(h: &HeaderMap) -> ReqHeaders {
    let mut out = ReqHeaders::new();
    for (k, v) in h.iter() {
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_str().as_bytes()) {
            if let Ok(val) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) {
                out.insert(name, val);
            }
        }
    }
    out
}

async fn follow(
    client: &reqwest::Client,
    method: &Method,
    url: &str,
    headers: ReqHeaders,
) -> Result<reqwest::Response> {
    match *method {
        Method::GET => httpx::get_follow(client, url, headers).await,
        Method::HEAD => httpx::head_follow(client, url, headers).await,
        _ => Err(Error::msg(format!("OCI 不支持方法 {method}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_manifest_path() {
        let (n, r) = split_end("library/nginx/manifests/latest", "/manifests/").unwrap();
        assert_eq!(n, "library/nginx");
        assert_eq!(r, "latest");
    }

    #[test]
    fn maps_common_distribution_errors() {
        assert_eq!(
            distribution_error(StatusCode::UNAUTHORIZED, "BLOB_UNKNOWN"),
            Some(("UNAUTHORIZED", "authentication required"))
        );
        assert_eq!(
            distribution_error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN"),
            Some(("BLOB_UNKNOWN", "blob unknown to registry"))
        );
        assert_eq!(
            distribution_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN"),
            Some(("MANIFEST_UNKNOWN", "manifest unknown"))
        );
    }
}
