use crate::app::TransferGuard;
use crate::error::{Error, Result};
use futures_util::{Stream, TryStreamExt};
use reqwest::{header, Client, Method, Response, StatusCode};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use url::Url;

/// why: 进度状态跟随流共享守卫，直通 Body 离开请求 future 后仍能正确结束。
#[derive(Clone)]
pub struct TransferProgress {
    guard: Arc<TransferGuard>,
    error: Arc<Mutex<Option<String>>>,
}

impl TransferProgress {
    pub fn new(guard: TransferGuard) -> Self {
        Self {
            guard: Arc::new(guard),
            error: Arc::new(Mutex::new(None)),
        }
    }

    pub fn start_attempt(&self, upstream: &str, total: Option<u64>) -> Result<()> {
        self.check()?;
        self.guard.start_attempt(upstream, total)
    }

    pub fn track<S>(
        &self,
        stream: S,
    ) -> impl futures_util::Stream<Item = std::result::Result<bytes::Bytes, S::Error>> + Unpin
    where
        S: futures_util::TryStream<Ok = bytes::Bytes> + Unpin,
    {
        let guard = self.guard.clone();
        let error = self.error.clone();
        stream.inspect_ok(move |chunk| {
            if error.lock().expect("progress error").is_some() {
                return;
            }
            match guard.received(chunk.len() as u64) {
                Ok(()) => {}
                Err(e) => *error.lock().expect("progress error") = Some(e.to_string()),
            }
        })
    }

    pub fn flush(&self, _size: u64) -> Result<()> {
        self.check()
    }

    fn check(&self) -> Result<()> {
        if let Some(error) = self.error.lock().expect("progress error").clone() {
            self.guard.fail(&error)?;
            return Err(Error::msg(error));
        }
        Ok(())
    }

    pub fn finish(&self) -> Result<()> {
        self.guard.finish()
    }

    pub fn fail(&self, error: &Error) -> Result<()> {
        self.guard.fail(&error.to_string())
    }

    /// why: 直通响应的完成、读失败和客户端取消发生在 Body 生命周期，不能在返回 Response 时提前移除。
    fn track_body<S>(
        self,
        mut stream: S,
    ) -> impl Stream<Item = std::io::Result<bytes::Bytes>> + Unpin
    where
        S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Unpin,
    {
        let mut ended = false;
        futures_util::stream::poll_fn(move |cx| {
            if ended {
                return Poll::Ready(None);
            }
            match Pin::new(&mut stream).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => match self.guard.received(chunk.len() as u64) {
                    Ok(()) => Poll::Ready(Some(Ok(chunk))),
                    Err(error) => {
                        ended = true;
                        let message = error.to_string();
                        let _ = self.guard.fail(&message);
                        Poll::Ready(Some(Err(std::io::Error::other(message))))
                    }
                },
                Poll::Ready(Some(Err(error))) => {
                    ended = true;
                    let message = error.to_string();
                    let _ = self.guard.fail(&message);
                    Poll::Ready(Some(Err(std::io::Error::other(message))))
                }
                Poll::Ready(None) => {
                    ended = true;
                    match self.guard.finish() {
                        Ok(()) => Poll::Ready(None),
                        Err(error) => {
                            Poll::Ready(Some(Err(std::io::Error::other(error.to_string()))))
                        }
                    }
                }
                Poll::Pending => Poll::Pending,
            }
        })
    }
}

/// 跨 host 重定向时丢掉 Authorization，避免把 Hub token 送到 S3。
pub async fn get_follow(
    client: &Client,
    url: &str,
    headers: header::HeaderMap,
) -> Result<Response> {
    request_follow(client, Method::GET, url, headers).await
}

pub async fn head_follow(
    client: &Client,
    url: &str,
    headers: header::HeaderMap,
) -> Result<Response> {
    request_follow(client, Method::HEAD, url, headers).await
}

async fn request_follow(
    client: &Client,
    method: Method,
    url: &str,
    mut headers: header::HeaderMap,
) -> Result<Response> {
    let mut current = Url::parse(url)?;
    for _ in 0..8 {
        let resp = client
            .request(method.clone(), current.clone())
            .headers(headers.clone())
            .send()
            .await?;
        if !is_follow_redirect(resp.status()) {
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

/// why: 304 也属于 3xx，但它是条件请求结果，没有 Location 且不应跟随。
fn is_follow_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

/// why: cache=false 时把上游响应原样转给客户端，不落盘。
pub fn into_axum(resp: Response) -> Result<axum::response::Response> {
    into_axum_inner(resp, None)
}

/// why: cache=false 的下载在客户端消费 Body 时才发生，进度必须绑到该流。
pub fn into_axum_tracked(
    resp: Response,
    transfer: TransferProgress,
) -> Result<axum::response::Response> {
    into_axum_inner(resp, Some(transfer))
}

fn into_axum_inner(
    resp: Response,
    transfer: Option<TransferProgress>,
) -> Result<axum::response::Response> {
    use axum::body::Body;
    use axum::http::{HeaderName, HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out_headers = axum::http::HeaderMap::new();
    for (name, value) in resp.headers().iter() {
        let lname = name.as_str();
        if matches!(
            lname,
            "content-type"
                | "content-length"
                | "content-range"
                | "accept-ranges"
                | "docker-content-digest"
                | "etag"
                | "last-modified"
                | "cache-control"
                | "expires"
                | "content-disposition"
                | "location"
                | "www-authenticate"
                | "docker-distribution-api-version"
        ) {
            if let (Ok(k), Ok(v)) = (
                HeaderName::from_bytes(lname.as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                out_headers.insert(k, v);
            }
        }
    }
    let body = match transfer {
        Some(transfer) => Body::from_stream(transfer.track_body(resp.bytes_stream())),
        None => {
            let stream = resp
                .bytes_stream()
                .map_err(|e| std::io::Error::other(e.to_string()));
            Body::from_stream(stream)
        }
    };
    let mut res = (status, body).into_response();
    *res.headers_mut() = out_headers;
    Ok(res)
}

pub fn header_str(resp: &Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// why: 命中大制品时必须按读盘背压发送，避免请求体积直接变成进程内存占用。
pub async fn file_body(path: &Path, start: u64, size: u64) -> Result<axum::body::Body> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let reader = file.take(size);
    Ok(axum::body::Body::from_stream(ReaderStream::new(reader)))
}

/// why: 本地缓存只支持单段 Range，拒绝多段可避免错误拼成一个连续响应。
pub fn parse_byte_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let raw = value.strip_prefix("bytes=")?;
    if raw.contains(',') || total == 0 {
        return None;
    }
    let (start, end) = raw.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 {
            return None;
        }
        let size = suffix.min(total);
        return Some((total - size, total - 1));
    }
    let start = start.parse::<u64>().ok()?;
    if start >= total {
        return None;
    }
    let end = if end.is_empty() {
        total - 1
    } else {
        end.parse::<u64>().ok()?.min(total - 1)
    };
    (start <= end).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfers::TransferRegistry;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn upstream_body() -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf).await.expect("read request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc")
                .await
                .expect("write response");
        });
        let response = Client::new()
            .get(format!("http://{addr}/blob"))
            .send()
            .await
            .expect("response");
        server.await.expect("server task");
        response
    }

    #[test]
    fn only_location_redirects_are_followed() {
        assert!(is_follow_redirect(StatusCode::FOUND));
        assert!(is_follow_redirect(StatusCode::PERMANENT_REDIRECT));
        assert!(!is_follow_redirect(StatusCode::MULTIPLE_CHOICES));
        assert!(!is_follow_redirect(StatusCode::NOT_MODIFIED));
    }

    #[test]
    fn parses_single_byte_ranges() {
        assert_eq!(parse_byte_range("bytes=2-4", 10), Some((2, 4)));
        assert_eq!(parse_byte_range("bytes=7-", 10), Some((7, 9)));
        assert_eq!(parse_byte_range("bytes=-3", 10), Some((7, 9)));
        assert_eq!(parse_byte_range("bytes=-30", 10), Some((0, 9)));
    }

    #[test]
    fn rejects_unsatisfiable_or_multiple_ranges() {
        assert_eq!(parse_byte_range("bytes=10-", 10), None);
        assert_eq!(parse_byte_range("bytes=5-2", 10), None);
        assert_eq!(parse_byte_range("bytes=0-1,4-5", 10), None);
        assert_eq!(parse_byte_range("items=0-1", 10), None);
    }

    #[tokio::test]
    async fn file_body_streams_only_selected_bytes() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/picapica-tests");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create test dir");
        let path = dir.join(format!("stream-{}.bin", std::process::id()));
        tokio::fs::write(&path, b"0123456789")
            .await
            .expect("write test file");

        let body = file_body(&path, 3, 4).await.expect("create file body");
        let bytes = axum::body::to_bytes(body, 4).await.expect("read body");
        assert_eq!(&bytes[..], b"3456");
        tokio::fs::remove_file(path)
            .await
            .expect("remove test file");
    }

    #[tokio::test]
    async fn not_modified_does_not_require_location() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf).await.expect("read request");
            stream
                .write_all(b"HTTP/1.1 304 Not Modified\r\nETag: abc\r\n\r\n")
                .await
                .expect("write response");
        });
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client");

        let resp = get_follow(
            &client,
            &format!("http://{addr}/index"),
            header::HeaderMap::new(),
        )
        .await
        .expect("304 response");

        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn tracked_body_finishes_only_after_eof() {
        let upstream = upstream_body().await;
        let registry = TransferRegistry::default();
        let guard = registry
            .begin(
                "docker",
                "library/demo",
                "layer",
                "https://a.example",
                Some(3),
            )
            .expect("begin transfer");
        let transfer = TransferProgress::new(guard);
        transfer
            .start_attempt("https://a.example", Some(3))
            .expect("start attempt");

        let response = into_axum_tracked(upstream, transfer).expect("tracked response");
        assert_eq!(registry.active_count(), 1);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");

        assert_eq!(&body[..], b"abc");
        assert_eq!(registry.active_count(), 0);
    }

    #[tokio::test]
    async fn dropping_tracked_body_removes_transfer() {
        let upstream = upstream_body().await;
        let registry = TransferRegistry::default();
        let guard = registry
            .begin(
                "docker",
                "library/demo",
                "layer",
                "https://a.example",
                Some(3),
            )
            .expect("begin transfer");
        let transfer = TransferProgress::new(guard);
        transfer
            .start_attempt("https://a.example", Some(3))
            .expect("start attempt");

        let response = into_axum_tracked(upstream, transfer).expect("tracked response");
        assert_eq!(registry.active_count(), 1);
        drop(response);
        assert_eq!(registry.active_count(), 0);
    }
}
