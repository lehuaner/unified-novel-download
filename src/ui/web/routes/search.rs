use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ui::web::state::AppState;

/// 单个外部源的并发搜索任务句柄（spawn_blocking 内构造 Client 并执行搜索）。
type ProviderSearchHandle = tokio::task::JoinHandle<anyhow::Result<(Vec<Value>, bool)>>;

/// 单次搜索请求的硬上限（与番茄外层超时一致）：本请求内的源必须在此时间内返回，
/// 未返回的源直接跳过。前端把「番茄」与「其余源」拆成两路并发请求，番茄返回即渲染，
/// 其余源后台继续、完成后增量补显。
const SEARCH_DEADLINE_SECS: u64 = 10;
const SEARCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(SEARCH_DEADLINE_SECS);

/// 免费平台（直接展示、可下载）。
const FREE_PROVIDERS: &[&str] = &["fanqie", "shuqi", "qimao"];
/// 资源站（不再作为平台展示；仅作为付费平台池成员参与聚合）。
const RESOURCE_PROVIDERS: &[&str] = &[
    "zishiba",
    "hen0",
    "guanshuya",
    "biqubee",
    "biquga",
    "dongtan",
];
/// 付费逻辑平台（池 + 官方源）。
const PAID_PLATFORMS: &[&str] = &["jinjiang"];

#[derive(Debug, Deserialize)]
pub(crate) struct SearchQuery {
    pub(crate) q: String,
    /// 指定搜索源（兼容旧前端/脚本）：`fanqie`、`shuqi`、`qimao`、`jinjiang`。
    pub(crate) provider: Option<String>,
    /// 批量指定搜索源（新前端一次请求）：逗号分隔。
    pub(crate) providers: Option<String>,
    /// 番茄分类 tab_type（1综合/3书籍/11短剧/2听书/19漫剧…），默认 1。
    pub(crate) tab: Option<i64>,
    /// 番茄筛选器选中项（逗号分隔的 selector_item_id）。
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
    let keyword = q.q.trim();
    if keyword.is_empty() {
        return Ok(Json(json!({"items": []})));
    }

    // 解析请求的源集合：provider / providers 二选一；空 = 全部逻辑平台。
    let mut requested: Vec<String> = Vec::new();
    if let Some(list) = q.providers.as_deref() {
        requested.extend(list.split(',').map(|s| s.trim().to_lowercase()));
    } else if let Some(one) = q.provider.as_deref() {
        requested.extend(one.split(',').map(|s| s.trim().to_lowercase()));
    }
    requested.retain(|s| !s.is_empty());
    let all = requested.is_empty() || requested.iter().any(|s| s == "all");
    let want = |id: &str| all || requested.iter().any(|s| s == id);
    let want_jinjiang = all || PAID_PLATFORMS.iter().any(|p| want(p));

    let tab = q.tab.unwrap_or(1).max(1);
    let selected_items = q.selected_items.clone();
    let offset = q.offset.unwrap_or(0);
    // 分页游标：番茄用 offset，外部源用 page（从 1 起）。
    let page = q.page.unwrap_or(1).max(1) as u32;
    let (timeout_secs, gs_cookie, gs_user_agent) = {
        let cfg = _state.config.lock().unwrap_or_else(|e| e.into_inner());
        (
            cfg.request_timeout.max(10),
            cfg.guanshuya_cookie.trim().to_string(),
            cfg.guanshuya_user_agent.trim().to_string(),
        )
    };

    // ── 并发发起各物理源搜索 ────────────────────────────────────
    // 注意：请求付费平台时也要搜免费三平台，用于"与免费平台重合则隐藏"的去重；
    // 但只有用户勾选的免费平台会进入展示列表。
    let mut handles: Vec<(&'static str, ProviderSearchHandle)> = Vec::new();
    // 本轮的统一截止时间：番茄、免费源、资源站、晋江聚合共享，绝不串行叠加。
    let deadline = tokio::time::Instant::now() + SEARCH_DEADLINE;

    // 番茄（unidbg 签名 sidecar 模式，支持分类/筛选/分页）。
    // 注意：只为「用户勾选」的番茄发起搜索；晋江去重只使用已勾选免费平台的结果，
    // 避免未勾选番茄时仍被其签名服务超时拖慢整轮搜索。
    let need_fanqie = want("fanqie");
    let fanqie_handle: Option<tokio::task::JoinHandle<anyhow::Result<Value>>> = if need_fanqie {
        let signer_url = _state.config_view.unidbg_signer_url.trim();
        if signer_url.is_empty() {
            None
        } else if !signer_reachable(signer_url) {
            // 仅记录日志，不塞进 warnings：每次搜索都弹提示会干扰用户。
            tracing::debug!(target: "search", "番茄签名服务不可达，本轮跳过番茄");
            None
        } else {
            let kw = keyword.to_string();
            let url = signer_url.to_string();
            let sel = selected_items.clone();
            let timeout_ms = timeout_secs * 1000;
            Some(tokio::task::spawn_blocking(move || {
                let client = crate::third_party::fq_api_client::FqApiClient::new(&url, timeout_ms)?;
                client.search_enriched(&kw, tab, sel.as_deref(), offset)
            }))
        }
    } else {
        None
    };

    // 外部源：只搜索用户勾选的源（加资源站旧客户端显式请求的兼容）。
    for provider in crate::provider::providers() {
        let pid = provider.id();
        if !want(pid) {
            continue;
        }
        let mut opts = crate::provider::SearchOptions::new(
            keyword,
            page,
            timeout_secs.min(SEARCH_DEADLINE_SECS),
        );
        opts.tab = Some(tab);
        opts.selected_items = selected_items.clone();
        opts.cookie = (!gs_cookie.is_empty()).then(|| gs_cookie.clone());
        opts.user_agent = (!gs_user_agent.is_empty()).then(|| gs_user_agent.clone());
        handles.push((
            pid,
            tokio::task::spawn_blocking(move || provider.search(&opts)),
        ));
    }

    // ── 收集结果 ──────────────────────────────────────────────
    let mut fanqie_meta: Option<Value> = None;
    let mut more_map: std::collections::HashMap<&'static str, bool> =
        std::collections::HashMap::new();
    let mut results: std::collections::HashMap<&'static str, Vec<Value>> =
        std::collections::HashMap::new();

    if let Some(handle) = fanqie_handle {
        // 外层超时兜底：签名客户端内部可能重试，避免拖住整轮搜索。
        match tokio::time::timeout_at(deadline, handle).await {
            Ok(Ok(Ok(resp))) => {
                let items = resp
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                results.insert("fanqie", items);
                fanqie_meta = Some(resp);
            }
            Ok(Ok(Err(e))) => {
                tracing::warn!(target: "search", error = %e, "番茄搜索失败（仅日志）")
            }
            Ok(Err(_)) => tracing::warn!(target: "search", "番茄搜索任务执行失败（仅日志）"),
            Err(_) => tracing::warn!(target: "search", "番茄搜索超时，已跳过（仅日志）"),
        }
    }

    for (pid, handle) in handles {
        let display_name = crate::provider::find(pid)
            .map(|p| p.display_name())
            .unwrap_or(pid);
        match tokio::time::timeout_at(deadline, handle).await {
            Ok(Ok(Ok((items, more)))) => {
                more_map.insert(pid, more);
                results.insert(pid, items);
            }
            Ok(Ok(Err(e))) => {
                tracing::warn!(target: "search", provider = pid, display = display_name, error = %e, "书源搜索失败（仅日志）")
            }
            Ok(Err(_)) => {
                tracing::warn!(target: "search", provider = pid, display = display_name, "书源搜索任务执行失败（仅日志）")
            }
            Err(_) => {
                tracing::info!(target: "search", provider = pid, "搜索超过总时长上限，本轮跳过");
            }
        }
    }

    // ── 组装展示条目（只含用户勾选的源）────────────────────────
    let mut all_items: Vec<Value> = Vec::new();
    for id in FREE_PROVIDERS {
        if want(id)
            && let Some(items) = results.get(id)
        {
            all_items.extend(items.iter().cloned());
        }
    }
    // 旧客户端可能显式请求资源站：保留能力，但展示为"其他"，不再暴露资源站平台。
    for id in RESOURCE_PROVIDERS {
        if want(id)
            && let Some(items) = results.get(id)
        {
            all_items.extend(items.iter().cloned());
        }
    }

    // ── 付费平台聚合（晋江）────────────────────────────────────
    let mut official_hits = 0usize;
    if want_jinjiang {
        // 免费三平台结果（含未勾选源）作为去重基准。
        let free_snapshot: Vec<Value> = FREE_PROVIDERS
            .iter()
            .flat_map(|id| results.get(*id).cloned().unwrap_or_default())
            .collect();
        let kw = keyword.to_string();
        let platform = PAID_PLATFORMS[0].to_string();
        let cookie = (!gs_cookie.is_empty()).then(|| gs_cookie.clone());
        let ua = (!gs_user_agent.is_empty()).then(|| gs_user_agent.clone());
        let outcome = tokio::task::spawn_blocking(move || {
            crate::aggregate::search(
                &platform,
                &kw,
                page,
                SEARCH_DEADLINE_SECS,
                &free_snapshot,
                cookie.as_deref(),
                ua.as_deref(),
            )
        });
        // 聚合层内部已用「官方源 + 池成员共享 10s 预算」自行截断并返回部分结果；
        // 外层仅保留异常容错（正常不会触达），避免再次把已完成结果整块丢掉。
        let aggregate_deadline = deadline + std::time::Duration::from_secs(3);
        match tokio::time::timeout_at(aggregate_deadline, outcome).await {
            Ok(Ok(o)) => {
                official_hits = o.official_hits;
                for w in &o.warnings {
                    tracing::warn!(target: "search", "聚合层: {w}");
                }
                all_items.extend(o.items);
            }
            Ok(Err(_)) => {
                tracing::warn!(target: "search", "晋江聚合搜索任务执行失败（仅日志）");
            }
            Err(_) => {
                tracing::info!(target: "search", "晋江聚合超过总时长上限，本轮跳过");
            }
        }
    }

    // 规范化：为每个搜索结果补齐 封面 / 简介 / 评分（供前端卡片展示，避免二次请求）
    let mut no_cover_keys: Vec<Vec<String>> = Vec::new();
    for item in &mut all_items {
        // 已被目标平台官方元数据覆盖的条目：不要再用下载源的 raw 字段回写
        // （否则资源站封面/简介会盖掉官方数据）。
        if item.get("meta_source").and_then(Value::as_str) == Some("official") {
            continue;
        }
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
            // 书旗（Shuqi）/ 姿势吧：raw 即简介字符串，直接作为 description。
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

    // 统一标注展示平台：聚合层已标注的条目保持原样；
    // 其余按 book_id 前缀映射（资源站 zs:/bx:/gs: 一律显示"其他"）。
    for item in &mut all_items {
        if item.get("display_platform").is_none() {
            let bid = item.get("book_id").and_then(Value::as_str).unwrap_or("");
            item["display_platform"] = json!(display_of_book_id(bid));
        }
        if item.get("_provider").is_none() {
            let dp = item
                .get("display_platform")
                .and_then(Value::as_str)
                .unwrap_or("fanqie")
                .to_string();
            item["_provider"] = json!(dp);
        }
    }

    // ── 能力元数据 / 翻页标记 ──────────────────────────────────
    let mut resp = json!({ "items": all_items });

    {
        let mut pm = serde_json::Map::new();
        if want("fanqie")
            && let Some(meta) = &fanqie_meta
        {
            let mut m = meta.clone();
            if let Some(o) = m.as_object_mut() {
                o.remove("items");
            }
            pm.insert("fanqie".to_string(), m);
        }
        for id in ["shuqi", "qimao"] {
            if !want(id) {
                continue;
            }
            if let Some(p) = crate::provider::find(id) {
                let has_more = more_map.get(id).copied().unwrap_or(false);
                pm.insert(id.to_string(), p.search_meta(has_more, tab));
            }
        }
        if want_jinjiang {
            pm.insert(
                "jinjiang".to_string(),
                json!({
                    "tabs": [],
                    "selector": Value::Null,
                    "has_more": false,
                    "official_hits": official_hits,
                    "pool": crate::pool::snapshot(),
                }),
            );
        }
        resp["provider_meta"] = Value::Object(pm);
    }

    // 旧前端兼容字段（顶层 tabs/selector 取番茄，其次七猫）。
    if want("fanqie")
        && let Some(meta) = &fanqie_meta
    {
        resp["tabs"] = meta.get("tabs").cloned().unwrap_or_else(|| json!([]));
        resp["selector"] = meta.get("selector").cloned().unwrap_or(Value::Null);
        resp["has_more"] = meta.get("has_more").cloned().unwrap_or(Value::Bool(false));
        resp["next_offset"] = meta.get("next_offset").cloned().unwrap_or_else(|| json!(0));
        resp["tab_type"] = meta.get("tab_type").cloned().unwrap_or_else(|| json!(1));
    } else if want("qimao")
        && let Some(qimao) = crate::provider::find("qimao")
    {
        resp["tabs"] = json!([]);
        resp["selector"] = qimao.selector();
        resp["has_more"] = json!(false);
        resp["next_offset"] = json!(0);
        resp["tab_type"] = json!(tab);
    }

    {
        let mut phm = serde_json::Map::new();
        if want("fanqie")
            && let Some(m) = &fanqie_meta
        {
            phm.insert(
                "fanqie".into(),
                m.get("has_more").cloned().unwrap_or(Value::Bool(false)),
            );
        }
        for id in ["shuqi", "qimao"] {
            if !want(id) {
                continue;
            }
            let has_more = more_map.get(id).copied().unwrap_or(false);
            phm.insert(id.into(), json!(has_more));
        }
        if want_jinjiang {
            phm.insert("jinjiang".into(), json!(false));
        }
        if !phm.is_empty() {
            resp["provider_has_more"] = json!(phm);
        }
    }

    Ok(Json(resp))
}

/// 番茄签名 sidecar 可达性探测（TCP 连接，600ms 上限，结果缓存 15s）。
///
/// 目的：sidecar 未启动时快速跳过番茄，避免其内部重试把整轮搜索拖到十几秒。
fn signer_reachable(signer_url: &str) -> bool {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    static CACHE: OnceLock<Mutex<Option<(Instant, bool)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let now = Instant::now();
    if let Ok(guard) = cache.lock()
        && let Some((at, ok)) = *guard
        && now.duration_since(at) < Duration::from_secs(15)
    {
        return ok;
    }

    let reachable = {
        use std::net::{TcpStream, ToSocketAddrs};
        let rest = signer_url.trim();
        let rest = rest
            .strip_prefix("http://")
            .or_else(|| rest.strip_prefix("https://"))
            .unwrap_or(rest);
        let authority = rest.split('/').next().unwrap_or("");
        if authority.is_empty() {
            false
        } else {
            let addr = if authority.contains(':') {
                authority.to_string()
            } else {
                format!("{authority}:80")
            };
            match addr.to_socket_addrs() {
                Ok(addrs) => addrs
                    .into_iter()
                    .any(|a| TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok()),
                Err(_) => false,
            }
        }
    };

    if let Ok(mut guard) = cache.lock() {
        *guard = Some((now, reachable));
    }
    reachable
}

/// book_id 前缀 → 逻辑展示平台。
fn display_of_book_id(book_id: &str) -> &'static str {
    if book_id.starts_with("sq:") {
        "shuqi"
    } else if book_id.starts_with("qm:") {
        "qimao"
    } else if book_id.starts_with("zs:")
        || book_id.starts_with("bx:")
        || book_id.starts_with("gs:")
        || book_id.starts_with("bb:")
        || book_id.starts_with("bg:")
        || book_id.starts_with("dt:")
    {
        "other"
    } else {
        "fanqie"
    }
}

#[cfg(test)]
mod tests {
    use super::display_of_book_id;

    #[test]
    fn book_id_maps_to_display_platform() {
        assert_eq!(display_of_book_id("sq:1"), "shuqi");
        assert_eq!(display_of_book_id("qm:1"), "qimao");
        assert_eq!(display_of_book_id("zs:1"), "other");
        assert_eq!(display_of_book_id("bx:1"), "other");
        assert_eq!(display_of_book_id("gs:1"), "other");
        assert_eq!(display_of_book_id("bb:1"), "other");
        assert_eq!(display_of_book_id("bg:1_2"), "other");
        assert_eq!(display_of_book_id("dt:1"), "other");
        assert_eq!(display_of_book_id("7496026299845053465"), "fanqie");
    }
}
