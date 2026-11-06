use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::Response;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::ui::web::state::AppState;

#[derive(Debug, Deserialize)]
pub(crate) struct CoverQuery {
    pub(crate) url: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BookCoverQuery {
    pub(crate) book_id: String,
}

/// 按 book_id 懒加载封面：来源站搜索页大多没有封面，这里按需拉一次详情页取封面，
/// 再 302 到 `/api/search-cover` 走统一代理/压缩。前端只会在无封面卡片进入视口时调用。
pub(crate) async fn api_book_cover(
    Query(q): Query<BookCoverQuery>,
) -> Result<Response, StatusCode> {
    let book_id = q.book_id.trim().to_string();
    if book_id.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let cover = tokio::task::spawn_blocking(move || {
        crate::provider::owner_of(&book_id)
            .and_then(|p| p.preview_summary(&book_id, 8))
            .and_then(|m| m.cover_url)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::NOT_FOUND)?;
    let proxied = format!("/api/search-cover?url={}", percent_encode(&cover));
    Ok(build_redirect_response(&proxied))
}

#[derive(Debug, Deserialize)]
pub(crate) struct OfficialCoverQuery {
    pub(crate) official_id: String,
}

/// 按官网 official_id 懒加载封面：判定为晋江的条目封面必须来自官网，
/// 不能用下载源封面顶替（设计约定 §7.4）。
pub(crate) async fn api_official_cover(
    Query(q): Query<OfficialCoverQuery>,
) -> Result<Response, StatusCode> {
    let official_id = q.official_id.trim().to_string();
    if official_id.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let cover = tokio::task::spawn_blocking(move || {
        crate::official::find("jjwxc")
            .and_then(|src| src.fetch_detail(&official_id, 8).ok())
            .and_then(|detail| detail.cover_url)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::NOT_FOUND)?;
    let proxied = format!("/api/search-cover?url={}", percent_encode(&cover));
    Ok(build_redirect_response(&proxied))
}
/// 只编码非 unreserved 字符，用于把封面 URL 放进 query 参数。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 搜索结果封面缩略图端点。
///
/// 流程：下载远程封面 → 压缩为 JPEG 缩略图（max 120px，quality 70）→ 内存缓存 → 返回。
/// 不写入服务器磁盘。
pub(crate) async fn api_search_cover(
    State(state): State<AppState>,
    Query(q): Query<CoverQuery>,
) -> Result<Response, StatusCode> {
    let url = q.url.trim();
    if url.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // 用 URL 的 SHA-256 摘要作为缓存键
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    let cache_key = hex::encode(hasher.finalize());

    // 命中缓存
    if let Some(cached) = state.cover_cache.get(&cache_key) {
        return Ok(build_jpeg_response(&cached));
    }

    // 下载远程封面
    let url_owned = url.to_string();
    let bytes = tokio::task::spawn_blocking(move || {
        let timeout = std::time::Duration::from_millis(8_000);
        crate::third_party::media_fetch::fetch_bytes(&url_owned, timeout)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::BAD_GATEWAY)?;

    if bytes.is_empty() {
        return Err(StatusCode::BAD_GATEWAY);
    }

    // 压缩为 JPEG 缩略图（max 120px 宽，quality 70）
    let thumb = crate::book_parser::image_utils::try_convert_to_jpeg(&bytes, 70, 120);

    match thumb {
        Some(thumb) => {
            // 存入内存缓存并返回
            let cached = state.cover_cache.put(cache_key, thumb);
            Ok(build_jpeg_response(&cached))
        }
        None => {
            // 无法解码（如 HEIC/AVIF），302 重定向到原始 URL，让浏览器直接处理
            Ok(build_redirect_response(url))
        }
    }
}

fn build_jpeg_response(data: &[u8]) -> Response {
    let mut resp = Response::new(Body::from(data.to_vec()));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    resp
}

/// 对无法服务端转换的格式（HEIC/AVIF），302 重定向到原始 URL。
fn build_redirect_response(url: &str) -> Response {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::FOUND;
    if let Ok(val) = HeaderValue::from_str(url) {
        resp.headers_mut().insert(header::LOCATION, val);
    }
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    resp
}
