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
    RE_QM_ID_QS
        .get_or_init(|| Regex::new(r"(?i)[?&](?:id|book_id|bookId|bid)=(\d+)").expect("regex"))
}
fn re_qm_book_path() -> &'static Regex {
    RE_QM_BOOK_PATH.get_or_init(|| Regex::new(r"(?i)/(?:book|detail|reader)/(\d+)").expect("regex"))
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
    kv.sort_by(|a, b| a.0.cmp(b.0));
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

/// 七猫搜索的分类/筛选可选参数（对齐 `examples/qimao_search_e2e` 抓包验证过的维度）。
/// 仅当字段为 `Some` 时才写入 query 并参与签名，保证默认搜索与既有行为完全一致。
#[derive(Default, Clone)]
pub struct QimaoSearchOpt {
    /// 分类 tab（0/2/3…），None 用 0。
    pub tab: Option<i64>,
    /// 性别（0/1/2），None 用 0。
    pub gender: Option<String>,
    /// 完结筛选（抓包验证值 "1"）。
    pub update_status: Option<String>,
    /// 字数筛选（抓包验证值 "2"）。
    pub words: Option<String>,
    /// 排序规则（抓包验证值 "1"）。
    pub collation_rule: Option<String>,
    /// 联想/相关查询（"1"/"2"）。
    pub include_query: Option<String>,
}

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

    /// 搜索小说（可带分类/筛选参数）。返回原始 JSON `data.books` 数组。
    /// 可选维度仅在有值时写入 query，`sign` 始终对完整参数集计算（与抓包算法一致）。
    pub fn search_with(
        &self,
        keyword: &str,
        page: u32,
        opt: &QimaoSearchOpt,
    ) -> Result<Vec<Value>> {
        let mut params: Vec<(&str, String)> = vec![
            ("extend", String::new()),
            ("tab", opt.tab.unwrap_or(0).to_string()),
            (
                "gender",
                opt.gender.clone().unwrap_or_else(|| "0".to_string()),
            ),
            ("refresh_state", "8".to_string()),
            ("page", page.to_string()),
            ("wd", keyword.to_string()),
            ("is_short_story_user", "0".to_string()),
        ];
        if let Some(v) = &opt.update_status {
            params.push(("update_status", v.clone()));
        }
        if let Some(v) = &opt.words {
            params.push(("words", v.clone()));
        }
        if let Some(v) = &opt.collation_rule {
            params.push(("collation_rule", v.clone()));
        }
        if let Some(v) = &opt.include_query {
            params.push(("include_query", v.clone()));
        }
        let mut pairs: Vec<(String, String)> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
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
        let mut pairs: Vec<(String, String)> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
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
        let params: Vec<(&str, String)> = vec![("chapter_ver", "0".to_string()), ("id", bid)];
        let mut pairs: Vec<(String, String)> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
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
            let title =
                pick_str(&c, &["title", "chapter_name", "name"]).unwrap_or_else(|| id.clone());
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
        let mut pairs: Vec<(String, String)> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
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
                    .filter_map(|t| {
                        t.get("title")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
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
pub fn prepare_qimao_plan(
    config: &Config,
    book_id: &str,
    meta_hint: BookMeta,
) -> Result<DownloadPlan> {
    info!(target: "download", book_id, "准备七猫下载计划");

    let timeout = config.request_timeout.max(5);
    let client = QimaoClient::new(timeout).context("init QimaoClient")?;

    let chapters_kv = client
        .chapter_list(book_id)
        .context("fetch qimao catalog")?;
    if chapters_kv.is_empty() {
        return Err(anyhow!("qimao catalog empty"));
    }

    let chapters: Vec<ChapterRef> = chapters_kv
        .into_iter()
        .map(|(id, title)| ChapterRef { id, title })
        .collect();
    let chapter_count = chapters.len();

    let mut dir_meta = client.book_meta(book_id).unwrap_or_default();
    dir_meta.chapter_count = dir_meta.chapter_count.or(Some(chapter_count));

    let merged = merge_meta_prefer_hint_name(dir_meta, meta_hint);

    // best-effort：下载封面
    if let Some(cover_url) = merged.cover_url.as_ref() {
        let folder = book_paths::book_folder_path(config, book_id, merged.book_name.as_deref());
        let _ = std::fs::create_dir_all(&folder);
        let cover_path = book_paths::canonical_cover_path(&folder, "jpg");
        if !cover_path.exists()
            && let Err(e) = download_cover(&client.http, cover_url, &cover_path)
        {
            debug!(target: "download", error = %e, "七猫封面下载失败（忽略）");
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
    let plain_map = client
        .fetch_all_chapters(book_id)
        .context("fetch qimao zip")?;

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

/// 搜索并返回 Web UI 所需的 JSON items，附带“是否还有下一页”（供加载更多）。
/// `page` 从 1 起；七猫每页约 10~11 条，返回满页视为可能还有下一页。
pub fn search_items(
    client: &QimaoClient,
    keyword: &str,
    page: u32,
    tab: Option<i64>,
    selected_items: Option<&str>,
) -> Result<(Vec<Value>, bool)> {
    let mut items: Vec<Value> = Vec::new();

    // 纯数字关键词且首页：先按 bookId 直查详情，命中则置顶
    if page <= 1 && !keyword.is_empty() && keyword.chars().all(|c| c.is_ascii_digit()) {
        let probe = if keyword.starts_with("qm:") {
            keyword.to_string()
        } else {
            format!("qm:{keyword}")
        };
        if let Some(mut meta) = client.book_meta(&probe)
            && let Some(name) = meta.book_name.take()
        {
            let d = meta
                .description
                .take()
                .map(|s| strip_html_tags(&s))
                .unwrap_or_default();
            items.push(serde_json::json!({
                "book_id": probe,
                "title": name,
                "author": meta.author.take().unwrap_or_default(),
                "raw": d,
                "description": d,
                "cover_url": meta.cover_url.take(),
                "category": meta.category.take(),
                "word_count": meta.word_count,
                "chapter_count": meta.chapter_count,
                "finished": meta.finished,
            }));
        }
    }

    let opt = qimao_opt_from(tab, selected_items);
    let books = client
        .search_with(keyword, page.max(1), &opt)
        .context("qimao search")?;
    let got = books.len();
    for b in books {
        // 听书条目没有 `id`，只有 `album_id`（实测），故一并作为标识候选。
        let bid = match pick_str(&b, &["id", "book_id", "bookId", "album_id"]) {
            Some(v) => v,
            None => continue,
        };
        let title = match pick_str(&b, &["title", "book_name", "name"]) {
            Some(v) => strip_html_tags(&v),
            None => continue,
        };
        let author = pick_str(&b, &["author", "author_name"]).unwrap_or_default();
        let author = strip_html_tags(&author);
        let desc =
            strip_html_tags(&pick_str(&b, &["intro", "desc", "description"]).unwrap_or_default());
        let cover = pick_str(&b, &["image_link", "cover", "coverUrl", "imgUrl"]);
        let book_id = format!("qm:{bid}");
        if items
            .iter()
            .any(|it| it["book_id"].as_str() == Some(&book_id))
        {
            continue;
        }
        let finished = pick_any_str(&b, &["is_over", "state", "finished"])
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"));
        items.push(serde_json::json!({
            "book_id": book_id,
            "title": title,
            "author": author,
            "raw": desc,
            "description": desc,
            "cover_url": cover,
            "category": pick_any_str(&b, &["category_name", "category", "cate", "first_category_name"]),
            "word_count": pick_any_str(&b, &["words_num", "word_count", "words"]).and_then(|s| s.parse::<usize>().ok()),
            "chapter_count": pick_any_str(&b, &["chapter_total", "chapter_num", "chapter_count"]).and_then(|s| s.parse::<usize>().ok()),
            "finished": finished,
            "score": pick_any_str(&b, &["score", "book_score"]).and_then(|s| s.parse::<f32>().ok()),
            // 品类标识：听书专辑带 is_audio=1/data_type=album，普通书为 novel。
            "content_kind": qimao_content_kind(&b),
        }));
    }
    Ok((items, got >= 10))
}

/// 构建七猫「筛选器」元数据，**复用番茄 selector 的同构结构**（相同 selector_item_id 命名），
/// 从而直接复用前端已有的分类/筛选按钮 UI 与交互；后端再把这些 id 映射到七猫 query 参数。
/// 说明：七猫上游对 words/collation_rule 的完整档位枚举未经抓包验证，此处仅暴露 examples
/// 已验证可命中的维度（完结 / 若干字数档 / 排序），未列出的番茄项对七猫安全忽略。
pub fn qimao_selector() -> Value {
    serde_json::json!({
        "rows": [
            {
                "row_name": "更新状态", "type": "creation_status", "selection_type": 2,
                "items": [
                    {"selector_item_id": "creation_status_end", "show_name": "完结", "value": "完结"}
                ]
            },
            {
                "row_name": "字数篇幅", "type": "word_num", "selection_type": 2,
                "items": [
                    {"selector_item_id": "word_num_lte30", "show_name": "30万字以内", "value": "30万字以内"},
                    {"selector_item_id": "word_num_gte30", "show_name": "30万字以上", "value": "30万字以上"},
                    {"selector_item_id": "word_num_gte100", "show_name": "100万字以上", "value": "100万字以上"}
                ]
            },
            {
                "row_name": "排序", "type": "order", "selection_type": 2,
                "items": [
                    {"selector_item_id": "sort_score", "show_name": "高分优先", "value": "高分优先"},
                    {"selector_item_id": "sort_new_book", "show_name": "新书推荐", "value": "新书推荐"},
                    {"selector_item_id": "sort_word_number", "show_name": "字数优先", "value": "字数优先"}
                ]
            }
        ],
        "type": 1
    })
}

/// 七猫搜索分类 tab 能力声明（经实测取证，非猜测）。
///
/// `tab_type` 统一用**公共（番茄）编号**作为语义，便于与番茄取并集时同名合并；
/// 实际请求时再由 `qimao_tab_value` 映射回七猫自己的 tab 取值。
///
/// 实测依据（同一关键词跨 4 组）：
/// - 七猫 tab=0：11 条，全部 `is_audio=0`、有 `id` → 书籍（纯小说）
/// - 七猫 tab=1：10 条，全部 `is_audio=1` + `data_type=album`/`album_id`/`audio_jump`/`voice_tag` → 听书
/// - 七猫 tab=3：14 条，额外带 `perfect_match`/`authors` 容器 → 综合
/// - 七猫 tab=2：返回 `show_type=7`、`sub_title="44帖子"`、`jump_url=…book_friend_detail…from=search_topic`
///   → 书友圈话题，**不是书目、不可下载，故不注册**；tab=4/5 实测 `is_have_results=0` 无结果。
pub fn qimao_tabs() -> Value {
    serde_json::json!([
        { "tab_type": 1, "title": "综合" },
        { "tab_type": 2, "title": "听书" },
        { "tab_type": 3, "title": "书籍" },
    ])
}

/// 公共 tab_type → 七猫实际 tab 取值。
/// 未指定或「综合」→ 0（保持原有默认行为不变）；听书→ 1；书籍→ 0。
/// 未注册的 tab（短剧/漫剧/漫画/社区等）回退 0，由前端 providers 归属机制保证不会下发。
fn qimao_tab_value(public_tab: Option<i64>) -> i64 {
    match public_tab.unwrap_or(1) {
        2 => 1, // 听书
        _ => 0, // 综合/书籍/未指定/不支持 → 书籍
    }
}

/// 条目品类：七猫有声专辑带 `data_type=album` 与 `is_audio=1`（实测听书 tab 下 10/10 命中）。
fn qimao_content_kind(b: &Value) -> &'static str {
    let album = pick_any_str(b, &["data_type"]).as_deref() == Some("album");
    let audio = pick_any_str(b, &["is_audio"]).as_deref() == Some("1");
    if album || audio { "audio" } else { "novel" }
}

/// 把前端传来的（番茄命名风格）selected_items 映射为七猫搜索参数。
/// 采用 examples 抓包已验证命中的取值：完结→update_status=1，字数→words=2，排序→collation_rule=1。
/// 分类 tab：七猫有自己的 tab 体系（实测 0=书籍、1=听书、3=综合），由 `qimao_tab_value` 从
/// 公共 tab_type 映射而来；未注册的公共 tab 回退 0，不会让七猫返回异常。
pub fn qimao_opt_from(tab: Option<i64>, selected_items: Option<&str>) -> QimaoSearchOpt {
    let mut opt = QimaoSearchOpt {
        tab: Some(qimao_tab_value(tab)),
        ..Default::default()
    };
    if let Some(s) = selected_items {
        for id in s.split(',').map(|x| x.trim()).filter(|x| !x.is_empty()) {
            if id == "creation_status_end" {
                opt.update_status = Some("1".to_string());
            } else if id.starts_with("word_num_") {
                opt.words = Some("2".to_string());
            } else if id.starts_with("sort_") {
                opt.collation_rule = Some("1".to_string());
            }
            // 其余番茄专属项（连载中/半年内完结/N日内更新等）七猫无对应，安全忽略。
        }
    }
    opt
}

/// 去掉搜索结果里的 `<font ...>` 等 HTML 标签，并解码 HTML 实体。
///
/// 七猫上游书名/简介里混有 `&nbsp;` `&ldquo;` `&#39;` 等实体，只删标签会把 `&nbsp;`
/// 原样显示在卡片上（如「主角：战神、陈修&nbsp;&nbsp;…」）。实体解码复用
/// `book_parser::html_utils::unescape_basic_entities`（支持命名/十进制/十六进制与嵌套多轮）。
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
    crate::book_parser::html_utils::unescape_basic_entities(&out).into_owned()
}

/// 提取字段并统一转为 String（兼容字符串/数字/布尔）。
fn pick_any_str(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(node) = v.get(*k) {
            if let Some(s) = node.as_str() {
                let t = s.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            } else if let Some(n) = node.as_i64() {
                return Some(n.to_string());
            } else if let Some(bv) = node.as_bool() {
                return Some(if bv { "1".to_string() } else { "0".to_string() });
            } else if let Some(f) = node.as_f64() {
                return Some(f.to_string());
            }
        }
    }
    None
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
        assert_eq!(
            normalize_book_input("qm:152109"),
            Some("qm:152109".to_string())
        );
        assert_eq!(
            normalize_book_input("QM:152109"),
            Some("qm:152109".to_string())
        );
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

    /// 公共 tab_type → 七猫实际 tab（语义由实测锁定：0=书籍、1=听书、3=综合）。
    #[test]
    fn test_qimao_tab_mapping() {
        assert_eq!(qimao_tab_value(None), 0);
        assert_eq!(qimao_tab_value(Some(1)), 0); // 综合→保持原默认行为
        assert_eq!(qimao_tab_value(Some(2)), 1); // 听书
        assert_eq!(qimao_tab_value(Some(3)), 0); // 书籍
        assert_eq!(qimao_tab_value(Some(11)), 0); // 未注册（短剧）回退，不得报错
        assert_eq!(qimao_opt_from(Some(2), None).tab, Some(1));
        assert_eq!(qimao_opt_from(Some(1), None).tab, Some(0));
    }

    #[test]
    fn test_qimao_content_kind() {
        let album: Value = serde_json::from_str(r#"{"data_type":"album","is_audio":"1"}"#).unwrap();
        assert_eq!(qimao_content_kind(&album), "audio");
        let book: Value = serde_json::from_str(r#"{"id":"123","is_audio":"0"}"#).unwrap();
        assert_eq!(qimao_content_kind(&book), "novel");
        let bare: Value = serde_json::from_str(r#"{"id":"123"}"#).unwrap();
        assert_eq!(qimao_content_kind(&bare), "novel");
    }

    #[test]
    fn test_strip_html_tags_decodes_entities() {
        // 七猫实测：简介含 &nbsp; 实体，不得原样上屏
        assert_eq!(
            strip_html_tags("主角：战神&nbsp;&nbsp;陈修<font color='#ff4242'>唐艺</font>"),
            "主角：战神  陈修唐艺"
        );
        // 嵌套实体与引号类实体
        assert_eq!(
            strip_html_tags("他说&amp;#34;你好&amp;#34;"),
            "他说\"你好\""
        );
        assert_eq!(strip_html_tags("A &amp; B &#39;C&#39;"), "A & B 'C'");
        // 无实体时不得改变原文
        assert_eq!(strip_html_tags("普通简介，无实体。"), "普通简介，无实体。");
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("a<b>c&d"), "a&lt;b&gt;c&amp;d");
    }
}
