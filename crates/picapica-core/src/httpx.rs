use crate::error::{Error, Result};
use reqwest::{header, Client, Response};
use url::Url;

/// 跨 host 重定向时丢掉 Authorization，避免把 Hub token 送到 S3。
pub async fn get_follow(
    client: &Client,
    url: &str,
    mut headers: header::HeaderMap,
) -> Result<Response> {
    let mut current = Url::parse(url)?;
    for _ in 0..8 {
        let resp = client
            .get(current.clone())
            .headers(headers.clone())
            .send()
            .await?;
        if !resp.status().is_redirection() {
            return Ok(resp);
        }
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::msg("redirect without Location"))?;
        let next = current.join(loc)?;
        if next.host_str() != current.host_str() {
            headers.remove(header::AUTHORIZATION);
        }
        current = next;
    }
    Err(Error::msg("too many redirects"))
}

pub async fn head_follow(
    client: &Client,
    url: &str,
    mut headers: header::HeaderMap,
) -> Result<Response> {
    let mut current = Url::parse(url)?;
    for _ in 0..8 {
        let resp = client
            .head(current.clone())
            .headers(headers.clone())
            .send()
            .await?;
        if !resp.status().is_redirection() {
            return Ok(resp);
        }
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::msg("redirect without Location"))?;
        let next = current.join(loc)?;
        if next.host_str() != current.host_str() {
            headers.remove(header::AUTHORIZATION);
        }
        current = next;
    }
    Err(Error::msg("too many redirects"))
}

pub fn header_str(resp: &Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}
