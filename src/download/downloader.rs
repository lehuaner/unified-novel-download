//! 下载主流程编排。
//!
//! 负责章节批量下载、保存与断点续传、finalize 等核心编排链路。
//! 具体子模块职责参见 `mod.rs`。

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use crossbeam_channel as channel;
use serde_json::{Map, Value, json};
use tracing::{debug, error, info, warn};

use crate::base_system::book_paths;
use crate::base_system::context::Config;
use crate::base_system::download_history::{
    DownloadHistoryRecord, DownloadMeta, append_download_history,
};
use crate::book_parser::book_manager::BookManager;
use crate::book_parser::finalize_utils;
use crate::book_parser::parser::ContentParser;

use super::progress::{make_reporter, segment_enabled};
use super::segment_pool::{
    SegmentCommentPool, count_segment_comment_cache_files, extract_item_version_map,
};
use super::third_party::{fetch_group_third_party, fetch_group_unidbg, validate_endpoints};
use crate::third_party::fq_api_client::FqApiClient;

use std::sync::atomic::AtomicBool;

// ── 向后兼容重导出（外部代码通过 download::downloader::Xxx 引用）──
pub use super::models::{
    BookMeta, BookNameAsker, BookNameOption, ChapterRange, ChapterRef, DownloadFlowOptions,
    DownloadMode, DownloadPlan, DownloadResult, FormatAsker, ProgressSnapshot, RetryFailed,
    SavePhase,
};
pub(crate) use super::plan::apply_range;
pub use super::plan::prepare_download_plan;
pub(crate) use super::progress::ProgressReporter;

// ── 使用已准备好的计划执行下载 ──────────────────────────────────

/// 使用已准备好的计划执行下载，并支持区间选择。
#[allow(dead_code)]
pub fn download_with_plan(
    config: &Config,
    plan: DownloadPlan,
    range: Option<ChapterRange>,
    progress: Option<Box<dyn FnMut(ProgressSnapshot) + Send>>,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> Result<()> {
    download_with_plan_flow(
        config,
        plan,
        None,
        DownloadFlowOptions {
            mode: DownloadMode::Resume,
            range,
            retry_failed: RetryFailed::Never,
            stage_callback: None,
            book_name_asker: None,
            format_asker: None,
        },
        progress,
        cancel_flag,
    )
}

pub fn download_with_plan_flow(
    config: &Config,
    plan: DownloadPlan,
    manager: Option<BookManager>,
    options: DownloadFlowOptions,
    progress: Option<Box<dyn FnMut(ProgressSnapshot) + Send>>,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> Result<()> {
    info!(target: "download", book_id = %plan.book_id, "启动下载");

    let DownloadFlowOptions {
        mode,
        range,
        mut retry_failed,
        mut stage_callback,
        mut book_name_asker,
        mut format_asker,
    } = options;

    let chosen_chapters = apply_range(&plan.chapters, range);
    if chosen_chapters.is_empty() {
        return Err(anyhow!("范围无效或章节为空"));
    }

    let mut manager = if let Some(manager) = manager {
        manager
    } else {
        let mut manager = init_manager_from_plan(config, &plan)?;
        let _ = manager.load_existing_status(&manager.book_id.clone(), &manager.book_name.clone());
        manager
    };

    if matches!(mode, DownloadMode::Full | DownloadMode::RangeIgnoreHistory) {
        manager.downloaded.clear();
    }

    let mut pending = match mode {
        DownloadMode::FailedOnly => pending_failed(&manager, &chosen_chapters),
        _ => pending_resume(&manager, &chosen_chapters),
    };

    let mut reporter = make_reporter(config, &chosen_chapters, &pending, progress);

    // 下载完成时写入下载存档的元数据（封面/简介/评分等），供下载库/历史卡片直接展示。
    let hist_meta = DownloadMeta {
        description: plan.meta.description.clone(),
        cover_url: plan
            .meta
            .cover_url
            .clone()
            .or_else(|| plan.meta.detail_cover_url.clone()),
        score: plan.meta.score,
        word_count: plan.meta.word_count,
        finished: plan.meta.finished,
        category: plan.meta.category.clone(),
        read_count_text: plan.meta.read_count_text.clone(),
    };

    loop {
        let book_name = manager.book_name.clone();
        let result = match download_chapters_into_manager(
            config,
            &plan.book_id,
            &book_name,
            &mut manager,
            &chosen_chapters,
            &pending,
            Some(&plan._raw),
            &mut reporter,
            cancel_flag.as_ref(),
        ) {
            Ok(v) => v,
            Err(e) => {
                let success = count_success_for_chosen(&manager, &chosen_chapters);
                let failed = chosen_chapters.len().saturating_sub(success);
                append_download_history(&DownloadHistoryRecord::new(
                    manager.book_id.clone(),
                    manager.book_name.clone(),
                    manager.author.clone(),
                    chosen_chapters.len(),
                    success,
                    failed,
                    "failed".to_string(),
                    hist_meta.clone(),
                ));
                return Err(e);
            }
        };

        if let Some(cb) = stage_callback.as_mut() {
            cb(result);
        }

        pending = pending_failed(&manager, &chosen_chapters);
        if pending.is_empty() {
            break;
        }

        let should_retry = match retry_failed {
            RetryFailed::Never => false,
            RetryFailed::Decide(ref mut f) => f(pending.len()),
        };

        if !should_retry {
            break;
        }

        reporter.reset_for_retry(chosen_chapters.len(), pending.len());
    }

    let finalize_result = finalize_from_manager(
        &mut manager,
        &chosen_chapters,
        Some(&plan._raw),
        Some(&mut reporter),
        cancel_flag.as_ref(),
        &mut book_name_asker,
        &mut format_asker,
    );

    let success = count_success_for_chosen(&manager, &chosen_chapters);
    let failed = chosen_chapters.len().saturating_sub(success);
    let status = if finalize_result.is_ok() && failed == 0 {
        "success"
    } else {
        "failed"
    };
    append_download_history(&DownloadHistoryRecord::new(
        manager.book_id.clone(),
        manager.book_name.clone(),
        manager.author.clone(),
        chosen_chapters.len(),
        success,
        failed,
        status.to_string(),
        hist_meta.clone(),
    ));

    finalize_result
}

// ── Manager 初始化与辅助 ──────────────────────────────────────

fn rename_old_folder_if_needed(config: &Config, book_id: &str, _new_book_name: &str) -> Result<()> {
    let stable_folder = config.migrate_status_folder_to_stable(book_id, None)?;
    if stable_folder.exists() {
        info!(
            target: "download",
            book_id,
            folder = %stable_folder.display(),
            "已按 BookID 解析稳定缓存目录"
        );
    }
    Ok(())
}

fn rename_cover_files_if_needed(folder: &Path, old_book_name: &str, new_book_name: &str) {
    let before = book_paths::find_existing_cover_file(folder, Some(old_book_name));
    let migrated = book_paths::migrate_legacy_cover_file(folder, Some(old_book_name))
        .or_else(|| book_paths::migrate_legacy_cover_file(folder, Some(new_book_name)));

    if let (Some(before), Some(after)) = (before, migrated)
        && before != after
    {
        info!(
            target: "download",
            old = %before.display(),
            new = %after.display(),
            "迁移封面文件到稳定名称"
        );
    }
}

pub(crate) fn init_manager_from_plan(config: &Config, plan: &DownloadPlan) -> Result<BookManager> {
    let meta = &plan.meta;
    let book_name = meta
        .book_name
        .clone()
        .unwrap_or_else(|| plan.book_id.clone());

    if let Err(e) = rename_old_folder_if_needed(config, &plan.book_id, &book_name) {
        debug!(
            target: "download",
            error = ?e,
            "重命名旧文件夹失败，将继续使用新文件夹"
        );
    }

    let mut manager = BookManager::new(config.clone(), &plan.book_id, &book_name)?;
    manager.book_id = plan.book_id.clone();
    manager.book_name = book_name;
    manager.author = meta.author.clone().unwrap_or_default();
    manager.description = meta.description.clone().unwrap_or_default();
    manager.tags = meta.tags.join("|");
    manager.finished = meta.finished;
    manager.end = meta.finished.unwrap_or(false);
    manager.chapter_count = meta.chapter_count;
    manager.word_count = meta.word_count;
    manager.score = meta.score;
    manager.read_count_text = meta.read_count_text.clone();
    manager.category = meta.category.clone();
    manager.original_book_name = meta.original_book_name.clone();
    manager.book_short_name = meta.book_short_name.clone();
    Ok(manager)
}

pub(crate) fn pending_resume(manager: &BookManager, chapters: &[ChapterRef]) -> Vec<ChapterRef> {
    chapters
        .iter()
        .filter(|ch| !matches!(manager.downloaded.get(&ch.id), Some((_, Some(_)))))
        .cloned()
        .collect()
}

pub(crate) fn pending_failed(manager: &BookManager, chapters: &[ChapterRef]) -> Vec<ChapterRef> {
    chapters
        .iter()
        .filter(|ch| matches!(manager.downloaded.get(&ch.id), Some((_, None))))
        .cloned()
        .collect()
}

fn count_success_for_chosen(manager: &BookManager, chapters: &[ChapterRef]) -> usize {
    chapters
        .iter()
        .filter(|ch| matches!(manager.downloaded.get(&ch.id), Some((_, Some(_)))))
        .count()
}

// ── 核心下载编排 ──────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(crate) fn download_chapters_into_manager(
    config: &Config,
    book_id: &str,
    book_name: &str,
    manager: &mut BookManager,
    chosen_chapters: &[ChapterRef],
    pending_chapters: &[ChapterRef],
    directory_raw: Option<&Value>,
    reporter: &mut ProgressReporter,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<DownloadResult> {
    // 书旗（Shuqi）路由：跳过番茄段评逻辑，直接走书旗下载流程。
    #[cfg(feature = "shuqi")]
    if crate::shuqi::is_shuqi_book_id(book_id) {
        return crate::shuqi::download_shuqi_into_manager(
            config,
            book_id,
            book_name,
            manager,
            chosen_chapters,
            pending_chapters,
            reporter,
            cancel,
        );
    }

    // 七猫（Qimao）路由：整本缓存 ZIP 下载，跳过番茄段评逻辑。
    #[cfg(feature = "qimao")]
    if crate::qimao::is_qimao_book_id(book_id) {
        return crate::qimao::download_qimao_into_manager(
            config,
            book_id,
            book_name,
            manager,
            chosen_chapters,
            pending_chapters,
            reporter,
            cancel,
        );
    }

    // 初始化段评进度：以磁盘缓存为准，避免断点续传时"假满"。
    if segment_enabled(config) && reporter.snapshot.comment_total > 0 {
        let seg_dir = manager.book_folder().join("segment_comments");
        let _ = std::fs::create_dir_all(&seg_dir);
        let cached = count_segment_comment_cache_files(&seg_dir);
        reporter.snapshot.comment_fetch = cached.min(reporter.snapshot.comment_total);
        reporter.snapshot.comment_saved = reporter.snapshot.comment_fetch;
        reporter.emit();
    }

    if pending_chapters.is_empty() {
        info!("没有需要下载的章节，跳过下载阶段（断点续传：仅补段评缓存）");
    }

    debug!(target: "download", pending = pending_chapters.len(), total = reporter.snapshot.chapter_total, "待下载章节统计");

    let item_versions = directory_raw
        .map(extract_item_version_map)
        .unwrap_or_default();
    let status_dir = manager.book_folder().to_path_buf();
    let mut seg_pool = SegmentCommentPool::new(
        config.clone(),
        book_id.to_string(),
        status_dir,
        item_versions,
        cancel.cloned(),
    );

    // 段评与正文同时开始：先为缺失缓存的章节提交段评抓取任务。
    if let Some(pool) = seg_pool.as_ref() {
        let seg_dir = manager.book_folder().join("segment_comments");
        for ch in chosen_chapters {
            let out_path = seg_dir.join(format!("{}.json", ch.id));
            if !out_path.exists() {
                pool.submit(&ch.id);
            }
        }
    }

    if pending_chapters.is_empty() {
        if let Some(pool) = seg_pool.as_mut() {
            pool.shutdown(reporter);
        }
        reporter.snapshot.group_done = reporter.snapshot.group_total;
        reporter.snapshot.saved_chapters = reporter.snapshot.chapter_total;
        reporter.emit();
        return Ok(DownloadResult::default());
    }

    let result = download_third_party_flow(
        config,
        book_id,
        book_name,
        manager,
        pending_chapters,
        reporter,
        cancel,
        seg_pool.as_ref(),
    );

    if let Some(pool) = seg_pool.as_mut() {
        pool.shutdown(reporter);
    }

    result
}

/// 第三方 API 模式下载流程（当前唯一实现路径）。
// 参数即流程所需的全部上下文，拆结构体会把简单问题复杂化，这里豁免 lint。
#[allow(clippy::too_many_arguments)]
fn download_third_party_flow(
    config: &Config,
    book_id: &str,
    book_name: &str,
    manager: &mut BookManager,
    pending_chapters: &[ChapterRef],
    reporter: &mut ProgressReporter,
    cancel: Option<&Arc<AtomicBool>>,
    seg_pool: Option<&SegmentCommentPool>,
) -> Result<DownloadResult> {
    // unidbg 签名 sidecar 模式
    if !config.unidbg_signer_url.trim().is_empty() {
        return download_unidbg_flow(
            config,
            book_id,
            book_name,
            manager,
            pending_chapters,
            reporter,
            cancel,
            seg_pool,
        );
    }

    if config.api_endpoints.is_empty() {
        return Err(anyhow!(
            "api_endpoints 不能为空（或设置 unidbg_signer_url）"
        ));
    }

    let probe_chapter_id = pending_chapters
        .first()
        .map(|c| c.id.as_str())
        .unwrap_or("");
    if probe_chapter_id.is_empty() {
        return Err(anyhow!("章节列表为空，无法预热第三方 API"));
    }

    let mut valid = validate_endpoints(config, probe_chapter_id);
    if valid.is_empty() {
        valid = config
            .api_endpoints
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if valid.is_empty() {
        return Err(anyhow!("第三方 API 地址池为空"));
    }

    info!(target: "download", endpoints = valid.len(), "第三方 API 地址池预热完成");

    let endpoints = Arc::new(std::sync::Mutex::new(valid));
    let picker = Arc::new(AtomicUsize::new(0));
    let worker_count = config.max_workers.max(1);
    let epub_mode = config.novel_format.eq_ignore_ascii_case("epub");

    let (tx_jobs, rx_jobs) = channel::unbounded::<Vec<ChapterRef>>();
    let (tx_res, rx_res) = channel::unbounded::<Result<(Vec<ChapterRef>, Value)>>();

    for group in build_dynamic_chapter_groups(pending_chapters) {
        tx_jobs.send(group.to_vec()).ok();
    }
    drop(tx_jobs);

    for _ in 0..worker_count {
        let rx = rx_jobs.clone();
        let tx = tx_res.clone();
        let cfg = config.clone();
        let endpoints = endpoints.clone();
        let picker = picker.clone();
        let cancel = cancel.cloned();
        std::thread::spawn(move || {
            for group in rx.iter() {
                if cancel
                    .as_ref()
                    .map(|c| c.load(Ordering::Relaxed))
                    .unwrap_or(false)
                {
                    let _ = tx.send(Err(anyhow!("用户停止下载")));
                    return;
                }
                let value = fetch_group_third_party(&cfg, &endpoints, &picker, &group, epub_mode);
                let _ = tx.send(value.map(|v| (group, v)));
            }
        });
    }
    drop(tx_res);

    let mut result = DownloadResult::default();
    for res in rx_res.iter() {
        if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
            return Err(anyhow!("用户停止下载"));
        }

        let (group, value) = res?;

        let parsed = ContentParser::extract_api_content(&value, config);
        for ch in &group {
            match parsed.get(&ch.id) {
                Some((content, title)) if !content.is_empty() => {
                    // 缓存统一保存为 XHTML 格式
                    let cleaned = extract_body_fragment(content);
                    manager.save_chapter(&ch.id, title, &cleaned);
                    manager.append_downloaded_chapter(&ch.id, title, &cleaned);
                    result.success += 1;
                    if let Some(pool) = seg_pool {
                        pool.submit(&ch.id);
                    }
                }
                _ => {
                    log_failed_chapter(ch, "章节内容缺失或为空");
                    manager.save_error_chapter(&ch.id, &ch.title);
                    result.failed += 1;
                }
            }
            reporter.inc_saved();
        }
        reporter.inc_group();
        if let Some(pool) = seg_pool {
            pool.drain_progress(reporter);
        }

        manager.save_download_status();
    }

    info!(
        target: "download",
        "第三方下载完成：{} ({} 章)",
        book_name,
        pending_chapters.len()
    );
    Ok(result)
}

/// unidbg 签名 sidecar 模式下载流程。
#[allow(clippy::too_many_arguments)]
fn download_unidbg_flow(
    config: &Config,
    book_id: &str,
    book_name: &str,
    manager: &mut BookManager,
    pending_chapters: &[ChapterRef],
    reporter: &mut ProgressReporter,
    cancel: Option<&Arc<AtomicBool>>,
    seg_pool: Option<&SegmentCommentPool>,
) -> Result<DownloadResult> {
    let signer_url = config.unidbg_signer_url.trim();
    if signer_url.is_empty() {
        return Err(anyhow!("unidbg_signer_url 未配置"));
    }

    info!(target: "download", url = signer_url, "使用 unidbg 签名 sidecar 模式");

    let client = FqApiClient::new(signer_url, 30_000)?;

    // 预热：确保 registerkey 可用。走 get_decryption_key 的 5 分钟缓存，
    // 同一会话内多本书/多次更新可命中缓存、跳过一次 sign+API 往返。
    info!(target: "download", "预热：获取 registerkey ...");
    match client.get_decryption_key() {
        Ok(_) => info!(target: "download", "registerkey 就绪（命中缓存则不重复请求）"),
        Err(e) => {
            warn!(target: "download", err = %e, "registerkey 获取失败，将继续尝试（可能由首次请求触发）");
        }
    }

    let worker_count = config.max_workers.max(1);
    let (tx_jobs, rx_jobs) = channel::unbounded::<Vec<ChapterRef>>();
    let (tx_res, rx_res) = channel::unbounded::<Result<(Vec<ChapterRef>, Value)>>();

    for group in build_dynamic_chapter_groups(pending_chapters) {
        tx_jobs.send(group.to_vec()).ok();
    }
    drop(tx_jobs);

    // FqApiClient 内部使用 Mutex 保护 registerkey 状态，可安全共享
    let client = Arc::new(client);
    let book_id = Arc::new(book_id.to_string());

    for _ in 0..worker_count {
        let rx = rx_jobs.clone();
        let tx = tx_res.clone();
        let client = client.clone();
        let book_id = book_id.clone();
        let cancel = cancel.cloned();
        std::thread::spawn(move || {
            for group in rx.iter() {
                if cancel
                    .as_ref()
                    .map(|c| c.load(Ordering::Relaxed))
                    .unwrap_or(false)
                {
                    let _ = tx.send(Err(anyhow!("用户停止下载")));
                    return;
                }
                let value = fetch_group_unidbg(&client, &book_id, &group);
                let _ = tx.send(value.map(|v| (group, v)));
            }
        });
    }
    drop(tx_res);

    let mut result = DownloadResult::default();
    for res in rx_res.iter() {
        if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
            return Err(anyhow!("用户停止下载"));
        }

        let (group, value) = res?;

        let parsed = ContentParser::extract_api_content(&value, config);
        for ch in &group {
            match parsed.get(&ch.id) {
                Some((content, title)) if !content.is_empty() => {
                    let cleaned = extract_body_fragment(content);
                    manager.save_chapter(&ch.id, title, &cleaned);
                    manager.append_downloaded_chapter(&ch.id, title, &cleaned);
                    result.success += 1;
                    if let Some(pool) = seg_pool {
                        pool.submit(&ch.id);
                    }
                }
                _ => {
                    log_failed_chapter(ch, "章节内容缺失或为空");
                    manager.save_error_chapter(&ch.id, &ch.title);
                    result.failed += 1;
                }
            }
            reporter.inc_saved();
        }
        reporter.inc_group();
        if let Some(pool) = seg_pool {
            pool.drain_progress(reporter);
        }

        manager.save_download_status();
    }

    info!(
        target: "download",
        "unidbg 下载完成：{} ({} 章)",
        book_name,
        pending_chapters.len()
    );
    Ok(result)
}

// ── Finalize ──────────────────────────────────────────────────

pub(crate) fn finalize_from_manager(
    manager: &mut BookManager,
    chosen: &[ChapterRef],
    directory_raw: Option<&Value>,
    mut reporter: Option<&mut ProgressReporter>,
    cancel: Option<&Arc<AtomicBool>>,
    book_name_asker: &mut Option<BookNameAsker>,
    format_asker: &mut Option<FormatAsker>,
) -> Result<()> {
    if manager.config.is_ask_after_download()
        && !manager.book_name_selected_after_download
        && let Some(asker) = book_name_asker.as_mut()
        && let Some(chosen_name) = asker(manager)
    {
        let old_name = manager.book_name.clone();
        manager.book_name = chosen_name.clone();
        manager.book_name_selected_after_download = true;
        if old_name != chosen_name {
            manager.remember_previous_book_name(&old_name);
            rename_cover_files_if_needed(manager.book_folder(), &old_name, &chosen_name);
        }
    }

    // 下载完后选择格式
    if manager.config.ask_format_after_download
        && !manager.format_selected_after_download
        && let Some(asker) = format_asker.as_mut()
        && let Some(chosen_fmt) = asker(manager)
    {
        info!(target: "download", "用户选择输出格式: {}", chosen_fmt);
        if let Err(err) = manager.config.apply_output_format_choice(&chosen_fmt) {
            warn!(target: "download", error = %err, "应用输出格式选择失败");
        }
        manager.format_selected_after_download = true;
    }

    debug!(target: "download", "保存下载状态");
    manager.save_download_status();

    let mut chapter_values = Vec::with_capacity(manager.downloaded.len());
    let mut finalized_ids = HashSet::with_capacity(chosen.len());
    for ch in chosen {
        if !finalized_ids.insert(&ch.id) {
            warn!(target: "download", id = %ch.id, title = %ch.title, "跳过最终输出中的重复章节");
            continue;
        }
        match manager.downloaded.get(&ch.id) {
            Some((title, Some(content))) => {
                let mut obj = Map::new();
                obj.insert("id".to_string(), Value::String(ch.id.clone()));
                obj.insert("title".to_string(), Value::String(title.clone()));
                obj.insert("content".to_string(), Value::String(content.clone()));
                chapter_values.push(Value::Object(obj));
            }
            Some((title, None)) => {
                let mut obj = Map::new();
                obj.insert("id".to_string(), Value::String(ch.id.clone()));
                obj.insert("title".to_string(), Value::String(title.clone()));
                obj.insert(
                    "content".to_string(),
                    Value::String("[本章下载失败]".to_string()),
                );
                chapter_values.push(Value::Object(obj));
            }
            None => {
                let mut obj = Map::new();
                obj.insert("id".to_string(), Value::String(ch.id.clone()));
                obj.insert("title".to_string(), Value::String(ch.title.clone()));
                obj.insert(
                    "content".to_string(),
                    Value::String("[本章下载失败]".to_string()),
                );
                chapter_values.push(Value::Object(obj));
            }
        }
    }

    let result_code = 0;
    let reporter_ref = reporter.as_deref_mut();
    let finalize_ok = finalize_utils::run_finalize(
        manager,
        &chapter_values,
        result_code,
        directory_raw,
        reporter_ref,
        cancel,
    );
    manager.save_download_status();

    let finished = manager.finished.unwrap_or(manager.end);
    let full_book_range = manager
        .chapter_count
        .map(|n| n == chosen.len())
        .unwrap_or(false);

    let all_success = count_success_for_chosen(manager, chosen) == chosen.len();
    if finalize_ok
        && manager.config.auto_clear_dump
        && finished
        && full_book_range
        && all_success
        && let Err(e) = manager.delete_status_folder()
    {
        error!(target: "book_manager", error = ?e, "删除状态目录失败");
    }

    if let Some(r) = reporter {
        r.finish_cli_bars();
    }

    Ok(())
}

pub(crate) fn collect_book_name_options(manager: &BookManager) -> Vec<BookNameOption> {
    let mut options: Vec<BookNameOption> = Vec::new();

    let default_name = manager.book_name.clone();
    if !default_name.is_empty() {
        options.push(BookNameOption {
            label: "默认书名".to_string(),
            value: default_name.clone(),
        });
    }

    if let Some(orig) = &manager.original_book_name
        && !orig.is_empty()
        && orig != &default_name
    {
        options.push(BookNameOption {
            label: "原始书名".to_string(),
            value: orig.clone(),
        });
    }

    if let Some(short) = &manager.book_short_name
        && !short.is_empty()
        && short != &default_name
    {
        let dup = manager
            .original_book_name
            .as_ref()
            .is_some_and(|o| o == short);
        if !dup {
            options.push(BookNameOption {
                label: "短书名".to_string(),
                value: short.clone(),
            });
        }
    }

    options
}

pub(crate) fn collect_output_format_options() -> Vec<BookNameOption> {
    vec![
        BookNameOption {
            label: "txt 格式".to_string(),
            value: "txt".to_string(),
        },
        BookNameOption {
            label: "epub 格式".to_string(),
            value: "epub".to_string(),
        },
        BookNameOption {
            label: "pdf 格式".to_string(),
            value: "pdf".to_string(),
        },
        BookNameOption {
            label: "散装文件".to_string(),
            value: "bulk_txt".to_string(),
        },
    ]
}

// ── 工具函数 ──────────────────────────────────────────────────

fn extract_body_fragment(input: &str) -> String {
    let lower = input.to_lowercase();
    if let Some(body_idx) = lower.find("<body")
        && let Some(open_end) = lower[body_idx..].find('>')
    {
        let start = body_idx + open_end + 1;
        if let Some(close_idx) = lower[start..].find("</body>") {
            return input[start..start + close_idx].to_string();
        }
    }
    input.to_string()
}

fn log_failed_chapter(chapter: &ChapterRef, reason: &str) {
    error!(
        target: "download",
        chapter_id = %chapter.id,
        chapter_title = %chapter.title,
        reason,
        "章节下载失败：{} ({})",
        chapter.title,
        chapter.id
    );
}

pub(crate) const MIN_DYNAMIC_GROUP_SIZE: usize = 15;
pub(crate) const MAX_DYNAMIC_GROUP_SIZE: usize = 25;

pub(crate) fn build_dynamic_chapter_groups(chapters: &[ChapterRef]) -> Vec<&[ChapterRef]> {
    if chapters.is_empty() {
        return Vec::new();
    }

    let len = chapters.len();
    if len <= MAX_DYNAMIC_GROUP_SIZE {
        return vec![chapters];
    }

    let min_groups = len.div_ceil(MAX_DYNAMIC_GROUP_SIZE);
    let max_groups = len / MIN_DYNAMIC_GROUP_SIZE;

    let group_count = if min_groups <= max_groups {
        max_groups
    } else {
        min_groups
    }
    .max(1);

    let base_size = len / group_count;
    let remainder = len % group_count;

    let mut groups = Vec::with_capacity(group_count);
    let mut start = 0;
    for idx in 0..group_count {
        let extra = usize::from(idx < remainder);
        let size = base_size + extra;
        let end = start + size;
        groups.push(&chapters[start..end]);
        start = end;
    }

    groups
}

pub(crate) fn dynamic_group_count(total: usize) -> usize {
    build_dynamic_chapter_groups(&vec![
        ChapterRef {
            id: String::new(),
            title: String::new(),
        };
        total
    ])
    .len()
}

#[allow(dead_code)]
fn merge_content_values(values: Vec<Value>) -> Value {
    let mut merged = json!({
        "code": 0,
        "data": {}
    });

    for value in values {
        let Some(data_map) = value.get("data").and_then(|v| v.as_object()) else {
            continue;
        };

        if let Some(merged_map) = merged.get_mut("data").and_then(|v| v.as_object_mut()) {
            for (cid, info) in data_map {
                merged_map.insert(cid.clone(), info.clone());
            }
        }
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_content_values_combines_data_entries() {
        let merged = merge_content_values(vec![
            json!({
                "code": 0,
                "data": {
                    "1": { "content": "A", "title": "甲" }
                }
            }),
            json!({
                "code": 0,
                "data": {
                    "2": { "content": "B", "title": "乙" }
                }
            }),
        ]);

        let data = merged.get("data").and_then(|v| v.as_object()).unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(
            data.get("1")
                .and_then(|v| v.get("content"))
                .and_then(|v| v.as_str()),
            Some("A")
        );
        assert_eq!(
            data.get("2")
                .and_then(|v| v.get("content"))
                .and_then(|v| v.as_str()),
            Some("B")
        );
    }

    #[test]
    fn merge_content_values_ignores_entries_without_data_map() {
        let merged = merge_content_values(vec![
            json!({"code": 0, "data": {"1": {"content": "A"}}}),
            json!({"code": 0, "message": "bad"}),
        ]);

        let data = merged.get("data").and_then(|v| v.as_object()).unwrap();
        assert_eq!(data.len(), 1);
        assert!(data.contains_key("1"));
    }

    #[test]
    fn format_failed_log_uses_title_and_id() {
        let ch = ChapterRef {
            id: "123".to_string(),
            title: "测试章节".to_string(),
        };

        let msg = format!("章节下载失败：{} ({})", ch.title, ch.id);
        assert_eq!(msg, "章节下载失败：测试章节 (123)");
    }

    #[test]
    fn build_dynamic_chapter_groups_keeps_group_size_within_range() {
        let chapters: Vec<ChapterRef> = (1..=80)
            .map(|i| ChapterRef {
                id: i.to_string(),
                title: format!("第{i}章"),
            })
            .collect();

        let groups = build_dynamic_chapter_groups(&chapters);
        assert_eq!(
            groups.iter().map(|g| g.len()).sum::<usize>(),
            chapters.len()
        );
        assert!(groups.iter().all(|g| (15..=25).contains(&g.len())));
        assert_eq!(groups.len(), 5);
    }

    #[test]
    fn build_dynamic_chapter_groups_allows_small_tail_as_single_group() {
        let chapters: Vec<ChapterRef> = (1..=14)
            .map(|i| ChapterRef {
                id: i.to_string(),
                title: format!("第{i}章"),
            })
            .collect();

        let groups = build_dynamic_chapter_groups(&chapters);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 14);
    }

    #[test]
    fn dynamic_group_count_matches_balanced_distribution() {
        assert_eq!(dynamic_group_count(0), 0);
        assert_eq!(dynamic_group_count(14), 1);
        assert_eq!(dynamic_group_count(25), 1);
        assert_eq!(dynamic_group_count(26), 2);
        assert_eq!(dynamic_group_count(30), 2);
        assert_eq!(dynamic_group_count(50), 3);
        assert_eq!(dynamic_group_count(80), 5);
    }

    #[test]
    fn migrate_old_folder_to_stable_book_id_cache_even_if_api_book_name_changed() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut config = crate::base_system::context::Config::default();
        config.save_path = temp_dir.path().display().to_string();

        let old_folder = temp_dir.path().join("123_旧书名");
        std::fs::create_dir_all(&old_folder).unwrap();
        std::fs::write(old_folder.join("status.json"), "{}\n").unwrap();

        rename_old_folder_if_needed(&config, "123", "新书名").unwrap();

        let new_folder = temp_dir.path().join("123");
        assert!(new_folder.join("status.json").exists());
        assert!(!old_folder.exists());
        assert!(!temp_dir.path().join("123_新书名").exists());
    }
}
