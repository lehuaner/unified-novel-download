use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::base_system::self_update::SelfUpdateOutcome;
use crate::ui::web::state::{AppState, SelfUpdateState};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) async fn api_self_update(
    State(state): State<AppState>,
) -> Result<Json<Value>, StatusCode> {
    if cfg!(feature = "docker") {
        return Err(StatusCode::BAD_REQUEST);
    }

    if !state.self_update.try_start() {
        let snap = state.self_update.snapshot();
        return Ok(Json(json!({
            "ok": true,
            "already_running": true,
            "status": snap,
        })));
    }

    let store = state.self_update.clone();
    let ticker_store = store.clone();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();

    thread::spawn(move || {
        loop {
            match stop_rx.recv_timeout(Duration::from_millis(600)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => ticker_store.tick_running(),
            }
        }
    });

    thread::spawn(move || {
        store.set(SelfUpdateState::Running, "check", 8, "检查最新版本…");
        thread::sleep(Duration::from_millis(150));
        store.set(SelfUpdateState::Running, "download", 18, "开始下载更新包…");

        let result = crate::base_system::self_update::check_for_updates(VERSION, true);

        let _ = stop_tx.send(());
        match result {
            Ok(SelfUpdateOutcome::UpToDate) => {
                store.finish_done("done", "已是最新版本，无需更新");
            }
            Ok(SelfUpdateOutcome::Skipped) => {
                store.finish_done("skipped", "已跳过更新");
            }
            Ok(SelfUpdateOutcome::UpdateLaunched) => {
                store.finish_done("restart", "更新已完成，服务正在重启");
            }
            Err(e) => {
                store.finish_failed("failed", format!("自更新失败: {e}"));
            }
        }
    });

    Ok(Json(json!({
        "ok": true,
        "already_running": false,
        "message": "self update started"
    })))
}

pub(crate) async fn api_self_update_status(
    State(state): State<AppState>,
) -> Result<Json<Value>, StatusCode> {
    Ok(Json(json!(state.self_update.snapshot())))
}
