use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(feature = "official-api")]
use std::time::Instant;

#[cfg(feature = "official-api")]
use tomato_novel_official_api::{SearchClient, SearchError};
#[cfg(feature = "official-api")]
use tracing::{info, warn};

#[cfg(feature = "official-api")]
use crate::base_system::logging::redact_log_endpoints;
use crate::ui::web::state::AppState;

#[derive(Debug, Deserialize)]
pub(crate) struct SearchQuery {
    pub(crate) q: String,
}

pub(crate) async fn api_search(
    State(_state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    #[cfg(not(feature = "official-api"))]
    {
        let keyword = q.q.trim();
        if keyword.is_empty() {
            return Ok(Json(json!({"items": []})));
        }

        let mut all_items: Vec<Value> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        // 番茄搜索（unidbg 签名 sidecar 模式）
        {
            let signer_url = _state.config_view.unidbg_signer_url.trim();
            if !signer_url.is_empty() {
                let kw = keyword.to_string();
                let url = signer_url.to_string();
                let timeout_ms = {
                    let cfg = _state.config.lock().unwrap_or_else(|e| e.into_inner());
                    (cfg.request_timeout as u64).max(10) * 1000
                };
                match tokio::task::spawn_blocking(move || {
                    let client = crate::third_party::fq_api_client::FqApiClient::new(&url, timeout_ms)?;
                    client.search_books(&kw)
                })
                .await
                {
                    Ok(Ok(items)) => all_items.extend(items),
                    Ok(Err(e)) => errors.push(format!("番茄搜索失败: {e}")),
                    Err(_) => errors.push("番茄搜索任务执行失败".to_string()),
                }
            }
        }

        // 书旗（Shuqi）搜索
        #[cfg(feature = "shuqi")]
        {
            let kw = keyword.to_string();
            match tokio::task::spawn_blocking(move || {
                let client = crate::shuqi::ShuqiClient::new(15)?;
                crate::shuqi::search_items(&client, &kw)
            })
            .await
            {
                Ok(Ok(items)) => all_items.extend(items),
                Ok(Err(e)) => errors.push(format!("书旗搜索失败: {e}")),
                Err(_) => errors.push("书旗搜索任务执行失败".to_string()),
            }
        }

        if all_items.is_empty() && !errors.is_empty() {
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": errors.join("; ") })),
            ));
        }

        let mut resp = json!({ "items": all_items });
        if !errors.is_empty() {
            resp["warnings"] = json!(errors);
        }
        Ok(Json(resp))
    }

    #[cfg(feature = "official-api")]
    {
        let keyword = q.q.trim().to_string();
        if keyword.is_empty() {
            return Ok(Json(json!({"items": []})));
        }

        let started = Instant::now();

        // 并发限制：最多 2 个同时进行的上游 API 请求。
        let _permit =
            _state.api_semaphore.acquire().await.map_err(|_| {
                api_error(StatusCode::SERVICE_UNAVAILABLE, "上游 API 并发限制已关闭")
            })?;

        let keyword_for_log = keyword.clone();
        let resp = tokio::task::spawn_blocking(move || {
            let client = SearchClient::new()?;
            client.search_books(&keyword)
        })
        .await
        .map_err(|err| {
            warn!(
                target: "search",
                surface = "web",
                stage = "worker_join",
                query = %keyword_for_log,
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %err,
                error_debug = ?err,
                "搜索后台任务失败"
            );
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "搜索任务执行失败")
        })?
        .map_err(|err| {
            let error_kind = match &err {
                SearchError::Iid(_) => "iid",
                SearchError::Http(_) => "network",
            };
            warn!(
                target: "search",
                surface = "web",
                stage = "upstream_request",
                error_kind,
                query = %keyword_for_log,
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %err,
                error_debug = ?err,
                "搜索请求失败"
            );
            let safe_error = redact_log_endpoints(&err.to_string());
            api_error(StatusCode::BAD_GATEWAY, format!("搜索失败: {safe_error}"))
        })?;

        info!(
            target: "search",
            surface = "web",
            query = %keyword_for_log,
            result_count = resp.books.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "搜索请求完成"
        );

        let items: Vec<Value> = resp
            .books
            .into_iter()
            .map(|b| {
                json!({
                    "book_id": b.book_id,
                    "title": b.title,
                    "author": b.author,
                    "raw": b.raw,
                })
            })
            .collect();

        Ok(Json(json!({"items": items})))
    }
}

#[cfg(feature = "official-api")]
fn api_error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": message.into() })))
}
