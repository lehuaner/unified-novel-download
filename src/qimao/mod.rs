//! 七猫（Qimao）小说提供商。
//!
//! 通过 `qm:<bookId>` 前缀路由，独立于番茄官方 API 与书旗。
//! 参考实现：swiftcat-downloader-flutter（`lib/core/api_client.dart`）。
//!
//! # 接口
//! - 搜索：`api-bc.wtzw.com/search/v1/words`
//! - 详情：`api-bc.wtzw.com/api/v4/book/detail`
//! - 目录：`api-ks.wtzw.com/api/v1/chapter/chapter-list`
//! - 正文：**整本缓存 ZIP** `api-bc.wtzw.com/api/v1/book/download` → `data.link`
//!
//! # 签名
//! 所有请求：`sign = md5( concat(sorted(k) => "k=v") + SIGN_KEY )`。
//!
//! # 正文解密
//! ZIP 内每个 `{chapterId}.txt` 文件内容是 Base64；解码后 `IV(16)+AES-128-CBC 密文`，
//! 固定 key = `AES_KEY_HEX`，PKCS7 去填充 → 纯文本（`\n` 分行）。
//!
//! # 与逐章源的差异
//! 七猫正文不是逐章请求，而是一次性拉取整本 ZIP 再本地解密；因此：
//! - 断点续传：解密出全本后，仅保存“待下载”章节（跳过已存在）。
//! - 范围选择：仍需下载整包，但只保存所选区间。

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use md5::{Digest, Md5};
use regex::Regex;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::base_system::book_paths;
use crate::base_system::context::Config;
use crate::download::models::{
    BookMeta, ChapterRef, DownloadPlan, DownloadResult, merge_meta_prefer_hint_name,
};

// ── 常量 ──────────────────────────────────────────────────────

const SIGN_KEY: &str = "d3dGiJc651gSQ8w1";
const AES_KEY_HEX: &str = "32343263636238323330643730396531";

const URL_BASE_BC: &str = "https://api-bc.wtzw.com";
const URL_BASE_KS: &str = "https://api-ks.wtzw.com";

/// 固定 app-version（仅用于请求头 sign，实测服务端不严格校验头签名）。
const APP_VERSION: &str = "73720";

// ── book_id 识别 ──────────────────────────────────────────────

static RE_QM_PREFIX: OnceLock<Regex> = OnceLock::new();
static RE_QM_ID_QS: OnceLock<Regex> = OnceLock::new();
static RE_QM_BOOK_PATH: OnceLock<Regex> = OnceLock::new();

fn re_qm_prefix() -> &'static Regex {
    RE_QM_PREFIX.get_or_init(|| Regex::new(r"(?i)^qm:(\d+)$").expect("regex"))
}
fn re_qm_id_qs() -> &'static Regex {
    RE_QM_ID_QS.get_or_init(|| Regex::new(r"(?i)[?&](?:id|book_id|bookId|bid)=(\d+)").expect("regex"))
}
fn re_qm_book_path() -> &'static Regex {
    RE_QM_BOOK_PATH
        .get_or_init(|| Regex::new(r"(?i)/(?:book|detail|reader)/(\d+)").expect("regex"))
}

/// 判断 book_id 是否为七猫来源（`qm:` 前缀）。
pub fn is_qimao_book_id(book_id: &str) -> bool {
    book_id.starts_with("qm:")
}

/// 从用户输入中识别七猫 book_id，返回规范化的 `qm:<digits>`。
/// 支持：`qm:152109`、`https://www.qimao.com/book/152109/`、`...?id=152109`。
pub fn normalize_book_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(caps) = re_qm_prefix().captures(trimmed) {
        return Some(format!("qm:{}", &caps[1]));
    }

    let lower = trimmed.to_lowercase();
    if !(lower.contains("qimao.com") || lower.contains("wtzw.com")) {
        return None;
    }

    if let Some(caps) = re_qm_id_qs().captures(trimmed) {
        return Some(format!("qm:{}", &caps[1]));
    }
    if let Some(caps) = re_qm_book_path().captures(trimmed) {
        return Some(format!("qm:{}", &caps[1]));
    }
    None
}

/// 从 `qm:<digits>` 中提取纯数字 bookId。
fn strip_qm_prefix(book_id: &str) -> &str {
    book_id.strip_prefix("qm:").unwrap_or(book_id)
}

// ── 签名 ──────────────────────────────────────────────────────

#[allow(dead_code)]
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

/// 参数签名：排序 key → 拼接 `k=v` → 追加 SIGN_KEY → md5。
fn sign_params(params: &[(&str, String)]) -> String {
    let mut kv: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    kv.sort_by(|a, b| a.0.cmp(&b.0));
    let mut s = String::new();
    for (k, v) in &kv {
        s.push_str(k);
        s.push('=');
        s.push_str(v);
    }
    s.push_str(SIGN_KEY);
    md5_hex(&s)
}

/// 请求头签名：对固定的 8 个基础头同样算法生成。
fn sign_headers() -> String {
    let mut kv: Vec<(&str, &str)> = vec![
        ("AUTHORIZATION", ""),
        ("app-version", APP_VERSION),
        ("application-id", "com.****.reader"),
        ("channel", "unknown"),
        ("net-env", "1"),
        ("platform", "android"),
        ("qm-params", ""),
        ("reg", "0"),
    ];
    kv.sort_by(|a, b| a.0.cmp(&b.0));
    let mut s = String::new();
    for (k, v) in &kv {
        s.push_str(k);
        s.push('=');
        s.push_str(v);
    }
    s.push_str(SIGN_KEY);
    md5_hex(&s)
}

// ── 内容解码 ──────────────────────────────────────────────────

/// base64 → AES-128-CBC(IV=前16字节) → UTF-8 纯文本。
fn decrypt_chapter_b64(b64_text: &str) -> Option<String> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64_text.trim())
        .ok()?;
    let plain = crate::third_party::fq_crypto::aes_cbc_decrypt(&raw, AES_KEY_HEX).ok()?;
    Some(String::from_utf8_lossy(&plain).into_owned())
}

/// 将七猫纯文本（`\n` 分行）转为 XHTML body 片段。
fn chapter_text_to_html(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push_str("<p>");
        out.push_str(&html_escape(trimmed));
        out.push_str("</p>");
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

fn pick_str(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        let Some(val) = v.get(k) else { continue };
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

// ── HTTP 客户端 ───────────────────────────────────────────────

pub struct QimaoClient {
    http: reqwest::blocking::Client,
}

impl QimaoClient {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs.max(5)))
            .user_agent("okhttp/4.9.2")
            .build()
            .context("build qimao HTTP client")?;
        Ok(Self { http })
    }

    /// 返回带签名的请求头 HeaderMap（与 swiftcat / 实测探针一致）。
    fn req_headers(&self) -> reqwest::header::HeaderMap {
        let mut m = reqwest::header::HeaderMap::new();
        for (k, v) in [
            ("AUTHORIZATION", String::new()),
            ("app-version", APP_VERSION.to_string()),
            ("application-id", "com.****.reader".to_string()),
            ("channel", "unknown".to_string()),
            ("net-env", "1".to_string()),
            ("platform", "android".to_string()),
            ("qm-params", String::new()),
            ("reg", "0".to_string()),
            ("sign", sign_headers()),
        ] {
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(&v),
            ) {
                m.insert(name, val);
            }
        }
        m
    }

    /// 搜索小说。返回原始 JSON `data.books` 数组。
    pub fn search(&self, keyword: &str, page: u32) -> Result<Vec<Value>> {
        let params: Vec<(&str, String)> = vec![
            ("extend", String::new()),
            ("tab", "0".to_string()),
            ("gender", "0".to_string()),
            ("refresh_state", "8".to_string()),
            ("page", page.to_string()),
            ("wd", keyword.to_string()),
            ("is_short_story_user", "0".to_string()),
        ];
        let mut pairs: Vec<(String, String)> =
            params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        pairs.push(("sign".to_string(), sign_params(&params)));

        let resp = self
            .http
            .get(format!("{URL_BASE_BC}/search/v1/words"))
            .query(&pairs)
            .headers(self.req_headers())
            .send()
            .context("qimao search request")?
            .json::<Value>()
            .context("qimao search response parse")?;

        let books = resp
            .get("data")
            .and_then(|d| d.get("books"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(books)
    }

    /// 书籍详情。返回 `data.book` 对象。
    pub fn book_detail(&self, book_id: &str) -> Result<Value> {
        let bid = strip_qm_prefix(book_id).to_string();
        let params: Vec<(&str, String)> = vec![
            ("id", bid),
            ("imei_ip", "2937357107".to_string()),
            ("teeny_mode", "0".to_string()),
        ];
        let mut pairs: Vec<(String, String)> =
            params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        pairs.push(("sign".to_string(), sign_params(&params)));

        let resp = self
            .http
            .get(format!("{URL_BASE_BC}/api/v4/book/detail"))
            .query(&pairs)
            .headers(self.req_headers())
            .send()
            .context("qimao detail request")?
            .json::<Value>()
            .context("qimao detail parse")?;

        let book = resp
            .get("data")
            .and_then(|d| d.get("book"))
            .cloned()
            .ok_or_else(|| anyhow!("qimao detail: missing `data.book`"))?;
        Ok(book)
    }

    /// 目录。返回按 `chapter_sort` 升序的章节列表 `(id, title)`。
    pub fn chapter_list(&self, book_id: &str) -> Result<Vec<(String, String)>> {
        let bid = strip_qm_prefix(book_id).to_string();
        let params: Vec<(&str, String)> =
            vec![("chapter_ver", "0".to_string()), ("id", bid)];
        let mut pairs: Vec<(String, String)> =
            params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        pairs.push(("sign".to_string(), sign_params(&params)));

        let resp = self
            .http
            .get(format!("{URL_BASE_KS}/api/v1/chapter/chapter-list"))
            .query(&pairs)
            .headers(self.req_headers())
            .send()
            .context("qimao chapter-list request")?
            .json::<Value>()
            .context("qimao chapter-list parse")?;

        let chapters = resp
            .get("data")
            .and_then(|d| d.get("chapter_lists"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut out: Vec<(String, String, i64)> = Vec::new();
        for c in chapters {
            let id = match pick_str(&c, &["id", "chapter_id", "chapterId"]) {
                Some(v) => v,
                None => continue,
            };
            let title = pick_str(&c, &["title", "chapter_name", "name"])
                .unwrap_or_else(|| id.clone());
            let sort = c
                .get("chapter_sort")
                .and_then(|v| v.as_i64())
                .unwrap_or(i64::MAX);
            out.push((id, title, sort));
        }
        out.sort_by_key(|(_, _, sort)| *sort);
        Ok(out.into_iter().map(|(id, title, _)| (id, title)).collect())
    }

    /// 获取整本缓存 ZIP 的下载链。
    pub fn cache_zip_link(&self, book_id: &str) -> Result<String> {
        let bid = strip_qm_prefix(book_id).to_string();
        let params: Vec<(&str, String)> = vec![
            ("id", bid),
            ("source", "1".to_string()),
            ("type", "2".to_string()),
            ("is_vip", "1".to_string()),
        ];
        let mut pairs: Vec<(String, String)> =
            params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        pairs.push(("sign".to_string(), sign_params(&params)));

        let resp = self
            .http
            .get(format!("{URL_BASE_BC}/api/v1/book/download"))
            .query(&pairs)
            .headers(self.req_headers())
            .send()
            .context("qimao download-link request")?
            .json::<Value>()
            .context("qimao download-link parse")?;

        let link = resp
            .get("data")
            .and_then(|d| d.get("link"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                let msg = resp
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                anyhow!("qimao download-link: no `data.link` (msg={msg})")
            })?;
        Ok(link.to_string())
    }

    /// 下载整本 ZIP 并解密为 `chapterId → 明文` 映射。
    pub fn fetch_all_chapters(&self, book_id: &str) -> Result<HashMap<String, String>> {
        let link = self.cache_zip_link(book_id)?;
        let bytes = self
            .http
            .get(&link)
            .headers(self.req_headers())
            .send()
            .context("qimao zip request")?
            .bytes()
            .context("qimao zip bytes")?;

        if bytes.is_empty() {
            return Err(anyhow!("qimao zip empty"));
        }

        let reader = std::io::Cursor::new(bytes.to_vec());
        let mut archive = zip::ZipArchive::new(reader).context("qimao zip open")?;

        let mut map: HashMap<String, String> = HashMap::new();
        for i in 0..archive.len() {
            let mut file = match archive.by_index(i) {
                Ok(f) => f,
                Err(_) => continue,
            };
            if !file.is_file() {
                continue;
            }
            let name = file.name().to_string();
            let stem = name
                .rsplit_once('/')
                .map(|(_, t)| t)
                .unwrap_or(&name)
                .trim_end_matches(".txt")
                .to_string();
            if stem.is_empty() {
                continue;
            }
            let mut buf = Vec::new();
            if file.read_to_end(&mut buf).is_err() {
                continue;
            }
            let enc = String::from_utf8_lossy(&buf);
            match decrypt_chapter_b64(&enc) {
                Some(plain) => {
                    map.insert(stem, plain);
                }
                None => {
                    debug!(target: "download", chapter = %stem, "七猫章节解密失败（忽略）");
                }
            }
        }

        if map.is_empty() {
            return Err(anyhow!("qimao zip: no chapters decrypted"));
        }
        Ok(map)
    }

    /// best-effort 从详情构造元数据。
    pub fn book_meta(&self, book_id: &str) -> Option<BookMeta> {
        let book = self.book_detail(book_id).ok()?;
        let tags = book
            .get("book_tag_list")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("title").and_then(|v| v.as_str()).map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Some(BookMeta {
            book_name: pick_str(&book, &["title", "book_name", "name"]),
            author: pick_str(&book, &["author", "author_name"]),
            description: pick_str(&book, &["intro", "desc", "description", "book_note"]),
            tags,
            cover_url: pick_str(&book, &["image_link", "cover", "coverUrl", "imgUrl"]),
            detail_cover_url: None,
            finished: pick_str(&book, &["is_over", "state", "finished"])
                .map(|s| s == "1" || s.eq_ignore_ascii_case("true")),
            chapter_count: pick_str(&book, &["chapter_total", "chapter_num", "chapter_count"])
                .and_then(|s| s.parse().ok()),
            word_count: pick_str(&book, &["words_num", "word_count", "words"])
                .and_then(|s| s.parse().ok()),
            score: None,
            read_count: None,
            read_count_text: None,
            book_short_name: None,
            original_book_name: None,
            first_chapter_title: None,
            last_chapter_title: None,
            category: pick_str(&book, &["category_name", "category", "cate"]),
            cover_primary_color: None,
        })
    }
}

// ── 下载计划准备 ──────────────────────────────────────────────

/// 准备七猫下载计划：拉目录、合并元数据、下载封面。
pub fn prepare_qimao_plan(config: &Config, book_id: &str, meta_hint: BookMeta) -> Result<DownloadPlan> {
    info!(target: "download", book_id, "准备七猫下载计划");

    let timeout = config.request_timeout.max(5);
    let client = QimaoClient::new(timeout).context("init QimaoClient")?;

    let chapters_kv = client.chapter_list(book_id).context("fetch qimao catalog")?;
    if chapters_kv.is_empty() {
        return Err(anyhow!("qimao catalog empty"));
    }

    let chapters: Vec<ChapterRef> = chapters_kv
        .into_iter()
        .map(|(id, title)| ChapterRef { id, title })
        .collect();
    let chapter_count = chapters.len();

    let mut dir_meta = client
        .book_meta(book_id)
        .unwrap_or_default();
    dir_meta.chapter_count = dir_meta.chapter_count.or(Some(chapter_count));

    let merged = merge_meta_prefer_hint_name(dir_meta, meta_hint);

    // best-effort：下载封面
    if let Some(cover_url) = merged.cover_url.as_ref() {
        let folder = book_paths::book_folder_path(config, book_id, merged.book_name.as_deref());
        let _ = std::fs::create_dir_all(&folder);
        let cover_path = book_paths::canonical_cover_path(&folder, "jpg");
        if !cover_path.exists() {
            if let Err(e) = download_cover(&client.http, cover_url, &cover_path) {
                debug!(target: "download", error = %e, "七猫封面下载失败（忽略）");
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

/// 七猫下载流程：一次性拉取整本 ZIP 解密，再逐章保存到 BookManager。
#[allow(clippy::too_many_arguments)]
pub fn download_qimao_into_manager(
    config: &Config,
    book_id: &str,
    book_name: &str,
    manager: &mut crate::book_parser::book_manager::BookManager,
    _chosen_chapters: &[ChapterRef],
    pending_chapters: &[ChapterRef],
    reporter: &mut crate::download::progress::ProgressReporter,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<DownloadResult> {
    if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
        return Err(anyhow!("用户停止下载"));
    }

    let timeout = config.request_timeout.max(5);
    let client = QimaoClient::new(timeout).context("init QimaoClient")?;

    info!(target: "download", book_id, "七猫：下载整本缓存包并解密");
    let plain_map = client.fetch_all_chapters(book_id).context("fetch qimao zip")?;

    let total = pending_chapters.len();
    reporter.snapshot.group_total = 1;
    reporter.snapshot.chapter_total = reporter.snapshot.chapter_total.max(total);

    let mut result = DownloadResult::default();
    for ch in pending_chapters {
        if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
            return Err(anyhow!("用户停止下载"));
        }
        match plain_map.get(&ch.id) {
            Some(text) if !text.trim().is_empty() => {
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
                warn!(target: "download", chapter_id = %ch.id, title = %ch.title, "七猫缓存包缺少该章节");
                manager.save_error_chapter(&ch.id, &ch.title);
                result.failed += 1;
            }
        }
        reporter.inc_saved();
    }
    reporter.inc_group();
    manager.save_download_status();

    info!(
        target: "download",
        "七猫下载完成：{} (待下载 {} 章，成功 {}，失败 {})",
        book_name, total, result.success, result.failed
    );
    Ok(result)
}

// ── 搜索（Web UI）─────────────────────────────────────────────

/// 搜索并返回 Web UI 所需的 JSON items。
pub fn search_items(client: &QimaoClient, keyword: &str) -> Result<Vec<Value>> {
    let mut items: Vec<Value> = Vec::new();

    // 纯数字关键词：先按 bookId 直查详情，命中则置顶
    if !keyword.is_empty() && keyword.chars().all(|c| c.is_ascii_digit()) {
        let probe = if keyword.starts_with("qm:") {
            keyword.to_string()
        } else {
            format!("qm:{keyword}")
        };
        if let Some(meta) = client.book_meta(&probe)
            && meta.book_name.is_some()
        {
            items.push(serde_json::json!({
                "book_id": probe,
                "title": meta.book_name,
                "author": meta.author.unwrap_or_default(),
                "raw": meta.description.unwrap_or_default(),
                "cover_url": meta.cover_url,
            }));
        }
    }

    let books = client.search(keyword, 1).context("qimao search")?;
    for b in books {
        let bid = match pick_str(&b, &["id", "book_id", "bookId"]) {
            Some(v) => v,
            None => continue,
        };
        let title = match pick_str(&b, &["title", "book_name", "name"]) {
            Some(v) => strip_html_tags(&v),
            None => continue,
        };
        let author = pick_str(&b, &["author", "author_name"]).unwrap_or_default();
        let author = strip_html_tags(&author);
        let desc = pick_str(&b, &["intro", "desc", "description"]).unwrap_or_default();
        let cover = pick_str(&b, &["image_link", "cover", "coverUrl", "imgUrl"]);
        let book_id = format!("qm:{bid}");
        if items
            .iter()
            .any(|it| it["book_id"].as_str() == Some(&book_id))
        {
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

/// 去掉搜索结果里的 `<font ...>` 等 HTML 标签。
fn strip_html_tags(s: &str) -> String {
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

// ── 测试 ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_params_matches_python() {
        // 与实测通过的 Python 版本对齐：id=152109 source=1 type=2 is_vip=1
        let params: Vec<(&str, String)> = vec![
            ("id", "152109".to_string()),
            ("source", "1".to_string()),
            ("type", "2".to_string()),
            ("is_vip", "1".to_string()),
        ];
        // 手工计算：排序 -> id,is_vip,source,type -> "id=152109is_vip=1source=1type=2"+SIGN_KEY
        let expect_input = format!("id=152109is_vip=1source=1type=2{SIGN_KEY}");
        assert_eq!(sign_params(&params), md5_hex(&expect_input));
    }

    #[test]
    fn test_normalize_book_input() {
        assert_eq!(normalize_book_input("qm:152109"), Some("qm:152109".to_string()));
        assert_eq!(normalize_book_input("QM:152109"), Some("qm:152109".to_string()));
        assert_eq!(
            normalize_book_input("https://www.qimao.com/book/152109/"),
            Some("qm:152109".to_string())
        );
        assert_eq!(
            normalize_book_input("https://www.qimao.com/detail?id=152109"),
            Some("qm:152109".to_string())
        );
        assert_eq!(normalize_book_input("152109"), None);
        assert_eq!(normalize_book_input(""), None);
    }

    #[test]
    fn test_is_qimao_book_id() {
        assert!(is_qimao_book_id("qm:152109"));
        assert!(!is_qimao_book_id("152109"));
        assert!(!is_qimao_book_id("sq:152109"));
    }

    #[test]
    fn test_chapter_text_to_html() {
        let html = chapter_text_to_html("第一段\n第二段\n\n第三段");
        assert!(html.contains("<p>第一段</p>"));
        assert!(html.contains("<p>第二段</p>"));
        assert!(html.contains("<p>第三段</p>"));
        assert!(!html.contains("\n"));
    }

    #[test]
    fn test_strip_html_tags() {
        assert_eq!(
            strip_html_tags("我的<font color='#ff4242'>战神</font>"),
            "我的战神"
        );
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("a<b>c&d"), "a&lt;b&gt;c&amp;d");
    }
}
