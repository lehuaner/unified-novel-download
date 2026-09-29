//! 搜索记录持久化（服务端落库，实现跨设备同步）。
//!
//! 说明：Web UI 可能被多台设备/多个浏览器同时访问。搜索记录保存在服务端
//! 数据目录的单个 JSON 文件中，所有客户端读取同一份，从而“落库”并跨设备同步。
//! 读写通过进程级互斥锁串行化，避免并发下的读改写竞态。

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::logging;

const SEARCH_HISTORY_FILE_NAME: &str = "search_history.json";
const SEARCH_HISTORY_MAX: usize = 30;

/// 串行化文件读改写的进程级锁（`Mutex::new` 为 const，可静态初始化）。
static SEARCH_HISTORY_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHistoryRecord {
    pub keyword: String,
    pub last_time: String,
    pub count: u32,
}

fn search_history_file_path() -> PathBuf {
    let logs_dir = logging::current_logs_dir().unwrap_or_else(|| PathBuf::from("logs"));
    logs_dir.join(SEARCH_HISTORY_FILE_NAME)
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn load_unlocked() -> Vec<SearchHistoryRecord> {
    let path = search_history_file_path();
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_unlocked(list: &[SearchHistoryRecord]) {
    let path = search_history_file_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(list) {
        let _ = fs::write(path, json);
    }
}

/// 读取全部搜索记录（最新在前）。
pub fn read_search_history() -> Vec<SearchHistoryRecord> {
    let _g = SEARCH_HISTORY_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    load_unlocked()
}

/// 记录一次搜索：命中已有关键词则计数 +1 并置顶，否则新增；总数封顶。
pub fn push_search_history(keyword: &str) -> Vec<SearchHistoryRecord> {
    let kw = keyword.trim();
    if kw.is_empty() {
        return read_search_history();
    }

    let _g = SEARCH_HISTORY_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut list = load_unlocked();
    let now = now_rfc3339();

    if let Some(pos) = list.iter().position(|r| r.keyword == kw) {
        let mut rec = list.remove(pos);
        rec.count = rec.count.saturating_add(1);
        rec.last_time = now;
        list.insert(0, rec);
    } else {
        list.insert(
            0,
            SearchHistoryRecord {
                keyword: kw.to_string(),
                last_time: now,
                count: 1,
            },
        );
    }

    if list.len() > SEARCH_HISTORY_MAX {
        list.truncate(SEARCH_HISTORY_MAX);
    }
    save_unlocked(&list);
    list
}

/// 删除单条搜索记录。
pub fn remove_search_history(keyword: &str) -> Vec<SearchHistoryRecord> {
    let kw = keyword.trim();
    let _g = SEARCH_HISTORY_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut list = load_unlocked();
    list.retain(|r| r.keyword != kw);
    save_unlocked(&list);
    list
}

/// 清空全部搜索记录。
pub fn clear_search_history() {
    let _g = SEARCH_HISTORY_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    save_unlocked(&[]);
}
