use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::base_system::search_history::{
    clear_search_history, push_search_history, read_search_history, remove_search_history,
};
use crate::ui::web::state::AppState;

#[derive(Debug, Deserialize)]
pub(crate) struct SearchHistoryQuery {
    /// 删除指定关键词时携带；为空表示清空全部。
    pub(crate) keyword: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SearchHistoryBody {
    pub(crate) keyword: String,
}

pub(crate) async fn api_search_history_list(State(_state): State<AppState>) -> Json<Value> {
    Json(json!({ "items": read_search_history() }))
}

pub(crate) async fn api_search_history_add(
    State(_state): State<AppState>,
    Json(body): Json<SearchHistoryBody>,
) -> Json<Value> {
    let items = push_search_history(&body.keyword);
    Json(json!({ "items": items }))
}

pub(crate) async fn api_search_history_delete(
    State(_state): State<AppState>,
    Query(q): Query<SearchHistoryQuery>,
) -> Json<Value> {
    let items = match q.keyword.as_deref() {
        Some(kw) if !kw.trim().is_empty() => remove_search_history(kw),
        _ => {
            clear_search_history();
            Vec::new()
        }
    };
    Json(json!({ "items": items }))
}
