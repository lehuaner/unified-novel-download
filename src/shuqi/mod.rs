//! 书旗（Shuqi）小说提供商。
//!
//! 通过 `sq:<bookId>` 前缀路由，独立于番茄官方 API。
//! 所有接口均使用公开 web 端点，无需登录、无需 anti-bot 头。
//!
//! # 接口
//! - 搜索：`read.xiaoshuo1-sm.com/novel/i.php?do=is_search`
//! - 目录：`content.shuqireader.com/openapi/book/chapterlist`
//! - 正文：`c13.shuqireader.com/pcapi/chapter/contentfree/{suffix}`
//!
//! # 签名
//! - 目录/书信息：`md5(bookId + timestamp + user_id + SKEY)`
//! - 搜索：`md5(timestamp)`

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use crossbeam_channel as channel;
use md5::{Digest, Md5};
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;
use tracing::{debug, info, warn};

use crate::base_system::book_paths;
use crate::base_system::context::Config;
use crate::download::downloader::build_dynamic_chapter_groups;
use crate::download::models::{
    BookMeta, ChapterRef, DownloadPlan, DownloadResult, merge_meta_prefer_hint_name,
};

// ── 常量 ──────────────────────────────────────────────────────

const SKEY: &str = "37e81a9d8f02596e1b895d07c171d5c9";
const USER_ID: &str = "8000000";

const URL_SEARCH: &str = "https://read.xiaoshuo1-sm.com/novel/i.php";
const URL_BOOK_INFO: &str = "https://content.shuqireader.com/openapi/book/info";
const URL_CATALOG: &str = "https://content.shuqireader.com/openapi/book/chapterlist";
const URL_CONTENT: &str = "https://c13.shuqireader.com/pcapi/chapter/contentfree/";

// ── book_id 识别 ──────────────────────────────────────────────

static RE_SQ_PREFIX: OnceLock<Regex> = OnceLock::new();
static RE_SQ_BID_QS: OnceLock<Regex> = OnceLock::new();
static RE_SQ_BOOK_PATH: OnceLock<Regex> = OnceLock::new();

fn re_sq_prefix() -> &'static Regex {
    RE_SQ_PREFIX.get_or_init(|| Regex::new(r"(?i)^sq:(\d+)$").expect("regex"))
}
fn re_sq_bid_qs() -> &'static Regex {
    RE_SQ_BID_QS.get_or_init(|| Regex::new(r"(?i)[?&]bid=(\d+)").expect("regex"))
}
fn re_sq_book_path() -> &'static Regex {
    RE_SQ_BOOK_PATH.get_or_init(|| Regex::new(r"(?i)/(?:book|reader)/(\d+)").expect("regex"))
}

/// 判断 book_id 是否为书旗来源（`sq:` 前缀）。
pub fn is_shuqi_book_id(book_id: &str) -> bool {
    book_id.starts_with("sq:")
}

/// 从用户输入中识别书旗 book_id，返回规范化的 `sq:<digits>`。
/// 支持：`sq:8969239`、`https://www.shuqi.com/reader?bid=8969239`、`https://www.shuqi.com/book/8969239`。
pub fn normalize_book_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    // sq: 前缀
    if let Some(caps) = re_sq_prefix().captures(trimmed) {
        return Some(format!("sq:{}", &caps[1]));
    }

    // URL：仅处理包含 shuqi.com 的链接
    let lower = trimmed.to_lowercase();
    if !lower.contains("shuqi.com") {
        return None;
    }

    if let Some(caps) = re_sq_bid_qs().captures(trimmed) {
        return Some(format!("sq:{}", &caps[1]));
    }
    if let Some(caps) = re_sq_book_path().captures(trimmed) {
        return Some(format!("sq:{}", &caps[1]));
    }
    None
}

/// 从 `sq:<digits>` 中提取纯数字 bookId。
fn strip_sq_prefix(book_id: &str) -> &str {
    book_id.strip_prefix("sq:").unwrap_or(book_id)
}

// ── 签名 ──────────────────────────────────────────────────────

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// 目录/书信息签名：`md5(bookId + timestamp + user_id + SKEY)`。
fn sign_catalog(book_id: &str, timestamp: u64) -> String {
    md5_hex(&format!("{book_id}{timestamp}{USER_ID}{SKEY}"))
}

/// 搜索签名：`md5(timestamp)`。
fn sign_search(timestamp: u64) -> String {
    md5_hex(&timestamp.to_string())
}

// ── 内容解码 ──────────────────────────────────────────────────

/// 书旗 ROT 变体：对 ASCII 字母做变换，非字母原样保留。
/// 经验证等价于 ROT13，但此处按原始 JS 公式逐字实现以保完全一致。
fn shuqi_rot_char(c: char) -> char {
    if !c.is_ascii_alphabetic() {
        return c;
    }
    let code = c as u32;
    let e = code / 97; // 0=大写(65..90), 1=小写(97..122)
    let lower = c.to_ascii_lowercase() as u32;
    let mut n = (lower - 83) % 26;
    if n == 0 {
        n = 26;
    }
    let offset = if e == 0 { 64 } else { 96 };
    char::from_u32(n + offset).unwrap_or(c)
}

/// 解码书旗章节正文：ROT → base64 → UTF-8。
fn decode_content(encoded: &str) -> Option<String> {
    let rotated: String = encoded.trim().chars().map(shuqi_rot_char).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(rotated.trim())
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// 将解码后的纯文本（含 `<br/>` 换行）转为 XHTML body 片段。
fn chapter_text_to_html(text: &str) -> String {
    let mut out = String::new();
    for seg in text.split("<br/>") {
        let stripped = strip_tags(seg);
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push_str("<p>");
        out.push_str(&html_escape(trimmed));
        out.push_str("</p>");
    }
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

// ── HTTP 客户端 ───────────────────────────────────────────────

pub struct ShuqiClient {
    http: reqwest::blocking::Client,
}

impl ShuqiClient {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs.max(5)))
            .user_agent("Mozilla/5.0 (Linux; Android 13) AppleWebKit/537.36")
            .build()
            .context("build shuqi HTTP client")?;
        Ok(Self { http })
    }

    /// 搜索小说。返回原始 JSON `data` 数组。
    pub fn search(&self, keyword: &str, page: u32) -> Result<Vec<Value>> {
        let ts = now_ts();
        let sign = sign_search(ts);
        let resp = self
            .http
            .get(URL_SEARCH)
            .query(&[
                ("do", "is_search"),
                ("q", keyword),
                ("filterMigu", "1"),
                ("ver", ""),
                ("platform", "3"),
                ("p", &page.to_string()),
                ("timestamp", &ts.to_string()),
                ("sign", &sign),
            ])
            .send()
            .context("search request")?
            .json::<Value>()
            .context("search response parse")?;

        let status = resp.get("status").and_then(|v| v.as_i64()).unwrap_or(0);
        if status != 1 {
            let msg = resp.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown");
            return Err(anyhow!("search failed: status={status}, msg={msg}"));
        }
        let data = resp
            .get("data")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(data)
    }

    /// 获取目录。返回 `(book_meta_fields, chapters_with_suffix)`。
    pub fn catalog(&self, book_id: &str) -> Result<CatalogResult> {
        let bid = strip_sq_prefix(book_id);
        let ts = now_ts();
        let sign = sign_catalog(bid, ts);
        let resp = self
            .http
            .get(URL_CATALOG)
            .query(&[
                ("user_id", USER_ID),
                ("bookId", bid),
                ("timestamp", &ts.to_string()),
                ("sign", &sign),
                ("platform", "0"),
            ])
            .send()
            .context("catalog request")?
            .json::<Value>()
            .context("catalog response parse")?;

        let data = resp
            .get("data")
            .ok_or_else(|| anyhow!("catalog: missing `data` field"))?;

        // 元数据
        let book_name = pick_str(data, &["bookName", "book_name", "name"]);
        let author = pick_str(data, &["authorName", "author", "author_name"]);
        let desc = pick_str(data, &["desc", "description", "intro", "bookDesc"]);
        let finished = pick_str(data, &["isFinish", "is_finish", "finished"])
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"));
        let category = pick_str(data, &["cat", "category", "bookCategory"]);
        let cover_url = pick_str(data, &["cover", "coverUrl", "cover_url", "imgUrl"]);

        // 章节列表：data.chapterList[].volumeList[].{chapterId, chapterName, payStatus, contUrlSuffix}
        let chapter_list = data
            .get("chapterList")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("catalog: missing `chapterList`"))?;

        let mut chapters: Vec<ShuqiChapter> = Vec::new();
        for vol in chapter_list {
            let volumes = vol.get("volumeList").and_then(|v| v.as_array()).map(|v| v.as_slice());
            let single = if volumes.is_some() {
                None
            } else {
                Some(std::slice::from_ref(vol))
            };
            let vols = volumes.unwrap_or_else(|| single.unwrap_or(&[]));
            for ch in vols {
                let id = pick_str(ch, &["chapterId", "chapter_id", "id"]);
                let title = pick_str(ch, &["chapterName", "chapter_name", "title", "name"]);
                let suffix = pick_str(ch, &["contUrlSuffix", "cont_url_suffix", "urlSuffix"]);
                let (id, title) = match (id, title) {
                    (Some(id), Some(title)) => (id, title),
                    (Some(id), None) => (id.clone(), id),
                    _ => continue,
                };
                chapters.push(ShuqiChapter {
                    id,
                    title,
                    suffix: suffix.unwrap_or_default(),
                });
            }
        }

        if chapters.is_empty() {
            return Err(anyhow!("catalog: chapter list is empty"));
        }

        Ok(CatalogResult {
            book_name,
            author,
            description: desc,
            finished,
            category,
            cover_url,
            chapters,
        })
    }

    /// 获取章节正文（解码后纯文本，含 `<br/>`）。
    pub fn chapter_content(&self, suffix: &str) -> Result<String> {
        let url = if suffix.starts_with('?') {
            format!("{URL_CONTENT}{suffix}")
        } else {
            format!("{URL_CONTENT}?{suffix}")
        };
        let resp = self
            .http
            .get(&url)
            .send()
            .context("content request")?
            .json::<Value>()
            .context("content response parse")?;

        let encoded = resp
            .get("ChapterContent")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("content: missing `ChapterContent`"))?;
        decode_content(encoded).ok_or_else(|| anyhow!("content: decode failed"))
    }

    /// 尝试拉取书信息补充元数据（best-effort，失败返回 None）。
    pub fn book_info(&self, book_id: &str) -> Option<BookMeta> {
        let bid = strip_sq_prefix(book_id);
        let ts = now_ts();
        let sign = sign_catalog(bid, ts);
        let resp = self
            .http
            .post(URL_BOOK_INFO)
            .form(&[
                ("user_id", USER_ID),
                ("bookId", bid),
                ("timestamp", &ts.to_string()),
                ("sign", &sign),
                ("platform", "0"),
            ])
            .send()
            .ok()?
            .json::<Value>()
            .ok()?;

        let data = resp.get("data").unwrap_or(&resp);
        Some(BookMeta {
            book_name: pick_str(data, &["bookName", "book_name", "name"]),
            author: pick_str(data, &["authorName", "author"]),
            description: pick_str(data, &["desc", "description", "intro", "bookDesc"]),
            tags: Vec::new(),
            cover_url: pick_str(data, &["cover", "coverUrl", "cover_url", "imgUrl"]),
            detail_cover_url: None,
            finished: pick_str(data, &["isFinish", "is_finish", "finished"])
                .map(|s| s == "1" || s.eq_ignore_ascii_case("true")),
            chapter_count: pick_str(data, &["chapterNum", "chapter_num", "chapterCount"])
                .and_then(|s| s.parse().ok()),
            word_count: pick_str(data, &["wordNum", "word_num", "wordCount", "words"])
                .and_then(|s| s.parse().ok()),
            score: pick_str(data, &["score", "rating"]).and_then(|s| s.parse().ok()),
            read_count: None,
            read_count_text: None,
            book_short_name: None,
            original_book_name: None,
            first_chapter_title: None,
            last_chapter_title: None,
            category: pick_str(data, &["cat", "category", "bookCategory"]),
            cover_primary_color: None,
        })
    }
}

// ── 数据结构 ──────────────────────────────────────────────────

pub struct CatalogResult {
    pub book_name: Option<String>,
    pub author: Option<String>,
    pub description: Option<String>,
    pub finished: Option<bool>,
    pub category: Option<String>,
    pub cover_url: Option<String>,
    pub chapters: Vec<ShuqiChapter>,
}

pub struct ShuqiChapter {
    pub id: String,
    pub title: String,
    pub suffix: String,
}

// ── JSON 工具 ────────────────────────────────────────────────

fn pick_str(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        let Some(val) = v.get(k) else { continue };
        // Try string first, then integer / float (Shuqi API returns bid as number).
        let s = val
            .as_str()
            .map(|s| s.trim().to_string())
            .or_else(|| val.as_i64().map(|n| n.to_string()))
            .or_else(|| val.as_u64().map(|n| n.to_string()))
            .or_else(|| val.as_f64().map(|n| n.to_string()))?;
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

// ── 下载计划准备 ──────────────────────────────────────────────

/// 准备书旗下载计划：拉取目录、合并元数据、下载封面。
pub fn prepare_shuqi_plan(
    config: &Config,
    book_id: &str,
    meta_hint: BookMeta,
) -> Result<DownloadPlan> {
    info!(target: "download", book_id, "准备书旗下载计划");

    let timeout = config.request_timeout.max(5);
    let client = ShuqiClient::new(timeout).context("init ShuqiClient")?;
    let catalog = client.catalog(book_id).context("fetch catalog")?;

    let chapters: Vec<ChapterRef> = catalog
        .chapters
        .iter()
        .map(|c| ChapterRef {
            id: c.id.clone(),
            title: c.title.clone(),
        })
        .collect();

    let chapter_count = chapters.len();
    let mut dir_meta = BookMeta {
        book_name: catalog.book_name,
        author: catalog.author,
        description: catalog.description,
        tags: Vec::new(),
        cover_url: catalog.cover_url,
        detail_cover_url: None,
        finished: catalog.finished,
        chapter_count: Some(chapter_count),
        word_count: None,
        score: None,
        read_count: None,
        read_count_text: None,
        book_short_name: None,
        original_book_name: None,
        first_chapter_title: None,
        last_chapter_title: None,
        category: catalog.category,
        cover_primary_color: None,
    };

    // best-effort: 用 book_info 补充元数据
    if let Some(info_meta) = client.book_info(book_id) {
        dir_meta.word_count = info_meta.word_count.or(dir_meta.word_count);
        dir_meta.score = info_meta.score.or(dir_meta.score);
        dir_meta.cover_url = dir_meta.cover_url.or(info_meta.cover_url);
        dir_meta.description = dir_meta.description.or(info_meta.description);
    }

    let merged = merge_meta_prefer_hint_name(dir_meta, meta_hint);

    // best-effort: 下载封面
    if let Some(cover_url) = merged.cover_url.as_ref() {
        let folder = book_paths::book_folder_path(config, book_id, merged.book_name.as_deref());
        let _ = std::fs::create_dir_all(&folder);
        let cover_path = book_paths::canonical_cover_path(&folder, "jpg");
        if !cover_path.exists() {
            if let Err(e) = download_cover(&client.http, cover_url, &cover_path) {
                debug!(target: "download", error = %e, "书旗封面下载失败（忽略）");
            }
        }
    }

    Ok(DownloadPlan {
        book_id: book_id.to_string(),
        meta: merged,
        chapters,
        _raw: Value::Null,
    })
}

fn download_cover(
    http: &reqwest::blocking::Client,
    url: &str,
    path: &std::path::Path,
) -> Result<()> {
    let resp = http.get(url).send().context("cover request")?;
    if !resp.status().is_success() {
        return Err(anyhow!("cover HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().context("cover bytes")?;
    if bytes.is_empty() {
        return Err(anyhow!("cover empty"));
    }
    std::fs::write(path, &bytes).context("cover write")?;
    Ok(())
}

// ── 下载流程 ──────────────────────────────────────────────────

/// 书旗章节下载流程：工作池并发拉取，逐章保存。
#[allow(clippy::too_many_arguments)]
pub fn download_shuqi_into_manager(
    config: &Config,
    book_id: &str,
    book_name: &str,
    manager: &mut crate::book_parser::book_manager::BookManager,
    _chosen_chapters: &[ChapterRef],
    pending_chapters: &[ChapterRef],
    reporter: &mut crate::download::progress::ProgressReporter,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<DownloadResult> {
    let timeout = config.request_timeout.max(5);
    let client = Arc::new(ShuqiClient::new(timeout).context("init ShuqiClient")?);

    // 重新拉取目录以获取 contUrlSuffix
    let catalog = client.catalog(book_id).context("fetch catalog for suffixes")?;
    let suffix_map: HashMap<String, String> = catalog
        .chapters
        .into_iter()
        .map(|c| (c.id, c.suffix))
        .collect();

    let worker_count = config.max_workers.max(1).min(8); // 对书旗上游保持礼貌并发
    let (tx_jobs, rx_jobs) = channel::unbounded::<Vec<ChapterRef>>();
    let (tx_res, rx_res) =
        channel::unbounded::<Result<(Vec<ChapterRef>, Vec<(String, String)>)>>();

    for group in build_dynamic_chapter_groups(pending_chapters) {
        tx_jobs.send(group.to_vec()).ok();
    }
    drop(tx_jobs);

    for _ in 0..worker_count {
        let rx = rx_jobs.clone();
        let tx = tx_res.clone();
        let client = Arc::clone(&client);
        let suffix_map = suffix_map.clone();
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
                let mut contents = Vec::with_capacity(group.len());
                for ch in &group {
                    match suffix_map.get(&ch.id) {
                        None => {
                            let _ = tx.send(Err(anyhow!(
                                "章节 {} 缺少 contUrlSuffix",
                                ch.id
                            )));
                            return;
                        }
                        Some(suffix) => {
                            match client.chapter_content(suffix) {
                                Ok(text) => contents.push((ch.id.clone(), text)),
                                Err(e) => {
                                    warn!(
                                        target: "download",
                                        chapter_id = %ch.id,
                                        error = %e,
                                        "书旗章节下载失败：{} ({})",
                                        ch.title, ch.id
                                    );
                                    // 跳过此章，标记为失败
                                }
                            }
                        }
                    }
                }
                let _ = tx.send(Ok((group, contents)));
            }
        });
    }
    drop(tx_res);

    let mut result = DownloadResult::default();
    for res in rx_res.iter() {
        if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
            return Err(anyhow!("用户停止下载"));
        }

        let (group, contents) = res?;

        let content_map: HashMap<String, String> = contents.into_iter().collect();
        for ch in &group {
            match content_map.get(&ch.id) {
                Some(text) if !text.is_empty() => {
                    let html = chapter_text_to_html(text);
                    if html.is_empty() {
                        manager.save_error_chapter(&ch.id, &ch.title);
                        result.failed += 1;
                    } else {
                        manager.save_chapter(&ch.id, &ch.title, &html);
                        manager.append_downloaded_chapter(&ch.id, &ch.title, &html);
                        result.success += 1;
                    }
                }
                _ => {
                    manager.save_error_chapter(&ch.id, &ch.title);
                    result.failed += 1;
                }
            }
            reporter.inc_saved();
        }
        reporter.inc_group();
        manager.save_download_status();
    }

    info!(
        target: "download",
        "书旗下载完成：{} ({} 章)",
        book_name,
        pending_chapters.len()
    );
    Ok(result)
}

// ── 搜索（Web UI）─────────────────────────────────────────────

/// 搜索并返回 Web UI 所需的 JSON items。
pub fn search_items(client: &ShuqiClient, keyword: &str) -> Result<Vec<Value>> {
    let mut items: Vec<Value> = Vec::new();

    // 纯数字关键词：先按 bookId 直查书旗 book/info，命中则置顶
    if keyword.chars().all(|c| c.is_ascii_digit()) && !keyword.is_empty() {
        if let Some(meta) = client.book_info(keyword) {
            if meta.book_name.is_some() {
                items.push(serde_json::json!({
                    "book_id": format!("sq:{keyword}"),
                    "title": meta.book_name,
                    "author": meta.author.unwrap_or_default(),
                    "raw": meta.description.unwrap_or_default(),
                    "cover_url": meta.cover_url,
                }));
            }
        }
    }

    // 关键词搜索
    let data = client.search(keyword, 1).context("shuqi search")?;
    for b in data {
        let bid = match pick_str(&b, &["bid", "bookId", "book_id", "id"]) {
            Some(v) => v,
            None => continue,
        };
        let title = match pick_str(&b, &["title", "bookName", "name"]) {
            Some(v) => v,
            None => continue,
        };
        let author = pick_str(&b, &["author", "authorName"]).unwrap_or_default();
        let desc = pick_str(&b, &["desc", "description", "intro"]).unwrap_or_default();
        let cover = pick_str(&b, &["cover", "coverUrl", "imgUrl"]);
        let book_id = format!("sq:{bid}");
        // 跳过直查已添加的重复项，但补上缺失的 cover_url
        if let Some(existing) = items.iter_mut().find(|it| it["book_id"].as_str() == Some(&book_id)) {
            if existing["cover_url"].as_str().is_none() {
                if let Some(c) = &cover {
                    existing["cover_url"] = serde_json::Value::String(c.clone());
                }
            }
            continue;
        }
        items.push(serde_json::json!({
            "book_id": book_id,
            "title": title,
            "author": author,
            "raw": desc,
            "cover_url": cover,
        }));
    }
    Ok(items)
}

// ── 测试 ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shuqi_rot_char() {
        assert_eq!(shuqi_rot_char('a'), 'n');
        assert_eq!(shuqi_rot_char('m'), 'z');
        assert_eq!(shuqi_rot_char('z'), 'm');
        assert_eq!(shuqi_rot_char('A'), 'N');
        assert_eq!(shuqi_rot_char('Z'), 'M');
        assert_eq!(shuqi_rot_char('0'), '0');
        assert_eq!(shuqi_rot_char('='), '=');
    }

    #[test]
    fn test_normalize_book_input() {
        assert_eq!(
            normalize_book_input("sq:8969239"),
            Some("sq:8969239".to_string())
        );
        assert_eq!(
            normalize_book_input("SQ:8969239"),
            Some("sq:8969239".to_string())
        );
        assert_eq!(
            normalize_book_input("https://www.shuqi.com/reader?bid=8969239"),
            Some("sq:8969239".to_string())
        );
        assert_eq!(
            normalize_book_input("https://www.shuqi.com/book/8969239"),
            Some("sq:8969239".to_string())
        );
        assert_eq!(normalize_book_input("8969239"), None);
        assert_eq!(normalize_book_input(""), None);
    }

    #[test]
    fn test_chapter_text_to_html() {
        let html = chapter_text_to_html("第一段<br/>第二段<br/><br/>第三段");
        assert!(html.contains("<p>第一段</p>"));
        assert!(html.contains("<p>第二段</p>"));
        assert!(html.contains("<p>第三段</p>"));
        assert!(!html.contains("<br/>"));
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("a<b>c&d"), "a&lt;b&gt;c&amp;d");
    }

    #[test]
    fn test_sign_search() {
        // sign = md5("1789354448") = a2808e95d4222ed102c341bb50d6d1b4
        let sign = sign_search(1789354448);
        assert_eq!(sign, "a2808e95d4222ed102c341bb50d6d1b4");
    }
}
