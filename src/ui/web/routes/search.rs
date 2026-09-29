use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ui::web::state::AppState;

#[derive(Debug, Deserialize)]
pub(crate) struct SearchQuery {
    pub(crate) q: String,
    /// 指定搜索源：`fanqie`、`shuqi`，留空表示全部。
    pub(crate) provider: Option<String>,
    /// 番茄分类 tab_type（1综合/3书籍/11短剧/2听书/19漫剧…），默认 1。
    pub(crate) tab: Option<i64>,
    /// 番茄筛选器选中项（逗号分隔的 selector_item_id，如 creation_status_end,word_num_lte30）。
    pub(crate) selected_items: Option<String>,
    /// 分页偏移（配合 has_more/next_offset 加载更多，仅番茄用）。
    pub(crate) offset: Option<usize>,
    /// 七猫/书旗页码（从 1 起，加载更多递增）。
    pub(crate) page: Option<usize>,
}

pub(crate) async fn api_search(
    State(_state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    {
        let keyword = q.q.trim();
        if keyword.is_empty() {
            return Ok(Json(json!({"items": []})));
        }

        let provider_filter = q.provider.as_deref().unwrap_or("").trim().to_lowercase();
        let want_fanqie =
            provider_filter.is_empty() || provider_filter == "all" || provider_filter == "fanqie";
        #[cfg(feature = "shuqi")]
        let want_shuqi =
            provider_filter.is_empty() || provider_filter == "all" || provider_filter == "shuqi";
        #[cfg(feature = "qimao")]
        let want_qimao =
            provider_filter.is_empty() || provider_filter == "all" || provider_filter == "qimao";
        #[cfg(all(not(feature = "shuqi"), not(feature = "qimao")))]
        let _ = provider_filter;

        let mut all_items: Vec<Value> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        // 两个源并发搜索：先各自 spawn_blocking，再统一 await，
        // 使总耗时从“番茄 + 书旗”降为“max(番茄, 书旗)”。

        // 番茄搜索（unidbg 签名 sidecar 模式，支持分类/筛选/分页）
        let tab = q.tab.unwrap_or(1).max(1);
        let selected_items = q.selected_items.clone();
        let offset = q.offset.unwrap_or(0);
        // 分页游标：番茄用 offset，书旗/七猫用 page（从 1 起）。
        let page = q.page.unwrap_or(1).max(1) as u32;
        let fanqie_handle = if want_fanqie {
            let signer_url = _state.config_view.unidbg_signer_url.trim();
            if !signer_url.is_empty() {
                let kw = keyword.to_string();
                let url = signer_url.to_string();
                let sel = selected_items.clone();
                let timeout_ms = {
                    let cfg = _state.config.lock().unwrap_or_else(|e| e.into_inner());
                    cfg.request_timeout.max(10) * 1000
                };
                Some(tokio::task::spawn_blocking(move || {
                    let client =
                        crate::third_party::fq_api_client::FqApiClient::new(&url, timeout_ms)?;
                    client.search_enriched(&kw, tab, sel.as_deref(), offset)
                }))
            } else {
                None
            }
        } else {
            None
        };

        // 书旗（Shuqi）搜索
        #[cfg(feature = "shuqi")]
        let shuqi_handle = if want_shuqi {
            let kw = keyword.to_string();
            let pg = page;
            Some(tokio::task::spawn_blocking(move || {
                let client = crate::shuqi::ShuqiClient::new(15)?;
                crate::shuqi::search_items(&client, &kw, pg)
            }))
        } else {
            None
        };

        // 七猫（Qimao）搜索
        #[cfg(feature = "qimao")]
        let qimao_handle = if want_qimao {
            let kw = keyword.to_string();
            let pg = page;
            let tb = Some(tab);
            let sel = selected_items.clone();
            Some(tokio::task::spawn_blocking(move || {
                let client = crate::qimao::QimaoClient::new(15)?;
                crate::qimao::search_items(&client, &kw, pg, tb, sel.as_deref())
            }))
        } else {
            None
        };

        let mut fanqie_meta: Option<Value> = None;
        #[cfg(feature = "shuqi")]
        let mut shuqi_more = false;
        #[cfg(feature = "qimao")]
        let mut qimao_more = false;
        if let Some(handle) = fanqie_handle {
            match handle.await {
                Ok(Ok(resp)) => {
                    if let Some(arr) = resp.get("items").and_then(|v| v.as_array()) {
                        all_items.extend(arr.iter().cloned());
                    }
                    fanqie_meta = Some(resp);
                }
                Ok(Err(e)) => errors.push(format!("番茄搜索失败: {e}")),
                Err(_) => errors.push("番茄搜索任务执行失败".to_string()),
            }
        }

        #[cfg(feature = "shuqi")]
        if let Some(handle) = shuqi_handle {
            match handle.await {
                Ok(Ok((items, more))) => {
                    shuqi_more = more;
                    all_items.extend(items);
                }
                Ok(Err(e)) => errors.push(format!("书旗搜索失败: {e}")),
                Err(_) => errors.push("书旗搜索任务执行失败".to_string()),
            }
        }

        #[cfg(feature = "qimao")]
        if let Some(handle) = qimao_handle {
            match handle.await {
                Ok(Ok((items, more))) => {
                    qimao_more = more;
                    all_items.extend(items);
                }
                Ok(Err(e)) => errors.push(format!("七猫搜索失败: {e}")),
                Err(_) => errors.push("七猫搜索任务执行失败".to_string()),
            }
        }

        if all_items.is_empty() && !errors.is_empty() {
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": errors.join("; ") })),
            ));
        }

        // 规范化：为每个搜索结果补齐 封面 / 简介 / 评分（供前端卡片展示，避免二次请求）
        let mut no_cover_keys: Vec<Vec<String>> = Vec::new();
        for item in &mut all_items {
            let raw = item.get("raw").cloned().unwrap_or(Value::Null);
            let mut got_cover = false;
            if let Some(obj) = raw.as_object() {
                use crate::base_system::json_extract::{
                    collect_maps, pick_cover, pick_description, pick_score,
                };
                let maps = collect_maps(&raw);
                let mut cover_url = pick_cover(obj);
                let mut description = None;
                let mut score = None;
                for m in maps {
                    if cover_url.is_none() {
                        cover_url = pick_cover(m);
                    }
                    if description.is_none() {
                        description = pick_description(m);
                    }
                    if score.is_none() {
                        score = pick_score(m);
                    }
                }
                // pick_cover 现在只返回绝对 http(s) URL；若无绝对封面则显式置空，
                // 覆盖掉 fq_api_client 可能写入的相对 coverUrl，避免前端代理 400/图裂。
                // 再统一把番茄/抖音图床的 HEIC 封面归一化为公网 JPEG（浏览器可渲染）。
                match cover_url {
                    Some(url) => {
                        let url = crate::base_system::json_extract::to_public_jpeg_cover(&url);
                        item["cover_url"] = json!(url);
                        got_cover = true;
                    }
                    None => item["cover_url"] = Value::Null,
                }
                if let Some(d) = description {
                    item["description"] = json!(d);
                }
                if let Some(s) = score {
                    item["score"] = json!(s);
                }
            } else if let Some(s) = raw.as_str() {
                // 书旗（Shuqi）：raw 即简介字符串，直接作为 description。
                if !s.trim().is_empty() {
                    item["description"] = json!(s);
                }
            }
            if !got_cover && let Some(obj) = item.get("raw").and_then(|r| r.as_object()) {
                no_cover_keys.push(obj.keys().cloned().collect());
            }
        }
        if !no_cover_keys.is_empty() {
            tracing::warn!(
                target: "search",
                no_cover = no_cover_keys.len(),
                sample_keys = ?no_cover_keys.first(),
                "部分搜索结果未提取到绝对封面 URL（可能上游返回了相对路径或字段名未覆盖，需扩展 json_extract::pick_cover 候选）"
            );
        }

        let mut resp = json!({ "items": all_items });
        if !errors.is_empty() {
            resp["warnings"] = json!(errors);
        }
        // 逐源自报能力（分类 tab / 筛选器 / 分页游标）：前端按「当前勾选源的并集」重建工具栏，
        // 故每个参与源都要写自己那一份，不能只在别的源缺席时才回退给下一个源。
        {
            let mut pm = serde_json::Map::new();
            if let Some(meta) = &fanqie_meta {
                let mut m = meta.clone();
                if let Some(o) = m.as_object_mut() {
                    o.remove("items"); // items 已在 all_items 里，不重复下发
                }
                pm.insert("fanqie".to_string(), m);
            }
            #[cfg(feature = "qimao")]
            if want_qimao {
                pm.insert(
                    "qimao".to_string(),
                    json!({
                        // 七猫实测有分类 tab（公共编号 1综合/2听书/3书籍，详见 qimao_tabs 文档）。
                        "tabs": crate::qimao::qimao_tabs(),
                        "selector": crate::qimao::qimao_selector(),
                        "has_more": qimao_more,
                        "next_offset": 0,
                        "tab_type": tab,
                    }),
                );
            }
            #[cfg(feature = "shuqi")]
            if want_shuqi {
                pm.insert(
                    "shuqi".to_string(),
                    json!({ "tabs": [], "selector": Value::Null, "has_more": shuqi_more }),
                );
            }
            resp["provider_meta"] = Value::Object(pm);
        }
        // 以下为旧前端的兼容字段（顶层只有一份 tabs/selector，取番茄优先、否则七猫）。
        // 新前端一律改用 provider_meta 做并集，本块仅保留向后兼容。
        // 番茄直连时附带分类/筛选/分页元数据，供前端构建工具栏。
        if let Some(meta) = &fanqie_meta {
            resp["tabs"] = meta.get("tabs").cloned().unwrap_or_else(|| json!([]));
            resp["selector"] = meta.get("selector").cloned().unwrap_or(Value::Null);
            // 不能改成 unwrap_or_default()：Value::default() 是 null，会改变“缺省无下一页”的语义。
            resp["has_more"] = meta.get("has_more").cloned().unwrap_or(Value::Bool(false));
            resp["next_offset"] = meta.get("next_offset").cloned().unwrap_or_else(|| json!(0));
            resp["tab_type"] = meta.get("tab_type").cloned().unwrap_or_else(|| json!(1));
        } else {
            // 番茄未参与（如仅选七猫）时，回退用七猫筛选器（同构 selector），供前端复用同一套分类/筛选按钮。
            #[cfg(feature = "qimao")]
            if want_qimao {
                resp["tabs"] = json!([]);
                resp["selector"] = crate::qimao::qimao_selector();
                resp["has_more"] = json!(false);
                resp["next_offset"] = json!(0);
                resp["tab_type"] = json!(tab);
            }
        }
        // 各 provider 的“是否还有下一页”，供前端按平台加载更多（番茄/七猫/书旗均可翻页）。
        {
            let mut phm = serde_json::Map::new();
            if let Some(m) = &fanqie_meta {
                phm.insert(
                    "fanqie".into(),
                    m.get("has_more").cloned().unwrap_or(Value::Bool(false)),
                );
            }
            #[cfg(feature = "qimao")]
            if want_qimao {
                phm.insert("qimao".into(), json!(qimao_more));
            }
            #[cfg(feature = "shuqi")]
            if want_shuqi {
                phm.insert("shuqi".into(), json!(shuqi_more));
            }
            if !phm.is_empty() {
                resp["provider_has_more"] = json!(phm);
            }
        }
        Ok(Json(resp))
    }
}
