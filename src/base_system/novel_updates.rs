//! 复用的“更新小说扫描”逻辑（供 TUI / Web / noui 共用）。

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::network_parser::network::{FanqieWebConfig, FanqieWebNetwork};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NovelUpdateRow {
    pub book_id: String,
    pub book_name: String,
    pub folder: PathBuf,
    pub local_total: usize,
    pub local_failed: usize,
    pub remote_total: usize,
    pub new_count: usize,
    pub has_update: bool,
    pub is_ignored: bool,
    /// 服务器返回的完结状态（true=完结）；#4 后台扫描据此判定是否停扫。
    #[serde(default)]
    pub finished: Option<bool>,
}

#[derive(Debug, Default, Clone)]
pub struct NovelUpdateScanResult {
    pub updates: Vec<NovelUpdateRow>,
    pub no_updates: Vec<NovelUpdateRow>,
}

#[derive(Debug, Clone)]
pub struct NovelUpdateProgress {
    pub row: NovelUpdateRow,
    pub scanned: usize,
    pub total: usize,
}

#[derive(Debug, Clone)]
struct LocalBookStatus {
    book_id: String,
    book_name: String,
    folder: PathBuf,
    local_total: usize,
    local_failed: usize,
    is_ignored: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UpdateCacheFile {
    #[serde(default)]
    entries: HashMap<String, CachedRemoteTotal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedRemoteTotal {
    remote_total: usize,
    checked_ms: u64,
    #[serde(default)]
    finished: Option<bool>,
}

const UPDATE_CACHE_FILE: &str = ".tnd_update_cache.json";
const UPDATE_CACHE_TTL_MS: u64 = 10 * 60 * 1000;
const UPDATE_SCAN_WORKERS: usize = 4;
/// 冷启动秒显用的完整扫描结果快照（区别于逐本 remote_total 的 TTL 缓存）。
const UPDATE_SNAPSHOT_FILE: &str = ".tnd_update_snapshot.json";
/// #4：连载中书籍后台静默重扫的调度状态（每本一条，记录递增周期与上次结果）。
const UPDATE_SCHEDULE_FILE: &str = ".tnd_update_schedule.json";
const SCAN_DAY_MS: u64 = 24 * 60 * 60 * 1000;
const SCAN_INTERVAL_CAP_DAYS: u64 = 10;

/// 完整扫描结果快照：供服务重启后冷启动秒显，避免首屏空白。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateSnapshot {
    pub updated_ms: u64,
    pub updates: Vec<NovelUpdateRow>,
    pub no_updates: Vec<NovelUpdateRow>,
}

/// 读取磁盘扫描快照（无或损坏返回 None）。
pub fn load_update_snapshot(save_dir: &Path) -> Option<UpdateSnapshot> {
    let raw = fs::read_to_string(save_dir.join(UPDATE_SNAPSHOT_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_update_snapshot(save_dir: &Path, snap: &UpdateSnapshot) {
    if let Ok(raw) = serde_json::to_string_pretty(snap) {
        let _ = fs::write(save_dir.join(UPDATE_SNAPSHOT_FILE), raw);
    }
}

/// 扫描保存目录下的书籍缓存文件夹（新版为 `<book_id>`，兼容旧版 `<book_id>_<book_name>`），并对比远端目录。
///
/// 备注："新章节" 以本地已知章节条目数（包含失败/空内容条目）为基准，避免把失败章误报成新章。
#[allow(dead_code)]
pub fn scan_novel_updates(save_dir: &Path) -> Result<NovelUpdateScanResult> {
    scan_novel_updates_with_progress(save_dir, |_| {})
}

/// 带进度回调的更新扫描（默认并发）。回调会在每本书拿到远端章节数后立即触发，适合 TUI/CLI 边扫边显示。
pub fn scan_novel_updates_with_progress<F>(
    save_dir: &Path,
    on_progress: F,
) -> Result<NovelUpdateScanResult>
where
    F: FnMut(NovelUpdateProgress),
{
    scan_novel_updates_with_workers(save_dir, UPDATE_SCAN_WORKERS, on_progress)
}

/// 可指定并发度的更新扫描；`workers=1` 即串行（一次一本），用于冷启动后台静默刷新以降低资源冲击。
/// 扫描完成后把完整结果落盘为快照，供下次冷启动秒显。
pub fn scan_novel_updates_with_workers<F>(
    save_dir: &Path,
    workers: usize,
    mut on_progress: F,
) -> Result<NovelUpdateScanResult>
where
    F: FnMut(NovelUpdateProgress),
{
    let local_books = collect_local_book_statuses(save_dir)?;
    if local_books.is_empty() {
        return Ok(NovelUpdateScanResult::default());
    }

    let total = local_books.len();
    let now = now_ms();
    let mut cache = load_update_cache(save_dir);
    let mut needs_refresh = Vec::new();
    let mut updates = Vec::new();
    let mut no_updates = Vec::new();
    let mut emitted = HashSet::new();
    let mut scanned = 0usize;

    let by_id: HashMap<String, LocalBookStatus> = local_books
        .iter()
        .cloned()
        .map(|book| (book.book_id.clone(), book))
        .collect();

    for book in &local_books {
        if let Some(cached) = cache.entries.get(&book.book_id) {
            let fresh = now.saturating_sub(cached.checked_ms) <= UPDATE_CACHE_TTL_MS;
            if fresh && cached.remote_total > 0 {
                record_update_row(
                    book,
                    cached.remote_total,
                    cached.finished,
                    total,
                    &mut scanned,
                    &mut emitted,
                    &mut updates,
                    &mut no_updates,
                    &mut on_progress,
                );
                continue;
            }
        }
        needs_refresh.push(book.book_id.clone());
    }

    if !needs_refresh.is_empty() {
        let fetched =
            fetch_remote_totals_streaming(needs_refresh, workers, |book_id, remote_total, finished| {
                cache.entries.insert(
                    book_id.clone(),
                    CachedRemoteTotal {
                        remote_total,
                        checked_ms: now,
                        finished,
                    },
                );

                if let Some(book) = by_id.get(&book_id) {
                    record_update_row(
                        book,
                        remote_total,
                        finished,
                        total,
                        &mut scanned,
                        &mut emitted,
                        &mut updates,
                        &mut no_updates,
                        &mut on_progress,
                    );
                }
            });

        // 如果本轮刷新失败但有旧缓存，先用旧缓存顶上，避免“无结果”导致 UI 看起来像书消失。
        for book in &local_books {
            if emitted.contains(&book.book_id) || fetched.contains_key(&book.book_id) {
                continue;
            }
            if let Some(cached) = cache.entries.get(&book.book_id)
                && cached.remote_total > 0
            {
                record_update_row(
                    book,
                    cached.remote_total,
                    cached.finished,
                    total,
                    &mut scanned,
                    &mut emitted,
                    &mut updates,
                    &mut no_updates,
                    &mut on_progress,
                );
            }
        }

        save_update_cache(save_dir, &cache);
    }

    updates.sort_by_key(|item| Reverse(item.new_count));

    let result = NovelUpdateScanResult {
        updates,
        no_updates,
    };
    save_update_snapshot(
        save_dir,
        &UpdateSnapshot {
            updated_ms: now_ms(),
            updates: result.updates.clone(),
            no_updates: result.no_updates.clone(),
        },
    );
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn record_update_row<F>(
    book: &LocalBookStatus,
    remote_total: usize,
    finished: Option<bool>,
    total: usize,
    scanned: &mut usize,
    emitted: &mut HashSet<String>,
    updates: &mut Vec<NovelUpdateRow>,
    no_updates: &mut Vec<NovelUpdateRow>,
    on_progress: &mut F,
) where
    F: FnMut(NovelUpdateProgress),
{
    if remote_total == 0 || !emitted.insert(book.book_id.clone()) {
        return;
    }

    let row = row_from_book(book, remote_total, finished);
    *scanned += 1;
    on_progress(NovelUpdateProgress {
        row: row.clone(),
        scanned: *scanned,
        total,
    });

    if row.is_ignored || !row.has_update {
        no_updates.push(row);
    } else {
        updates.push(row);
    }
}

fn row_from_book(book: &LocalBookStatus, remote_total: usize, finished: Option<bool>) -> NovelUpdateRow {
    let new_count = remote_total.saturating_sub(book.local_total);
    let has_update = new_count > 0 || book.local_failed > 0;

    NovelUpdateRow {
        book_id: book.book_id.clone(),
        book_name: book.book_name.clone(),
        folder: book.folder.clone(),
        local_total: book.local_total,
        local_failed: book.local_failed,
        remote_total,
        new_count,
        has_update,
        is_ignored: book.is_ignored,
        finished,
    }
}

fn collect_local_book_statuses(save_dir: &Path) -> Result<Vec<LocalBookStatus>> {
    if !save_dir.exists() {
        return Ok(Vec::new());
    }

    let dir_reader =
        fs::read_dir(save_dir).with_context(|| format!("read dir {}", save_dir.display()))?;

    let mut books = Vec::new();
    for entry in dir_reader.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let (book_id, legacy_name) = match parse_book_folder_name(name) {
            Some(v) => v,
            _ => continue,
        };

        // 只扫描真正有状态文件的目录；预览阶段仅有 cover.* 的缓存目录不能被误报为“已下载小说”。
        let Some(status_value) = read_status_json(&path, &book_id) else {
            continue;
        };
        let counts = counts_from_status(&status_value);
        let is_ignored = status_value
            .get("ignore_updates")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let book_name = status_value
            .get("book_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .unwrap_or(legacy_name);
        let (local_total, _local_ok, local_failed) = counts.unwrap_or((0, 0, 0));

        books.push(LocalBookStatus {
            book_id,
            book_name,
            folder: path,
            local_total,
            local_failed,
            is_ignored,
        });
    }

    Ok(books)
}

fn load_update_cache(save_dir: &Path) -> UpdateCacheFile {
    let path = save_dir.join(UPDATE_CACHE_FILE);
    let Ok(raw) = fs::read_to_string(path) else {
        return UpdateCacheFile::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_update_cache(save_dir: &Path, cache: &UpdateCacheFile) {
    let path = save_dir.join(UPDATE_CACHE_FILE);
    let Ok(raw) = serde_json::to_string_pretty(cache) else {
        return;
    };
    let _ = fs::write(path, raw);
}

/// 单本刷新远端章节数（供预览页“信息更新”联动下载库徽标）：强制绕过 TTL 拉取该书最新
/// remote_total，写回逐本缓存，并返回其更新行（本地计数即时重算）。
pub fn refresh_one_remote(save_dir: &Path, book_id: &str) -> Result<Option<NovelUpdateRow>> {
    let Some(book) = collect_local_book_statuses(save_dir)?
        .into_iter()
        .find(|b| b.book_id == book_id)
    else {
        return Ok(None);
    };
    let client = FanqieWebNetwork::new(FanqieWebConfig::default())?;
    let remote_total = client
        .fetch_chapter_list(book_id)
        .map(|l| l.len())
        .unwrap_or(0);
    if remote_total == 0 {
        return Ok(None);
    }
    let finished = client.get_book_info(book_id).8;
    let mut cache = load_update_cache(save_dir);
    cache.entries.insert(
        book_id.to_string(),
        CachedRemoteTotal {
            remote_total,
            checked_ms: now_ms(),
            finished,
        },
    );
    save_update_cache(save_dir, &cache);
    Ok(Some(row_from_book(&book, remote_total, finished)))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn fetch_remote_totals_streaming<F>(
    book_ids: Vec<String>,
    max_workers: usize,
    mut on_result: F,
) -> HashMap<String, Option<bool>>
where
    F: FnMut(String, usize, Option<bool>),
{
    if book_ids.is_empty() {
        return HashMap::new();
    }

    let workers = book_ids.len().clamp(1, max_workers.max(1));
    let queue = Arc::new(Mutex::new(VecDeque::from(book_ids)));
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::with_capacity(workers);

    for _ in 0..workers {
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        handles.push(thread::spawn(move || fetch_remote_totals_worker(queue, tx)));
    }
    drop(tx);

    let mut results = HashMap::new();
    for (book_id, remote_total, finished) in rx {
        results.insert(book_id.clone(), finished);
        on_result(book_id, remote_total, finished);
    }

    for handle in handles {
        let _ = handle.join();
    }

    results
}

fn fetch_remote_totals_worker(
    queue: Arc<Mutex<VecDeque<String>>>,
    tx: mpsc::Sender<(String, usize, Option<bool>)>,
) {
    let Ok(client) = FanqieWebNetwork::new(FanqieWebConfig::default()) else {
        return;
    };
    while let Some(book_id) = queue.lock().ok().and_then(|mut q| q.pop_front()) {
        let total = client
            .fetch_chapter_list(&book_id)
            .map(|list| list.len())
            .filter(|n| *n > 0);
        if let Some(total) = total {
            // #4：同时拉取服务器完结状态（详情页），供后台扫描判定“连载转完结→停扫”。
            let finished = client.get_book_info(&book_id).8;
            let _ = tx.send((book_id, total, finished));
        }
    }
}

fn parse_book_folder_name(name: &str) -> Option<(String, String)> {
    if name.chars().all(|c| c.is_ascii_digit()) {
        return Some((name.to_string(), name.to_string()));
    }

    let (id, title) = name.split_once('_')?;
    if id.chars().all(|c| c.is_ascii_digit()) {
        Some((id.to_string(), title.to_string()))
    } else {
        None
    }
}

/// 读取某本书本地状态文件中 "downloaded" 的统计信息：
/// - total: 条目数（包含失败/空内容的条目）
/// - ok: 成功下载的条目数（content/text 非空）
/// - failed: total - ok
pub fn read_downloaded_counts(folder: &Path, book_id: &str) -> Option<(usize, usize, usize)> {
    let value = read_status_json(folder, book_id)?;
    counts_from_status(&value)
}

/// 仅统计成功下载的章节数（content/text 非空）。
pub fn read_downloaded_ok_count(folder: &Path, book_id: &str) -> Option<usize> {
    let (_total, ok, _failed) = read_downloaded_counts(folder, book_id)?;
    Some(ok)
}

/// 读取书籍的ignore_updates标志
#[allow(dead_code)]
pub fn read_ignore_updates_flag(folder: &Path, book_id: &str) -> bool {
    read_status_json(folder, book_id)
        .and_then(|v| v.get("ignore_updates")?.as_bool())
        .unwrap_or(false)
}

/// 一次性读取下载计数和忽略标志，避免重复读取同一文件。
#[allow(dead_code)]
pub fn read_status_counts_and_ignore(
    folder: &Path,
    book_id: &str,
) -> (Option<(usize, usize, usize)>, bool) {
    match read_status_json(folder, book_id) {
        Some(value) => {
            let counts = counts_from_status(&value);
            let ignored = value
                .get("ignore_updates")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            (counts, ignored)
        }
        None => (None, false),
    }
}

/// 读取并解析 status.json（或旧格式 chapter_status_<id>.json）。
pub(crate) fn read_status_json(folder: &Path, book_id: &str) -> Option<Value> {
    let status_new = folder.join("status.json");
    let status_old = folder.join(format!("chapter_status_{}.json", book_id));
    let path = if status_new.exists() {
        status_new
    } else if status_old.exists() {
        status_old
    } else {
        return None;
    };
    let data = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

/// 从已解析的 status JSON 中提取下载计数。
fn counts_from_status(value: &Value) -> Option<(usize, usize, usize)> {
    let downloaded = value.get("downloaded")?.as_object()?;
    let total = downloaded.len();
    let mut ok = 0usize;
    for (_cid, pair) in downloaded {
        match pair {
            Value::Array(arr) => {
                if arr.get(1).and_then(|v| v.as_str()).is_some() {
                    ok += 1;
                }
            }
            Value::Object(obj)
                if obj
                    .get("content")
                    .or_else(|| obj.get("text"))
                    .and_then(|v| v.as_str())
                    .is_some() =>
            {
                ok += 1;
            }
            _ => {}
        }
    }
    let failed = total.saturating_sub(ok);
    Some((total, ok, failed))
}

// ── #4 连载中书籍后台静默重扫 ───────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ScheduleEntry {
    /// 首次被识别为连载中的时刻（首扫排在其次日）。
    first_seen_ms: u64,
    /// 上次扫描时刻；0 表示登记后尚未扫过（据此排“次日首扫”）。
    last_scan_ms: u64,
    /// 当前扫描间隔（天），1..=10。有更新重置为 1，无更新递增。
    interval_days: u64,
    /// 上次扫描时的远端章节数，用于判断本轮是否有更新。
    last_remote_total: usize,
    /// 服务器完结态缓存。
    finished: Option<bool>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ScheduleFile {
    #[serde(default)]
    entries: HashMap<String, ScheduleEntry>,
}

fn load_schedule(save_dir: &Path) -> ScheduleFile {
    let path = save_dir.join(UPDATE_SCHEDULE_FILE);
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_schedule(save_dir: &Path, sf: &ScheduleFile) {
    let path = save_dir.join(UPDATE_SCHEDULE_FILE);
    if let Ok(raw) = serde_json::to_string_pretty(sf) {
        let _ = fs::write(path, raw);
    }
}

/// 后台扫描一轮：只对“连载中”的书按各自递增周期做单本探测（串行，一次一本）。
/// 返回本轮实际重扫并产生新结果的行，供调用方回写内存快照。规则（严格照需求）：
/// - 只扫连载中（服务器 finished==Some(false)）的书；新入的连载书次日作为第一次扫描。
/// - 有更新（章节数增大）→ 间隔回到 1 天；无更新 → 间隔 +1 天，封顶 10 天。
/// - 服务器完结（finished==true）→ 移出调度，不再扫描。
/// 扫描范围：`collect_local_book_statuses` 仅收录纯数字目录（番茄），故七猫/书旗不在此路径。
pub fn serializing_scan_due(save_dir: &Path) -> Result<Vec<NovelUpdateRow>> {
    let books = collect_local_book_statuses(save_dir)?;
    let now = now_ms();
    let mut schedule = load_schedule(save_dir);
    let mut cache = load_update_cache(save_dir);

    // 1) 依已知完结态登记候选（仅未忽略且确知连载中的书）。
    for book in &books {
        if book.is_ignored {
            schedule.entries.remove(&book.book_id);
            continue;
        }
        if schedule.entries.contains_key(&book.book_id) {
            continue;
        }
        if cache.entries.get(&book.book_id).and_then(|c| c.finished) == Some(false) {
            schedule.entries.insert(
                book.book_id.clone(),
                ScheduleEntry {
                    first_seen_ms: now,
                    last_scan_ms: 0,
                    interval_days: 1,
                    last_remote_total: cache
                        .entries
                        .get(&book.book_id)
                        .map(|c| c.remote_total)
                        .unwrap_or(0),
                    finished: Some(false),
                },
            );
        }
    }

    // 2) 先算出到点的 id（避免边遍历边改 map），再串行逐本探测。
    let due: Vec<String> = schedule
        .entries
        .iter()
        .filter(|(_, e)| e.finished != Some(true))
        .filter(|(_, e)| {
            let next = if e.last_scan_ms == 0 {
                e.first_seen_ms + SCAN_DAY_MS
            } else {
                e.last_scan_ms + e.interval_days.max(1) * SCAN_DAY_MS
            };
            now >= next
        })
        .map(|(k, _)| k.clone())
        .collect();

    if due.is_empty() {
        save_schedule(save_dir, &schedule);
        return Ok(Vec::new());
    }

    let client = FanqieWebNetwork::new(FanqieWebConfig::default())?;
    let mut changed = Vec::new();
    for bid in due {
        let Some(book) = books.iter().find(|b| b.book_id == bid) else {
            schedule.entries.remove(&bid);
            continue;
        };
        let total = client
            .fetch_chapter_list(&bid)
            .map(|l| l.len())
            .filter(|n| *n > 0);
        let Some(total) = total else {
            // 探测失败：仅顺延时间，避免每个 tick 反复重试同一本；不动 last_remote_total/间隔。
            if let Some(e) = schedule.entries.get_mut(&bid) {
                e.last_scan_ms = now;
            }
            continue;
        };
        let finished = client.get_book_info(&bid).8;

        {
            let e = schedule.entries.entry(bid.clone()).or_insert(ScheduleEntry {
                first_seen_ms: now,
                last_scan_ms: 0,
                interval_days: 1,
                last_remote_total: 0,
                finished: None,
            });
            let grew = total > e.last_remote_total;
            if finished == Some(true) {
                e.finished = Some(true);
            } else {
                e.finished = finished;
                if grew {
                    e.interval_days = 1;
                } else {
                    e.interval_days = (e.interval_days.max(1) + 1).min(SCAN_INTERVAL_CAP_DAYS);
                }
            }
            e.last_scan_ms = now;
            e.last_remote_total = total;
        }

        cache.entries.insert(
            bid.clone(),
            CachedRemoteTotal {
                remote_total: total,
                checked_ms: now,
                finished,
            },
        );
        changed.push(row_from_book(book, total, finished));

        if finished == Some(true) {
            schedule.entries.remove(&bid);
        }
    }

    save_update_cache(save_dir, &cache);
    save_schedule(save_dir, &schedule);
    Ok(changed)
}
