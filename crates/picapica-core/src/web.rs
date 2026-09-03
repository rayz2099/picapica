use axum::http::header;
use axum::response::{Html, IntoResponse};

pub async fn index() -> impl IntoResponse {
    Html(include_str!("web/index.html"))
}

/// 浏览器会无条件打 /favicon.ico；不能当仓库名进数据面。
pub async fn favicon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")],
        include_str!("web/favicon.svg"),
    )
}
