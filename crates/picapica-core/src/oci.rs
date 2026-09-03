use crate::app::App;
use crate::config::Repo;
use crate::error::{Error, Result};
use crate::httpx;
use crate::probe::order_urls;
use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use reqwest::header::HeaderMap as ReqHeaders;
use std::sync::Arc;

const ACCEPT: &str = "application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v1+json";

pub async fn handle(app: Arc<App>, repo: Repo, rest: &str, req: Request<Body>) -> Result<Response> {
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
    if reference.starts_with("sha256:") {
        if app.store.has_digest(reference) {
            return serve_file(&app, reference, true).await;
        }
        let body = fetch_manifest(&app, &repo, &name, reference, req.headers()).await?;
        app.store.persist_bytes(reference, &body).await?;
        app.store.put_tag(&repo.name, &name, reference, reference)?;
        return serve_bytes(body, true);
    }
    if method == Method::HEAD {
        if let Some(d) = revalidate(&app, &repo, &name, reference, req.headers()).await? {
            let path = app.store.blob_path(&d);
            let len = tokio::fs::metadata(&path).await?.len();
            return Ok((
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, manifest_ct()),
                    (
                        header::HeaderName::from_static("docker-content-digest"),
                        header::HeaderValue::from_str(&d).expect("digest"),
                    ),
                    (header::CONTENT_LENGTH, header::HeaderValue::from(len)),
                ],
                Body::empty(),
            )
                .into_response());
        }
    }
    let digest = revalidate(&app, &repo, &name, reference, req.headers()).await?;
    if let Some(d) = digest {
        if method == Method::HEAD {
            return serve_file(&app, &d, true).await;
        }
        return serve_file(&app, &d, true).await;
    }
    Err(Error::msg(format!("manifest 不存在: {name}:{reference}")))
}

async fn blob(
    app: Arc<App>,
    repo: Repo,
    name: &str,
    digest: &str,
    req: Request<Body>,
) -> Result<Response> {
    let name = normalize_name(name, &repo);
    if !digest.starts_with("sha256:") {
        return Err(Error::msg("blob 必须是 sha256 digest"));
    }
    if app.store.has_digest(digest) {
        return serve_file(&app, digest, false).await;
    }
    let digest_lock = app.digest_lock(digest).await;
    let _g = digest_lock.lock().await;
    if app.store.has_digest(digest) {
        return serve_file(&app, digest, false).await;
    }
    fetch_blob(&app, &repo, &name, digest, req.headers()).await?;
    if req.method() == Method::HEAD {
        let path = app.store.blob_path(digest);
        let len = tokio::fs::metadata(&path).await?.len();
        return Ok((
            StatusCode::OK,
            [
                (
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static("application/octet-stream"),
                ),
                (header::CONTENT_LENGTH, header::HeaderValue::from(len)),
            ],
            Body::empty(),
        )
            .into_response());
    }
    serve_file(&app, digest, false).await
}

async fn revalidate(
    app: &App,
    repo: &Repo,
    name: &str,
    tag: &str,
    inbound: &HeaderMap,
) -> Result<Option<String>> {
    let ranked = app.ranks_for(&repo.name).await;
    let mut last_err = None;
    for (base, proxy) in order_urls(repo, ranked.as_deref()) {
        match pull_manifest(app, repo, &base, &proxy, name, tag, inbound).await {
            Ok(digest) => {
                app.store.put_tag(&repo.name, name, tag, &digest)?;
                return Ok(Some(digest));
            }
            Err(e) => {
                tracing::warn!(upstream = %base, error = %e, "manifest 上游失败");
                last_err = Some(e);
            }
        }
    }
    if let Some(local) = app.store.get_tag(&repo.name, name, tag)? {
        if app.store.has_digest(&local) {
            return Ok(Some(local));
        }
    }
    Err(last_err.unwrap_or_else(|| Error::msg("所有上游都失败")))
}

async fn fetch_manifest(
    app: &App,
    repo: &Repo,
    name: &str,
    reference: &str,
    inbound: &HeaderMap,
) -> Result<bytes::Bytes> {
    let ranked = app.ranks_for(&repo.name).await;
    let mut last_err = None;
    for (base, proxy) in order_urls(repo, ranked.as_deref()) {
        match pull_manifest_bytes(app, repo, &base, &proxy, name, reference, inbound).await {
            Ok(b) => return Ok(b),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| Error::msg("所有上游都失败")))
}

async fn fetch_blob(
    app: &App,
    repo: &Repo,
    name: &str,
    digest: &str,
    inbound: &HeaderMap,
) -> Result<()> {
    let ranked = app.ranks_for(&repo.name).await;
    let mut last_err = None;
    for (base, proxy) in order_urls(repo, ranked.as_deref()) {
        match pull_blob(app, repo, &base, &proxy, name, digest, inbound).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(upstream = %base, error = %e, "blob 上游失败");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| Error::msg("所有上游都失败")))
}

async fn pull_manifest(
    app: &App,
    repo: &Repo,
    base: &str,
    proxy: &str,
    name: &str,
    tag: &str,
    inbound: &HeaderMap,
) -> Result<String> {
    let bytes = pull_manifest_bytes(app, repo, base, proxy, name, tag, inbound).await?;
    let digest = app.store.write_blob(&bytes).await?;
    Ok(digest)
}

async fn pull_manifest_bytes(
    app: &App,
    repo: &Repo,
    base: &str,
    proxy: &str,
    name: &str,
    reference: &str,
    inbound: &HeaderMap,
) -> Result<bytes::Bytes> {
    let client = app.egress.read().await.client(proxy)?;
    let url = format!(
        "{}/v2/{}/manifests/{}",
        base.trim_end_matches('/'),
        name,
        reference
    );
    let headers = oci_headers(inbound, true);
    let token = token_for(&client, repo, base, proxy, name).await?;
    let mut h = headers;
    if let Some(t) = &token {
        h.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {t}")).expect("token"),
        );
    }
    let resp = httpx::get_follow(&client, &url, req_headers(&h)).await?;
    if resp.status() == StatusCode::UNAUTHORIZED {
        let t = challenge_token(&client, repo, &resp, name).await?;
        let mut h2 = oci_headers(inbound, true);
        h2.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {t}")).expect("token"),
        );
        let resp = httpx::get_follow(&client, &url, req_headers(&h2)).await?;
        return read_ok(resp).await;
    }
    read_ok(resp).await
}

async fn pull_blob(
    app: &App,
    repo: &Repo,
    base: &str,
    proxy: &str,
    name: &str,
    digest: &str,
    inbound: &HeaderMap,
) -> Result<()> {
    let client = app.egress.read().await.client(proxy)?;
    let url = format!(
        "{}/v2/{}/blobs/{}",
        base.trim_end_matches('/'),
        name,
        digest
    );
    let mut h = oci_headers(inbound, false);
    if let Some(t) = token_for(&client, repo, base, proxy, name).await? {
        h.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {t}")).expect("token"),
        );
    }
    let resp = httpx::get_follow(&client, &url, req_headers(&h)).await?;
    let resp = if resp.status() == StatusCode::UNAUTHORIZED {
        let t = challenge_token(&client, repo, &resp, name).await?;
        let mut h2 = oci_headers(inbound, false);
        h2.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {t}")).expect("token"),
        );
        httpx::get_follow(&client, &url, req_headers(&h2)).await?
    } else {
        resp
    };
    if !resp.status().is_success() {
        return Err(Error::msg(format!(
            "blob {} HTTP {}",
            digest,
            resp.status()
        )));
    }
    let (got, _) = app
        .store
        .persist_stream(Some(digest), resp.bytes_stream())
        .await?;
    app.store.put_tag(&repo.name, name, digest, &got)?;
    Ok(())
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
    let ping = format!("{}/v2/", base.trim_end_matches('/'));
    let resp = client.get(&ping).send().await?;
    if resp.status() != StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    Ok(Some(challenge_token(client, repo, &resp, name).await?))
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

async fn read_ok(resp: reqwest::Response) -> Result<bytes::Bytes> {
    if !resp.status().is_success() {
        return Err(Error::msg(format!("manifest HTTP {}", resp.status())));
    }
    Ok(resp.bytes().await?)
}

fn manifest_ct() -> header::HeaderValue {
    "application/vnd.docker.distribution.manifest.v2+json"
        .parse()
        .unwrap()
}

async fn serve_file(app: &App, digest: &str, is_manifest: bool) -> Result<Response> {
    let path = app.store.blob_path(digest);
    let data = tokio::fs::read(&path).await?;
    serve_bytes_digest(data, digest, is_manifest)
}

fn serve_bytes(body: bytes::Bytes, is_manifest: bool) -> Result<Response> {
    Ok((
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            if is_manifest {
                manifest_ct()
            } else {
                header::HeaderValue::from_static("application/octet-stream")
            },
        )],
        body,
    )
        .into_response())
}

fn serve_bytes_digest(data: Vec<u8>, digest: &str, is_manifest: bool) -> Result<Response> {
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                if is_manifest {
                    manifest_ct()
                } else {
                    header::HeaderValue::from_static("application/octet-stream")
                },
            ),
            (
                header::HeaderName::from_static("docker-content-digest"),
                header::HeaderValue::from_str(digest).expect("digest"),
            ),
        ],
        data,
    )
        .into_response())
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
}
