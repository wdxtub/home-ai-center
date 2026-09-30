//! 前端静态资源托管。
//!
//! 镜像里内嵌 `web/dist`，单容器即可提供管理台——不需要 Nginx，
//! 也不需要额外拷文件。开发时用 Vite dev server 代理到后端。

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use include_dir::{include_dir, Dir};

/// 编译期内嵌。目录不存在时 `include_dir!` 会 panic——
/// 这是**刻意的**：镜像里没有前端说明构建漏了步，应该当场失败。
static ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../web/dist");

/// SPA 回退：任何未匹配的路径都交给前端路由处理。
pub async fn serve(req: Request) -> Response {
    let uri = req.uri().clone();
    let path = uri.path().trim_start_matches('/');

    let candidate = if path.is_empty() { "index.html" } else { path };

    match ASSETS.get_file(candidate) {
        Some(f) => {
            let mime = mime_of(candidate);
            let is_html = mime == "text/html; charset=utf-8";
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                // index.html 绝不能缓存：否则发版后用户一直拿旧的资源清单
                .header(
                    header::CACHE_CONTROL,
                    if is_html { "no-cache" } else { "public, max-age=31536000, immutable" },
                )
                .body(Body::from(f.contents().to_vec()))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        // 未知路径且**没有扩展名** → SPA 路由（/nodes/3 之类）
        None if !candidate.contains('.') => index().into_response(),
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "not found",
        )
            .into_response(),
    }
}

fn index() -> Response {
    match ASSETS.get_file("index.html") {
        Some(f) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(f.contents().to_vec()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "管理台未构建：镜像里缺少 web/dist",
        )
            .into_response(),
    }
}

fn mime_of(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "map" => "application/json; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// 镜像里是否真的带了前端。仅用于 `/api/health` 的自检输出。
pub fn has_frontend() -> bool {
    ASSETS.get_file("index.html").is_some()
}
