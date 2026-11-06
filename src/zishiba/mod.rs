//! 姿势吧（zishiba.com，帝国CMS 架构）小说源适配。
//!
//! 接口清单（依据 examples/zishiba_解析结果 抓包，2026-11-05）：
//! - 搜索：`POST /e/search/index.php`，表单 `tbname=bookname&show=title,writer&tempid=1&keyboard=<关键字>`
//!   成功返回 `302 → /e/search/result/?searchid=N`；失败返回 200「信息提示」错误页。
//! - 搜索结果：`GET /e/search/result/?searchid=N`，条目 `li.search-wrap-first`，单页封顶 20 条、无翻页。
//! - 详情：`GET /book/{url_id}/`，`og:novel:*` meta 全套 + `data-bookid`（内部 ID）。
//!   注意：URL ID 与内部 ID 不同，目录接口用内部 ID，章节页 URL 用 URL ID。
//! - 目录：`GET /e/extend/bookpage/pages.php?id={内部ID}&pageNum={0起}&dz=asc`（XHR，JSON，
//!   `pageSize=10`，`totalPage` 页数，`list[{id,title,pic}]`）。
//! - 正文：`GET /book/{url_id}/{章节id}.html`，`h2#title` + `div#text` 内 `<p>` 段落。
//!
//! 两大站点限制（抓包实锤）与对策：
//! - **搜索冷却 30 秒**：按 Cookie `vbemnlastsearchtime`（会话维度）裁决，被拒请求不刷新、
//!   真正执行的搜索（含「无结果」）都刷新。对策：搜索链路**净身请求**（不回发该 Cookie），
//!   每次搜索等同「新访客首搜」；两次搜索间保留礼貌最小间隔；若站点实际按 IP 限频
//!   （净身仍被拒），兜底等待 30s+裕量后重试一次。不做代理轮换等进一步规避。
//! - **搜索关键字 2~20（实为 UTF-8 字节，EmpireCMS `strlen`）**：9 汉字 27B 被拒、
//!   5 汉字 15B 通过（sid567/sid569 夹逼）。对策：搜索前按字符边界截断到 ≤20 字节
//!   （纯汉字即 6 字/18B），不足 2 字节直接报错。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel as channel;
use regex::Regex;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::base_system::book_paths;
use crate::base_system::context::Config;
use crate::download::downloader::build_dynamic_chapter_groups;
use crate::download::models::{
    BookMeta, ChapterRef, DownloadPlan, DownloadResult, merge_meta_prefer_hint_name,
};

// ── 常量 ──────────────────────────────────────────────────────

const SITE_BASE: &str = "https://www.zishiba.com";

/// 服务端搜索冷却窗口（错误页原文「系统限制的搜索时间间隔为 30 秒」）。仅兜底路径使用。
const SEARCH_COOLDOWN: Duration = Duration::from_secs(30);
/// 冷却兜底的安全裕量（防时钟/网络抖动导致的边缘拒绝）。
const COOLDOWN_MARGIN: Duration = Duration::from_secs(1);
/// 净身请求下的礼貌最小搜索间隔（防连点突发；抓包显示站点无此要求）。
const SEARCH_MIN_GAP: Duration = Duration::from_secs(3);
/// 目录分页间隔（10 页目录逐页拉，留一点礼貌间隔）。
const TOC_PAGE_GAP: Duration = Duration::from_millis(150);
/// 目录页数保护上限（防站点异常返回超大 totalPage 导致死循环）。
const TOC_MAX_PAGES: u32 = 2_000;
/// 搜索关键字字节上限（站点报错文案写「2~20 个字符」，实测按 UTF-8 字节计）。
const KEYWORD_MAX_BYTES: usize = 20;
/// 搜索关键字字节下限。
const KEYWORD_MIN_BYTES: usize = 2;

// ── book_id 识别与规范化 ──────────────────────────────────────

/// `zs:` 前缀即姿势吧（识字吧）book_id。
pub fn is_zishiba_book_id(book_id: &str) -> bool {
    book_id.starts_with("zs:")
}

/// 去掉 `zs:` 前缀，返回纯数字部分（无前缀则原样返回）。
pub fn strip_zs_prefix(book_id: &str) -> &str {
    book_id.strip_prefix("zs:").unwrap_or(book_id)
}

/// 从用户输入识别姿势吧 book_id：`zs:113075` 前缀，或 zishiba.com 的书籍/章节页 URL。
/// 返回带前缀的规范化 book_id（如 `zs:113075`），非本源输入返回 None。
pub fn normalize_book_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("zs:")
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return Some(format!("zs:{rest}"));
    }
    // https://www.zishiba.com/book/113075/ 或 .../book/113075/1334231.html
    re_book_url()
        .captures(trimmed)
        .map(|c| format!("zs:{}", &c[1]))
}

/// 从 `zs:` book_id 提取数字 ID（URL ID）。非法输入报错。
fn parse_book_url_id(book_id: &str) -> Result<u64> {
    let rest = strip_zs_prefix(book_id);
    rest.parse::<u64>()
        .with_context(|| format!("非法姿势吧 book_id: {book_id:?}"))
}

// ── 关键字预处理 ──────────────────────────────────────────────

/// 站点按 UTF-8 字节数校验关键字 2~20（报错文案写「字符」实为字节）。
/// 超长时按字符边界截断到 ≤20 字节（纯汉字即 6 字/18B）；不足 2 字节报错。
fn sanitize_keyword(raw: &str) -> Result<String> {
    let kw = raw.trim();
    if kw.is_empty() {
        return Err(anyhow!("搜索关键字为空"));
    }
    if kw.len() < KEYWORD_MIN_BYTES {
        return Err(anyhow!(
            "搜索关键字过短：站点限制至少 {KEYWORD_MIN_BYTES} 字节（输入 {kw:?} 仅 {} 字节）",
            kw.len()
        ));
    }
    if kw.len() <= KEYWORD_MAX_BYTES {
        return Ok(kw.to_string());
    }
    // 按字符边界回退，避免切出半个多字节字符。
    let mut end = KEYWORD_MAX_BYTES;
    while end > 0 && !kw.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = &kw[..end];
    info!(
        target: "zishiba",
        raw_bytes = kw.len(),
        truncated_bytes = truncated.len(),
        truncated = truncated,
        "搜索关键字超过站点 20 字节上限，已按字符边界截断"
    );
    Ok(truncated.to_string())
}

// ── 正则缓存 ──────────────────────────────────────────────────

fn re_book_url() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)zishiba\.com/book/(\d+)").expect("compile re_book_url"))
}

fn re_searchid() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"searchid=(\d+)").expect("compile re_searchid"))
}

fn re_search_item() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<li class="search-wrap-first">(.*?)</li>"#)
            .expect("compile re_search_item")
    })
}

fn re_item_book() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?:https?://www\.zishiba\.com)?/book/(\d+)/?"#).expect("compile re_item_book")
    })
}

fn re_item_title() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<span class="title">(.*?)</span>"#).expect("compile re_item_title")
    })
}

fn re_item_author() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"作者[:：]\s*([^<]*?)\s*</h5>").expect("compile re_item_author"))
}

fn re_item_cover() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<img[^>]*src="([^"]+)""#).expect("compile re_item_cover"))
}

fn re_item_any_p() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<p>(.*?)</p>").expect("compile re_item_any_p"))
}

fn re_item_category() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<em class="green">(.*?)</em>"#).expect("compile re_item_category")
    })
}

fn re_book_internal_id() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"data-bookid="(\d+)""#).expect("compile re_book_internal_id"))
}

fn re_chapter_title() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<h2 id="title">(.*?)</h2>"#).expect("compile re_chapter_title"))
}

fn re_paragraph() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<p[^>]*>(.*?)</p>").expect("compile re_paragraph"))
}

// ── HTML 小工具 ───────────────────────────────────────────────

/// 剥掉字符串中的 HTML 标签（项目手法：手写字符循环，见 qimao/shuqi 同名实现）。
fn strip_tags(s: &str) -> String {
    if !s.contains('<') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for ch in s.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// 实体解码 + 剥标签 + 压缩空白的统一清洗。
fn clean_text(s: &str) -> String {
    let unescaped = crate::book_parser::html_utils::unescape_basic_entities(s);
    strip_tags(unescaped.as_ref())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 提取 `<meta property="{prop}" content="...">` 的值（站点模板属性顺序固定，字符串查找足够）。
fn og_value(html: &str, prop: &str) -> Option<String> {
    let marker = format!(r#"<meta property="{prop}" content=""#);
    let start = html.find(&marker)? + marker.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    Some(rest[..end].trim().to_string())
}

/// 「信息提示」错误页中提取原因文本（`cd-popup-container` 内第一个 `<p>`）。
fn extract_message(html: &str) -> Option<String> {
    let pos = html.find("cd-popup-container")?;
    let seg = &html[pos..];
    let p_start = seg.find("<p>")? + 3;
    let rest = &seg[p_start..];
    let p_end = rest.find("</p>")?;
    Some(clean_text(&rest[..p_end]))
}

fn is_message_page(html: &str) -> bool {
    html.contains("信息提示") && html.contains("cd-popup-container")
}

// ── 数据结构 ──────────────────────────────────────────────────

/// 搜索结果条目（解析自结果页，字段缺失为空串）。
#[derive(Debug, Clone)]
pub struct ZishibaSearchItem {
    /// 书籍 URL ID（`/book/{book_id}/`）。
    pub book_id: u64,
    pub title: String,
    pub author: String,
    pub description: String,
    pub cover_url: String,
    /// 品类标签（站点固定为「小说」）。
    pub category: String,
}

/// 书籍详情（解析自 `og:novel:*` meta）。
#[derive(Debug, Clone)]
pub struct ZishibaBookDetail {
    /// 内部 ID（目录接口使用，`data-bookid`；注意与章节页 URL 用的 URL ID 不同）。
    pub internal_id: u64,
    pub book_name: String,
    pub author: String,
    pub description: String,
    pub cover_url: String,
    pub category: String,
    pub finished: Option<bool>,
}

/// 目录分页。
pub(crate) struct TocPage {
    pub(crate) total: u64,
    pub(crate) total_page: u64,
    pub(crate) chapters: Vec<ChapterRef>,
}

// ── 客户端 ────────────────────────────────────────────────────

pub struct ZishibaClient {
    http: reqwest::blocking::Client,
    min_gap: Duration,
    /// 上次「真正执行」的搜索时刻（302 或「无结果」）。仅冷却兜底路径使用。
    last_executed: Mutex<Option<Instant>>,
    /// 上次搜索 POST 发起时刻（礼貌间隔节流）。
    last_post: Mutex<Option<Instant>>,
}

impl ZishibaClient {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(5)))
            // 站点文档页（/、/book/*、/e/search/result/*）的 WAF 会直接断开「桌面 UA」的连接
            // （实测桌面 Chrome/curl/reqwest 全部被拒，移动端 UA 正常），故全程使用移动端 UA。
            .user_agent(
                "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36",
            )
            // 搜索 POST 返回 302，必须自己接住以区分「执行成功 / 被拒」。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build zishiba HTTP client")?;
        Ok(Self {
            http,
            min_gap: SEARCH_MIN_GAP,
            last_executed: Mutex::new(None),
            last_post: Mutex::new(None),
        })
    }

    /// 两次搜索 POST 之间的礼貌最小间隔（净身请求下站点无硬性限制，此为自律节流）。
    fn throttle_gap(&self) {
        let wait = {
            let prev = self.last_post.lock().unwrap_or_else(|e| e.into_inner());
            match *prev {
                Some(t) => {
                    let elapsed = t.elapsed();
                    (elapsed < self.min_gap).then(|| self.min_gap - elapsed)
                }
                None => None,
            }
        };
        if let Some(d) = wait {
            thread::sleep(d);
        }
        *self.last_post.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    fn mark_executed(&self) {
        *self.last_executed.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    /// 冷却兜底：距上次「真正执行」不足 30s+裕量时等待补齐（未知则等满一个窗口）。
    fn wait_cooldown_fallback(&self) {
        let need = SEARCH_COOLDOWN + COOLDOWN_MARGIN;
        let wait = {
            let prev = self.last_executed.lock().unwrap_or_else(|e| e.into_inner());
            match *prev {
                Some(t) => {
                    let elapsed = t.elapsed();
                    (elapsed < need).then(|| need - elapsed)
                }
                None => Some(need),
            }
        };
        if let Some(d) = wait {
            warn!(
                target: "zishiba",
                wait_secs = d.as_secs(),
                "识字吧搜索被冷却拒绝（站点疑似按 IP 限频），等待冷却窗口后重试一次"
            );
            thread::sleep(d);
        }
    }

    /// 搜索。空结果（站点明确「没有搜索到相关的内容」）返回空列表而非错误。
    pub fn search(&self, raw_keyword: &str) -> Result<Vec<ZishibaSearchItem>> {
        let keyword = sanitize_keyword(raw_keyword)?;
        match self.attempt_search(&keyword) {
            Ok(items) => Ok(items),
            Err(AttemptError::Cooldown) => {
                self.wait_cooldown_fallback();
                match self.attempt_search(&keyword) {
                    Ok(items) => Ok(items),
                    Err(AttemptError::Cooldown) => {
                        Err(anyhow!("识字吧搜索冷却中（已等待重试仍被拒），请稍后再试"))
                    }
                    Err(AttemptError::Other(e)) => Err(e),
                }
            }
            Err(AttemptError::Other(e)) => Err(e),
        }
    }

    fn attempt_search(
        &self,
        keyword: &str,
    ) -> std::result::Result<Vec<ZishibaSearchItem>, AttemptError> {
        self.throttle_gap();
        let url = format!("{SITE_BASE}/e/search/index.php");
        // 表单与首页搜索框一致；reqwest 的 form 编码会把 show 的逗号转成 %2C，与抓包一致。
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("tbname", "bookname"),
                ("show", "title,writer"),
                ("tempid", "1"),
                ("keyboard", keyword),
            ])
            .send()
            .context("识字吧搜索请求失败")
            .map_err(AttemptError::Other)?;
        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let searchid = re_searchid()
                .captures(&location)
                .and_then(|c| c[1].parse::<u64>().ok())
                .ok_or_else(|| anyhow!("识字吧搜索重定向缺少 searchid: {location}"))
                .map_err(AttemptError::Other)?;
            // 302 = 本次搜索已在服务端真正执行（会刷新冷却）。
            self.mark_executed();
            let items = self
                .fetch_search_result(searchid)
                .map_err(AttemptError::Other)?;
            return Ok(items);
        }
        let body = resp
            .text()
            .context("识字吧搜索响应读取失败")
            .map_err(AttemptError::Other)?;
        if is_message_page(&body) {
            let msg = extract_message(&body).unwrap_or_default();
            if msg.contains("时间间隔") {
                // 冷却拒绝：服务端未执行本次搜索，不推进 last_executed。
                return Err(AttemptError::Cooldown);
            }
            if msg.contains("没有搜索到") {
                // 「无结果」也算真正执行（抓包证实会刷新冷却）。
                self.mark_executed();
                return Ok(Vec::new());
            }
            if msg.contains("关键字") {
                return Err(AttemptError::Other(anyhow!(
                    "识字吧拒绝搜索关键字（站点限制 {KEYWORD_MIN_BYTES}~{KEYWORD_MAX_BYTES} 字节）：{msg}"
                )));
            }
            return Err(AttemptError::Other(anyhow!("识字吧搜索失败：{msg}")));
        }
        Err(AttemptError::Other(anyhow!(
            "识字吧搜索返回异常 HTTP {status}"
        )))
    }

    /// GET 并返回响应文本。结果页/详情/目录/章节共用。
    /// 站点 WAF 会随机掐断连接（强制重协商后断开），传输错误时换新连接重试一次。
    fn get_text(&self, url: &str, xhr: bool) -> Result<String> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..2u32 {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(300));
            }
            let outcome =
                (|| -> std::result::Result<(reqwest::StatusCode, String), reqwest::Error> {
                    let mut req = self.http.get(url);
                    if xhr {
                        req = req
                            .header("X-Requested-With", "XMLHttpRequest")
                            .header(reqwest::header::REFERER, format!("{SITE_BASE}/book/"));
                    }
                    let resp = req.send()?;
                    let status = resp.status();
                    let text = resp.text()?;
                    Ok((status, text))
                })();
            match outcome {
                Ok((status, text)) if status.is_success() => return Ok(text),
                Ok((status, _)) => {
                    return Err(anyhow!("识字吧请求 HTTP {status}：{url}"));
                }
                Err(e) => {
                    debug!(target: "zishiba", url, attempt, error = %e, "识字吧请求传输错误，将重试");
                    last_err =
                        Some(anyhow::Error::new(e).context(format!("识字吧请求失败：{url}")));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("识字吧请求失败：{url}")))
    }

    fn fetch_search_result(&self, searchid: u64) -> Result<Vec<ZishibaSearchItem>> {
        let url = format!("{SITE_BASE}/e/search/result/?searchid={searchid}");
        let html = self.get_text(&url, false)?;
        if is_message_page(&html) {
            let msg = extract_message(&html).unwrap_or_default();
            return Err(anyhow!("识字吧搜索结果页返回提示：{msg}"));
        }
        Ok(parse_search_result(&html))
    }

    /// 书籍详情（`/book/{url_id}/`，og:novel:* meta）。
    pub fn book_detail(&self, url_id: u64) -> Result<ZishibaBookDetail> {
        let url = format!("{SITE_BASE}/book/{url_id}/");
        let html = self.get_text(&url, false)?;
        parse_book_detail(&html).ok_or_else(|| anyhow!("识字吧详情页解析失败（url_id={url_id}）"))
    }

    /// 目录分页（`pages.php?id={内部ID}&pageNum={0起}&dz=asc`，JSON）。
    pub(crate) fn toc_page(&self, internal_id: u64, page_num: u32) -> Result<TocPage> {
        let url = format!(
            "{SITE_BASE}/e/extend/bookpage/pages.php?id={internal_id}&pageNum={page_num}&dz=asc"
        );
        let text = self.get_text(&url, true)?;
        parse_toc_page(&text)
    }

    /// 全量目录：按 totalPage 逐页拉取拼接。
    pub fn fetch_all_chapters(&self, internal_id: u64) -> Result<Vec<ChapterRef>> {
        let first = self.toc_page(internal_id, 0)?;
        let total_page = first.total_page.min(TOC_MAX_PAGES as u64).max(1);
        let mut chapters = first.chapters;
        for page in 1..total_page {
            thread::sleep(TOC_PAGE_GAP);
            let page_data = self.toc_page(internal_id, page as u32)?;
            chapters.extend(page_data.chapters);
        }
        if chapters.is_empty() {
            return Err(anyhow!("识字吧目录为空（internal_id={internal_id}）"));
        }
        Ok(chapters)
    }

    /// 章节正文。返回 (标题, 干净的 `<p>` 段落 HTML)。
    pub fn chapter_content(&self, url_id: u64, chapter_id: &str) -> Result<(String, String)> {
        let url = format!("{SITE_BASE}/book/{url_id}/{chapter_id}.html");
        let html = self.get_text(&url, false)?;
        if is_message_page(&html) {
            let msg = extract_message(&html).unwrap_or_default();
            return Err(anyhow!("识字吧章节页返回提示：{msg}"));
        }
        parse_chapter(&html)
            .ok_or_else(|| anyhow!("识字吧章节页解析失败（chapter_id={chapter_id}）"))
    }
}

enum AttemptError {
    /// 服务端冷却拒绝（未执行本次搜索）。
    Cooldown,
    Other(anyhow::Error),
}

// ── 解析（纯函数，夹具回归测试覆盖）────────────────────────────

/// 解析搜索结果页 → 条目列表。
fn parse_search_result(html: &str) -> Vec<ZishibaSearchItem> {
    let mut items = Vec::new();
    for cap in re_search_item().captures_iter(html) {
        let block = &cap[1];
        let Some(book_cap) = re_item_book().captures(block) else {
            continue;
        };
        let Ok(book_id) = book_cap[1].parse::<u64>() else {
            continue;
        };
        let title = re_item_title()
            .captures(block)
            .map(|c| clean_text(&c[1]))
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let author = re_item_author()
            .captures(block)
            .map(|c| clean_text(&c[1]))
            .unwrap_or_default();
        let description = re_item_any_p()
            .captures(block)
            .map(|c| clean_text(&c[1]))
            .unwrap_or_default();
        let cover_url = re_item_cover()
            .captures(block)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        let category = re_item_category()
            .captures(block)
            .map(|c| clean_text(&c[1]))
            .unwrap_or_default();
        items.push(ZishibaSearchItem {
            book_id,
            title,
            author,
            description,
            cover_url,
            category,
        });
    }
    items
}

/// 解析书籍详情页（og:novel:* meta）。
fn parse_book_detail(html: &str) -> Option<ZishibaBookDetail> {
    let book_name = og_value(html, "og:novel:book_name")
        .or_else(|| og_value(html, "og:title"))
        .map(|s| clean_text(&s))
        .filter(|s| !s.is_empty())?;
    let internal_id = re_book_internal_id()
        .captures(html)
        .and_then(|c| c[1].parse::<u64>().ok())?;
    let author = og_value(html, "og:novel:author")
        .map(|s| clean_text(&s))
        .unwrap_or_default();
    let description = og_value(html, "og:description")
        .map(|s| clean_text(&s))
        .unwrap_or_default();
    let cover_url = og_value(html, "og:image").unwrap_or_default();
    let category = og_value(html, "og:novel:category")
        .map(|s| clean_text(&s))
        .unwrap_or_default();
    let finished = og_value(html, "og:novel:status").map(|s| {
        let s = clean_text(&s);
        s.contains("完本") || s.contains("完结")
    });
    Some(ZishibaBookDetail {
        internal_id,
        book_name,
        author,
        description,
        cover_url,
        category,
        finished,
    })
}

/// 解析目录分页 JSON（`total` 为字符串、`totalPage` 为数字，兼容两种类型）。
fn parse_toc_page(text: &str) -> Result<TocPage> {
    let v: Value = serde_json::from_str(text).context("识字吧目录 JSON 解析失败")?;
    let pick_u64 = |key: &str| -> Option<u64> {
        match v.get(key) {
            Some(Value::String(s)) => s.trim().parse().ok(),
            Some(Value::Number(n)) => n.as_u64(),
            _ => None,
        }
    };
    let mut chapters = Vec::new();
    if let Some(list) = v.get("list").and_then(Value::as_array) {
        for it in list {
            let Some(id) = it.get("id").and_then(Value::as_str).map(str::to_string) else {
                continue;
            };
            let title = it
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);
            let Some(title) = title else { continue };
            chapters.push(ChapterRef { id, title });
        }
    }
    Ok(TocPage {
        total: pick_u64("total").unwrap_or(chapters.len() as u64),
        total_page: pick_u64("totalPage").unwrap_or(1),
        chapters,
    })
}

/// 解析章节页 → (标题, `<p>` 段落 HTML)。正文取 `id="text"` 到 `id="blankDiv"` 之间。
fn parse_chapter(html: &str) -> Option<(String, String)> {
    let title = re_chapter_title()
        .captures(html)
        .map(|c| clean_text(&c[1]))
        .unwrap_or_default();
    let text_pos = html.find(r#"id="text""#)?;
    let open_end = html[text_pos..].find('>')? + text_pos + 1;
    let seg_end = html.find(r#"id="blankDiv""#).unwrap_or(html.len());
    let seg = &html[open_end..seg_end];
    // 截到最后一个 </div> 之前（防御正文内出现嵌套 div）。
    let seg = match seg.rfind("</div>") {
        Some(p) => &seg[..p],
        None => seg,
    };
    let mut paragraphs = Vec::new();
    for cap in re_paragraph().captures_iter(seg) {
        let inner = cap[1].trim();
        if inner.is_empty() {
            continue;
        }
        let plain = strip_tags(inner);
        if plain.trim().is_empty() {
            continue;
        }
        let unescaped = crate::book_parser::html_utils::unescape_basic_entities(inner);
        paragraphs.push(format!("<p>{}</p>", unescaped.trim()));
    }
    if paragraphs.is_empty() {
        return None;
    }
    Some((title, paragraphs.join("\n")))
}

// ── 下载计划 ──────────────────────────────────────────────────

pub fn prepare_zishiba_plan(
    config: &Config,
    book_id: &str,
    meta_hint: BookMeta,
) -> Result<DownloadPlan> {
    info!(target: "download", book_id, "准备姿势吧（识字吧）下载计划");

    let url_id = parse_book_url_id(book_id)?;
    let timeout = config.request_timeout.max(5);
    let client = ZishibaClient::new(timeout).context("init ZishibaClient")?;

    let detail = client
        .book_detail(url_id)
        .context("fetch zishiba book detail")?;
    let chapters = client
        .fetch_all_chapters(detail.internal_id)
        .context("fetch zishiba catalog")?;
    let chapter_count = chapters.len();

    let dir_meta = BookMeta {
        book_name: Some(detail.book_name.clone()),
        author: (!detail.author.is_empty()).then(|| detail.author.clone()),
        description: (!detail.description.is_empty()).then(|| detail.description.clone()),
        tags: Vec::new(),
        cover_url: (!detail.cover_url.is_empty()).then(|| detail.cover_url.clone()),
        detail_cover_url: None,
        finished: detail.finished,
        chapter_count: Some(chapter_count),
        word_count: None,
        score: None,
        read_count: None,
        read_count_text: None,
        book_short_name: None,
        original_book_name: None,
        first_chapter_title: chapters.first().map(|c| c.title.clone()),
        last_chapter_title: chapters.last().map(|c| c.title.clone()),
        category: (!detail.category.is_empty()).then(|| detail.category.clone()),
        cover_primary_color: None,
    };

    let merged = merge_meta_prefer_hint_name(dir_meta, meta_hint);

    // best-effort：下载封面
    if let Some(cover_url) = merged.cover_url.as_ref() {
        let folder = book_paths::book_folder_path(config, book_id, merged.book_name.as_deref());
        let _ = std::fs::create_dir_all(&folder);
        let cover_path = book_paths::canonical_cover_path(&folder, "jpg");
        if !cover_path.exists()
            && let Err(e) = download_cover(&client.http, cover_url, &cover_path)
        {
            debug!(target: "download", error = %e, "姿势吧封面下载失败（忽略）");
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

/// 姿势吧章节下载流程：工作池并发拉取章节页，逐章保存（同书旗逐章型）。
#[allow(clippy::too_many_arguments)]
pub fn download_zishiba_into_manager(
    config: &Config,
    book_id: &str,
    book_name: &str,
    manager: &mut crate::book_parser::book_manager::BookManager,
    _chosen_chapters: &[ChapterRef],
    pending_chapters: &[ChapterRef],
    reporter: &mut crate::download::progress::ProgressReporter,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<DownloadResult> {
    let url_id = parse_book_url_id(book_id)?;
    let timeout = config.request_timeout.max(5);
    let client = Arc::new(ZishibaClient::new(timeout).context("init ZishibaClient")?);

    // 个人小站，并发保持克制（书旗为 1..8）。
    let worker_count = config.max_workers.clamp(1, 4);
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
                    match client.chapter_content(url_id, &ch.id) {
                        Ok((_title, html)) if !html.trim().is_empty() => {
                            contents.push((ch.id.clone(), html));
                        }
                        Ok(_) => {
                            warn!(
                                target: "download",
                                chapter_id = %ch.id,
                                "姿势吧章节正文为空：{} ({})",
                                ch.title, ch.id
                            );
                        }
                        Err(e) => {
                            warn!(
                                target: "download",
                                chapter_id = %ch.id,
                                error = %e,
                                "姿势吧章节下载失败：{} ({})",
                                ch.title, ch.id
                            );
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

        let content_map: std::collections::HashMap<String, String> = contents.into_iter().collect();
        for ch in &group {
            match content_map.get(&ch.id) {
                Some(html) if !html.is_empty() => {
                    manager.save_chapter(&ch.id, &ch.title, html);
                    manager.append_downloaded_chapter(&ch.id, &ch.title, html);
                    result.success += 1;
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
        "姿势吧下载完成：{} ({} 章，成功 {}，失败 {})",
        book_name,
        pending_chapters.len(),
        result.success,
        result.failed
    );
    Ok(result)
}

// ── 搜索（Web UI / TUI）───────────────────────────────────────

/// 搜索并返回 Web UI 所需的 JSON items。
/// 结果页单页封顶 20 条且无翻页控件，`has_more` 恒为 false，`page>1` 返回空。
pub fn search_items(
    client: &ZishibaClient,
    keyword: &str,
    page: u32,
) -> Result<(Vec<Value>, bool)> {
    if page > 1 {
        return Ok((Vec::new(), false));
    }
    let items = client.search(keyword)?;
    let out = items
        .into_iter()
        .map(|it| {
            json!({
                "book_id": format!("zs:{}", it.book_id),
                "title": it.title,
                "author": it.author,
                "raw": it.description,
                "description": it.description,
                "cover_url": (!it.cover_url.is_empty()).then_some(it.cover_url),
                "category": (!it.category.is_empty()).then_some(it.category),
                // 结果页不含这些字段，按约定置 Null 不补全。
                "word_count": Value::Null,
                "chapter_count": Value::Null,
                "finished": Value::Null,
                "score": Value::Null,
            })
        })
        .collect();
    Ok((out, false))
}

// ── BookProvider 适配器 ────────────────────────────────────────

/// 姿势吧源适配器：实现统一 [`crate::provider::BookProvider`]，
/// 全部委托给模块内既有自由函数（保持旧调用与测试不变）。
pub struct ZishibaProvider;

impl crate::provider::BookProvider for ZishibaProvider {
    fn id(&self) -> &'static str {
        "zishiba"
    }

    fn display_name(&self) -> &'static str {
        "姿势吧"
    }

    fn owns_book_id(&self, book_id: &str) -> bool {
        is_zishiba_book_id(book_id)
    }

    fn normalize_book_input(&self, input: &str) -> Option<String> {
        normalize_book_input(input)
    }

    // 纯数字不回退 zishiba：ID 无区分度，误回退会下载到错误的书。

    fn search(
        &self,
        opts: &crate::provider::SearchOptions,
    ) -> anyhow::Result<(Vec<serde_json::Value>, bool)> {
        let client = ZishibaClient::new(opts.timeout_secs)?;
        search_items(&client, &opts.keyword, opts.page)
    }

    fn preview_summary(
        &self,
        book_id: &str,
        timeout_secs: u64,
    ) -> Option<crate::download::models::BookMeta> {
        // 目录是 10 章/页（123 章 = 13 页），预览只需要总数 + 首章/末章：
        // 只取第 1 页与最后一页，避免向站点拉 13 页目录。
        let url_id = parse_book_url_id(book_id).ok()?;
        let client = ZishibaClient::new(timeout_secs).ok()?;
        let detail = client.book_detail(url_id).ok()?;
        let first = client.toc_page(detail.internal_id, 0).ok()?;
        let total_pages = first.total_page.max(1);
        let last_title = if total_pages > 1 {
            client
                .toc_page(detail.internal_id, (total_pages - 1) as u32)
                .ok()
                .and_then(|p| p.chapters.last().map(|c| c.title.clone()))
        } else {
            first.chapters.last().map(|c| c.title.clone())
        };

        Some(crate::download::models::BookMeta {
            book_name: Some(detail.book_name),
            author: (!detail.author.is_empty()).then_some(detail.author),
            description: (!detail.description.is_empty()).then_some(detail.description),
            tags: Vec::new(),
            cover_url: (!detail.cover_url.is_empty()).then_some(detail.cover_url),
            detail_cover_url: None,
            finished: detail.finished,
            chapter_count: Some(first.total as usize),
            word_count: None,
            score: None,
            read_count: None,
            read_count_text: None,
            book_short_name: None,
            original_book_name: None,
            first_chapter_title: first.chapters.first().map(|c| c.title.clone()),
            last_chapter_title: last_title,
            category: (!detail.category.is_empty()).then_some(detail.category),
            cover_primary_color: None,
        })
    }

    fn prepare_plan(
        &self,
        config: &crate::base_system::context::Config,
        book_id: &str,
        meta_hint: crate::download::models::BookMeta,
    ) -> anyhow::Result<crate::download::models::DownloadPlan> {
        prepare_zishiba_plan(config, book_id, meta_hint)
    }

    fn download_into_manager(
        &self,
        config: &crate::base_system::context::Config,
        book_id: &str,
        book_name: &str,
        manager: &mut crate::book_parser::book_manager::BookManager,
        chosen_chapters: &[crate::download::models::ChapterRef],
        pending_chapters: &[crate::download::models::ChapterRef],
        reporter: &mut crate::download::progress::ProgressReporter,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> anyhow::Result<crate::download::models::DownloadResult> {
        download_zishiba_into_manager(
            config,
            book_id,
            book_name,
            manager,
            chosen_chapters,
            pending_chapters,
            reporter,
            cancel,
        )
    }

    fn fetch_update_state(
        &self,
        book_id: &str,
        _book_name: Option<&str>,
    ) -> Option<(usize, Option<bool>)> {
        // 详情页取完结态（og:novel:status）+ 目录首页取章节数（total）。
        let client = ZishibaClient::new(crate::provider::UPDATE_FETCH_TIMEOUT_SECS).ok()?;
        let url_id = strip_zs_prefix(book_id).parse::<u64>().ok()?;
        let detail = client.book_detail(url_id).ok()?;
        let toc = client.toc_page(detail.internal_id, 0).ok()?;
        let total = usize::try_from(toc.total).ok()?;
        if total == 0 {
            return None;
        }
        Some((total, detail.finished))
    }
}

// ── 离线解析回归（夹具来自抓包，防解析回归）────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keyword_follows_site_byte_limit() {
        // 抓包实锤：9 汉字 27B 被拒（sid567）、5 汉字 15B 通过（sid569）→ 上限 20 字节。
        // 13 字全名（41B）应截断为前 6 字（18B）。
        let kw = sanitize_keyword("救了病娇后他总想杀我[穿书]").unwrap();
        assert_eq!(kw, "救了病娇后他");
        assert_eq!(kw.len(), 18);
        // 25 个 ASCII → 截 20。
        assert_eq!(sanitize_keyword(&"a".repeat(25)).unwrap(), "a".repeat(20));
        // 20 字节恰好不截断。
        assert_eq!(sanitize_keyword(&"b".repeat(20)).unwrap().len(), 20);
        // 界内原样（仅 trim）。
        assert_eq!(sanitize_keyword(" 病娇 ").unwrap(), "病娇");
        // 10 汉字（30B）→ 前 6 字，按字符边界切。
        assert_eq!(sanitize_keyword(&"汉".repeat(10)).unwrap(), "汉".repeat(6));
        // 过短 / 空 → 明确报错。
        assert!(sanitize_keyword("a").is_err());
        assert!(sanitize_keyword("   ").is_err());
        assert!(sanitize_keyword("").is_err());
    }

    #[test]
    fn normalize_book_input_variants() {
        assert_eq!(
            normalize_book_input("zs:113075").as_deref(),
            Some("zs:113075")
        );
        assert_eq!(
            normalize_book_input("ZS:113075").as_deref(),
            Some("zs:113075")
        );
        assert_eq!(
            normalize_book_input("https://www.zishiba.com/book/113075/").as_deref(),
            Some("zs:113075")
        );
        assert_eq!(
            normalize_book_input("https://www.zishiba.com/book/113075/1334231.html").as_deref(),
            Some("zs:113075")
        );
        // 非本源输入不认领。
        assert_eq!(normalize_book_input("113075"), None);
        assert_eq!(normalize_book_input("qm:123"), None);
        assert_eq!(normalize_book_input("https://www.shuqi.com/book/123"), None);
        assert_eq!(normalize_book_input("zs:abc"), None);
    }

    #[test]
    #[ignore = "实网 bench：姿势吧详情/目录单请求耗时"]
    fn bench_preview_sources() {
        let client = ZishibaClient::new(20).expect("client");
        let t = std::time::Instant::now();
        let d = client.book_detail(114317).expect("detail");
        println!("detail={:?} internal_id={}", t.elapsed(), d.internal_id);
        let t = std::time::Instant::now();
        let p = client.toc_page(d.internal_id, 0).expect("toc0");
        println!(
            "toc0={:?} total={} total_page={} page_size={}",
            t.elapsed(),
            p.total,
            p.total_page,
            p.chapters.len()
        );
        let t = std::time::Instant::now();
        let all = client.fetch_all_chapters(d.internal_id).expect("all");
        println!("all={:?} count={}", t.elapsed(), all.len());
    }

    #[test]
    fn parse_search_result_matches_capture() {
        // 抓包真值：searchid=2987（关键字「病娇」）20 条，首条 zs:123808。
        let html = include_str!("testdata/search_result_2987.html");
        let items = parse_search_result(html);
        assert_eq!(items.len(), 20);
        assert_eq!(items[0].book_id, 123808);
        assert_eq!(items[0].title, "社恐小姐被病娇带回家之后");
        assert_eq!(items[0].author, "西样");
        assert_eq!(items[0].category, "小说");
        assert!(
            items[0]
                .cover_url
                .contains("4da6ed8ebb09690207b2ad9509603a37.jpg")
        );
        assert!(!items[0].description.is_empty());

        // 抓包真值：searchid=16830（关键字「救了病娇后他」）恰 1 条 zs:113075。
        let html2 = include_str!("testdata/search_result_16830.html");
        let items2 = parse_search_result(html2);
        assert_eq!(items2.len(), 1);
        assert_eq!(items2[0].book_id, 113075);
        assert_eq!(items2[0].title, "救了病娇后他总想杀我[穿书]");
        assert_eq!(items2[0].author, "星棘");
    }

    #[test]
    fn parse_book_detail_matches_capture() {
        let html = include_str!("testdata/book_113075.html");
        let detail = parse_book_detail(html).expect("应能解析详情");
        assert_eq!(detail.internal_id, 115955);
        assert_eq!(detail.book_name, "救了病娇后他总想杀我[穿书]");
        assert_eq!(detail.author, "星棘");
        assert_eq!(detail.finished, Some(true));
        assert!(
            detail
                .cover_url
                .contains("06f1ad13359df936c1d098624e924082.jpg")
        );
        assert!(!detail.description.is_empty());
        // og meta 里未入结构体的字段也按抓包值校验（回归模板解析）。
        assert_eq!(
            og_value(html, "og:novel:latest_chapter_name").as_deref(),
            Some("第98章 番外6 襄陵奇遇记（二）")
        );
        assert_eq!(
            og_value(html, "og:novel:update_time").as_deref(),
            Some("2024-10-07 08:43:26")
        );
    }

    #[test]
    fn parse_toc_pages_match_capture() {
        let page0 = parse_toc_page(include_str!("testdata/toc_page0.json")).unwrap();
        assert_eq!(page0.total, 98);
        assert_eq!(page0.total_page, 10);
        assert_eq!(page0.chapters.len(), 10);
        assert_eq!(page0.chapters[0].id, "1334231");
        assert_eq!(page0.chapters[0].title, "第1章");

        let page9 = parse_toc_page(include_str!("testdata/toc_page9.json")).unwrap();
        assert_eq!(page9.chapters.len(), 8);
        assert_eq!(page9.chapters.last().unwrap().id, "1334419");
        assert_eq!(
            page9.chapters.last().unwrap().title,
            "第98章 番外6 襄陵奇遇记（二）"
        );
    }

    #[test]
    fn parse_chapter_matches_capture() {
        let html = include_str!("testdata/chapter_1334231.html");
        let (title, body) = parse_chapter(html).expect("应能解析章节");
        assert_eq!(title, "第1章");
        assert!(body.contains("白稚醒了。"), "正文应含抓包真值开头段");
        assert!(!body.contains("<script"), "正文不应残留 script");
        assert!(!body.contains("cd-popup"), "不应混入弹窗元素");
        // 抓包正文 90+ 段。
        assert!(body.matches("<p>").count() >= 80, "段落数应接近抓包正文");
    }

    #[test]
    fn parse_error_pages_from_capture() {
        let cooldown = extract_message(include_str!("testdata/err_cooldown.html")).unwrap();
        assert!(
            cooldown.contains("时间间隔"),
            "冷却错误页应含「时间间隔」: {cooldown}"
        );
        assert!(is_message_page(include_str!("testdata/err_cooldown.html")));

        let no_result = extract_message(include_str!("testdata/err_no_result.html")).unwrap();
        assert!(no_result.contains("没有搜索到"));

        let too_long = extract_message(include_str!("testdata/err_keyword_len.html")).unwrap();
        assert!(too_long.contains("关键字只能在"));
    }
}

// ── 实网 E2E（非重放：直连 zishiba.com，断言值以抓包为真值锚点）──
//
// 运行：cargo test zishiba -- --ignored --nocapture
// 负载：单链路、人类手速级（搜索间隔 ≥3s 礼貌间隔 + 显式 sleep）。
//
// 2026-11-06 实测：站点文档页 WAF 拒绝「桌面 UA」（连接直接断开，桌面 Chrome 也不例外），
// 移动端 UA 全部放行 —— 适配器因此全程使用移动端 UA，两个用例在任何网络均可直连运行。
#[cfg(test)]
mod live_e2e {
    use super::*;
    use std::sync::Mutex;

    /// 串行化实网用例，防并行测试互相污染冷却观测。
    static LIVE_LOCK: Mutex<()> = Mutex::new(());

    /// 搜索 POST 的原始观测结果（供实网用例断言）。
    enum RawSearchPost {
        /// 302 + searchid（服务端真正执行了本次搜索）。
        Executed {
            searchid: u64,
            set_cookie: Option<String>,
        },
        /// 200「信息提示」错误页（未执行）。
        Rejected { message: String },
    }

    /// 直接发一次搜索 POST（绕过适配层节流/重试，用于观察站点原始行为）。
    fn raw_search_post(client: &ZishibaClient, raw_keyword: &str) -> RawSearchPost {
        let resp = client
            .http
            .post(format!("{SITE_BASE}/e/search/index.php"))
            .form(&[
                ("tbname", "bookname"),
                ("show", "title,writer"),
                ("tempid", "1"),
                ("keyboard", raw_keyword),
            ])
            .send()
            .expect("搜索 POST 请求失败");
        let status = resp.status();
        let set_cookie = resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .find_map(|v| {
                let s = v.to_str().ok()?;
                s.starts_with("vbemnlastsearchtime=")
                    .then(|| s.split(';').next().unwrap_or("").to_string())
            });
        if status.is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let searchid = re_searchid()
                .captures(&location)
                .and_then(|c| c[1].parse::<u64>().ok())
                .expect("302 重定向应携带 searchid");
            return RawSearchPost::Executed {
                searchid,
                set_cookie,
            };
        }
        let body = resp.text().expect("读取错误页");
        let message = extract_message(&body).unwrap_or_default();
        assert!(
            is_message_page(&body),
            "非重定向响应应是「信息提示」错误页，实际 HTTP {status}"
        );
        RawSearchPost::Rejected { message }
    }

    /// A：搜索 POST 层实网验证 —— 冷却绕过 + 关键字截断（双向：站点限制 + 适配层对策）。
    #[test]
    #[ignore = "实网 e2e：需联网访问 zishiba.com（搜索 POST 层，任何网络可跑）"]
    fn e2e_live_search_post_layer() {
        let _guard = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let client = ZishibaClient::new(20).expect("init live client");

        // ① 首搜「病娇」。抓包锚点：302 + searchid=2987 + Set-Cookie vbemnlastsearchtime。
        let t0 = Instant::now();
        match raw_search_post(&client, "病娇") {
            RawSearchPost::Executed {
                searchid,
                set_cookie,
            } => {
                println!(
                    "[①] 首搜「病娇」→ 302 执行成功，searchid={searchid}（抓包 2987），耗时 {:.2}s",
                    t0.elapsed().as_secs_f32()
                );
                assert!(
                    set_cookie.is_some(),
                    "服务端应下发 vbemnlastsearchtime Cookie"
                );
                if searchid != 2987 {
                    println!(
                        "[①][备注] searchid 与抓包不同（{searchid}≠2987），站点缓存结果集变动，不影响断言"
                    );
                }
            }
            RawSearchPost::Rejected { message } => {
                panic!("① 首搜被拒（不应触发冷却）：{message}");
            }
        }

        // ② 冷却绕过实测：5 秒后（30s 窗口内）同词再 POST。净身请求应再次 302；
        //    若收到冷却错误页说明站点按 IP 限频（净身假设失效，兜底路径仍可用），如实记录。
        thread::sleep(Duration::from_secs(5));
        let t1 = Instant::now();
        match raw_search_post(&client, "病娇") {
            RawSearchPost::Executed { searchid, .. } => {
                println!(
                    "[②] 30s 窗口内连搜 → 302 执行成功，searchid={searchid}，耗时 {:.2}s —— 净身请求绕过生效",
                    t1.elapsed().as_secs_f32()
                );
            }
            RawSearchPost::Rejected { message } => {
                assert!(
                    message.contains("时间间隔"),
                    "② 窗口内连搜被拒但原因异常：{message}"
                );
                println!(
                    "[②][重要] 30s 窗口内连搜被冷却拒绝 —— 站点按 IP 限频，净身绕过在本网络不生效，\
                     适配层将走兜底等待路径（结果仍有保证）"
                );
            }
        }

        // ③a 截断前：原始全名（14 字 / 38 字节）直接 POST，镜像抓包 sid567（27 字节被拒），站点应拒绝。
        let full_title = "救了病娇后他总想杀我[穿书]";
        assert_eq!(full_title.len(), 38, "全名应为 38 字节（抓包锚点）");
        match raw_search_post(&client, full_title) {
            RawSearchPost::Executed { .. } => {
                println!(
                    "[③a] 38 字节关键字被站点接受 —— 站点限制已放宽（与抓包不符，需复核 20 字节上限）"
                );
            }
            RawSearchPost::Rejected { message } => {
                assert!(
                    message.contains("关键字只能在"),
                    "③a 38 字节关键字应触发「2~20 个字符」错误页，实际：{message}"
                );
                println!(
                    "[③a] 38 字节关键字 → 站点拒绝：「{message}」（与抓包 sid567 一致，站点限制仍为 2~20 字节）"
                );
            }
        }

        // ③b 截断后：适配层截断为「救了病娇后他」（18 字节，抓包 sid569 同款）再 POST，应 302 执行成功。
        let truncated = sanitize_keyword(full_title).expect("适配层应能截断");
        assert_eq!(truncated, "救了病娇后他");
        thread::sleep(Duration::from_secs(3));
        let t2 = Instant::now();
        match raw_search_post(&client, &truncated) {
            RawSearchPost::Executed { searchid, .. } => {
                println!(
                    "[③b] 截断词「{truncated}」（18 字节）→ 302 执行成功，searchid={searchid}（抓包 16830），耗时 {:.2}s",
                    t2.elapsed().as_secs_f32()
                );
            }
            RawSearchPost::Rejected { message } => {
                panic!("③b 截断后关键字（18 字节）仍被拒：{message}");
            }
        }

        // ④ 目录接口实网验证（/e/extend/ 与搜索端点同属站点放行的接口路径）。
        //    抓包真值：internal_id=115955，total=98，首章 1334231，末章 1334419。
        let chapters = client
            .fetch_all_chapters(115955)
            .expect("④ 目录拉取失败（/e/extend/ 接口）");
        assert_eq!(chapters.len(), 98, "章节数应为 98（抓包 total=98）");
        assert_eq!(chapters[0].id, "1334231");
        assert_eq!(chapters[0].title, "第1章");
        let last = chapters.last().unwrap();
        assert_eq!(last.id, "1334419");
        assert_eq!(last.title, "第98章 番外6 襄陵奇遇记（二）");
        println!(
            "[④] 目录实网逐页拉取 {} 章，首章「{}」末章「{}」—— 目录接口验证通过",
            chapters.len(),
            chapters[0].title,
            last.title
        );
    }

    /// B：完整链路实网验证 —— 结果页/详情/目录/正文/真实下载。
    #[test]
    #[ignore = "实网 e2e：需联网访问 zishiba.com（完整链路：结果页/详情/目录/正文/下载 3 章）"]
    fn e2e_live_full_chain() {
        let _guard = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let client = ZishibaClient::new(20).expect("init live client");

        // ⓪ 预检：HTML 文档页可达性（站点 WAF 拒绝桌面 UA；适配器用移动端 UA，正常应通过）。
        if let Err(e) = client.get_text(SITE_BASE, false) {
            panic!(
                "⓪ 预检失败：无法访问 zishiba.com 文档页。\
                 适配器已使用移动端 UA，此错误通常是站点临时故障或网络出口被限制。\
                 底层错误：{e}"
            );
        }
        println!("[⓪] 预检通过：HTML 文档页可达");

        // ① 搜索「病娇」。抓包真值：searchid=2987，20 条，首条 zs:123808《社恐小姐被病娇带回家之后》。
        let t0 = Instant::now();
        let items = client.search("病娇").expect("① 搜索「病娇」失败");
        println!(
            "[①] 「病娇」命中 {} 条，耗时 {:.2}s",
            items.len(),
            t0.elapsed().as_secs_f32()
        );
        assert!(!items.is_empty(), "「病娇」应有结果（抓包 20 条）");
        let first = items
            .iter()
            .find(|it| it.book_id == 123808)
            .expect("抓包首条 zs:123808《社恐小姐被病娇带回家之后》应在结果中");
        assert_eq!(first.title, "社恐小姐被病娇带回家之后");
        assert_eq!(first.author, "西样");

        // ② 冷却实测：5 秒后（30s 窗口内）同词再搜。净身请求应即时成功。
        thread::sleep(Duration::from_secs(5));
        let t1 = Instant::now();
        let items2 = client
            .search("病娇")
            .expect("② 30s 窗口内连搜最终失败（兜底重试也未成功）");
        let dt = t1.elapsed();
        println!(
            "[②] 30s 窗口内连搜 {} 条，耗时 {:.2}s{}",
            items2.len(),
            dt.as_secs_f32(),
            if dt >= SEARCH_COOLDOWN {
                "（触发兜底等待 → 站点按 IP 限频）"
            } else {
                "（净身请求绕过生效）"
            }
        );
        assert!(!items2.is_empty());

        // ③ 长关键字：全名 14 字/38B > 站点 20B 上限 → 适配层自动截断为「救了病娇后他」（18B）。
        //    抓包真值：截断词搜索（searchid=16830）恰命中 zs:113075。
        let t2 = Instant::now();
        let items3 = client
            .search("救了病娇后他总想杀我[穿书]")
            .expect("③ 长关键字搜索失败（截断逻辑未生效？）");
        println!(
            "[③] 全名搜索（自动截断为 18 字节）命中 {} 条，耗时 {:.2}s",
            items3.len(),
            t2.elapsed().as_secs_f32()
        );
        assert!(
            items3.iter().any(|it| it.book_id == 113075),
            "截断词「救了病娇后他」应命中 zs:113075《救了病娇后他总想杀我[穿书]》"
        );

        // ④ 详情。抓包真值：书名 / 星棘 / 完本 / data-bookid=115955。
        let detail = client.book_detail(113075).expect("④ 详情拉取失败");
        assert_eq!(detail.book_name, "救了病娇后他总想杀我[穿书]");
        assert_eq!(detail.author, "星棘");
        assert_eq!(detail.finished, Some(true));
        assert_eq!(detail.internal_id, 115955);
        println!(
            "[④] 详情：{} / {} / 完本 / 内部ID {}",
            detail.book_name, detail.author, detail.internal_id
        );

        // ⑤ 全量目录。抓包真值：total=98，首章 1334231「第1章」，末章 1334419「第98章 番外6 襄陵奇遇记（二）」。
        let chapters = client
            .fetch_all_chapters(detail.internal_id)
            .expect("⑤ 目录拉取失败");
        assert_eq!(chapters.len(), 98, "章节数应为 98（抓包 total=98）");
        assert_eq!(chapters[0].id, "1334231");
        assert_eq!(chapters[0].title, "第1章");
        let last = chapters.last().unwrap();
        assert_eq!(last.id, "1334419");
        assert_eq!(last.title, "第98章 番外6 襄陵奇遇记（二）");
        println!(
            "[⑤] 目录 {} 章，首章「{}」末章「{}」",
            chapters.len(),
            chapters[0].title,
            last.title
        );

        // ⑥ 第 1 章正文。抓包真值：h2#title=「第1章」，正文起于「白稚醒了。」。
        let (title, body) = client
            .chapter_content(113075, "1334231")
            .expect("⑥ 正文拉取失败");
        assert_eq!(title, "第1章");
        assert!(body.contains("白稚醒了"), "正文应含抓包真值开头段");
        assert!(!body.contains("<script"), "正文不应残留 script");
        println!("[⑥] 第1章正文 {} 字", body.chars().count());

        // ⑦ 真实下载（临时目录，仅前 3 章）：计划准备 → 工作池下载 → BookManager 落盘。
        let mut cfg = Config::default();
        let tmp = std::env::temp_dir().join(format!("undl_zishiba_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("创建临时目录");
        cfg.save_path = tmp.to_string_lossy().to_string();

        let hint = BookMeta {
            book_name: Some(detail.book_name.clone()),
            ..Default::default()
        };
        let plan = prepare_zishiba_plan(&cfg, "zs:113075", hint).expect("⑦ 下载计划失败");
        assert_eq!(plan.chapters.len(), 98);
        assert_eq!(plan.meta.author.as_deref(), Some("星棘"));

        let chosen: Vec<ChapterRef> = plan.chapters.iter().take(3).cloned().collect();
        let pending: Vec<ChapterRef> = chosen.clone();
        let mut manager = crate::book_parser::book_manager::BookManager::new(
            cfg.clone(),
            "zs:113075",
            plan.meta
                .book_name
                .as_deref()
                .unwrap_or("救了病娇后他总想杀我[穿书]"),
        )
        .expect("BookManager 创建失败");
        let mut reporter = crate::download::progress::make_reporter(&cfg, &chosen, &pending, None);
        let result = download_zishiba_into_manager(
            &cfg,
            "zs:113075",
            plan.meta
                .book_name
                .as_deref()
                .unwrap_or("救了病娇后他总想杀我[穿书]"),
            &mut manager,
            &chosen,
            &pending,
            &mut reporter,
            None,
        )
        .expect("⑦ 下载执行失败");
        println!(
            "[⑦] 下载 {} 章：成功 {}，失败 {}（临时目录 {}）",
            pending.len(),
            result.success,
            result.failed,
            tmp.display()
        );
        assert_eq!(result.success, 3);
        assert_eq!(result.failed, 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
