use crate::app::App;
use crate::error::{Error, Result};
use crate::httpx;
use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub(super) struct HeadMeta {
    pub(super) digest: Option<String>,
    pub(super) content_type: Option<String>,
    pub(super) content_length: Option<u64>,
}

pub(super) fn response_head(resp: &reqwest::Response) -> Result<HeadMeta> {
    let digest = response_header(resp, "docker-content-digest")?;
    let content_type = response_header(resp, "content-type")?;
    let content_length = match response_header(resp, "content-length")? {
        Some(raw) => Some(
            raw.parse::<u64>()
                .map_err(|_| Error::msg(format!("上游 Content-Length 非法: {raw}")))?,
        ),
        None => None,
    };
    Ok(HeadMeta {
        digest,
        content_type,
        content_length,
    })
}

fn response_header(resp: &reqwest::Response, name: &str) -> Result<Option<String>> {
    resp.headers()
        .get(name)
        .map(|value| {
            value
                .to_str()
                .map(str::to_string)
                .map_err(|e| Error::msg(format!("上游响应头 {name} 非法: {e}")))
        })
        .transpose()
}

/// why: HEAD 只返回与 GET 相同的实体元数据，绝不能把上游或缓存正文带给客户端。
pub(super) fn head_response(
    meta: &HeadMeta,
    expected: Option<&str>,
    manifest: bool,
) -> Result<Response> {
    let digest = expected
        .map(str::to_string)
        .or_else(|| meta.digest.clone())
        .ok_or_else(|| Error::msg("OCI HEAD 缺少 Docker-Content-Digest"))?;
    let content_type = match meta.content_type.as_deref() {
        Some(value) => header::HeaderValue::from_str(value)
            .map_err(|e| Error::msg(format!("上游 Content-Type 非法: {e}")))?,
        None if !manifest => header::HeaderValue::from_static("application/octet-stream"),
        None => return Err(Error::msg("manifest HEAD 缺少 Content-Type")),
    };
    let digest_value = header::HeaderValue::from_str(&digest)
        .map_err(|e| Error::msg(format!("Docker-Content-Digest 非法: {e}")))?;
    let mut response = (StatusCode::OK, Body::empty()).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    let digest_name = header::HeaderName::from_static("docker-content-digest");
    headers.insert(digest_name, digest_value);
    if let Some(length) = meta.content_length {
        headers.insert(header::CONTENT_LENGTH, header::HeaderValue::from(length));
    }
    Ok(response)
}

pub(super) fn verify_body_digest(expected: &str, body: &[u8]) -> Result<()> {
    let mut hasher = Sha256::new();
    hasher.update(body);
    let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
    if actual != expected {
        return Err(Error::msg(format!(
            "manifest digest mismatch: want {expected} got {actual}"
        )));
    }
    Ok(())
}

/// why: 多架构 index 与单平台 manifest 必须返回各自 media type，否则 Docker 会拒绝正文。
fn manifest_ct(body: &[u8]) -> Result<header::HeaderValue> {
    let value: serde_json::Value = serde_json::from_slice(body)?;
    let media_type = value
        .get("mediaType")
        .and_then(|value| value.as_str())
        .ok_or_else(|| Error::msg("OCI manifest 缺少 mediaType"))?;
    header::HeaderValue::from_str(media_type)
        .map_err(|e| Error::msg(format!("OCI mediaType 非法: {e}")))
}

pub(super) async fn serve_file(
    app: &App,
    digest: &str,
    is_manifest: bool,
    inbound: &HeaderMap,
) -> Result<Response> {
    let path = app.store.blob_path(digest)?;
    if is_manifest {
        let data = tokio::fs::read(&path).await?;
        return serve_bytes_digest(app, bytes::Bytes::from(data), digest, true);
    }
    let total = tokio::fs::metadata(&path).await?.len();
    // why: 缓存没有保存验证器，无法证明 If-Range 命中时必须忽略 Range 返回完整实体。
    let range = if inbound.contains_key(header::IF_RANGE) {
        None
    } else {
        inbound.get(header::RANGE)
    };
    if let Some(value) = range {
        let selected = value
            .to_str()
            .ok()
            .and_then(|value| httpx::parse_byte_range(value, total));
        let Some((start, end)) = selected else {
            return range_not_satisfiable(digest, total);
        };
        let size = end - start + 1;
        let body = httpx::file_body(&path, start, size).await?;
        app.record_bytes_served(size);
        return blob_file_response(
            StatusCode::PARTIAL_CONTENT,
            digest,
            size,
            Some((start, end, total)),
            body,
        );
    }
    let body = httpx::file_body(&path, 0, total).await?;
    app.record_bytes_served(total);
    blob_file_response(StatusCode::OK, digest, total, None, body)
}

fn blob_file_response(
    status: StatusCode,
    digest: &str,
    size: u64,
    range: Option<(u64, u64, u64)>,
    body: Body,
) -> Result<Response> {
    let digest = header::HeaderValue::from_str(digest)
        .map_err(|e| Error::msg(format!("Docker-Content-Digest 非法: {e}")))?;
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size)
        .header(header::ACCEPT_RANGES, "bytes")
        .header("docker-content-digest", digest);
    if let Some((start, end, total)) = range {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}"),
        );
    }
    response
        .body(body)
        .map_err(|e| Error::msg(format!("构造 OCI blob 响应失败: {e}")))
}

fn range_not_satisfiable(digest: &str, total: u64) -> Result<Response> {
    let digest = header::HeaderValue::from_str(digest)
        .map_err(|e| Error::msg(format!("Docker-Content-Digest 非法: {e}")))?;
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{total}"))
        .header(header::ACCEPT_RANGES, "bytes")
        .header("docker-content-digest", digest)
        .body(Body::empty())
        .map_err(|e| Error::msg(format!("构造 OCI Range 响应失败: {e}")))
}

pub(super) async fn serve_file_head(
    app: &App,
    digest: &str,
    is_manifest: bool,
) -> Result<Response> {
    let path = app.store.blob_path(digest)?;
    let content_length = tokio::fs::metadata(&path).await?.len();
    let content_type = if is_manifest {
        let data = tokio::fs::read(&path).await?;
        manifest_ct(&data)?
            .to_str()
            .expect("静态 media type")
            .to_string()
    } else {
        "application/octet-stream".to_string()
    };
    let meta = HeadMeta {
        digest: Some(digest.to_string()),
        content_type: Some(content_type),
        content_length: Some(content_length),
    };
    head_response(&meta, Some(digest), is_manifest)
}

pub(super) fn serve_bytes_digest(
    app: &App,
    data: bytes::Bytes,
    digest: &str,
    is_manifest: bool,
) -> Result<Response> {
    let content_type = if is_manifest {
        manifest_ct(&data)?
    } else {
        header::HeaderValue::from_static("application/octet-stream")
    };
    app.record_bytes_served(data.len() as u64);
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
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
    fn manifest_uses_declared_media_type() {
        let body = br#"{"mediaType":"application/vnd.oci.image.index.v1+json"}"#;
        let content_type = manifest_ct(body).expect("media type");
        assert_eq!(content_type, "application/vnd.oci.image.index.v1+json");
    }

    #[tokio::test]
    async fn head_response_has_metadata_without_body() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let meta = HeadMeta {
            digest: Some(digest.to_string()),
            content_type: Some("application/octet-stream".to_string()),
            content_length: Some(123),
        };

        let response = head_response(&meta, Some(digest), false).expect("构造 HEAD 响应");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "123");
        assert_eq!(response.headers()["docker-content-digest"], digest);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("读取响应体");
        assert!(body.is_empty());
    }

    #[test]
    fn manifest_digest_mismatch_is_rejected() {
        let expected = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let err = verify_body_digest(expected, b"tampered").expect_err("摘要变化必须失败");
        assert!(err.to_string().contains("digest mismatch"));
    }
}
