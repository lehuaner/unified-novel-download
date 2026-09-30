use std::path::Path;
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

    if state.update_scan.take_boot_scan() {
        // 冷启动破例：先载入磁盘快照秒显，再后台串行（一次一本）扫描刷新一次。
        if let Some(snap) = novel_updates::load_update_snapshot(&save_dir) {
            state.update_scan.load_snapshot(
                snap.updates.into_iter().map(row_from_update).collect(),
                snap.no_updates.into_iter().map(row_from_update).collect(),
                snap.updated_ms,
            );
        }
        if state.update_scan.try_start(save_dir_display.clone()) {
            let store = state.update_scan.clone();
            let dir = save_dir.clone();
            thread::spawn(move || {
                if let Err(err) = scan_updates(&dir, 1, store.clone()) {
                    store.finish_failed(err.to_string());
                }
            });
        }
    } else if q.start.unwrap_or(true) && state.update_scan.try_start(save_dir_display.clone()) {
        // 手动刷新：并发扫描（默认 4）。
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

/// 预览页“信息更新”联动：强制刷新单本远端章节数，回写缓存与内存快照，返回该书最新行。
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
    let res = tokio::task::spawn_blocking(move || novel_updates::refresh_one_remote(&save_dir, &book_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    match res {
        Some(row) => {
            let out = row_from_update(row);
            state.update_scan.update_one(out.clone());
            Ok(Json(json!({ "ok": true, "row": out })))
        }
        None => Ok(Json(json!({ "ok": false, "row": Value::Null }))),
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
    }
}
