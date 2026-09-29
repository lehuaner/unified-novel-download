use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::base_system::book_paths::{
    COVER_IMAGE_EXTENSIONS, book_folder_name, find_existing_cover_file,
};
use crate::base_system::context::safe_fs_name;
use crate::base_system::download_history::read_download_history_deduped;
use crate::base_system::json_extract::to_public_jpeg_cover;
use crate::ui::web::state::{AppState, LibraryScanRow, LibraryScanStore};

#[derive(Debug, Deserialize)]
pub(crate) struct LibraryQuery {
    pub(crate) path: Option<String>,
    /// 按文件名/文件夹名关键词模糊过滤（忽略大小写）
    pub(crate) name: Option<String>,
    /// 是否启动一次新扫描。默认 true；轮询/启动预缓存时会传 false。
    pub(crate) start: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BooksQuery {
    pub(crate) limit: Option<usize>,
    pub(crate) name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CoverQuery {
    pub(crate) book_id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeleteBooksReq {
    /// 要删除的、相对于下载根目录的成品路径列表（一本书的全部输出）。
    pub(crate) paths: Vec<String>,
}

/// 主输出文件的格式优先级（大者优先）。
fn ext_rank(ext: &str) -> u8 {
    match ext {
        "epub" => 4,
        "pdf" => 3,
        "txt" => 2,
        _ => 1,
    }
}

#[derive(Default)]
struct BookAgg {
    stem: String,
    main_rel: String,
    main_ext: String,
    main_rank: u8,
    audio_rel: Option<String>,
    size: u64,
    modified_ms: u64,
    paths: Vec<String>,
}

/// 扫描下载根目录，把同一“书名词干”的成品文件/音频目录聚合为一本书。
/// 以磁盘实际存在的输出为准（book_id 命名的内部缓存目录被隐藏）。
fn scan_books(root: &Path) -> Vec<BookAgg> {
    let mut map: HashMap<String, BookAgg> = HashMap::new();
    let rd = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    for ent in rd.flatten() {
        let path = ent.path();
        let meta = match ent.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let fname = match path.file_name().and_then(|s| s.to_str()) {
            Some(f) => f.to_string(),
            None => continue,
        };
        let is_dir = meta.is_dir();
        let modified_ms = meta.modified().ok().and_then(system_time_ms).unwrap_or(0);
        let size = if is_dir { 0 } else { meta.len() };

        // 分类：得到 (stem, 角色)。role: 0=主, 1=音频, 2=忽略
        let (stem, role, ext_for_main) = if is_dir {
            if is_internal_book_cache_dir(root, &path) {
                continue;
            }
            if let Some(base) = fname.strip_suffix("_audio") {
                (base.to_string(), 1u8, String::new())
            } else if fname.ends_with("_原图") || fname.ends_with("_images") {
                continue;
            } else {
                (fname.clone(), 0u8, String::new()) // 批量 TXT 目录输出
            }
        } else {
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let stem = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            match ext.as_str() {
                "epub" | "pdf" | "txt" => (stem, 0u8, ext),
                "mp3" | "wav" => (stem, 1u8, String::new()),
                _ => continue,
            }
        };

        let a = map.entry(stem.clone()).or_insert_with(|| BookAgg {
            stem: stem.clone(),
            ..Default::default()
        });
        a.paths.push(fname.clone());
        a.modified_ms = a.modified_ms.max(modified_ms);
        a.size += size;
        match role {
            1 => {
                if a.audio_rel.is_none() {
                    a.audio_rel = Some(fname.clone());
                }
            }
            _ => {
                // 主输出：文件用 ext_rank，批量目录给中间优先级 2。
                let rank = if is_dir { 2 } else { ext_rank(&ext_for_main) };
                if rank >= a.main_rank && a.main_rel.is_empty() || rank > a.main_rank {
                    a.main_rel = fname.clone();
                    a.main_ext = ext_for_main.clone();
                    a.main_rank = rank;
                }
            }
        }
    }
    map.into_values().collect()
}

/// `GET /api/library/books`：按磁盘成品扫描的小说库，用历史记录补全元数据。
pub(crate) async fn api_library_books(
    State(state): State<AppState>,
    Query(q): Query<BooksQuery>,
) -> Json<Value> {
    let root = state.library_root.as_ref().clone();
    let limit = q.limit.unwrap_or(500).clamp(1, 2000);
    let name_kw = q.name.clone();
    let books = tokio::task::spawn_blocking(move || build_books(&root, limit, name_kw.as_deref()))
        .await
        .unwrap_or_default();
    Json(json!({ "books": books }))
}

/// `GET /api/library/cover?book_id=...`：返回下载时保存到该书缓存目录的封面图片。
pub(crate) async fn api_library_cover(
    State(state): State<AppState>,
    Query(q): Query<CoverQuery>,
) -> Response {
    let root = state.library_root.as_ref().clone();
    let book_id = q.book_id.clone();
    let found = tokio::task::spawn_blocking(move || {
        let dir = root.join(book_folder_name(&book_id, None));
        find_existing_cover_file(&dir, None).and_then(|p| std::fs::read(&p).ok().map(|b| (p, b)))
    })
    .await
    .ok()
    .flatten();
    match found {
        Some((path, bytes)) => {
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let ct = match ext.as_str() {
                "png" => "image/png",
                "webp" => "image/webp",
                "gif" => "image/gif",
                _ => "image/jpeg",
            };
            let mut resp = Response::new(axum::body::Body::from(bytes));
            *resp.status_mut() = StatusCode::OK;
            if let Ok(v) = HeaderValue::from_str(ct) {
                resp.headers_mut().insert(header::CONTENT_TYPE, v);
            }
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400"),
            );
            resp
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(axum::body::Body::empty())
            .unwrap(),
    }
}

fn build_books(root: &Path, limit: usize, name_kw: Option<&str>) -> Vec<Value> {
    let aggs = scan_books(root);

    // 历史去重记录 → 以 safe 书名匹配，补全封面/作者/评分等。
    let recs = read_download_history_deduped(2000, None);
    let mut hmap: HashMap<String, &crate::base_system::download_history::DownloadHistoryRecord> =
        HashMap::new();
    for r in &recs {
        hmap.entry(safe_fs_name(&r.book_name, "_", 120))
            .or_insert(r);
        hmap.entry(r.book_name.clone()).or_insert(r);
    }

    let kw = name_kw.map(|s| s.to_lowercase());
    let mut out: Vec<Value> = Vec::new();
    let mut aggs = aggs;
    aggs.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms));
    for a in aggs {
        // 主输出：优先正文书，其次音频目录（用 zip）。
        let (main_rel, main_is_dir) = if !a.main_rel.is_empty() {
            (a.main_rel.clone(), a.main_ext.is_empty())
        } else if let Some(ref ar) = a.audio_rel {
            (ar.clone(), true)
        } else {
            continue;
        };
        // 目录主输出（批量 TXT / 音频夹）走打包下载；单文件直接下。
        let dl_as_zip = main_is_dir;

        let rec = hmap.get(&a.stem).copied();
        // 目录型主输出（批量 TXT / 音频夹）名字与系统/仓库目录（.git、target、logs…）
        // 无法区分，仅当能匹配到下载历史（确为已下载小说）时才计入，避免噪声。
        // 单文件（epub/pdf/txt）文件名即书名，始终保留。
        if main_is_dir && rec.is_none() {
            continue;
        }
        let title = rec
            .map(|r| r.book_name.clone())
            .unwrap_or_else(|| a.stem.clone());
        if let Some(ref k) = kw
            && !title.to_lowercase().contains(k)
            && !a.stem.to_lowercase().contains(k)
        {
            continue;
        }
        let author = rec.map(|r| r.author.clone()).unwrap_or_default();
        // 封面优先用下载时已存到缓存目录(<book_id>/cover.*)的图片；
        // 其次回退下载历史里的 cover_url（归一化为公网 JPEG）。
        let mut cover_url = None;
        if let Some(r) = rec {
            if !r.book_id.trim().is_empty() {
                let dir = root.join(book_folder_name(&r.book_id, None));
                if find_existing_cover_file(&dir, Some(&r.book_name)).is_some() {
                    cover_url = Some(format!("/api/library/cover?book_id={}", r.book_id));
                }
            }
            if cover_url.is_none() && !r.cover_url.trim().is_empty() {
                cover_url = Some(to_public_jpeg_cover(&r.cover_url));
            }
        }

        out.push(json!({
            "stem": a.stem,
            "book_id": rec.map(|r| r.book_id.clone()).unwrap_or_default(),
            "title": title,
            "author": author,
            "cover_url": cover_url,
            "category": rec.map(|r| r.category.clone()).unwrap_or_default(),
            "description": rec.map(|r| r.description.clone()).unwrap_or_default(),
            "score": rec.and_then(|r| r.score),
            "word_count": rec.and_then(|r| r.word_count),
            "chapter_count": rec.map(|r| r.selected_chapters).filter(|n| *n > 0),
            "finished": rec.and_then(|r| r.finished),
            "read_count_text": rec.map(|r| r.read_count_text.clone()).unwrap_or_default(),
            "format": a.main_ext,
            "main_rel": main_rel,
            "main_is_dir": main_is_dir,
            "dl_zip": dl_as_zip,
            "has_audio": a.audio_rel.is_some(),
            "audio_rel": a.audio_rel,
            "size": a.size,
            "modified_ms": a.modified_ms,
            "paths": a.paths,
        }));
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// `POST /api/library/delete`：删除一本书的全部成品（仅限根目录下一层，防越权）。
pub(crate) async fn api_library_delete(
    State(state): State<AppState>,
    Json(req): Json<DeleteBooksReq>,
) -> Result<Json<Value>, axum::http::StatusCode> {
    let root = state.library_root.as_ref().clone();
    let n = tokio::task::spawn_blocking(move || delete_paths(&root, &req.paths))
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({ "ok": true, "deleted": n })))
}

fn delete_paths(root: &Path, paths: &[String]) -> usize {
    let root_canon = match std::fs::canonicalize(root) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let mut n = 0;
    for rel in paths {
        let rel_clean = rel.replace('\\', "/").trim_matches('/').to_string();
        // 仅允许根目录下一层的普通名，禁止分隔符/.. 越权。
        if rel_clean.is_empty() || rel_clean.contains('/') || rel_clean.contains("..") {
            continue;
        }
        let target = root_canon.join(&rel_clean);
        let canon = match std::fs::canonicalize(&target) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !canon.starts_with(&root_canon) || canon == root_canon {
            continue;
        }
        let removed = if canon.is_dir() {
            std::fs::remove_dir_all(&canon).is_ok()
        } else {
            std::fs::remove_file(&canon).is_ok()
        };
        if removed {
            n += 1;
        }
    }
    n
}

pub(crate) async fn api_library(
    State(state): State<AppState>,
    Query(q): Query<LibraryQuery>,
) -> Json<Value> {
    let base = state.library_root.clone();
    let rel = normalize_rel(q.path.unwrap_or_default());
    let should_start = q.start.unwrap_or(true) || state.library_scan.snapshot(&rel).is_none();

    if should_start {
        spawn_library_scan(
            base.as_ref().clone(),
            rel.clone(),
            state.library_scan.clone(),
        );
    }

    let snapshot = state
        .library_scan
        .snapshot(&rel)
        .unwrap_or_else(|| empty_snapshot(rel.clone()));
    let mut items = snapshot.items;

    if let Some(ref kw) = q.name {
        let kw_lower = kw.to_lowercase();
        items.retain(|i| i.name.to_lowercase().contains(&kw_lower));
    }

    Json(json!({
        "root": base.to_string_lossy(),
        "path": snapshot.path,
        "items": items,
        "running": snapshot.running,
        "scanned": snapshot.scanned,
        "error": snapshot.error,
        "started_ms": snapshot.started_ms,
        "updated_ms": snapshot.updated_ms,
    }))
}

pub(crate) fn spawn_library_scan(root: PathBuf, rel: String, store: Arc<LibraryScanStore>) {
    let rel = normalize_rel(rel);
    if !store.try_start(rel.clone()) {
        return;
    }

    thread::spawn(move || {
        if let Err(err) = scan_library_streaming(&root, &rel, &store) {
            store.finish_failed(&rel, err.to_string());
        }
    });
}

fn normalize_rel(rel: String) -> String {
    rel.replace('\\', "/").trim_matches('/').to_string()
}

fn empty_snapshot(path: String) -> crate::ui::web::state::LibraryScanInfo {
    crate::ui::web::state::LibraryScanInfo {
        path,
        running: false,
        scanned: 0,
        items: Vec::new(),
        error: None,
        started_ms: 0,
        updated_ms: 0,
    }
}

fn is_allowed_ext(ext: &str) -> bool {
    matches!(ext, "epub" | "txt" | "mp3" | "wav")
}

fn scan_library_streaming(root: &Path, rel: &str, store: &LibraryScanStore) -> std::io::Result<()> {
    let root_canon = std::fs::canonicalize(root)?;

    let target = if rel.trim().is_empty() {
        root_canon.clone()
    } else {
        let joined = root_canon.join(rel);
        let canon = std::fs::canonicalize(&joined)?;
        if !canon.starts_with(&root_canon) {
            store.finish(rel, Vec::new());
            return Ok(());
        }
        canon
    };

    let mut items = Vec::new();
    let mut scanned = 0usize;
    for entry in std::fs::read_dir(&target)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let Some(item) = item_from_entry(&root_canon, &path, &meta) else {
            continue;
        };
        scanned += 1;
        store.push_item(rel, item.clone(), scanned);
        items.push(item);
    }

    store.finish(rel, items);
    Ok(())
}

fn item_from_entry(root: &Path, path: &Path, meta: &std::fs::Metadata) -> Option<LibraryScanRow> {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let rel_path = rel.to_string_lossy().replace('\\', "/");
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&rel_path)
        .to_string();
    let modified_ms = meta.modified().ok().and_then(system_time_ms);

    if meta.is_dir() {
        if is_internal_book_cache_dir(root, path) {
            return None;
        }
        return Some(LibraryScanRow {
            kind: "dir".to_string(),
            name,
            rel_path,
            ext: String::new(),
            // 不再逐个目录统计子文件，避免书很多时根目录读取被每本书的子目录 IO 放大。
            size: 0,
            file_count: None,
            modified_ms,
        });
    }

    if !meta.is_file() {
        return None;
    }

    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !is_allowed_ext(&ext) {
        return None;
    }

    Some(LibraryScanRow {
        kind: "file".to_string(),
        name,
        rel_path,
        ext,
        size: meta.len(),
        file_count: None,
        modified_ms,
    })
}

fn is_internal_book_cache_dir(root: &Path, path: &Path) -> bool {
    if path.parent() != Some(root) {
        return false;
    }

    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };

    // 缓存目录只以 book_id 命名（可能带 `_书名` 后缀）：
    //   - 纯数字：7410970834589715518
    //   - 带源前缀（safe_fs_name 会把半角 : 转成全角 ：）：sq：8554746 / qm：xxxx
    // 这类目录是下载中间缓存，不属于“下载结果”，应一律隐藏。
    let head = name.split_once('_').map_or(name, |(a, _)| a);
    let normalized = head.replace('\u{FF1A}', ":");
    let (prefix, digits) = match normalized.split_once(':') {
        Some((p, d)) => (p.to_ascii_lowercase(), d),
        None => (String::new(), normalized.as_str()),
    };
    let looks_like_book_id = !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && (prefix.is_empty() || prefix == "sq" || prefix == "qm");
    if looks_like_book_id {
        return true;
    }

    // 旧版：带缓存标记文件的目录也视为内部缓存。
    path.join("status.json").is_file()
        || path.join("downloaded_chapters.jsonl").is_file()
        || path.join(format!("chapter_status_{head}.json")).is_file()
        || COVER_IMAGE_EXTENSIONS
            .iter()
            .any(|ext| path.join(format!("cover.{ext}")).is_file())
        || path.join("images").is_dir()
}

fn system_time_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::{LibraryScanStore, scan_library_streaming};

    #[test]
    fn root_scan_hides_book_cache_but_keeps_downloadable_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let cache = root.join("7047850786264449550");
        let audio = root.join("测试书_audio");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&audio).unwrap();
        std::fs::write(cache.join("status.json"), "{}\n").unwrap();
        std::fs::write(root.join("测试书.txt"), "content").unwrap();
        std::fs::write(audio.join("001.mp3"), "audio").unwrap();

        let store = LibraryScanStore::default();
        scan_library_streaming(root, "", &store).unwrap();
        let root_items = store.snapshot("").unwrap().items;
        let root_names: Vec<_> = root_items.iter().map(|item| item.name.as_str()).collect();

        assert!(!root_names.contains(&"7047850786264449550"));
        assert!(root_names.contains(&"测试书.txt"));
        assert!(root_names.contains(&"测试书_audio"));

        scan_library_streaming(root, "测试书_audio", &store).unwrap();
        let audio_items = store.snapshot("测试书_audio").unwrap().items;
        assert_eq!(audio_items.len(), 1);
        assert_eq!(audio_items[0].name, "001.mp3");
    }

    #[test]
    fn root_scan_hides_legacy_book_cache_with_status_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let cache = root.join("7047850786264449550_旧书名");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("status.json"), "{}\n").unwrap();

        let store = LibraryScanStore::default();
        scan_library_streaming(root, "", &store).unwrap();

        assert!(store.snapshot("").unwrap().items.is_empty());
    }
}
