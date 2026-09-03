use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// 所有可恢复失败都走这里，调用方必须看见原因。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Msg(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Url(#[from] url::ParseError),
}

impl Error {
    pub fn msg(m: impl Into<String>) -> Self {
        Self::Msg(m.into())
    }

    pub fn status(&self) -> StatusCode {
        let t = self.to_string();
        if t.contains("不存在") || t.contains("not found") {
            StatusCode::NOT_FOUND
        } else if t.contains("未授权") || t.contains("令牌") {
            StatusCode::UNAUTHORIZED
        } else if t.contains("不允许") {
            StatusCode::CONFLICT
        } else if t.contains("缺少") || t.contains("非法") {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::BAD_GATEWAY
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let code = self.status();
        tracing::warn!(error = %self, status = %code, "request failed");
        (code, self.to_string()).into_response()
    }
}

pub type Result<T> = std::result::Result<T, Error>;
