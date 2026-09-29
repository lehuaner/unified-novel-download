use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::base_system::download_history::{read_download_history, read_download_history_deduped};
use crate::base_system::json_extract::to_public_jpeg_cover;
use crate::ui::web::state::AppState;

#[derive(Debug, Deserialize)]
pub(crate) struct HistoryQuery {
    pub(crate) limit: Option<usize>,
    pub(crate) q: Option<String>,
    /// `all` 返回全量原始记录（含同一 book_id 的多条历史）；默认为去重后的最新一条。
    pub(crate) view: Option<String>,
}

pub(crate) async fn api_history(
    State(_state): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Value>, StatusCode> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let keyword = q.q.as_deref();
    let mut items = if q.view.as_deref() == Some("all") {
        read_download_history(limit, keyword)
    } else {
        read_download_history_deduped(limit, keyword)
    };

    // 存档封面可能为签名 HEIC（列表卡片无法渲染），统一归一化为公网 JPEG。
    for it in &mut items {
        if !it.cover_url.trim().is_empty() {
            it.cover_url = to_public_jpeg_cover(&it.cover_url);
        }
    }

    Ok(Json(json!({
        "items": items,
        "limit": limit,
        "keyword": q.q,
    })))
}
