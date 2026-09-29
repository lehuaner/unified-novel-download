//! 下载历史持久化（JSONL）。
//!
//! 记录每次下载/更新任务的关键信息，便于跨会话追溯。

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::logging;

const HISTORY_FILE_NAME: &str = "download_history.jsonl";

/// 串行化对历史文件的写与全量重写，避免 append 与 rewrite 交叉。
static HISTORY_WRITE_LOCK: Mutex<()> = Mutex::new(());

pub fn history_file_path() -> PathBuf {
    let logs_dir = logging::current_logs_dir().unwrap_or_else(|| PathBuf::from("logs"));
    logs_dir.join(HISTORY_FILE_NAME)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadHistoryRecord {
    pub timestamp: String,
    pub book_id: String,
    pub book_name: String,
    pub author: String,
    pub selected_chapters: usize,
    pub success_chapters: usize,
    pub failed_chapters: usize,
    pub progress: String,
    pub status: String,
    /// 以下元数据在下载完成时存档，供下载库/历史卡片直接展示，无需二次请求上游。
    /// 旧记录缺字段时由 `#[serde(default)]` 兜底为空。
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub cover_url: String,
    #[serde(default)]
    pub score: Option<f32>,
    #[serde(default)]
    pub word_count: Option<usize>,
    #[serde(default)]
    pub finished: Option<bool>,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub read_count_text: String,
}

/// 下载完成时可附带存档的书籍元数据（由 `BookMeta` 构造，避免 base_system 反向依赖 download 层）。
#[derive(Debug, Clone, Default)]
pub struct DownloadMeta {
    pub description: Option<String>,
    pub cover_url: Option<String>,
    pub score: Option<f32>,
    pub word_count: Option<usize>,
    pub finished: Option<bool>,
    pub category: Option<String>,
    pub read_count_text: Option<String>,
}

impl DownloadHistoryRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        book_id: String,
        book_name: String,
        author: String,
        selected_chapters: usize,
        success_chapters: usize,
        failed_chapters: usize,
        status: String,
        meta: DownloadMeta,
    ) -> Self {
        let timestamp = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
        let progress = format!(
            "成功 {}/{} 章，失败 {} 章",
            success_chapters, selected_chapters, failed_chapters
        );

        Self {
            timestamp,
            book_id,
            book_name,
            author,
            selected_chapters,
            success_chapters,
            failed_chapters,
            progress,
            status,
            description: meta.description.unwrap_or_default(),
            cover_url: meta.cover_url.unwrap_or_default(),
            score: meta.score,
            word_count: meta.word_count,
            finished: meta.finished,
            category: meta.category.unwrap_or_default(),
            read_count_text: meta.read_count_text.unwrap_or_default(),
        }
    }
}

pub fn append_download_history(record: &DownloadHistoryRecord) {
    let _g = HISTORY_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let logs_dir = logging::current_logs_dir().unwrap_or_else(|| PathBuf::from("logs"));
    if fs::create_dir_all(&logs_dir).is_err() {
        return;
    }

    let path = logs_dir.join(HISTORY_FILE_NAME);
    let mut file = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => f,
        Err(_) => return,
    };

    let line = match serde_json::to_string(record) {
        Ok(v) => v,
        Err(_) => return,
    };

    let _ = writeln!(file, "{line}");
    let _ = file.flush();
}

pub fn read_download_history(limit: usize, keyword: Option<&str>) -> Vec<DownloadHistoryRecord> {
    let mut out = read_all_history_records();

    if let Some(k) = keyword
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
    {
        out.retain(|rec| {
            let hay = format!(
                "{} {} {} {} {}",
                rec.book_id, rec.book_name, rec.author, rec.progress, rec.status
            )
            .to_ascii_lowercase();
            hay.contains(&k)
        });
    }

    // 已按时间倒序（read_all 内 reverse）；截断。
    if limit > 0 && out.len() > limit {
        out.truncate(limit);
    }
    out
}

/// 读取全部历史（最新在前），不做关键词过滤与截断。
fn read_all_history_records() -> Vec<DownloadHistoryRecord> {
    let path = history_file_path();
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<DownloadHistoryRecord> = Vec::new();
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<DownloadHistoryRecord>(trimmed) {
            out.push(rec);
        }
    }
    out.reverse();
    out
}

/// 按 book_id 去重：同一本书只保留最新一条（“去重从下载本身着手”的读取侧实现）。
/// 最新记录若为失败，仍会返回（供“查询后端仍可见”）；前端自行隐藏。
pub fn read_download_history_deduped(
    limit: usize,
    keyword: Option<&str>,
) -> Vec<DownloadHistoryRecord> {
    let all = read_all_history_records();
    let kw = keyword
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<DownloadHistoryRecord> = Vec::new();
    for rec in all {
        if !seen.insert(rec.book_id.clone()) {
            continue; // 同一 book_id 已有更新的一条
        }
        if let Some(k) = kw.as_deref() {
            let hay = format!(
                "{} {} {} {} {}",
                rec.book_id, rec.book_name, rec.author, rec.progress, rec.status
            )
            .to_ascii_lowercase();
            if !hay.contains(k) {
                continue;
            }
        }
        out.push(rec);
    }
    if limit > 0 && out.len() > limit {
        out.truncate(limit);
    }
    out
}

/// 补档：若该 book_id 尚无成功/存档记录，则写入一条元数据存档（status=archive）。
/// 已有非失败记录时不覆盖（避免顶掉真实下载进度）；返回是否新写。
pub fn archive_book_meta(book_id: &str, record: &DownloadHistoryRecord) -> bool {
    let existing = read_all_history_records();
    let has_good = existing
        .iter()
        .any(|r| r.book_id == book_id && r.status != "failed");
    if has_good {
        return false;
    }
    append_download_history(record);
    true
}
