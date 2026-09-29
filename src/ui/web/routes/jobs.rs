use std::sync::atomic::Ordering;
use std::thread;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::base_system::book_id::resolve_book_id;
use crate::base_system::json_extract::to_public_jpeg_cover;
use crate::download::downloader as dl;
use crate::ui::web::state::{
    AppState, JobBookMeta, JobInfo, JobState, RECENT_DONE_JOB_RETENTION_MS, jobs_epoch_ms,
};

#[derive(Debug, Deserialize)]
pub(crate) struct ListJobsQuery {
    /// 按 job id 精确过滤
    pub(crate) id: Option<u64>,
    /// 按书名/书ID关键词模糊过滤（忽略大小写）
    pub(crate) name: Option<String>,
    /// 返回全部任务；默认会自动隐藏/清理 2 小时前已完成的任务。
    pub(crate) all: Option<bool>,
    /// 增量轮询游标：仅返回 updated_ms 严格大于该值的变化项 + 该时刻后被移除的 id。
    /// 缺省时等价于 0（首次全量同步，但仍只发精简动态字段）。
    pub(crate) since: Option<u64>,
    /// true 时返回完整任务对象（含 book_id/title/author/meta 静态数据），供单任务配置弹窗等场景。
    pub(crate) full: Option<bool>,
}

/// 动态视图（轮询反复传输的部分）：只含身份/状态/进度/待配置项，不带书籍元数据。
/// book_id 属于任务身份（同书去重、“更新中”判定、无名时回退真实标识都要它），
/// 固定 8~22 字节；真正占体积的书名/作者/封面/简介走 /api/jobs/meta 一次性拉取。
fn slim_job_json(j: &JobInfo) -> Value {
    let mut o = json!({
        "id": j.id,
        "book_id": j.book_id,
        "state": j.state,
        "updated_ms": j.updated_ms,
        // 上游元数据是否已就绪：true 才值得让前端去 /api/jobs/meta 取一次（且只取一次）。
        "has_meta": j.meta_ready,
    });
    // 无值就不占字节：避免稳态轮询每 1.5s 拖一串 null 键。
    if let Some(ref p) = j.progress {
        o["progress"] =
            json!({ "saved_chapters": p.saved_chapters, "chapter_total": p.chapter_total });
    }
    if let Some(ref msg) = j.message {
        o["message"] = json!(msg);
    }
    if j.book_name_options.as_ref().is_some_and(|v| !v.is_empty()) {
        o["book_name_options"] = json!(j.book_name_options);
    }
    if j.format_options.as_ref().is_some_and(|v| !v.is_empty()) {
        o["format_options"] = json!(j.format_options);
    }
    o
}

/// 完整视图（静态 + 动态）：仅单次请求使用，不进 1.5s 轮询通道。
fn full_job_json(j: &JobInfo) -> Value {
    let mut o = slim_job_json(j);
    o["created_ms"] = json!(j.created_ms);
    o["title"] = json!(j.title);
    o["author"] = json!(j.author);
    o["meta"] = json!(j.meta);
    o
}

fn trimmed(s: &Option<String>) -> Option<String> {
    let v = s.as_deref().unwrap_or("").trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// 本任务 prepare 出的上游元数据 → 任务静态视图（只取自身字段，不跨接口补分）。
fn meta_from_plan(m: &dl::BookMeta) -> JobBookMeta {
    let cover = m
        .cover_url
        .as_deref()
        .or(m.detail_cover_url.as_deref())
        .map(str::trim)
        .filter(|u| u.starts_with("http://") || u.starts_with("https://") || u.starts_with('/'))
        .map(to_public_jpeg_cover)
        .filter(|u| !u.is_empty());
    JobBookMeta {
        cover_url: cover,
        description: trimmed(&m.description),
        category: trimmed(&m.category),
        word_count: m.word_count.filter(|n| *n > 0),
        chapter_count: m.chapter_count.filter(|n| *n > 0),
        score: m.score,
        finished: m.finished,
        tags: m.tags.clone(),
        read_count_text: trimmed(&m.read_count_text),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateJobReq {
    pub(crate) book_id: String,
    pub(crate) range_start: Option<usize>,
    pub(crate) range_end: Option<usize>,
    /// 搜索/预览卡片携带的封面图 URL（优先于上游 web 拓取，供下载库展示）。
    #[serde(default)]
    pub(crate) cover_url: Option<String>,
}

pub(crate) async fn list_jobs(
    State(state): State<AppState>,
    Query(q): Query<ListJobsQuery>,
) -> Json<Value> {
    // 精确单查/关键词查：直接给完整对象（调用方需要 options/元数据，不关心 payload 体积）。
    let want_full = q.full.unwrap_or(false) || q.id.is_some() || q.name.is_some();
    if q.id.is_none() && q.name.is_none() && !q.all.unwrap_or(false) {
        state
            .jobs
            .prune_done_older_than(RECENT_DONE_JOB_RETENTION_MS);
    }

    if want_full {
        let items: Vec<Value> = state
            .jobs
            .list()
            .iter()
            .filter(|j| q.id.is_none_or(|id| j.id == id))
            .filter(|j| {
                q.name.as_deref().is_none_or(|kw| {
                    let kw_lower = kw.to_lowercase();
                    j.title
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&kw_lower)
                        || j.book_id.to_lowercase().contains(&kw_lower)
                })
            })
            .map(full_job_json)
            .collect();
        return Json(json!({
            "items": items,
            "mode": "full",
            "epoch_ms": jobs_epoch_ms(),
            "done_retention_ms": RECENT_DONE_JOB_RETENTION_MS,
        }));
    }

    let sync = state.jobs.sync_since(q.since.unwrap_or(0));
    let items: Vec<Value> = sync.changed.iter().map(slim_job_json).collect();
    Json(json!({
        "items": items,
        "removed": sync.removed_ids,
        "cursor": sync.cursor,
        "epoch_ms": jobs_epoch_ms(),
        "done_retention_ms": RECENT_DONE_JOB_RETENTION_MS,
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct JobMetaQuery {
    /// 逗号分隔的 job id 列表
    pub(crate) ids: Option<String>,
}

/// 任务静态元数据（书名/作者/封面/简介等）：每个任务生命周期内只拉一次，
/// 不进 1.5s 轮询通道，避免重复传输相同的大块数据。元数据未就绪的任务不出现在响应里。
pub(crate) async fn job_meta(
    State(state): State<AppState>,
    Query(q): Query<JobMetaQuery>,
) -> Json<Value> {
    let ids: Vec<u64> = q
        .ids
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse::<u64>().ok())
        .take(64)
        .collect();
    let mut out = serde_json::Map::new();
    for (id, view) in state.jobs.static_views(&ids) {
        // book_id 已由轮询项提供，这里只发大块元数据。
        let Ok(mut v) = serde_json::to_value(&view) else {
            continue;
        };
        if let Some(o) = v.as_object_mut() {
            o.remove("book_id");
        }
        out.insert(id.to_string(), v);
    }
    Json(json!({ "items": out }))
}

pub(crate) async fn create_job(
    State(state): State<AppState>,
    Json(req): Json<CreateJobReq>,
) -> Result<Json<Value>, StatusCode> {
    let book_id_raw = req.book_id.clone();
    let book_id = tokio::task::spawn_blocking(move || resolve_book_id(&book_id_raw))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::BAD_REQUEST)?;
    if book_id.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // 提交时卡片携带的封面：归一化后直接入库，排队/解析阶段就能显示。
    let cover_hint = req
        .cover_url
        .clone()
        .map(|s| to_public_jpeg_cover(s.trim()))
        .filter(|s| !s.is_empty());

    // 并发限制：同时只允许一个活跃任务（Queued 或 Running），防止 API 被滥用为多用户服务。
    if state.jobs.count_active() >= 1 {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    // Validate range parameters if provided
    if let (Some(start), Some(end)) = (req.range_start, req.range_end) {
        if start < 1 || end < 1 || start > end {
            return Err(StatusCode::BAD_REQUEST);
        }
    } else if req.range_start.is_some() || req.range_end.is_some() {
        // Both range_start and range_end must be provided together
        return Err(StatusCode::BAD_REQUEST);
    }

    let handle = state.jobs.create(book_id.clone(), cover_hint.clone());
    let book_id_for_resp = book_id.clone();
    let cover_for_resp = cover_hint.clone();

    let jobs = state.jobs.clone();
    let cfg = state
        .config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let range_start = req.range_start;
    let range_end = req.range_end;

    thread::spawn(move || {
        jobs.set_running(handle.id);

        let meta_hint = dl::BookMeta {
            cover_url: cover_hint,
            ..Default::default()
        };
        let plan = match dl::prepare_download_plan(&cfg, &book_id, meta_hint) {
            Ok(p) => p,
            Err(e) => {
                jobs.set_failed(handle.id, format!("prepare plan failed: {e}"));
                return;
            }
        };

        jobs.set_meta(
            handle.id,
            plan.meta.book_name.clone(),
            plan.meta.author.clone(),
        );
        // 任务自己的上游元数据：下载库“进行中卡片”据此与成品卡同构渲染。
        jobs.set_book_meta(handle.id, meta_from_plan(&plan.meta));

        let id = handle.id;
        let jobs_cb = jobs.clone();

        // Build chapter range if specified
        let range = if let (Some(start), Some(end)) = (range_start, range_end) {
            let total = plan.chapters.len();
            if start >= 1 && end >= 1 && start <= end && end <= total {
                Some(dl::ChapterRange { start, end })
            } else {
                None
            }
        } else {
            None
        };

        let jobs_ask = jobs.clone();
        let book_name_asker = move |manager: &crate::book_parser::book_manager::BookManager| {
            let options = dl::collect_book_name_options(manager);
            if options.len() <= 1 {
                return None;
            }
            let (tx, rx) = std::sync::mpsc::channel();
            jobs_ask.set_book_name_options(id, options, tx);
            rx.recv().ok().flatten()
        };

        let jobs_fmt = jobs.clone();
        let format_asker = move |_manager: &crate::book_parser::book_manager::BookManager| {
            let options = dl::collect_output_format_options();
            let (tx, rx) = std::sync::mpsc::channel();
            jobs_fmt.set_format_options(id, options, tx);
            rx.recv().ok().flatten()
        };

        let result = dl::download_with_plan_flow(
            &cfg,
            plan,
            None,
            dl::DownloadFlowOptions {
                mode: dl::DownloadMode::Resume,
                range,
                retry_failed: {
                    let mut retried = false;
                    dl::RetryFailed::Decide(Box::new(move |_pending_len| {
                        if retried {
                            return false;
                        }
                        retried = true;
                        true
                    }))
                },
                stage_callback: None,
                book_name_asker: Some(Box::new(book_name_asker)),
                format_asker: Some(Box::new(format_asker)),
            },
            Some(Box::new(move |snap| jobs_cb.set_progress(id, snap))),
            Some(handle.cancel.clone()),
        );

        match result {
            Ok(_) => jobs.set_done(handle.id),
            Err(e) => {
                if handle.cancel.load(Ordering::Relaxed) {
                    // ensure state is canceled
                    let _ = jobs.request_cancel(handle.id);
                } else {
                    jobs.set_failed(handle.id, format!("download failed: {e}"));
                }
            }
        }
    });

    Ok(Json(
        json!({ "id": handle.id, "book_id": book_id_for_resp, "state": JobState::Queued, "cover_url": cover_for_resp }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct BookNameChoiceReq {
    pub(crate) value: Option<String>,
}

pub(crate) async fn submit_book_name_choice(
    State(state): State<AppState>,
    Path(id): Path<u64>,
    Json(req): Json<BookNameChoiceReq>,
) -> Result<Json<Value>, StatusCode> {
    if state.jobs.submit_book_name_choice(id, req.value) {
        Ok(Json(json!({"ok": true})))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

pub(crate) async fn submit_format_choice(
    State(state): State<AppState>,
    Path(id): Path<u64>,
    Json(req): Json<BookNameChoiceReq>,
) -> Result<Json<Value>, StatusCode> {
    if state.jobs.submit_format_choice(id, req.value) {
        Ok(Json(json!({"ok": true})))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

pub(crate) async fn cancel_job(
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> Result<Json<Value>, StatusCode> {
    if state.jobs.request_cancel_and_remove(id) {
        Ok(Json(json!({"ok": true})))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

pub(crate) async fn delete_job(
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> Result<Json<Value>, StatusCode> {
    if state.jobs.remove(id) {
        Ok(Json(json!({"ok": true})))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}
