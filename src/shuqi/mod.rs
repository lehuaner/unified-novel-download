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
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use regex::Regex;
use serde_json::Value;
use sha1::Sha1;
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

const URL_BOOK_INFO: &str = "https://content.shuqireader.com/openapi/book/info";
const URL_CATALOG: &str = "https://content.shuqireader.com/openapi/book/chapterlist";
const URL_CONTENT: &str = "https://c13.shuqireader.com/pcapi/chapter/contentfree/";

// ── 原生网关（Api Gateway sv=3.0）──────────────────────────────
// 搜索走 native_v3，字段含评分/官方高亮/标签，召回优于老 web 接口（i.php）。
// 签名算法为普通 HMAC-SHA1（非 SecurityGuard 白盒），密钥从 APK 明文提取：
//   signStr = urlencoded 请求体 + x-sq-public + x-sq-cen + x-sq-res-encrypt + eagleeye-traceid
//   x-sq-signature = hex(HMAC-SHA1(GW_SECRET, signStr))
// x-sq-timestamp / x-sq-nonce 不参与签名，请求唯一性由 eagleeye-traceid 提供。
const GW_BASE: &str = "https://stars.shuqireader.com";
const GW_PATH_SEARCH: &str = "/sqan/render/render/search/native_v3";
const GW_APP_KEY: &str = "23011413";
const GW_SV: &str = "3.0";
const GW_PLATFORM: &str = "an";
const GW_CEN: &str = "L2pB";
const GW_RES_ENCRYPT: &str = "cA==";
const GW_REQ_ENCRYPT_PARAM: &str = "x-sq-public:x-sq-res-encrypt:x-sq-cen";
/// 网关签名密钥（classes10.dex / libAppRuntime_V1_6.so 明文）。
/// 若书旗升级换 key，请求会返回 401 `Api Gateway Sign ERROR`，需重新从新版 APK 提取。
const GW_SECRET: &str = "c0c2f4e21a1e2c9bc3da8ad1a1a1294b";
/// 设备信息密文 blob（App 逐请求随机生成，但服务端实测不校验内容，故固定复用）。
const GW_PUBLIC_BLOB: &str = "M2NBuJAMDIHjkWxE6yGUjjr0Ot/GRzOQDhnrClZo+U5a6TPCrdcDbAMIwz/NkLkO6phgKQ0BLC6avphLHeepvdPZY2FNkhkF347NjhI4nM9cgIw2HPYhbChdfWwxnVtSEr6YoKHBIpAdfWVcXywGQ2PbWR1hIi2FpzM1T6zH7ljGw8Xi8PCpKeUPklsrccrjf99b8DoWx/zpbepkCM0jZF45kYxsn6aP9UCFccjm9y6+VBPir6jztiGzpq7EwMkdu5H7DegOgA1TbW1Swxczy4dCO2dZM/ShMnX6IqZz1OWcxpK0Ssx4XNiZRenvi/HpchhQAe469RjSeoP8QpoCZ/W3FIMZvml8ve0FJGPKOU0OHmaGvkYqh/izb4dIzTVCL1k3DOTGJU11SQMjvDuTHWgFFXamsuQzrrPnx1bf05vgSrPXpYKF5jx8AKQa5msBhUk16llgWfUD4gQ/Z9dbZXeUzfsFi2gtTi98s91NJIGSvyGrvoy6adZDtMr8FTJ+zDf3rqY9CQKQ5az6D9Hto0N1X5+uWslrvoooYW2QXoumlpZqs2jA8/LMJGY+sdWV2bVutfKhPKe9PLfbMAQSmQeVCkd56ZZmvfnSWZVcdp6v0ttrua7/vFOeMXy9Y815o+pPBMyl7dGg3Uk4K76DdbroieiSoaLHsOKeQP11pbU27yJW42v0/TcHEVKlS0espVL1eQIYTFTV3Cd1+nASAUCbItIh1bEUiwXnztp6QMMiRc8KynegBS9siWmj862xa50vGxXyYP8M/aAndg37kmjQz8KLs+Q=";

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

/// 网关签名：`hex(HMAC-SHA1(GW_SECRET, signStr))`，40 位小写十六进制。
fn gw_sign(sign_str: &str) -> String {
    let mut mac =
        Hmac::<Sha1>::new_from_slice(GW_SECRET.as_bytes()).expect("hmac key 长度任意合法");
    mac.update(sign_str.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// 组装待签串：urlencoded 请求体 ‖ x-sq-public ‖ x-sq-cen ‖ x-sq-res-encrypt ‖ traceId。
fn gw_sign_str(form_body: &str, trace_id: &str) -> String {
    format!("{form_body}{GW_PUBLIC_BLOB}{GW_CEN}{GW_RES_ENCRYPT}{trace_id}")
}

/// percent 编码：仅保留 unreserved `A-Za-z0-9._-~`，其余按 UTF-8 字节大写百分号编码，
/// 与 App 侧 `URLEncoder` + dex 中 `j()` 修正（`*`/`+`/`%7E`）后的线上形态一致。
fn form_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 请求唯一标识：10 位小写十六进制（同时用于 x-sq-nonce 与 eagleeye-traceid）。
fn new_trace_id() -> String {
    let h = uuid::Uuid::new_v4().simple().to_string();
    h[..10].to_string()
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

    /// 原生网关搜索：POST `native_v3`，返回 `data.moduleInfos` 原始数组。
    ///
    /// 失败（HTTP 非 200 / 网关 `status` 非 200）一律 `Err` 上抛，**不回落到老 web 接口、不补字段**。
    pub fn native_search_modules(&self, keyword: &str, page: u32) -> Result<Vec<Value>> {
        // 字段名按 ASCII 升序拼接（与 App 的 TreeMap 一致），空值仍保留 `key=`。
        let pagination = format!(r#"{{"page":{page},"pageSize":10}}"#);
        let body = [
            ("fromSug", "1"),
            ("kind", ""),
            ("page", "searchResultV3"),
            ("pagination", pagination.as_str()),
            ("query", keyword),
            ("relatedBid", ""),
            ("showPost", "0"),
            ("showTypes", "aiSearch,drama"),
        ]
        .iter()
        .map(|(k, v)| format!("{k}={}", form_quote(v)))
        .collect::<Vec<_>>()
        .join("&");

        let trace = new_trace_id();
        let signature = gw_sign(&gw_sign_str(&body, &trace));
        let ts_ms = now_ts() * 1000;

        let resp = self
            .http
            .post(format!("{GW_BASE}{GW_PATH_SEARCH}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json, text/plain, */*")
            .header("user-agent", "okhttp/3.12.13")
            .header("x-sq-s-key", GW_APP_KEY)
            .header("x-sq-sv", GW_SV)
            .header("x-sq-req-platform", GW_PLATFORM)
            .header("x-sq-api-encrypt", "0")
            .header("x-sq-req-encrypt-param", GW_REQ_ENCRYPT_PARAM)
            .header("x-sq-public", GW_PUBLIC_BLOB)
            .header("x-sq-cen", GW_CEN)
            .header("x-sq-res-encrypt", GW_RES_ENCRYPT)
            .header("x-sq-timestamp", ts_ms.to_string())
            .header("x-sq-nonce", trace.as_str())
            .header("eagleeye-traceid", trace.as_str())
            .header("x-sq-signature", signature.as_str())
            .body(body)
            .send()
            .context("书旗 native_v3 请求失败")?;
        let code = resp.status().as_u16();
        let json: Value = resp
            .json()
            .with_context(|| format!("书旗 native_v3 响应解析失败（HTTP {code}）"))?;

        let status = json.get("status").and_then(Value::as_i64).unwrap_or(0);
        let msg = json.get("message").and_then(Value::as_str).unwrap_or("");
        if code != 200 || status != 200 {
            // 401 + Sign ERROR = 网关 key/算法失效（多为 App 升级换密钥），需重新提取 key。
            if code == 401 || msg.contains("Sign") {
                return Err(anyhow!(
                    "书旗网关签名失效（HTTP {code} status={status} message={msg}）：\
                     需重新从新版 APK 提取 x-sq-signature 密钥"
                ));
            }
            return Err(anyhow!(
                "书旗 native_v3 失败：HTTP {code} status={status} message={msg}"
            ));
        }

        let mods = json
            .get("data")
            .and_then(|d| d.get("moduleInfos"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        debug!("书旗 native_v3 返回 {} 个模块", mods.len());
        Ok(mods)
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
            let volumes = vol
                .get("volumeList")
                .and_then(|v| v.as_array())
                .map(|v| v.as_slice());
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

    /// 按书名走网关搜索取连载/完结态（`book.state`：2=完结、1=连载），并用 bookId 精确匹配。
    ///
    /// 背景：`openapi/book/chapterlist` 的响应里根本不存在 `isFinish`（`state` 键存在但为 null），
    /// 而网关搜索卡片带 `state`。
    ///
    /// 为何不用 bid 当关键词：实测 `native_search_modules("9349463", 1)` 会正常返回十余张卡片
    /// （均带 state），但**其中不包含 9349463 本身**（书旗把纯数字当文本匹配了别的书）。
    /// 因此用书名搜索，再按 bookId 对齐；匹配不到就返回 None，绝不用同名异书的卡片充数。
    /// 本方法**仅供更新调度判定使用**，不参与搜索结果的展示语义。
    pub fn book_state_by_search(&self, book_id: &str, book_name: &str) -> Option<bool> {
        let bid = strip_sq_prefix(book_id).to_string();
        let kw = book_name.trim();
        if kw.is_empty() {
            return None;
        }
        let modules = self.native_search_modules(kw, 1).ok()?;
        for m in modules {
            if m.get("displayTemplate").and_then(Value::as_str) != Some("SearchBookV2") {
                continue;
            }
            let Some(b) = m.get("book").filter(|x| x.is_object()) else {
                continue;
            };
            if native_book_id(b).as_deref() != Some(bid.as_str()) {
                continue;
            }
            if let Some(finished) = native_finished(b) {
                return Some(finished);
            }
        }
        None
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
            score: pick_str(data, &["score", "rating"])
                .or_else(|| {
                    data.get("scoreInfo")
                        .and_then(|si| si.get("bookScore"))
                        .and_then(|v| {
                            v.as_str()
                                .map(str::to_string)
                                .or_else(|| v.as_f64().map(|n| n.to_string()))
                        })
                })
                .and_then(|s| s.parse().ok()),
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
        if !cover_path.exists()
            && let Err(e) = download_cover(&client.http, cover_url, &cover_path)
        {
            debug!(target: "download", error = %e, "书旗封面下载失败（忽略）");
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
    let catalog = client
        .catalog(book_id)
        .context("fetch catalog for suffixes")?;
    let suffix_map: HashMap<String, String> = catalog
        .chapters
        .into_iter()
        .map(|c| (c.id, c.suffix))
        .collect();

    let worker_count = config.max_workers.clamp(1, 8); // 对书旗上游保持礼貌并发
    let (tx_jobs, rx_jobs) = channel::unbounded::<Vec<ChapterRef>>();
    let (tx_res, rx_res) = channel::unbounded::<Result<(Vec<ChapterRef>, Vec<(String, String)>)>>();

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
                            let _ = tx.send(Err(anyhow!("章节 {} 缺少 contUrlSuffix", ch.id)));
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

/// 取字符串字段；空串视为“无”返回 None（前端据此不渲染，不做兜底补全）。
fn native_str(b: &Value, key: &str) -> Option<String> {
    b.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

/// 评分：`novelScore` 为十分制字符串（实测可能为空串）。空串/非法值返回 null，
/// **绝不向 book/info 补分**。
fn native_score(b: &Value) -> Value {
    let num = match b.get("novelScore") {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    match num {
        Some(x) => serde_json::json!(x),
        None => Value::Null,
    }
}

/// bookId 可能以数字或字符串返回。
fn native_book_id(b: &Value) -> Option<String> {
    match b.get("bookId") {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        _ => None,
    }
}

/// `state`：2=完结、1=连载（native_v3 抓包实证），其余值按未知返回 None，不猜测。
fn native_finished(b: &Value) -> Option<bool> {
    match b.get("state").and_then(Value::as_i64) {
        Some(2) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// 字符串数组字段（tags）→ Vec<String>；非数组或缺失返回空列表。
fn native_str_array(b: &Value, key: &str) -> Vec<String> {
    b.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 角标：cornerTagExt[].text（原创/独家等），只取接口给的文本。
fn native_badges(b: &Value) -> Vec<String> {
    b.get("cornerTagExt")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("text").and_then(Value::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 搜索并返回 Web UI 所需的 JSON items，附带“是否还有下一页”。
///
/// 数据源为原生网关 `native_v3`（含 novelScore / displayBookName 官方高亮 / tags / wordCount）。
/// 字段严格原样直显：接口没给的一律为 null，前端不渲染；请求失败直接 `Err` 上抛，
/// 不回落到老 web 接口（i.php）。
pub fn search_items(client: &ShuqiClient, keyword: &str, page: u32) -> Result<(Vec<Value>, bool)> {
    let mut items: Vec<Value> = Vec::new();

    // 纯数字关键词且首页：先按 bookId 直查书旗 book/info，命中则置顶（独立结果，非字段补全）
    if page <= 1
        && keyword.chars().all(|c| c.is_ascii_digit())
        && !keyword.is_empty()
        && let Some(meta) = client.book_info(keyword)
        && meta.book_name.is_some()
    {
        items.push(serde_json::json!({
            "book_id": format!("sq:{keyword}"),
            "title": meta.book_name,
            "author": meta.author.unwrap_or_default(),
            "raw": meta.description.clone().unwrap_or_default(),
            "description": meta.description.unwrap_or_default(),
            "cover_url": meta.cover_url,
            "score": meta.score,
            "word_count": meta.word_count,
            "chapter_count": meta.chapter_count,
            "finished": meta.finished,
            "category": meta.category,
        }));
    }

    let modules = client.native_search_modules(keyword, page)?;
    let mut book_count = 0usize;
    for m in modules {
        // 只取书籍卡片；SearchDividerV2 / SearchRecommendV3 / SearchAICard / SearchGoldRank 不入列表。
        if m.get("displayTemplate").and_then(Value::as_str) != Some("SearchBookV2") {
            continue;
        }
        let Some(b) = m.get("book").filter(|x| x.is_object()) else {
            continue;
        };
        book_count += 1;
        let Some(bid) = native_book_id(b) else {
            continue;
        };
        let Some(title) = native_str(b, "bookName") else {
            continue;
        };
        let book_id = format!("sq:{bid}");
        // 与数字直查置顶项去重
        if items
            .iter()
            .any(|it| it["book_id"].as_str() == Some(book_id.as_str()))
        {
            continue;
        }
        let desc = native_str(b, "desc").unwrap_or_default();
        let cover = native_str(b, "imgUrl")
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"));
        items.push(serde_json::json!({
            "book_id": book_id,
            "title": title,
            // 官方高亮书名（含 <em>）；缺失则前端退回纯书名，不自算高亮
            "display_title": native_str(b, "displayBookName"),
            "official_highlight": true,
            "author": native_str(b, "authorName").unwrap_or_default(),
            "raw": desc,
            "description": desc,
            "cover_url": cover,
            "score": native_score(b),
            "word_count": b.get("wordCount").and_then(Value::as_u64),
            "chapter_count": Value::Null,
            "category": Value::Null,
            "finished": native_finished(b),
            "tags": native_str_array(b, "tags"),
            "badges": native_badges(b),
            "bottom_text": native_str(b, "bottomText"),
            "koc_display_info": native_str(b, "kocDisplayInfo"),
        }));
    }

    // native_v3 分页实测可用（page=1/2/3 结果互不重复）；本页无书卡即停止翻页。
    Ok((items, book_count > 0))
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
    fn test_gw_sign_matches_captured_sample() {
        // 真实抓包样本：POST stars.shuqireader.com/sqan/dipper/api/notification/switch/status
        let body = "notificationBarPermission=1&notificationPermission=1";
        let trace = "162edb3331";
        assert_eq!(
            gw_sign(&gw_sign_str(body, trace)),
            "da36b2812f49ad5fa0d20014df5df8ea57430a67"
        );
    }

    #[test]
    fn test_form_quote_matches_app_encoding() {
        assert_eq!(
            form_quote("醉酒错人"),
            "%E9%86%89%E9%85%92%E9%94%99%E4%BA%BA"
        );
        assert_eq!(
            form_quote(r#"{"page":1,"pageSize":10}"#),
            "%7B%22page%22%3A1%2C%22pageSize%22%3A10%7D"
        );
        assert_eq!(form_quote(""), "");
    }

    #[test]
    fn test_native_score_and_state() {
        let ok: Value = serde_json::from_str(r#"{"novelScore":"8.6","state":2}"#).unwrap();
        assert_eq!(native_score(&ok), serde_json::json!(8.6));
        assert_eq!(native_finished(&ok), Some(true));
        // 空评分必须是 null，不得补全
        let none: Value = serde_json::from_str(r#"{"novelScore":"","state":1}"#).unwrap();
        assert!(native_score(&none).is_null());
        assert_eq!(native_finished(&none), Some(false));
        let unknown: Value = serde_json::from_str(r#"{"novelScore":"9.1","state":7}"#).unwrap();
        assert_eq!(native_score(&unknown), serde_json::json!(9.1));
        assert_eq!(native_finished(&unknown), None);
    }

    /// 联网端到端（手动跑）：`cargo test --features shuqi -- --ignored --nocapture shuqi`
    /// 验证书旗网关签名 + 字段映射，并断言“接口无评分必须为 null”。
    #[test]
    #[ignore = "需要联网访问书旗网关"]
    fn e2e_native_v3_search() {
        let client = ShuqiClient::new(15).expect("client");
        let (items, has_more) = search_items(&client, "醉酒错人", 1).expect("native_v3 搜索失败");
        println!("has_more={has_more} 条数={}", items.len());
        let mut scored = 0;
        for it in items.iter().take(12) {
            println!(
                "  {} | score={} | finished={} | wc={} | tags={}",
                it["book_id"].as_str().unwrap_or("?"),
                it["score"],
                it["finished"],
                it["word_count"],
                it["tags"]
            );
            println!("    title  = {}", it["title"]);
            println!("    display = {}", it["display_title"]);
            if it["score"].is_number() {
                scored += 1;
            } else {
                assert!(it["score"].is_null(), "无评分必须为 null，不得被补全");
            }
        }
        assert!(!items.is_empty(), "native_v3 应返回结果");
        assert!(
            items.iter().any(|it| it["display_title"].is_string()),
            "应拿到官方 displayBookName"
        );
        println!("有评分 {scored} / 共 {} 本", items.len());
    }

    /// 联网端到端（手动跑）：验证“按书名搜索 + bookId 精确匹配”能拿到 chapterlist 缺失的连载/完结态。
    /// 同时留反证：按 bid 搜索命中不到目标书本身。
    /// `cargo test -- --ignored --nocapture e2e_book_state_by_search`
    #[test]
    #[ignore = "需要联网访问书旗网关"]
    fn e2e_book_state_by_search() {
        let client = ShuqiClient::new(15).expect("client");

        // 反证：bid 当关键词时，网关返回的卡片不包含目标书（因此不能靠 bid 取状态）。
        let by_bid = client
            .native_search_modules("9349463", 1)
            .expect("native_v3 by bid 请求失败");
        let hit_self = by_bid.iter().any(|m| {
            m.get("book")
                .and_then(native_book_id)
                .as_deref()
                .is_some_and(|id| id == "9349463")
        });
        println!("by-bid 卡片数={} 含目标书={hit_self}", by_bid.len());
        assert!(
            !hit_self,
            "实测：bid 当关键词不得命中目标书，否则应改回 bid 方案"
        );

        // 正用：书名搜索 + bookId 对齐。
        let got = client.book_state_by_search("sq:9349463", "戏神！");
        println!("sq:9349463 书名搜索-> {got:?}");
        assert!(got.is_some(), "按书名搜索应能取到该书连载/完结态");

        // 同书名但 id 不对 → 必须 None，不得拿同名异书的卡片充数。
        let wrong = client.book_state_by_search("sq:1", "戏神！");
        println!("sq:1 书名搜索-> {wrong:?}");
        assert_eq!(wrong, None);
    }
}
