use std::path::{Path, PathBuf};
use std::thread;

use crate::base_system::novel_updates;
use anyhow::Result;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ui::web::state::{AppState, UpdateScanRow, UpdateScanStore};

#[derive(Debug, Deserialize)]
pub(crate) struct UpdatesQuery {
    /// 是否启动一次新扫描。默认 true；前端轮询进度时会传 false，避免扫描结束后立刻重开。
    pub(crate) start: Option<bool>,
}

pub(crate) async fn api_updates(
    State(state): State<AppState>,
    Query(q): Query<UpdatesQuery>,
) -> Result<Json<Value>, StatusCode> {
    let cfg = state
        .config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let save_dir = cfg.default_save_dir();
    let save_dir_display = save_dir.display().to_string();

    if q.start.unwrap_or(true) && state.update_scan.try_start(save_dir_display.clone()) {
        // 手动刷新：并发扫描（默认 4）。冷启动载入+串行扫描已迁至服务启动时 boot_scan。
        let store = state.update_scan.clone();
        thread::spawn(move || {
            if let Err(err) = scan_updates(&save_dir, 4, store.clone()) {
                store.finish_failed(err.to_string());
            }
        });
    }

    let snapshot = state.update_scan.snapshot();
    Ok(Json(json!({
        "running": snapshot.running,
        "scanned": snapshot.scanned,
        "total": snapshot.total,
        "save_dir": if snapshot.save_dir.is_empty() { save_dir_display } else { snapshot.save_dir },
        "updates": snapshot.updates,
        "no_updates": snapshot.no_updates,
        "error": snapshot.error,
        "started_ms": snapshot.started_ms,
        "updated_ms": snapshot.updated_ms,
    })))
}

/// 冷启动（服务启动时调用一次）：载入磁盘快照秒显，再后台串行（一次一本）扫描刷新一次。
pub(crate) fn boot_scan(state: &AppState) {
    let cfg = state
        .config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let save_dir = cfg.default_save_dir();
    let save_dir_display = save_dir.display().to_string();
    if let Some(snap) = novel_updates::load_update_snapshot(&save_dir) {
        state.update_scan.load_snapshot(
            snap.updates.into_iter().map(row_from_update).collect(),
            snap.no_updates.into_iter().map(row_from_update).collect(),
            snap.updated_ms,
        );
    }
    if state.update_scan.try_start(save_dir_display) {
        let store = state.update_scan.clone();
        thread::spawn(move || {
            if let Err(err) = scan_updates(&save_dir, 1, store.clone()) {
                store.finish_failed(err.to_string());
                return;
            }
            // 冷启动串行扫完顺手修正一次陈旧行（不触网）并把内存快照落盘。
            recompute_and_persist(&store, &save_dir);
        });
    }
}

/// #4：后台连载扫描器（仅 Web 服务模式）。每 30 分钟醒一次，对“连载中”的书按递增周期做单本探测。
/// 与全量/冷启动扫描互斥（store.is_running），串行、低频，尽量不打上游。
pub(crate) fn spawn_serializing_scheduler(state: AppState) {
    let save_dir = state
        .config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .default_save_dir();
    let store = state.update_scan.clone();
    tokio::spawn(async move {
        use std::time::Duration;
        // 默认 30 分钟醒一次；可用 UNDL_SCAN_TICK_SECS 覆写（仅测试/运维，不改默认行为）。
        let tick_secs = std::env::var("UNDL_SCAN_TICK_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(1800);
        let mut ticker = tokio::time::interval(Duration::from_secs(tick_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 首个 tick 立即返回，跳过（boot_scan 刚跑完，无需立刻再扫）。
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if store.is_running() {
                continue;
            }
            let dir = save_dir.clone();
            let store2 = store.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(rows) = novel_updates::serializing_scan_due(&dir) {
                    for it in rows {
                        store2.update_one(row_from_update(it));
                    }
                }
                // 特性1+2：本地已无状态目录的陈旧行在此修正（能兜底则重算、否则剔除），
                // 并把修正后的内存快照落盘，避免重启后旧徽标复活。
                recompute_and_persist(&store2, &dir);
            })
            .await;
        }
    });
}

/// 单本查更新（预览页“信息更新”与下载库“查更新”共用）：强制刷新该书远端章节数，
/// 回写逐本缓存与内存快照并落盘；本地无状态目录时后端用下载历史判定进度，
/// 章节数比缓存增长则把该书重新纳入递增调度。
#[derive(Debug, Deserialize)]
pub(crate) struct RefreshOneQuery {
    pub(crate) book_id: String,
}

pub(crate) async fn api_updates_refresh_one(
    State(state): State<AppState>,
    Query(q): Query<RefreshOneQuery>,
) -> Result<Json<Value>, StatusCode> {
    let cfg = state
        .config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let save_dir = cfg.default_save_dir();
    let book_id = q.book_id.clone();
    let scan_dir = save_dir.clone();
    let res =
        tokio::task::spawn_blocking(move || novel_updates::refresh_one_remote(&scan_dir, &book_id))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
    match res {
        Some(row) => {
            let out = row_from_update(row);
            state.update_scan.update_one(out.clone());
            // 单本刷新结果同步落盘，重启后不会回到旧章节数。
            recompute_and_persist(&state.update_scan, &save_dir);
            Ok(Json(json!({ "ok": true, "row": out })))
        }
        None => {
            // 进度无从取得（无状态目录且无成功历史）或远端不可达：不回写徽标，仅修正一次陈旧行。
            recompute_and_persist(&state.update_scan, &save_dir);
            Ok(Json(json!({ "ok": false, "row": Value::Null })))
        }
    }
}

fn scan_updates(
    save_dir: &Path,
    workers: usize,
    store: std::sync::Arc<UpdateScanStore>,
) -> Result<()> {
    let scan = novel_updates::scan_novel_updates_with_workers(save_dir, workers, |progress| {
        store.push_progress(progress.scanned, progress.total);
    })?;

    store.finish(
        save_dir.display().to_string(),
        scan.updates.into_iter().map(row_from_update).collect(),
        scan.no_updates.into_iter().map(row_from_update).collect(),
    );
    Ok(())
}

fn row_from_update(it: novel_updates::NovelUpdateRow) -> UpdateScanRow {
    UpdateScanRow {
        book_id: it.book_id,
        book_name: it.book_name,
        folder: it
            .folder
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
        local_total: it.local_total,
        local_failed: it.local_failed,
        remote_total: it.remote_total,
        new_count: it.new_count,
        has_update: it.has_update,
        is_ignored: it.is_ignored,
        finished: it.finished,
    }
}

/// `UpdateScanRow` → `NovelUpdateRow`：folder 统一还原为绝对路径，
/// 使落盘快照与后端扫描结果保持同一形态（前端只用目录名，不受影响）。
fn row_to_novel(save_dir: &Path, it: UpdateScanRow) -> novel_updates::NovelUpdateRow {
    let folder = {
        let p = PathBuf::from(&it.folder);
        if p.is_absolute() { p } else { save_dir.join(p) }
    };
    novel_updates::NovelUpdateRow {
        book_id: it.book_id,
        book_name: it.book_name,
        folder,
        local_total: it.local_total,
        local_failed: it.local_failed,
        remote_total: it.remote_total,
        new_count: it.new_count,
        has_update: it.has_update,
        is_ignored: it.is_ignored,
        finished: it.finished,
    }
}

/// 陈旧行修正 + 快照落盘（不触网）。
///
/// 单本刷新与后台增量扫描以往只改内存/逐本缓存，磁盘快照停在最后一次全量扫描的结果，
/// 服务重启就会把陈旧徽标再载回来；同时，“状态目录已被 auto_clear_dump 删除”的书
/// 已永远脱离扫描集合，旧行无人重算。本函数每轮把这两件事一并处理。
fn recompute_and_persist(store: &std::sync::Arc<UpdateScanStore>, save_dir: &Path) {
    let snap = store.snapshot();
    // 全量扫描进行中：由扫描自身的 finish + 落盘负责，不交叉覆盖。
    if snap.running {
        return;
    }

    let mut updates: Vec<novel_updates::NovelUpdateRow> = snap
        .updates
        .into_iter()
        .map(|r| row_to_novel(save_dir, r))
        .collect();
    let mut no_updates: Vec<novel_updates::NovelUpdateRow> = snap
        .no_updates
        .into_iter()
        .map(|r| row_to_novel(save_dir, r))
        .collect();
    let changed = novel_updates::recompute_or_prune_rows(save_dir, &mut updates, &mut no_updates);
    if changed > 0 {
        store.replace_rows(
            updates.iter().cloned().map(row_from_update).collect(),
            no_updates.iter().cloned().map(row_from_update).collect(),
        );
    }

    let after = store.snapshot();
    novel_updates::persist_update_snapshot(
        save_dir,
        after
            .updates
            .into_iter()
            .map(|r| row_to_novel(save_dir, r))
            .collect(),
        after
            .no_updates
            .into_iter()
            .map(|r| row_to_novel(save_dir, r))
            .collect(),
    );
}
