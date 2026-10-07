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

use crate::base_system::book_paths::book_folder_name;
use crate::base_system::download_history;
use crate::network_parser::network::{FanqieWebConfig, FanqieWebNetwork};
#[cfg(feature = "qimao")]
use crate::qimao::QimaoClient;
#[cfg(feature = "shuqi")]
use crate::shuqi::ShuqiClient;

/// 更新检测的书籍来源，决定“远端章节数/完结态”走哪条抓取通路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookSource {
    Fanqie,
    Qimao,
    Shuqi,
}

/// 由 book_id 判定来源：`sq:` 书旗、`qm:` 七猫，其余（纯数字）为番茄。
pub fn source_of(book_id: &str) -> BookSource {
    if book_id.starts_with("sq:") {
        BookSource::Shuqi
    } else if book_id.starts_with("qm:") {
        BookSource::Qimao
    } else {
        BookSource::Fanqie
    }
}

/// 按来源抓取远端 `(章节总数, 服务器完结态)`。
/// 番茄走 web 目录接口 + 详情页；七猫走目录 + 详情（`is_over`）；
/// 书旗 catalog 只给章节数（实测无 `isFinish`），完结态靠网关搜索按 bookId 对齐兜底。
/// `book_name` 仅书旗兜底时需要。抓不到有效总数返回 `None`，调用方据此保留旧缓存，
/// 不把 0 当成扫描结果。
pub fn fetch_remote_state(book_id: &str, book_name: Option<&str>) -> Option<(usize, Option<bool>)> {
    match source_of(book_id) {
        BookSource::Fanqie => {
            let client = FanqieWebNetwork::new(FanqieWebConfig::default()).ok()?;
            let total = client
                .fetch_chapter_list(book_id)
                .map(|l| l.len())
                .filter(|n| *n > 0)?;
            // 同时拉详情页完结态，供“连载转完结→停扫”与手动回队判定。
            Some((total, client.get_book_info(book_id).8))
        }
        BookSource::Qimao => fetch_qimao_state(book_id),
        BookSource::Shuqi => fetch_shuqi_state(book_id, book_name),
    }
}

/// 七猫：目录取章节数 + 详情取完结态。未启用 `qimao` feature 时不报错，当作“无从校验”。
#[cfg(feature = "qimao")]
fn fetch_qimao_state(book_id: &str) -> Option<(usize, Option<bool>)> {
    let client = QimaoClient::new(UPDATE_FETCH_TIMEOUT_SECS).ok()?;
    let total = client.chapter_list(book_id).ok()?.len();
    if total == 0 {
        return None;
    }
    let finished = client.book_meta(book_id).and_then(|m| m.finished);
    Some((total, finished))
}

#[cfg(not(feature = "qimao"))]
fn fetch_qimao_state(_book_id: &str) -> Option<(usize, Option<bool>)> {
    None
}

/// 书旗：catalog 取章节数；完结态 catalog 接口不供（实测无 `isFinish`、`state` 为 null），
/// 因此再走一次网关搜索按 bookId 对齐取 `state`。未启用 `shuqi` feature 时当作“无从校验”。
#[cfg(feature = "shuqi")]
fn fetch_shuqi_state(book_id: &str, book_name: Option<&str>) -> Option<(usize, Option<bool>)> {
    let client = ShuqiClient::new(UPDATE_FETCH_TIMEOUT_SECS).ok()?;
    let catalog = client.catalog(book_id).ok()?;
    let total = catalog.chapters.len();
    if total == 0 {
        return None;
    }
    let finished = catalog
        .finished
        .or_else(|| book_name.and_then(|n| client.book_state_by_search(book_id, n)));
    Some((total, finished))
}

#[cfg(not(feature = "shuqi"))]
fn fetch_shuqi_state(_book_id: &str, _book_name: Option<&str>) -> Option<(usize, Option<bool>)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_save_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("undl_novel_updates_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp save dir");
        dir
    }

    /// 把 `(String, String)` 结果转成可比较的 owned 元组，避开 tuple 无 Deref 的写法。
    fn parsed(name: &str) -> Option<(String, String)> {
        parse_book_folder_name(name)
    }

    #[test]
    fn parse_folder_name_covers_three_sources_and_legacy() {
        // 番茄：纯数字目录
        assert_eq!(
            parsed("7496026299845053465"),
            Some((
                "7496026299845053465".to_string(),
                "7496026299845053465".to_string()
            ))
        );
        // 书旗/七猫：safe_fs_name 把 `:` 转成全角 `：`，目录名形如 sq：9349463
        assert_eq!(
            parsed("sq：9349463"),
            Some(("sq:9349463".to_string(), "sq：9349463".to_string()))
        );
        assert_eq!(
            parsed("qm：195631"),
            Some(("qm:195631".to_string(), "qm：195631".to_string()))
        );
        // 兼容半角冒号与旧版 `_<书名>` 后缀
        assert_eq!(
            parsed("sq:8554746"),
            Some(("sq:8554746".to_string(), "sq:8554746".to_string()))
        );
        assert_eq!(
            parsed("qm：111_some book"),
            Some(("qm:111".to_string(), "qm：111_some book".to_string()))
        );
        // 旧版番茄：<数字>_<书名>
        assert_eq!(
            parsed("7020269838396296228_重启人生"),
            Some(("7020269838396296228".to_string(), "重启人生".to_string()))
        );
        // 非书籍目录必须被拒（不能把 target/logs/.cargo 当成书）
        assert_eq!(parsed("target"), None);
        assert_eq!(parsed("logs"), None);
        assert_eq!(parsed("sq：abc"), None);
        assert_eq!(parsed("_123"), None);
    }

    #[test]
    fn source_of_maps_prefixes() {
        assert_eq!(source_of("7496026299845053465"), BookSource::Fanqie);
        assert_eq!(source_of("sq:9349463"), BookSource::Shuqi);
        assert_eq!(source_of("qm:195631"), BookSource::Qimao);
    }

    #[test]
    fn stale_row_without_state_dir_is_recomputed_or_dropped() {
        let dir = temp_save_dir("prune");
        let _ = fs::create_dir_all(&dir);

        // A：状态目录存在且有 status.json → 原样保留（交给正常扫描重算）
        let keep_id = "7000000000000000001";
        let keep_dir = dir.join(keep_id);
        fs::create_dir_all(&keep_dir).unwrap();
        fs::write(
            keep_dir.join("status.json"),
            format!(
                r#"{{"book_id":"{keep_id}","book_name":"保留","downloaded":{{"1":["t","c"]}}}}"#
            ),
        )
        .unwrap();

        // B：目录已被删（auto_clear_dump 场景）、且无成功历史可兜底 → 剔除
        let drop_id = "9999999999999999999";

        let mut updates = vec![
            NovelUpdateRow {
                book_id: keep_id.to_string(),
                book_name: "保留".to_string(),
                folder: keep_dir.clone(),
                local_total: 1,
                local_failed: 0,
                remote_total: 1,
                new_count: 0,
                has_update: false,
                is_ignored: false,
                finished: Some(true),
            },
            NovelUpdateRow {
                book_id: drop_id.to_string(),
                book_name: "幽灵书".to_string(),
                folder: dir.join(drop_id),
                local_total: 732,
                local_failed: 0,
                remote_total: 1282,
                new_count: 550,
                has_update: true,
                is_ignored: false,
                finished: Some(true),
            },
        ];
        let mut no_updates = Vec::new();
        let changed = recompute_or_prune_rows(&dir, &mut updates, &mut no_updates);

        assert_eq!(changed, 1, "只修正目录消失的那行");
        assert_eq!(
            updates.len() + no_updates.len(),
            1,
            "不可校验的幽灵行被剔除，保留行仍在"
        );
        let survived = updates
            .iter()
            .chain(no_updates.iter())
            .find(|r| r.book_id == keep_id);
        assert!(survived.is_some(), "状态目录尚存的书不能被误删");
        assert!(
            updates
                .iter()
                .chain(no_updates.iter())
                .all(|r| r.book_id != drop_id),
            "无本地进度也无远端缓存的行必须被剔除"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finished_book_with_history_backfill_yields_no_update() {
        let dir = temp_save_dir("backfill");
        let book_id = "7000000000000000002";
        // 逐本缓存已知远端 1282 章
        let mut cache = UpdateCacheFile::default();
        cache.entries.insert(
            book_id.to_string(),
            CachedRemoteTotal {
                remote_total: 1282,
                checked_ms: now_ms(),
                finished: Some(true),
            },
        );
        save_update_cache(&dir, &cache);

        // 状态目录已不存在，但本地进度可由下载历史兜底时，重算应得出 new_count=0。
        // 下载历史为全局文件，单测不能依赖其内容，故直接验证 row_from_book 口径：
        // 本地=1282 / 远端=1282 → 无更新。
        let book = LocalBookStatus {
            book_id: book_id.to_string(),
            book_name: "已完本".to_string(),
            folder: dir.join(book_id),
            local_total: 1282,
            local_failed: 0,
            is_ignored: false,
        };
        let row = row_from_book(&book, 1282, Some(true));
        assert_eq!(row.new_count, 0);
        assert!(!row.has_update);
        let _ = fs::remove_dir_all(&dir);
    }
}

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
/// 七猫/书旗更新检测的 HTTP 超时（与番茄 `FanqieWebConfig::default()` 保持一致口径）。
/// 两个源均未启用时不会被引用，故随 feature 条件保留。
#[cfg(any(feature = "qimao", feature = "shuqi"))]
const UPDATE_FETCH_TIMEOUT_SECS: u64 = 15;
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

/// 把当前扫描结果落盘为冷启动快照。
/// Web 侧在单本刷新 / 增量扫描 / 幽灵行剔除后都要调它，否则内存修正不会持久化，
/// 服务重启会再次从旧快照载入陈旧徽标。
pub fn persist_update_snapshot(
    save_dir: &Path,
    updates: Vec<NovelUpdateRow>,
    no_updates: Vec<NovelUpdateRow>,
) {
    save_update_snapshot(
        save_dir,
        &UpdateSnapshot {
            updated_ms: now_ms(),
            updates,
            no_updates,
        },
    );
}

/// 陈旧行修正：对快照中“本地已无状态目录”的行做一次性不触网的处理。
///
/// 背景：完结全成功后 `auto_clear_dump` 会删除状态目录，该书从此脱离扫描集合，
/// 旧行（如 `local=732 / remote=1282 / new_count=550`）再也没人重算，会长期挂着假徽标。
/// 处理规则：
/// - 状态目录仍在 → 原样保留（正常扫描路径会重算它）；
/// - 目录已没了但本地进度可用下载历史兜底、且逐本缓存有远端章节数 → 用两者原地重算
///   （全量下完的书自然得出 `new_count=0`，从而落入 no_updates，徽标消失）；
/// - 两个口径都取不到 → 剔除该行（已无从校验，留着只会误导）。
///
/// 返回本轮被修正/剔除的行数，调用方据此决定是否落盘快照。不发起任何网络请求。
pub fn recompute_or_prune_rows(
    save_dir: &Path,
    updates: &mut Vec<NovelUpdateRow>,
    no_updates: &mut Vec<NovelUpdateRow>,
) -> usize {
    let cache = load_update_cache(save_dir);
    let mut pending: Vec<NovelUpdateRow> = Vec::with_capacity(updates.len() + no_updates.len());
    pending.append(updates);
    pending.append(no_updates);

    let mut changed = 0usize;
    let mut kept: Vec<NovelUpdateRow> = Vec::with_capacity(pending.len());
    for row in pending {
        let dir = if row.folder.is_absolute() {
            row.folder.clone()
        } else {
            save_dir.join(&row.folder)
        };
        if read_status_json(&dir, &row.book_id).is_some() {
            kept.push(row);
            continue;
        }
        changed += 1;
        let Some(book) = local_status_for(save_dir, &row.book_id) else {
            continue;
        };
        let Some(cached) = cache.entries.get(&row.book_id) else {
            continue;
        };
        let mut recomputed = row_from_book(&book, cached.remote_total, cached.finished);
        recomputed.folder = dir;
        kept.push(recomputed);
    }

    updates.clear();
    no_updates.clear();
    for row in kept {
        if row.is_ignored || !row.has_update {
            no_updates.push(row);
        } else {
            updates.push(row);
        }
    }
    updates.sort_by_key(|item| Reverse(item.new_count));
    changed
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
        needs_refresh.push((book.book_id.clone(), Some(book.book_name.clone())));
    }

    if !needs_refresh.is_empty() {
        let fetched = fetch_remote_totals_streaming(
            needs_refresh,
            workers,
            |book_id, remote_total, finished| {
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
            },
        );

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

fn row_from_book(
    book: &LocalBookStatus,
    remote_total: usize,
    finished: Option<bool>,
) -> NovelUpdateRow {
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

/// 取某本书的本地进度快照：优先读状态目录（status.json）；目录已不存在或已无 status.json 时
/// （完结全成功后 `auto_clear_dump` 会删掉整个目录）回退到下载历史里最近一次成功记录的章节数。
/// 两条通路都取不到 → None（宁可不报，也不把“进度未知”当成“0 章已下”而报出一大拨假可更新）。
fn local_status_for(save_dir: &Path, book_id: &str) -> Option<LocalBookStatus> {
    if let Some(b) = collect_local_book_statuses(save_dir)
        .ok()
        .and_then(|v| v.into_iter().find(|x| x.book_id == book_id))
    {
        return Some(b);
    }
    let rec = download_history::read_download_history_deduped(0, None)
        .into_iter()
        .find(|r| r.book_id == book_id && r.status == "success")?;
    Some(LocalBookStatus {
        book_id: rec.book_id.clone(),
        book_name: rec.book_name.clone(),
        folder: save_dir.join(book_folder_name(book_id, None)),
        local_total: rec.success_chapters,
        local_failed: 0,
        is_ignored: false,
    })
}

/// 把某本书重新纳入递增调度观察：间隔重置 1 天并打 `watch` 位，
/// 使其即使服务器已完结也会被继续复查（作者补章/重新连载场景）。
fn rearm_watch(save_dir: &Path, book_id: &str, total: usize, finished: Option<bool>) {
    let now = now_ms();
    let mut sf = load_schedule(save_dir);
    let e = sf
        .entries
        .entry(book_id.to_string())
        .or_insert(ScheduleEntry {
            first_seen_ms: now,
            last_scan_ms: now,
            interval_days: 1,
            last_remote_total: total,
            finished,
            watch: true,
        });
    e.watch = true;
    e.interval_days = 1;
    e.last_scan_ms = now;
    e.last_remote_total = total;
    e.finished = finished;
    save_schedule(save_dir, &sf);
}

/// 单本刷新远端章节数（预览页“信息更新”与下载库“查更新”共用）：强制绕过 TTL 拉取该书最新
/// remote_total，本地进度用 `local_status_for`（无状态目录时用下载历史兜底），回写逐本缓存；
/// 若章节数比缓存增长（作者补章/复更）则把该书重新纳入递增调度。
pub fn refresh_one_remote(save_dir: &Path, book_id: &str) -> Result<Option<NovelUpdateRow>> {
    let Some(book) = local_status_for(save_dir, book_id) else {
        return Ok(None);
    };
    let Some((remote_total, finished)) = fetch_remote_state(book_id, Some(book.book_name.as_str()))
    else {
        return Ok(None);
    };

    let mut cache = load_update_cache(save_dir);
    let prev_total = cache
        .entries
        .get(book_id)
        .map(|c| c.remote_total)
        .unwrap_or(0);
    cache.entries.insert(
        book_id.to_string(),
        CachedRemoteTotal {
            remote_total,
            checked_ms: now_ms(),
            finished,
        },
    );
    save_update_cache(save_dir, &cache);

    // 增长才回队（无增长保持“完结即停扫”，不把完结书无意义地拉回调度）。
    if prev_total > 0 && remote_total > prev_total {
        rearm_watch(save_dir, book_id, remote_total, finished);
    }
    Ok(Some(row_from_book(&book, remote_total, finished)))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 待刷新队列元素：`(book_id, 书名)`。书名当前仅书旗用于“目录接口不供完结态”时的
/// 网关搜索兜底，其余来源忽略该字段。
type PendingRefreshItem = (String, Option<String>);

fn fetch_remote_totals_streaming<F>(
    books: Vec<PendingRefreshItem>,
    max_workers: usize,
    mut on_result: F,
) -> HashMap<String, Option<bool>>
where
    F: FnMut(String, usize, Option<bool>),
{
    if books.is_empty() {
        return HashMap::new();
    }

    let workers = books.len().clamp(1, max_workers.max(1));
    let queue = Arc::new(Mutex::new(VecDeque::from(books)));
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
    queue: Arc<Mutex<VecDeque<PendingRefreshItem>>>,
    tx: mpsc::Sender<(String, usize, Option<bool>)>,
) {
    while let Some((book_id, book_name)) = queue.lock().ok().and_then(|mut q| q.pop_front()) {
        // 按来源分派抓取；番茄仍复用带节流的 web 目录通路。
        if let Some((total, finished)) = fetch_remote_state(&book_id, book_name.as_deref()) {
            let _ = tx.send((book_id, total, finished));
        }
    }
}

/// 缓存目录名 → `(book_id, 展示名)`。
///
/// 兼容三类：纯数字（番茄）、`sq：/qm：`（书旗/七猫，`safe_fs_name` 会把 `:` 转成全角 `：`，
/// 故半角/全角都吃下，并可带旧版 `_<书名>` 后缀）、以及旧版 `<数字>_<书名>`。
fn parse_book_folder_name(name: &str) -> Option<(String, String)> {
    if name.chars().all(|c| c.is_ascii_digit()) && !name.is_empty() {
        return Some((name.to_string(), name.to_string()));
    }

    for (tag, canonical) in [("sq", "sq:"), ("qm", "qm:")] {
        for sep in ['：', ':'] {
            let Some(rest) = name.strip_prefix(&format!("{tag}{sep}")) else {
                continue;
            };
            let digits = rest.split('_').next().unwrap_or("").trim();
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                return Some((format!("{canonical}{digits}"), name.to_string()));
            }
        }
    }

    let (id, title) = name.split_once('_')?;
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
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
    /// 手动观察位：用户在下载库/预览页点“查更新”且发现章节数增长时置位。
    /// 置位的书**即使服务器已完结也继续按递增周期复查**（作者可能补章/重新连载）。
    #[serde(default)]
    watch: bool,
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

/// 后台扫描一轮：对“连载中”的书按各自递增周期做单本探测（串行，一次一本），
/// 外加“手动查更新后回队的书”（`watch=true`，完结也不停）。
/// 返回本轮实际重扫并产生新结果的行，供调用方回写内存快照。规则：
/// - 自动登记只针对连载中（服务器 finished==Some(false)）的书；新入的连载书次日作为第一次扫描。
/// - 有更新（章节数增大）→ 间隔回到 1 天；无更新 → 间隔 +1 天，封顶 10 天。
/// - 服务器完结（finished==true）且非手动观察 → 移出调度，不再扫描。
/// - 手动观察（watch）的书完结后仍按递增周期复查，直到用户忽略更新或目录消失。
///
/// 扫描范围：`collect_local_book_statuses` 收录番茄/七猫/书旗三源的状态目录；
/// 目录已被 `auto_clear_dump` 清理的书用下载历史兜底本地进度。
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
                    watch: false,
                },
            );
        }
    }

    // 2) 先算出到点的 id（避免边遍历边改 map），再串行逐本探测。
    // `watch` 条目不受完结态限制（作者补章/复更场景需继续复查）。
    let due: Vec<String> = schedule
        .entries
        .iter()
        .filter(|(_, e)| e.finished != Some(true) || e.watch)
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

    let mut changed = Vec::new();
    for bid in due {
        // 状态目录已被清理时（完结全成功）用下载历史兜底，保证手动回队的书仍能被复查。
        let found = books
            .iter()
            .find(|b| b.book_id == bid)
            .cloned()
            .or_else(|| local_status_for(save_dir, &bid));
        let Some(book) = found else {
            schedule.entries.remove(&bid);
            continue;
        };
        let Some((total, finished)) = fetch_remote_state(&bid, Some(book.book_name.as_str()))
        else {
            // 探测失败：仅顺延时间，避免每个 tick 反复重试同一本；不动 last_remote_total/间隔。
            if let Some(e) = schedule.entries.get_mut(&bid) {
                e.last_scan_ms = now;
            }
            continue;
        };

        {
            let e = schedule
                .entries
                .entry(bid.clone())
                .or_insert(ScheduleEntry {
                    first_seen_ms: now,
                    last_scan_ms: 0,
                    interval_days: 1,
                    last_remote_total: 0,
                    finished: None,
                    watch: false,
                });
            let grew = total > e.last_remote_total;
            // 连载中、以及“完结但手动观察”的书：有增长→重置 1 天，无增长→递增封顶。
            if finished != Some(true) || e.watch {
                if grew {
                    e.interval_days = 1;
                } else {
                    e.interval_days = (e.interval_days.max(1) + 1).min(SCAN_INTERVAL_CAP_DAYS);
                }
            }
            e.finished = finished;
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
        changed.push(row_from_book(&book, total, finished));

        // 完结且非手动观察 → 移出调度（保持“完结即停扫”）。
        let keep_watch = schedule.entries.get(&bid).map(|e| e.watch).unwrap_or(false);
        if finished == Some(true) && !keep_watch {
            schedule.entries.remove(&bid);
        }
    }

    save_update_cache(save_dir, &cache);
    save_schedule(save_dir, &schedule);
    Ok(changed)
}
