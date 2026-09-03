use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};

pub async fn index() -> impl IntoResponse {
    Html(include_str!("web/index.html"))
}

/// why: 控制面静态文件走显式白名单，避免 /ui/xxx 掉进数据面被当成仓库名。
pub async fn asset(Path(file): Path<String>) -> Response {
    match file.as_str() {
        "app.js" => js(include_str!("web/app.js")),
        "i18n.js" => js(include_str!("web/i18n.js")),
        "shell.css" => css(include_str!("web/shell.css")),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 浏览器会无条件打 /favicon.ico；不能当仓库名进数据面。
pub async fn favicon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")],
        include_str!("web/favicon.svg"),
    )
}

fn js(body: &'static str) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

fn css(body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], body).into_response()
}
