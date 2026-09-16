use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::ui::web::state::AppState;

#[derive(Debug, Deserialize)]
pub(crate) struct CoverQuery {
    pub(crate) url: String,
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
    let thumb = crate::book_parser::image_utils::try_convert_to_jpeg(&bytes, 70, 120)
        .ok_or(StatusCode::UNSUPPORTED_MEDIA_TYPE)?;

    // 存入内存缓存并返回
    let cached = state.cover_cache.put(cache_key, thumb);
    Ok(build_jpeg_response(&cached))
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
