//! 笔下文学（www.hen0.com，杰奇 CMS 改版 + Bootstrap 5 模板）小说源适配。
//!
//! 接口清单（依据 examples/笔下文学_解析结果 抓包 2026-11-05 + 非重放 e2e 2026-11-06）：
//! - 搜索：`POST /search.html`，表单 `searchkey=<GBK urlencode>`，无其他字段。
//!   精确命中书名 → `302 → /book/{id}/`；模糊命中 → 200 结果表格（单页封顶 30 条、无翻页）；
//!   关键词 <2 字符或触发节流 → 200 杰奇「出现错误！」页。
//! - 详情：`GET /book/{id}/`（**必须带尾斜杠**，无尾斜杠 404），
//!   `h1.book-title` + `p.book-author` + `div.book-meta`（分类/状态/字数/更新/最新）+
//!   `div.book-tags` 简介 + `img.book-cover-large`。
//! - 目录：详情页即第 1 页（60 章/页），`/book/{id}_{n}/` 翻页（`a.onclick`「下一页」）。
//!   页内 `h5.volume-title` + `ul.chapter-list` 成对；第一个卷块固定为「最新章节」预览，必须跳过。
//! - 正文：`GET /book/{id}/{cid}.html`，章内分页 `{cid}_{n}.html`（`chapter-nav`「下一页」），
//!   容器 `div#chapter-content`，段落以 `<br/>` 分隔。
//! - 封面：`GET /files/article/image/{bid÷1000}/{bid}/{bid}s.jpg`（模板 src 为双斜杠，需规范化）。
//! - `keywords.php` 为 SEO 统计端点（空 200），无需请求。
//!
//! 站点两大特性（实网 e2e 实锤）与对策：
//! - **搜索节流为纯 Cookie 侧**：响应 `Set-Cookie: jieqiVisitTime=jieqiArticlesearchTime=<ts>`，
//!   下次搜索对比该时间戳，间隔 <10s 返回「出现错误！…两次搜索的间隔时间不得少于 10 秒」页；
//!   **服务端无 IP/会话侧计数**。reqwest 默认不持久化 Cookie（本项目未启用 `cookies`
//!   feature）→ 天然净身请求，节流不触发。仍保留礼貌最小间隔；若未来站点按 IP 限频，
//!   兜底等待 10s+裕量后重试一次。
//! - **正文 CSS 伪元素注入**：部分段落的真实文本不写入 HTML 文本节点，而是
//!   `<span class="p_N">`（空标签）+ 同容器 `<style>` 中
//!   `#chapter-content .p_N::after{content:'…'}`。页内与纯 HTML 段落**混排**（同页、同章均可能
//!   两种形态并存），必须统一走「HTML 文本节点 + 按 DOM 顺序回填 CSS content」的提取管线，
//!   否则 CSS 注入段落全部丢失（实测存在整页几乎全靠 CSS 的章节）。
//!
//! 编码：全站 GBK（meta 声明与实际一致），请求体与响应均需显式 GBK 编解码。
//! 无效章节 id 会 200 软回退书籍页，章节成功判定以 `chapter-content` 容器存在为准。

use std::collections::HashMap;
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

const SITE_BASE: &str = "https://www.hen0.com";

/// 服务端搜索冷却窗口（错误页原文「两次搜索的间隔时间不得少于 10 秒」）。仅兜底路径使用。
const SEARCH_COOLDOWN: Duration = Duration::from_secs(10);
/// 冷却兜底的安全裕量。
const COOLDOWN_MARGIN: Duration = Duration::from_secs(1);
/// 净身请求下的礼貌最小搜索间隔（站点无硬性限制，此为自律节流）。
const SEARCH_MIN_GAP: Duration = Duration::from_secs(2);
/// 目录翻页间隔（60 章/页，礼貌节奏）。
const TOC_PAGE_GAP: Duration = Duration::from_millis(150);
/// 目录页数保护上限（防异常返回导致死循环）。
const TOC_MAX_PAGES: u32 = 2_000;
/// 章内分页保护上限（实测 3 页/章量级，留足裕量）。
const CHAPTER_MAX_PAGES: u32 = 50;
/// 搜索关键字最短字符数（实测单字符返回空结果页，杰奇按字符校验）。
const KEYWORD_MIN_CHARS: usize = 2;
/// 搜索关键字最长字符数（杰奇 articlesearch 默认上限，超长截断防误拒）。
const KEYWORD_MAX_CHARS: usize = 50;

// ── book_id 识别与规范化 ──────────────────────────────────────

/// `bx:` 前缀即笔下文学 book_id。
pub fn is_hen0_book_id(book_id: &str) -> bool {
    book_id.starts_with("bx:")
}

/// 去掉 `bx:` 前缀，返回纯数字部分（无前缀则原样返回）。
pub fn strip_bx_prefix(book_id: &str) -> &str {
    book_id.strip_prefix("bx:").unwrap_or(book_id)
}

/// 从用户输入识别笔下文学 book_id：`bx:592874` 前缀，或 hen0.com 的书籍/章节页 URL。
/// 返回带前缀的规范化 book_id（如 `bx:592874`），非本源输入返回 None。
pub fn normalize_book_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("bx:")
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return Some(format!("bx:{rest}"));
    }
    // https://www.hen0.com/book/592874/ 或 .../book/592874/122773716.html
    re_book_url()
        .captures(trimmed)
        .map(|c| format!("bx:{}", &c[1]))
}

/// 从 `bx:` book_id 提取数字 ID。非法输入报错。
fn parse_book_numeric_id(book_id: &str) -> Result<u64> {
    let rest = strip_bx_prefix(book_id);
    rest.parse::<u64>()
        .with_context(|| format!("非法笔下文学 book_id: {book_id:?}"))
}

// ── 关键字预处理 ──────────────────────────────────────────────

/// 站点校验：单字符关键词返回空结果页（实测「的」→ 空表格）。超长按字符边界截断。
fn sanitize_keyword(raw: &str) -> Result<String> {
    let kw = raw.trim();
    if kw.chars().count() < KEYWORD_MIN_CHARS {
        return Err(anyhow!(
            "搜索关键字过短：站点要求至少 {KEYWORD_MIN_CHARS} 个字符"
        ));
    }
    if kw.chars().count() <= KEYWORD_MAX_CHARS {
        return Ok(kw.to_string());
    }
    let truncated: String = kw.chars().take(KEYWORD_MAX_CHARS).collect();
    info!(
        target: "hen0",
        truncated = truncated.as_str(),
        "搜索关键字超过站点上限，已按字符边界截断"
    );
    Ok(truncated)
}

// ── 编码工具 ──────────────────────────────────────────────────

/// 全站 GBK 解码（响应头/meta 均声明 gbk，与实际一致，显式解码不依赖 HTTP 头）。
fn decode_gbk(bytes: &[u8]) -> String {
    let (cow, _, _) = encoding_rs::GBK.decode(bytes);
    cow.into_owned()
}

/// GBK 字节按百分号编码（表单值用；与浏览器 x-www-form-urlencoded 行为一致）。
fn gbk_form_encode(s: &str) -> String {
    let (bytes, _, _) = encoding_rs::GBK.encode(s);
    let mut out = String::with_capacity(bytes.len() * 3);
    for b in bytes.iter() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── 正则缓存 ──────────────────────────────────────────────────

fn re_book_url() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)hen0\.com/book/(\d+)").expect("compile re_book_url"))
}

/// 精确命中 302 的 Location：`/book/{id}/`。
fn re_location_book() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:https?://www\.hen0\.com)?/book/(\d+)/?$")
            .expect("compile re_location_book")
    })
}

/// 搜索结果行：`<td><a href=".../book/{id}/">{书名}</a></td>`。
fn re_search_row() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<tr[^>]*>\s*<td><a href="(?:https?://www\.hen0\.com)?/book/(\d+)/?"[^>]*>(.*?)</a></td>\s*<td>(.*?)</td>\s*<td[^>]*>(.*?)</td>\s*<td[^>]*>(.*?)</td>\s*<td[^>]*>(.*?)</td>"#)
            .expect("compile re_search_row")
    })
}

/// 表格区（含表头）。
fn re_result_table() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<div class="table-responsive">.*?<tbody>(.*?)</tbody>"#)
            .expect("compile re_result_table")
    })
}

fn re_book_title() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<h1 class="book-title">(.*?)</h1>"#).expect("compile re_book_title")
    })
}

fn re_book_author() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<p class="book-author">\s*作者：\s*(.*?)\s*</p>"#)
            .expect("compile re_book_author")
    })
}

fn re_book_cover() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<img src="([^"]+)"[^>]*class="book-cover-large""#)
            .expect("compile re_book_cover")
    })
}

/// book-meta 内的 5 个 label（分类/状态/字数/更新/最新，顺序固定但按前缀匹配而非下标）。
fn re_meta_labels() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<div class="book-meta">(.*?)</div>"#).expect("compile re_meta_labels")
    })
}

fn re_label_span() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<span class="label">(.*?)</span>"#).expect("compile re_label_span")
    })
}

fn re_description() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<div class="book-tags">(.*?)</div>"#).expect("compile re_description")
    })
}

/// 卷块：`h5.volume-title`（或 div）+ `ul.chapter-list`。
fn re_volume_block() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?s)<(?:h[1-6]|div)[^>]*class="volume-title"[^>]*>(.*?)</(?:h[1-6]|div)>\s*<ul class="chapter-list">(.*?)</ul>"#,
        )
        .expect("compile re_volume_block")
    })
}

/// 本站章节链接（限制在指定 bid 下）。
fn re_chapter_link(bid: &str) -> Regex {
    Regex::new(&format!(
        r#"(?s)<a[^>]*href="(?:https?://www\.hen0\.com)?/book/{bid}/(\d+)\.html"[^>]*>(.*?)</a>"#
    ))
    .expect("compile re_chapter_link")
}

/// 目录「下一页」：`<a href="/book/{bid}_{n}/" class="onclick">下一页</a>`。
fn re_toc_next(bid: &str) -> Regex {
    Regex::new(&format!(
        r#"<a[^>]*href="/book/{bid}_(\d+)/"[^>]*class="onclick"[^>]*>\s*下一页"#
    ))
    .expect("compile re_toc_next")
}

/// 目录任意分页号（兜底：取比当前页更大的号）。
fn re_toc_any_page(bid: &str) -> Regex {
    Regex::new(&format!(r#"/book/{bid}_(\d+)/"#)).expect("compile re_toc_any_page")
}

fn re_chapter_title() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<h1 class="chapter-title">(.*?)</h1>"#).expect("compile re_chapter_title")
    })
}

/// 正文容器（非贪婪；容器内无嵌套 div，style/span/br 均不是 div）。
fn re_chapter_content() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<div class="chapter-content[^"]*" id="chapter-content">(.*?)</div>"#)
            .expect("compile re_chapter_content")
    })
}

/// 章内导航整块（内嵌 row/col div，非贪婪会截断，需取到三个连续 `</div>` 收尾）。
fn re_chapter_nav() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<div class="chapter-nav">(.*?)</div>\s*</div>\s*</div>"#)
            .expect("compile re_chapter_nav")
    })
}

/// CSS 伪元素注入规则：`#chapter-content .p_N::after{content:'…'}`。
fn re_css_para() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"#chapter-content \.p_(\d+)::after\{content:'([^']*)'\}")
            .expect("compile re_css_para")
    })
}

/// 目录页 CSS 注入规则：`.p_{章节id}::after{content:'章节名'}`（无 `#chapter-content` 前缀）。
fn re_toc_css_para() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\.p_(\d+)::after\{content:'([^']*)'\}").expect("compile re_toc_css_para")
    })
}

/// 空 span 标记：`<span class="p_N">`。
fn re_para_span() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<span class="p_(\d+)">"#).expect("compile re_para_span"))
}

/// `<style>…</style>` 块。
fn re_style_block() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<style[^>]*>.*?</style>").expect("compile re_style_block"))
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
    out.to_string()
}

/// 实体解码 + 剥标签 + 压缩空白的统一清洗。
fn clean_text(s: &str) -> String {
    let unescaped = crate::book_parser::html_utils::unescape_basic_entities(s);
    strip_tags(unescaped.as_ref())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// 相对/双斜杠封面 URL 规范化为绝对 URL：
/// `/files/article/image//592/592874/…`（模板自带双斜杠）→ `https://www.hen0.com/files/…`。
fn normalize_cover_url(src: &str) -> Option<String> {
    let trimmed = src.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("https://") {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        return Some(format!("https://{host}/{}", collapse_slashes(path)));
    }
    if let Some(rest) = trimmed.strip_prefix("http://") {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        return Some(format!("http://{host}/{}", collapse_slashes(path)));
    }
    let path = trimmed.trim_start_matches('/');
    Some(format!("{SITE_BASE}/{}", collapse_slashes(path)))
}

fn collapse_slashes(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut prev_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if !prev_slash {
                out.push(ch);
            }
            prev_slash = true;
        } else {
            out.push(ch);
            prev_slash = false;
        }
    }
    out
}

/// 杰奇「出现错误！」页识别与原因提取（`<title>出现错误！</title>` + `错误原因：…`）。
fn jieqi_error_reason(html: &str) -> Option<String> {
    if !html.contains("出现错误") {
        return None;
    }
    let pos = html.find("错误原因：")? + "错误原因：".len();
    let rest = &html[pos..];
    let end = rest.find("<br").unwrap_or(rest.len());
    Some(clean_text(&rest[..end]))
}

// ── 数据结构 ──────────────────────────────────────────────────

/// 搜索结果条目（解析自结果表格，字段缺失为空串）。
#[derive(Debug, Clone)]
pub struct Hen0SearchItem {
    pub book_id: u64,
    pub title: String,
    pub author: String,
    /// 最新章节标题（含 `<a>` 时已剥标签）。
    pub latest_chapter: String,
    /// 更新时间（YY-MM-DD）。
    pub update_date: String,
    /// 状态（连载/已完成），空表示未提供。
    pub status: String,
}

/// 书籍详情（解析自 `/book/{id}/`）。
#[derive(Debug, Clone)]
pub struct Hen0BookDetail {
    pub book_id: u64,
    pub book_name: String,
    pub author: String,
    pub description: String,
    pub cover_url: String,
    pub category: String,
    pub finished: Option<bool>,
    /// 字数（「20万字」→ 200000；无法解析为 None）。
    pub word_count: Option<usize>,
    /// 目录第 1 页章节。
    pub toc_first_page: Vec<ChapterRef>,
    /// 目录下一页号（无则目录只有一页）。
    pub toc_next_page: Option<u32>,
}

// ── 客户端 ────────────────────────────────────────────────────

pub struct Hen0Client {
    http: reqwest::blocking::Client,
    /// 上次搜索 POST 发起时刻（礼貌间隔节流；净身请求下站点无硬性限制）。
    last_post: Mutex<Option<Instant>>,
}

impl Hen0Client {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(5)))
            .user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36 Edg/156.0.0.0",
            )
            // 搜索 POST 返回 302（精确命中），必须自己接住以区分「执行成功 / 被拒」。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build hen0 HTTP client")?;
        Ok(Self {
            http,
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
                    (elapsed < SEARCH_MIN_GAP).then(|| SEARCH_MIN_GAP - elapsed)
                }
                None => None,
            }
        };
        if let Some(d) = wait {
            thread::sleep(d);
        }
        *self.last_post.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    /// GET 并返回 GBK 解码后的文本。传输错误换新连接重试一次。
    pub(crate) fn get_text(&self, url: &str) -> Result<String> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..2u32 {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(300));
            }
            let outcome =
                (|| -> std::result::Result<(reqwest::StatusCode, Vec<u8>), reqwest::Error> {
                    let resp = self.http.get(url).send()?;
                    let status = resp.status();
                    let bytes = resp.bytes()?.to_vec();
                    Ok((status, bytes))
                })();
            match outcome {
                Ok((status, bytes)) if status.is_success() => return Ok(decode_gbk(&bytes)),
                Ok((status, _)) => {
                    return Err(anyhow!("笔下文学请求 HTTP {status}：{url}"));
                }
                Err(e) => {
                    debug!(target: "hen0", url, attempt, error = %e, "笔下文学请求传输错误，将重试");
                    last_err =
                        Some(anyhow::Error::new(e).context(format!("笔下文学请求失败：{url}")));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("笔下文学请求失败：{url}")))
    }

    /// 搜索。空结果返回空列表；被节流（理论上净身请求不会发生）走 10s 兜底重试一次。
    pub fn search(&self, raw_keyword: &str) -> Result<Vec<Hen0SearchItem>> {
        let keyword = sanitize_keyword(raw_keyword)?;
        match self.attempt_search(&keyword) {
            Ok(items) => Ok(items),
            Err(SearchError::Cooldown) => {
                let need = SEARCH_COOLDOWN + COOLDOWN_MARGIN;
                warn!(
                    target: "hen0",
                    wait_secs = need.as_secs(),
                    "笔下文学搜索被冷却拒绝（站点疑似按 IP 限频），等待冷却窗口后重试一次"
                );
                thread::sleep(need);
                match self.attempt_search(&keyword) {
                    Ok(items) => Ok(items),
                    Err(SearchError::Cooldown) => Err(anyhow!(
                        "笔下文学搜索冷却中（已等待重试仍被拒），请稍后再试"
                    )),
                    Err(SearchError::Other(e)) => Err(e),
                }
            }
            Err(SearchError::Other(e)) => Err(e),
        }
    }

    fn attempt_search(
        &self,
        keyword: &str,
    ) -> std::result::Result<Vec<Hen0SearchItem>, SearchError> {
        self.throttle_gap();
        let url = format!("{SITE_BASE}/search.html");
        let body = format!("searchkey={}", gbk_form_encode(keyword));
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Origin", SITE_BASE)
            .header("Referer", format!("{SITE_BASE}/search.html"))
            .body(body)
            .send()
            .context("笔下文学搜索请求失败")
            .map_err(SearchError::Other)?;
        let status = resp.status();
        if status.is_redirection() {
            // 精确命中书名：302 → /book/{id}/。结果页语义由调用方借详情页补全。
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let id = re_location_book()
                .captures(&location)
                .and_then(|c| c[1].parse::<u64>().ok())
                .ok_or_else(|| anyhow!("笔下文学搜索重定向缺少 book_id: {location}"))
                .map_err(SearchError::Other)?;
            let _ = id; // 精确命中由 parse 调用方处理（search_exact_id）
            return Ok(Self::exact_hit(id));
        }
        let html = resp
            .bytes()
            .map(|b| decode_gbk(&b))
            .context("笔下文学搜索响应读取失败")
            .map_err(SearchError::Other)?;
        if let Some(reason) = jieqi_error_reason(&html) {
            if reason.contains("间隔") {
                return Err(SearchError::Cooldown);
            }
            return Err(SearchError::Other(anyhow!("笔下文学搜索失败：{reason}")));
        }
        if !html.contains("table-responsive") {
            return Err(SearchError::Other(anyhow!(
                "笔下文学搜索返回异常 HTTP {status}（无结果表格）"
            )));
        }
        Ok(parse_search_result(&html))
    }

    /// 精确命中的哨兵值：搜索层只负责识别，条目由 `search_exact_id` 借详情页补全。
    fn exact_hit(id: u64) -> Vec<Hen0SearchItem> {
        vec![Hen0SearchItem {
            book_id: id,
            title: String::new(),
            author: String::new(),
            latest_chapter: String::new(),
            update_date: String::new(),
            status: String::new(),
        }]
    }

    /// 判断 attempt_search 的结果是否为「精确命中」（title 为空的哨兵条目）。
    fn is_exact_hit(items: &[Hen0SearchItem]) -> bool {
        items.len() == 1 && items[0].title.is_empty()
    }

    /// 书籍详情 + 目录第 1 页。
    pub fn book_detail(&self, book_id: u64) -> Result<Hen0BookDetail> {
        // 站点要求尾斜杠（无尾斜杠 404，2026-11-06 实测）。
        let url = format!("{SITE_BASE}/book/{book_id}/");
        let html = self.get_text(&url)?;
        parse_book_detail(&html)
            .ok_or_else(|| anyhow!("笔下文学详情页解析失败（book_id={book_id}）"))
    }

    /// 目录第 n 页（n 从 2 起；第 1 页在详情页内）。
    fn toc_page(&self, book_id: u64, page: u32) -> Result<(Vec<ChapterRef>, Option<u32>)> {
        let url = format!("{SITE_BASE}/book/{book_id}_{page}/");
        let html = self.get_text(&url)?;
        Ok(parse_toc_page(&html, book_id, page))
    }

    /// 全量目录：详情页第 1 页 + `_n` 翻页拼接（跳过「最新章节」预览块）。
    /// 全量目录：从已解析的详情页（=目录第 1 页）起 `_n` 翻页拼接
    /// （跳过「最新章节」预览块）。
    pub fn fetch_all_chapters(
        &self,
        book_id: u64,
        detail: &Hen0BookDetail,
    ) -> Result<Vec<ChapterRef>> {
        let mut chapters = detail.toc_first_page.clone();
        let mut next = detail.toc_next_page;
        let mut guard = 0u32;
        while let Some(page) = next {
            guard += 1;
            if guard > TOC_MAX_PAGES {
                return Err(anyhow!(
                    "笔下文学目录翻页超过保护上限（{TOC_MAX_PAGES} 页）"
                ));
            }
            thread::sleep(TOC_PAGE_GAP);
            let (page_chapters, next_page) = self.toc_page(book_id, page)?;
            chapters.extend(page_chapters);
            next = next_page;
        }
        // 章节按出现顺序去重（分页边界重复极罕见，防御性处理）。
        let mut seen = std::collections::HashSet::new();
        chapters.retain(|c| seen.insert(c.id.clone()));
        if chapters.is_empty() {
            return Err(anyhow!("笔下文学目录为空（book_id={book_id}）"));
        }
        Ok(chapters)
    }

    /// 章节正文（含章内分页 + CSS 注入合并）。返回 (页面标题, `<p>` 段落 HTML)。
    ///
    /// 章节标题自带 `(1/5)` 这样的总分页数：抓到第 1 页后，第 2..N 页并行抓取
    /// （最多 5 页/批），显著缩短单章耗时；标题无页数或并行失败时回退串行。
    pub fn chapter_content(&self, book_id: u64, chapter_id: &str) -> Result<(String, String)> {
        let mut paragraphs: Vec<String> = Vec::new();
        let first_url = format!("{SITE_BASE}/book/{book_id}/{chapter_id}.html");
        let first_html = self.get_text(&first_url)?;
        let (page_title, first_paras, first_next) = parse_chapter_page(&first_html, chapter_id)
            .ok_or_else(|| {
                anyhow!("笔下文学章节页解析失败（chapter_id={chapter_id}，可能已失效）")
            })?;
        append_page_paragraphs(&mut paragraphs, first_paras, false);

        if let Some(total) = parse_page_total(&page_title).filter(|n| *n > 1)
            && let Some(pages) = self.fetch_chapter_pages_parallel(book_id, chapter_id, total)
        {
            for paras in pages {
                append_page_paragraphs(&mut paragraphs, paras, true);
            }
            if paragraphs.is_empty() {
                return Err(anyhow!("笔下文学章节正文为空（chapter_id={chapter_id}）"));
            }
            return Ok((page_title, paragraphs.join("\n")));
        }

        // 兜底：标题无页数 / 并行抓取失败 → 按导航「下一页」串行。
        let mut next = first_next;
        let mut join_next_page = next.is_some();
        let mut guard = 1u32;
        while let Some(n) = next {
            guard += 1;
            if guard > CHAPTER_MAX_PAGES {
                return Err(anyhow!(
                    "笔下文学章内分页超过保护上限（{CHAPTER_MAX_PAGES} 页）：chapter={chapter_id}"
                ));
            }
            let pid = format!("{chapter_id}_{n}");
            let url = format!("{SITE_BASE}/book/{book_id}/{pid}.html");
            let html = self.get_text(&url)?;
            let (_title, page_paras, next_page) = parse_chapter_page(&html, chapter_id)
                .ok_or_else(|| anyhow!("笔下文学章节页解析失败（chapter_id={pid}，可能已失效）"))?;
            append_page_paragraphs(&mut paragraphs, page_paras, join_next_page);
            next = next_page;
            join_next_page = next.is_some();
        }
        if paragraphs.is_empty() {
            return Err(anyhow!("笔下文学章节正文为空（chapter_id={chapter_id}）"));
        }
        Ok((page_title, paragraphs.join("\n")))
    }

    /// 并行抓取第 2..total 页（最多 5 页/批），任一页失败返回 None 触发串行兜底。
    fn fetch_chapter_pages_parallel(
        &self,
        book_id: u64,
        chapter_id: &str,
        total: u32,
    ) -> Option<Vec<Vec<String>>> {
        let pages: Vec<u32> = (2..=total).collect();
        let mut out: Vec<Vec<String>> = Vec::with_capacity(pages.len());
        for chunk in pages.chunks(5) {
            let results: Vec<Option<Vec<String>>> = std::thread::scope(|scope| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|p| {
                        let url = format!("{SITE_BASE}/book/{book_id}/{chapter_id}_{p}.html");
                        let cid = chapter_id.to_string();
                        scope.spawn(move || -> Option<Vec<String>> {
                            let html = self.get_text(&url).ok()?;
                            let (_title, paras, _next) = parse_chapter_page(&html, &cid)?;
                            Some(paras)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().ok().flatten())
                    .collect()
            });
            for r in results {
                out.push(r?);
            }
        }
        Some(out)
    }
}

enum SearchError {
    /// 服务端冷却拒绝（未执行本次搜索）。
    Cooldown,
    Other(anyhow::Error),
}

// ── 解析（纯函数，夹具回归测试覆盖）────────────────────────────

/// 解析搜索结果表格 → 条目列表（无结果为空列表）。
fn parse_search_result(html: &str) -> Vec<Hen0SearchItem> {
    let mut items = Vec::new();
    let Some(tbody) = re_result_table().captures(html).map(|c| c[1].to_string()) else {
        return items;
    };
    for cap in re_search_row().captures_iter(&tbody) {
        let Ok(book_id) = cap[1].parse::<u64>() else {
            continue;
        };
        let title = clean_text(&cap[2]);
        if title.is_empty() {
            continue;
        }
        items.push(Hen0SearchItem {
            book_id,
            title,
            author: clean_text(&cap[3]),
            latest_chapter: clean_text(&cap[4]),
            update_date: clean_text(&cap[5]),
            status: clean_text(&cap[6]),
        });
    }
    items
}

/// 从 label 文本（如 `状态：完本`）中取值部分。
fn label_value(label: &str) -> &str {
    label.split_once('：').map(|(_, v)| v).unwrap_or(label)
}

/// 「20万字」→ 200000；「3.5万」→ 35000；无数字返回 None。
fn parse_word_count(text: &str) -> Option<usize> {
    let t = text.trim().strip_suffix('字').unwrap_or(text.trim()).trim();
    let (num_str, mult) = if let Some(rest) = t.strip_suffix("万") {
        (rest, 10_000usize)
    } else if let Some(rest) = t.strip_suffix("千") {
        (rest, 1_000)
    } else {
        (t, 1)
    };
    let n: f64 = num_str
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>()
        .parse()
        .ok()?;
    Some((n * mult as f64) as usize)
}

/// 解析详情页（=目录第 1 页）。
fn parse_book_detail(html: &str) -> Option<Hen0BookDetail> {
    let book_id = re_book_url()
        .captures(html)
        .and_then(|c| c[1].parse::<u64>().ok())?;
    let book_name = re_book_title()
        .captures(html)
        .map(|c| clean_text(&c[1]))
        .filter(|s| !s.is_empty())?;
    let author = re_book_author()
        .captures(html)
        .map(|c| clean_text(&c[1]))
        .unwrap_or_default();
    let description = re_description()
        .captures(html)
        .map(|c| clean_text(&c[1]))
        .unwrap_or_default();
    let cover_url = re_book_cover()
        .captures(html)
        .and_then(|c| normalize_cover_url(&c[1]))
        .unwrap_or_default();

    let labels: Vec<String> = re_meta_labels()
        .captures(html)
        .map(|c| {
            re_label_span()
                .captures_iter(&c[1])
                .map(|l| clean_text(&l[1]))
                .collect()
        })
        .unwrap_or_default();
    let mut category = String::new();
    let mut finished = None;
    let mut word_count = None;
    for label in &labels {
        let value = label_value(label);
        if let Some(rest) = label.strip_prefix("分类：") {
            category = rest.trim().to_string();
        } else if let Some(rest) = label.strip_prefix("状态：") {
            let v = rest.trim();
            finished = Some(v.contains("完本") || v.contains("完结"));
        } else if let Some(rest) = label.strip_prefix("字数：") {
            word_count = parse_word_count(rest);
        } else if label.starts_with("最新：") {
            let _ = value;
        }
    }

    let (toc_first_page, toc_next_page) = parse_toc_page(html, book_id, 1);
    Some(Hen0BookDetail {
        book_id,
        book_name,
        author,
        description,
        cover_url,
        category,
        finished,
        word_count,
        toc_first_page,
        toc_next_page,
    })
}

/// 解析目录页：卷块（跳过「最新章节」预览）+ 下一页号。
/// 兜底：无卷块时取整页本站章节链接。
pub(crate) fn parse_toc_page(
    html: &str,
    book_id: u64,
    current_page: u32,
) -> (Vec<ChapterRef>, Option<u32>) {
    let bid = book_id.to_string();
    let mut chapters: Vec<ChapterRef> = Vec::new();
    let mut has_volume = false;
    for cap in re_volume_block().captures_iter(html) {
        let volume = clean_text(&cap[1]);
        if volume.contains("最新章节") {
            continue;
        }
        has_volume = true;
        let re_ch = re_chapter_link(&bid);
        for ch in re_ch.captures_iter(&cap[2]) {
            chapters.push(ChapterRef {
                id: ch[1].to_string(),
                title: clean_text(&ch[2]),
            });
        }
    }
    if !has_volume {
        let re_ch = re_chapter_link(&bid);
        for ch in re_ch.captures_iter(html) {
            chapters.push(ChapterRef {
                id: ch[1].to_string(),
                title: clean_text(&ch[2]),
            });
        }
        // 兜底路径可能混入「开始阅读/最新」等重复链接，按 id 去重保序。
        let mut seen = std::collections::HashSet::new();
        chapters.retain(|c| seen.insert(c.id.clone()));
    }
    // 目录页的章节名同样可能走 CSS 注入（`.p_{cid}::after{content:'…'}`），
    // 锚点内为空 `<span></span>`；按章节 id 回查补全。
    let toc_css: HashMap<u32, String> = re_toc_css_para()
        .captures_iter(html)
        .filter_map(|c| {
            let id = c[1].parse::<u32>().ok()?;
            Some((id, clean_text(&c[2].replace("\\'", "'"))))
        })
        .collect();
    for ch in &mut chapters {
        if ch.title.is_empty()
            && let Ok(cid) = ch.id.parse::<u32>()
            && let Some(t) = toc_css.get(&cid)
        {
            ch.title = t.clone();
        }
    }
    let next_page = re_toc_next(&bid)
        .captures(html)
        .and_then(|c| c[1].parse::<u32>().ok())
        .or_else(|| {
            // 兜底：取比当前页更大的分页号中的最大值（排除自身与「上一页」回链）。
            re_toc_any_page(&bid)
                .captures_iter(html)
                .filter_map(|c| c[1].parse::<u32>().ok())
                .filter(|n| *n > current_page)
                .max()
        });
    (chapters, next_page)
}

/// 从章内导航块中解析「下一页」链接 → 章内页号（仅当仍指向当前章节时）。
fn parse_chapter_next_page(nav_html: &str, chapter_id: &str) -> Option<u32> {
    // 遍历导航内的全部锚点，取剥标签后文本含「下一页」者（锚点内可能内嵌 <i> 图标标签）。
    let re_anchor = Regex::new(r#"(?s)<a[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).ok()?;
    let href = re_anchor
        .captures_iter(nav_html)
        .find(|c| clean_text(&c[2]).contains("下一页"))
        .map(|c| c[1].to_string())?;
    let re = Regex::new(&format!(
        r#"(?:/book/\d+/)?{}(?:_(\d+))?\.html$"#,
        regex::escape(chapter_id)
    ))
    .ok()?;
    let cap = re.captures(&href)?;
    // 无 `_N` 后缀 = 指向其他章节（下一章），章内分页结束。
    cap.get(1)?.as_str().parse::<u32>().ok()
}

/// 章内分页提示行：`-->>本章未完，点击下一页继续阅读`。
///
/// 实网 10 本 / 31 页样本中该提示文本完全一致；用宽松匹配兼容站点的
/// 措辞微调（如“本章尚未结束”“请翻页继续阅读”），同时限制长度避免误伤正文。
fn is_page_break_notice(text: &str) -> bool {
    let s: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if s.chars().count() > 40 {
        return false;
    }
    (s.contains("本章未完") || s.contains("本章尚未") || s.contains("本章未结束"))
        && (s.contains("下一页") || s.contains("继续阅读") || s.contains("翻页"))
}

/// 上一页末段是否应直接拼接下一页首段。
///
/// 站点按固定长度切页：末段没有句末标点时，句子大概率被截断（实测
/// “让那群” + “混蛋付出代价。”）；有句末标点时则是段落边界（实测
/// “……看看。” + “我那时想，切。”），不应硬拼。
fn page_break_joins(last_paragraph_html: &str) -> bool {
    let body = last_paragraph_html
        .strip_suffix("</p>")
        .unwrap_or(last_paragraph_html);
    let body = body.strip_prefix("<p>").unwrap_or(body).trim_end();
    let mut chars = body.chars().rev();
    let Some(last) = chars.next() else {
        return false;
    };
    let terminal = |c: char| matches!(c, '。' | '！' | '？' | '；' | '.' | '!' | '?' | ';');
    if terminal(last) {
        return false;
    }
    // 闭合引号/括号：看它前面的字符是否句末标点。
    if matches!(last, '”' | '」' | '』' | '）' | ')') {
        return !chars.next().map(terminal).unwrap_or(false);
    }
    true
}

/// 从标题 `第120章(1/5)` 解析总页数；无标记/非法返回 None。
fn parse_page_total(title: &str) -> Option<u32> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\((\d+)\s*/\s*(\d+)\)").expect("page total regex"));
    let caps = re.captures(title)?;
    let cur = caps.get(1)?.as_str().parse::<u32>().ok()?;
    let total = caps.get(2)?.as_str().parse::<u32>().ok()?;
    (cur >= 1 && total >= cur).then_some(total)
}

/// 追加一页的段落：上一页句子被分页截断时与当前页首段直接拼接。
fn append_page_paragraphs(paragraphs: &mut Vec<String>, page_paras: Vec<String>, join_first: bool) {
    let mut first = true;
    for para in page_paras {
        if first
            && join_first
            && let Some(last) = paragraphs.last_mut()
            && page_break_joins(last)
        {
            let head = last
                .strip_suffix("</p>")
                .unwrap_or(last.as_str())
                .to_string();
            let tail = para.strip_prefix("<p>").unwrap_or(para.as_str());
            *last = format!("{head}{tail}");
        } else {
            paragraphs.push(para);
        }
        first = false;
    }
}

/// 提取单页正文段落（统一处理纯 HTML 文本与 CSS 伪元素注入）。
///
/// 做法：先收集同页 `<style>` 中 `#chapter-content .p_N::after{content:'…'}` 的文本，
/// 移除 style 块；再按 DOM 顺序遍历容器：`<span class="p_N">` 处回填 css[N]，
/// 其余文本节点原样保留（标签保留，`<br/>` 作为段落分隔符存活）；
/// 最后按 `<br>` 切段、剥标签、清洗。
fn extract_paragraphs(seg: &str) -> Vec<String> {
    let css: HashMap<u32, String> = re_css_para()
        .captures_iter(seg)
        .filter_map(|c| {
            let n = c[1].parse::<u32>().ok()?;
            // CSS 字符串内的 `\'` 转义还原。
            Some((n, c[2].replace("\\'", "'")))
        })
        .collect();
    let no_style = re_style_block().replace_all(seg, "");
    let mut merged = String::with_capacity(no_style.len());
    let mut pos = 0usize;
    for cap in re_para_span().captures_iter(&no_style) {
        // 不变量：captures_iter 产出的每个 Captures 的 group(0) 恒为 Some。
        let whole = cap.get(0).expect("group(0) always present");
        merged.push_str(&no_style[pos..whole.start()]);
        if let Ok(n) = cap[1].parse::<u32>()
            && let Some(text) = css.get(&n)
        {
            merged.push_str(text);
        }
        pos = whole.end();
    }
    merged.push_str(&no_style[pos..]);

    // 按 <br> 切段（merged 里标签已保留，br 是唯一分隔符）。
    let mut parts: Vec<String> = Vec::new();
    let mut rest = merged.as_str();
    while let Some(idx) = rest.find("<br") {
        parts.push(rest[..idx].to_string());
        let after = &rest[idx..];
        rest = match after.find('>') {
            Some(end) => &after[end + 1..],
            None => "",
        };
    }
    parts.push(rest.to_string());
    let mut paragraphs = Vec::new();
    for part in parts {
        let plain = clean_text(&part);
        if plain.is_empty() || is_page_break_notice(&plain) {
            continue;
        }
        paragraphs.push(format!("<p>{}</p>", html_escape(&plain)));
    }
    paragraphs
}

/// 解析章节单页 → (标题, 段落 HTML, 章内下一页号)。
/// 无 `chapter-content` 容器返回 None（站点对无效章节 200 软回退书籍页）。
fn parse_chapter_page(html: &str, chapter_id: &str) -> Option<(String, Vec<String>, Option<u32>)> {
    let content = re_chapter_content()
        .captures(html)
        .map(|c| c[1].to_string())?;
    let title = re_chapter_title()
        .captures(html)
        .map(|c| clean_text(&c[1]))
        .unwrap_or_default();
    let paragraphs = extract_paragraphs(&content);
    let next_page = re_chapter_nav()
        .captures(html)
        .and_then(|c| parse_chapter_next_page(&c[1], chapter_id));
    if paragraphs.is_empty() {
        return None;
    }
    Some((title, paragraphs, next_page))
}

// ── 下载计划 ──────────────────────────────────────────────────

pub fn prepare_hen0_plan(
    config: &Config,
    book_id: &str,
    meta_hint: BookMeta,
) -> Result<DownloadPlan> {
    info!(target: "download", book_id, "准备笔下文学下载计划");

    let bid = parse_book_numeric_id(book_id)?;
    let timeout = config.request_timeout.max(5);
    let client = Hen0Client::new(timeout).context("init Hen0Client")?;

    let detail = client.book_detail(bid).context("fetch hen0 book detail")?;
    // 防御校验：详情页解析出的 book_id 应与 URL 一致（不一致说明站点结构变化）。
    if detail.book_id != bid {
        return Err(anyhow!(
            "笔下文学详情页 book_id 与请求不一致（{}/{bid}）",
            detail.book_id
        ));
    }
    let chapters = client.fetch_all_chapters(bid, &detail)?;
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
        word_count: detail.word_count,
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
            debug!(target: "download", error = %e, "笔下文学封面下载失败（忽略）");
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

/// 笔下文学章节下载流程：工作池并发拉取章节（含章内分页），逐章保存。
#[allow(clippy::too_many_arguments)]
pub fn download_hen0_into_manager(
    config: &Config,
    book_id: &str,
    book_name: &str,
    manager: &mut crate::book_parser::book_manager::BookManager,
    _chosen_chapters: &[ChapterRef],
    pending_chapters: &[ChapterRef],
    reporter: &mut crate::download::progress::ProgressReporter,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<DownloadResult> {
    let bid = parse_book_numeric_id(book_id)?;
    let timeout = config.request_timeout.max(5);
    let client = Arc::new(Hen0Client::new(timeout).context("init Hen0Client")?);

    // 实测整本 135 章：4 线程 ~31s / 0 失败；6 线程会被站点拒绝，故上限 4。
    let worker_count = config.hen0_workers.clamp(1, 4);
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
                    match client.chapter_content(bid, &ch.id) {
                        Ok((_page_title, html)) if !html.trim().is_empty() => {
                            contents.push((ch.id.clone(), html));
                        }
                        Ok(_) => {
                            warn!(
                                target: "download",
                                chapter_id = %ch.id,
                                "笔下文学章节正文为空：{} ({})",
                                ch.title, ch.id
                            );
                        }
                        Err(e) => {
                            warn!(
                                target: "download",
                                chapter_id = %ch.id,
                                error = %e,
                                "笔下文学章节下载失败：{} ({})",
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

        let content_map: HashMap<String, String> = contents.into_iter().collect();
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
        "笔下文学下载完成：{} ({} 章，成功 {}，失败 {})",
        book_name,
        pending_chapters.len(),
        result.success,
        result.failed
    );
    Ok(result)
}

// ── 搜索（Web UI / TUI）───────────────────────────────────────

/// 搜索并返回 Web UI 所需的 JSON items。
///
/// - 精确命中（302）→ 借详情页补全元数据，返回单条。
/// - 模糊命中 → 结果表格（单页封顶 30 条、无翻页控件），`has_more` 恒 false。
pub fn search_items(client: &Hen0Client, keyword: &str, page: u32) -> Result<(Vec<Value>, bool)> {
    if page > 1 {
        return Ok((Vec::new(), false));
    }
    let items = client.search(keyword)?;
    let exact = Hen0Client::is_exact_hit(&items);
    let out = if exact {
        // 精确命中：借详情页补全元数据；详情失败则退化为最小条目（book_id 必有）。
        let it = &items[0];
        let detail = client.book_detail(it.book_id).ok();
        let cover = detail
            .as_ref()
            .and_then(|d| normalize_cover_url(&d.cover_url))
            .filter(|u| !u.is_empty());
        vec![json!({
            "book_id": format!("bx:{}", it.book_id),
            "title": detail.as_ref().map(|d| d.book_name.clone()).unwrap_or_default(),
            "author": detail.as_ref().map(|d| d.author.clone()).unwrap_or_default(),
            "raw": detail.as_ref().map(|d| d.description.clone()).unwrap_or_default(),
            "description": detail.as_ref().map(|d| d.description.clone()).unwrap_or_default(),
            "cover_url": cover,
            "category": detail.as_ref().and_then(|d| (!d.category.is_empty()).then(|| d.category.clone())),
            "finished": detail.as_ref().and_then(|d| d.finished),
            "word_count": detail.as_ref().and_then(|d| d.word_count),
            "chapter_count": Value::Null,
            "score": Value::Null,
        })]
    } else {
        items
            .iter()
            .map(|it| {
                let finished = match it.status.as_str() {
                    s if s.contains("完") => Some(true),
                    s if s.contains("连载") => Some(false),
                    _ => None,
                };
                json!({
                    "book_id": format!("bx:{}", it.book_id),
                    "title": it.title,
                    "author": it.author,
                    "latest_chapter": it.latest_chapter,
                    "update_date": it.update_date,
                    "raw": Value::Null,
                    "description": Value::Null,
                    "cover_url": Value::Null,
                    "category": Value::Null,
                    "finished": finished,
                    "word_count": Value::Null,
                    "chapter_count": Value::Null,
                    "score": Value::Null,
                })
            })
            .collect()
    };
    Ok((out, false))
}

// ── BookProvider 适配器 ────────────────────────────────────────

/// 笔下文学源适配器：实现统一 [`crate::provider::BookProvider`]，
/// 全部委托给模块内既有自由函数（保持旧调用与测试不变）。
pub struct Hen0Provider;

impl crate::provider::BookProvider for Hen0Provider {
    fn id(&self) -> &'static str {
        "hen0"
    }

    fn display_name(&self) -> &'static str {
        "笔下文学"
    }

    fn owns_book_id(&self, book_id: &str) -> bool {
        is_hen0_book_id(book_id)
    }

    fn normalize_book_input(&self, input: &str) -> Option<String> {
        normalize_book_input(input)
    }

    // 纯数字不回退 hen0：ID 无区分度，误回退会下载到错误的书。

    fn search(
        &self,
        opts: &crate::provider::SearchOptions,
    ) -> anyhow::Result<(Vec<serde_json::Value>, bool)> {
        let client = Hen0Client::new(opts.timeout_secs)?;
        search_items(&client, &opts.keyword, opts.page)
    }

    fn prepare_plan(
        &self,
        config: &crate::base_system::context::Config,
        book_id: &str,
        meta_hint: crate::download::models::BookMeta,
    ) -> anyhow::Result<crate::download::models::DownloadPlan> {
        prepare_hen0_plan(config, book_id, meta_hint)
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
        download_hen0_into_manager(
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

    fn preview_summary(&self, book_id: &str, timeout_secs: u64) -> Option<BookMeta> {
        let bid = strip_bx_prefix(book_id).parse::<u64>().ok()?;
        let client = Hen0Client::new(timeout_secs).ok()?;
        let detail = client.book_detail(bid).ok()?;
        Some(BookMeta {
            book_name: Some(detail.book_name),
            author: (!detail.author.is_empty()).then_some(detail.author),
            cover_url: (!detail.cover_url.is_empty()).then_some(detail.cover_url),
            category: (!detail.category.is_empty()).then_some(detail.category),
            finished: detail.finished,
            word_count: detail.word_count,
            ..Default::default()
        })
    }

    fn fetch_update_state(
        &self,
        book_id: &str,
        _book_name: Option<&str>,
    ) -> Option<(usize, Option<bool>)> {
        // 详情页取完结态（状态：完本）+ 目录逐页计数（60 章/页）。
        let client = Hen0Client::new(crate::provider::UPDATE_FETCH_TIMEOUT_SECS).ok()?;
        let bid = strip_bx_prefix(book_id).parse::<u64>().ok()?;
        let detail = client.book_detail(bid).ok()?;
        // 目录总数需逐页累计（详情页即目录第 1 页，`_n` 翻页）；保护上限与适配器一致。
        let mut total = detail.toc_first_page.len();
        let mut next = detail.toc_next_page;
        let mut guard = 0u32;
        while let Some(page) = next {
            guard += 1;
            if guard > 2_000 {
                break;
            }
            let url = format!("https://www.hen0.com/book/{bid}_{page}/");
            let html = client.get_text(&url).ok()?;
            let (chapters, next_page) = parse_toc_page(&html, bid, page);
            total += chapters.len();
            next = next_page;
        }
        if total == 0 {
            return None;
        }
        Some((total, detail.finished))
    }
}

// ── 离线解析回归（夹具来自抓包+脱敏，防解析回归）────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_book_input_variants() {
        assert_eq!(
            normalize_book_input("bx:592874").as_deref(),
            Some("bx:592874")
        );
        assert_eq!(
            normalize_book_input("BX:592874").as_deref(),
            Some("bx:592874")
        );
        assert_eq!(
            normalize_book_input("https://www.hen0.com/book/592874/").as_deref(),
            Some("bx:592874")
        );
        assert_eq!(
            normalize_book_input("https://www.hen0.com/book/592874/122773716.html").as_deref(),
            Some("bx:592874")
        );
        // 非本源输入不认领。
        assert_eq!(normalize_book_input("592874"), None);
        assert_eq!(normalize_book_input("sq:123"), None);
        assert_eq!(normalize_book_input("https://www.shuqi.com/book/123"), None);
        assert_eq!(normalize_book_input("bx:abc"), None);
        assert_eq!(normalize_book_input(""), None);
    }

    #[test]
    fn sanitize_keyword_bounds() {
        assert!(
            sanitize_keyword("的").is_err(),
            "单字符应被拒（实测空结果）"
        );
        assert_eq!(sanitize_keyword(" 病娇 ").unwrap(), "病娇");
        let long = "汉".repeat(60);
        assert_eq!(
            sanitize_keyword(&long).unwrap(),
            "汉".repeat(KEYWORD_MAX_CHARS)
        );
    }

    #[test]
    fn parse_search_result_matches_capture() {
        let html = decode_gbk(include_bytes!("testdata/search_result_chuanshu.html"));
        let items = parse_search_result(&html);
        assert_eq!(items.len(), 30, "模糊结果单页封顶 30 条");
        assert_eq!(items[0].book_id, 611317);
        assert_eq!(items[0].title, "扶腰（穿书）");
        assert_eq!(items[0].author, "华三千");
        assert_eq!(items[0].status, "已完成");
        assert_eq!(items[0].update_date, "26-10-17");
        assert!(!items[0].latest_chapter.is_empty());
    }

    #[test]
    fn parse_search_no_result_is_empty() {
        let html = decode_gbk(include_bytes!("testdata/search_no_result.html"));
        let items = parse_search_result(&html);
        assert!(items.is_empty(), "无结果页为空表格，应返回空列表而非错误");
    }

    #[test]
    fn throttled_page_is_jieqi_error() {
        let html = &decode_gbk(include_bytes!("testdata/search_throttled.html"));
        let reason = jieqi_error_reason(html).expect("节流页应识别为杰奇错误页");
        assert!(reason.contains("间隔"), "原因应含「间隔」: {reason}");
        assert!(reason.contains("10"), "原因应含冷却窗口（10 秒）: {reason}");
    }

    #[test]
    fn parse_book_detail_matches_capture() {
        let html = &decode_gbk(include_bytes!("testdata/book_592874.html"));
        let detail = parse_book_detail(html).expect("应能解析详情");
        assert_eq!(detail.book_id, 592874);
        assert_eq!(detail.book_name, "救了病娇后他总想杀我[穿书]");
        assert_eq!(detail.author, "星棘");
        assert_eq!(detail.finished, Some(true));
        assert_eq!(detail.category, "都市言情");
        assert_eq!(detail.word_count, Some(200000));
        assert!(
            detail
                .cover_url
                .starts_with("https://www.hen0.com/files/article/image/592/592874/"),
            "封面应为规范化绝对 URL（单斜杠）: {}",
            detail.cover_url
        );
        assert!(
            !detail.description.is_empty(),
            "简介应为占位文本（脱敏夹具）"
        );
        assert_eq!(detail.toc_first_page.len(), 60, "目录第 1 页 60 章");
        assert_eq!(detail.toc_next_page, Some(2));
    }

    #[test]
    fn parse_toc_page2_and_total() {
        let html = &decode_gbk(include_bytes!("testdata/toc_592874_2.html"));
        let (chapters, next) = parse_toc_page(html, 592874, 2);
        assert_eq!(chapters.len(), 38, "目录第 2 页 38 章");
        assert_eq!(next, None, "第 2 页为末页，无下一页");
        let detail =
            parse_book_detail(&decode_gbk(include_bytes!("testdata/book_592874.html"))).unwrap();
        assert_eq!(
            detail.toc_first_page.len() + chapters.len(),
            98,
            "两页合计应为 98 章（与抓包/实网一致）"
        );
        assert_eq!(chapters.last().unwrap().id, "122773813");
        assert_eq!(
            chapters.last().unwrap().title,
            "第98章 番外6 襄陵奇遇记（二）"
        );
    }

    #[test]
    fn parse_chapter_mixed_html_and_css() {
        // 第 1 章第 1 页：89 个 CSS 注入段（45 非空）+ 4 个纯 HTML 文本段混排。
        let html = &decode_gbk(include_bytes!("testdata/chapter_122773716_p1.html"));
        let (title, paras, next) = parse_chapter_page(html, "122773716").expect("应能解析章节页");
        assert!(title.starts_with("第1章"), "标题应含章节名: {title}");
        assert!(title.contains("(1/3)"), "标题应含章内分页标记: {title}");
        assert_eq!(next, Some(2), "第 1 页应存在章内下一页");
        assert_eq!(
            paras.len(),
            45 + 1,
            "段落总数应等于 CSS 非空段 + 纯文本段（脱敏夹具）"
        );
        let joined = paras.join("\n");
        assert!(joined.contains("【CSS段1】"), "CSS 注入段应被回填");
        assert!(joined.contains("【文本段1】"), "纯 HTML 段应被保留");
        assert!(!joined.contains("::after"), "style 块不应残留到正文");
        assert!(!joined.contains("<span"), "span 标记不应残留");
        assert_eq!(
            paras.first().unwrap(),
            "<p>【CSS段1】</p>",
            "首段应为 CSS 段1"
        );
    }

    #[test]
    fn parse_chapter_drops_page_break_notice() {
        // 真实页面结构：正文末段后跟 `-->>本章未完，点击下一页继续阅读`。
        let html = r#"<h1 class="chapter-title">测试章</h1>
<div class="chapter-content" id="chapter-content">第一段正文<br/><center>-->>本章未完，点击下一页继续阅读<br/></center></div>"#;
        let (_title, paras, next) = parse_chapter_page(html, "1").expect("应能解析");
        assert_eq!(next, None);
        assert_eq!(paras, vec!["<p>第一段正文</p>"], "分页提示行必须被剔除");
        assert!(!paras.join("").contains("本章未完"));
    }

    #[test]
    fn page_break_join_heuristics() {
        // 被截断的句子（无句末标点）→ 拼接
        assert!(page_break_joins("<p>重活一次，我定要一雪前耻，让那群</p>"));
        assert!(page_break_joins("<p>他轻声说，</p>"));
        // 完整段落（有句末标点/闭合引号）→ 保留段落边界
        assert!(!page_break_joins("<p>去更多更远的地方看看。</p>"));
        assert!(!page_break_joins("<p>他说：“好。”</p>"));
        assert!(!page_break_joins("<p>结束！</p>"));
        assert!(!page_break_joins("<p></p>"));
        // 提示行识别
        assert!(is_page_break_notice("-->>本章未完，点击下一页继续阅读"));
        assert!(is_page_break_notice("本章尚未结束，请翻页继续阅读"));
        assert!(!is_page_break_notice("他翻到下一页，继续阅读这本小说。"));
    }

    #[test]
    fn parse_chapter_css_tail_page() {
        // 第 1 章第 3 页（末页）：3 个 span 仅 p_0 有文本，无「下一页」。
        let html = &decode_gbk(include_bytes!("testdata/chapter_122773716_p3_css.html"));
        let (_title, paras, next) = parse_chapter_page(html, "122773716").expect("应能解析章节页");
        assert_eq!(next, None, "末页应无章内下一页");
        assert_eq!(paras.len(), 1, "空段应被丢弃，仅保留 1 个非空 CSS 段");
        assert_eq!(paras[0], "<p>【CSS段1】</p>");
    }

    #[test]
    fn parse_chapter_allcss_page() {
        // 第 98 章第 1 页：99 个 span 全 CSS 注入（50 非空）+ 4 个文本段。
        let html = &decode_gbk(include_bytes!("testdata/chapter_122773813_p1_allcss.html"));
        let (title, paras, next) = parse_chapter_page(html, "122773813").expect("应能解析章节页");
        assert!(title.starts_with("第98章"), "标题应含章节名: {title}");
        assert!(title.contains("(1/3)"));
        assert_eq!(next, Some(2));
        assert_eq!(paras.len(), 50 + 1, "段落总数应等于 CSS 非空段 + 纯文本段");
        let joined = paras.join("\n");
        assert!(joined.contains("【CSS段50】"));
        assert!(!joined.contains("content:"), "style 块不应残留");
    }

    #[test]
    fn invalid_chapter_page_returns_none() {
        // 无效章节 200 软回退书籍页（无 chapter-content）→ 解析返回 None。
        let html = &decode_gbk(include_bytes!("testdata/book_592874.html"));
        assert!(parse_chapter_page(html, "1").is_none());
    }

    #[test]
    fn cover_url_normalization() {
        assert_eq!(
            normalize_cover_url("/files/article/image//592/592874/592874s.jpg").as_deref(),
            Some("https://www.hen0.com/files/article/image/592/592874/592874s.jpg")
        );
        assert_eq!(
            normalize_cover_url("https://www.hen0.com/files/a//b.jpg").as_deref(),
            Some("https://www.hen0.com/files/a/b.jpg")
        );
        assert_eq!(normalize_cover_url(""), None);
    }

    #[test]
    fn word_count_parsing() {
        assert_eq!(parse_word_count("20万字"), Some(200000));
        assert_eq!(parse_word_count("3.5万"), Some(35000));
        assert_eq!(parse_word_count("1200字"), Some(1200));
        assert_eq!(parse_word_count("完本"), None);
    }
}

// ── 实网 E2E（非重放：直连 hen0.com，断言值以抓包为真值锚点）──
//
// 运行：cargo test hen0 --features hen0 -- --ignored --nocapture
// 负载：单链路、人类手速级（搜索间隔 ≥2s 礼貌间隔）。
// 依据 2026-11-06 非重放 e2e：目录 60+38=98 章、第1章 3 页、末章 3 页。
#[cfg(test)]
mod live_e2e {
    use super::*;
    use std::sync::Mutex;

    /// 串行化实网用例，防并行测试互相污染节流观测。
    static LIVE_LOCK: Mutex<()> = Mutex::new(());

    /// A：搜索层实网验证 —— 精确命中 302 短路 + 模糊表格 + 净身连搜免节流。
    #[test]
    #[ignore = "实网 e2e：需联网访问 hen0.com（搜索层，任何网络可跑）"]
    fn e2e_live_search_layer() {
        let _guard = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let client = Hen0Client::new(20).expect("init live client");

        // ① 精确书名搜索 → 302 短路 → search_items 借详情页补全为单条。
        let (is_exact, hit_id) = client
            .search("救了病娇后他总想杀我")
            .map(|v| {
                (
                    Hen0Client::is_exact_hit(&v),
                    v.first().map(|x| x.book_id).unwrap_or(0),
                )
            })
            .expect("① 精确搜索失败");
        assert!(is_exact, "① 精确命中应识别为 302 短路");
        assert_eq!(hit_id, 592874, "① 精确命中应解析出 book_id");
        let (json_items, json_more) = {
            let (v, m) =
                search_items(&client, "救了病娇后他总想杀我", 1).expect("① search_items 失败");
            assert_eq!(v.len(), 1, "精确命中应返回单条");
            assert_eq!(v[0]["book_id"].as_str(), Some("bx:592874"));
            assert_eq!(v[0]["author"].as_str(), Some("星棘"));
            (v, m)
        };
        assert!(!json_more);
        assert!(json_items[0]["cover_url"].is_null() || json_items[0]["cover_url"].is_string());
        println!("[①] 精确命中 bx:592874，书名={}", json_items[0]["title"]);

        // ② 模糊搜索 → 结果表格 30 条封顶。
        let (items2, more2) = search_items(&client, "穿书", 1).expect("② 模糊搜索失败");
        assert!(!items2.is_empty(), "② 「穿书」应有结果");
        assert!(!more2, "② 结果页无翻页控件，has_more 恒 false");
        assert!(items2.len() <= 30);
        println!("[②] 「穿书」命中 {} 条", items2.len());

        // ③ 净身连搜免节流：2s 间隔连续两次搜索均应成功（节流为纯 Cookie 侧）。
        let _ = client.search("白月光").expect("③ 连搜第一次失败");
        client
            .search("病娇")
            .expect("③ 2s 间隔连搜第二次失败（节流未绕过？）");
        println!("[③] 净身连搜（2s 间隔）两次均成功 —— 节流绕过生效");
    }

    /// C：章内分页提示剔除 + 跨页断句拼接（用户指定样本，实网 5 页）。
    #[test]
    #[ignore = "实网 e2e：需联网访问 hen0.com（分页提示剔除 + 跨页拼接）"]
    fn e2e_live_page_break_notice_and_join() {
        let _guard = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let client = Hen0Client::new(20).expect("init live client");
        let (_title, body) = client
            .chapter_content(586264, "121251888")
            .expect("正文拉取失败");
        assert!(!body.contains("本章未完"), "分页提示行应被剔除");
        assert!(!body.contains("点击下一页"), "分页提示行应被剔除");
        // 页1末“让那群” + 页2首“混蛋付出代价。” —— 断句必须直接拼接。
        assert!(body.contains("让那群混蛋付出代价"), "跨页断句应直接拼接");
        // 页2末“看看。” + 页3首“我那时想...” —— 完整段落不应硬拼。
        assert!(!body.contains("看看。我那时想"), "完整段落边界不应被硬拼");
        let cjk = body
            .chars()
            .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
            .count();
        assert!(cjk > 2000, "正文应完整，实际 {cjk} 汉字");
        println!("[分页] 正文 {cjk} 汉字；提示行已剔除；跨页断句已拼接");
    }

    /// D：多本书样本 —— 分页提示剔除（实网覆盖不同书/不同页数）。
    #[test]
    #[ignore = "实网 e2e：需联网访问 hen0.com（多本书分页提示剔除）"]
    fn e2e_live_page_break_notice_samples() {
        let _guard = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let client = Hen0Client::new(20).expect("init live client");
        let samples: &[(u64, &str)] = &[
            (586264, "121251888"), // 5 页
            (586264, "121251887"), // 5 页
            (611317, "129032249"), // 6 页
            (611179, "128987119"), // 3 页
            (611094, "128967763"), // 4 页
            (610969, "128902907"), // 4 页
            (610865, "128836248"), // 3 页
        ];
        for (bid, cid) in samples {
            let (_title, body) = client
                .chapter_content(*bid, cid)
                .unwrap_or_else(|e| panic!("{bid}/{cid} 正文拉取失败: {e}"));
            assert!(!body.contains("本章未完"), "{bid}/{cid} 仍残留分页提示行");
            assert!(!body.contains("点击下一页"), "{bid}/{cid} 仍残留分页提示行");
            let cjk = body
                .chars()
                .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
                .count();
            assert!(cjk > 500, "{bid}/{cid} 正文过短（{cjk} 汉字）");
            println!("[{bid}/{cid}] {cjk} 汉字，无分页提示");
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    /// B：完整链路实网验证 —— 详情/目录/正文/真实下载。
    #[test]
    #[ignore = "实网 e2e：需联网访问 hen0.com（完整链路：详情/目录/正文/下载 3 章）"]
    fn e2e_live_full_chain() {
        let _guard = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let client = Hen0Client::new(20).expect("init live client");

        // ① 详情。抓包/实网真值：书名/作者/完本/20万字/98 章（60+38）。
        let detail = client.book_detail(592874).expect("① 详情拉取失败");
        assert_eq!(detail.book_name, "救了病娇后他总想杀我[穿书]");
        assert_eq!(detail.author, "星棘");
        assert_eq!(detail.finished, Some(true));
        println!(
            "[①] 详情：{} / {} / 完本={:?} / 字数={:?}",
            detail.book_name, detail.author, detail.finished, detail.word_count
        );

        // ② 全量目录。真值：98 章，首章 122773716「第1章」，末章 122773813「第98章 番外6 襄陵奇遇记（二）」。
        let chapters = client
            .fetch_all_chapters(592874, &detail)
            .expect("② 目录拉取失败");
        assert_eq!(chapters.len(), 98, "章节数应为 98（抓包 total=98）");
        assert_eq!(chapters[0].id, "122773716");
        let last = chapters.last().unwrap();
        assert_eq!(last.id, "122773813");
        assert_eq!(last.title, "第98章 番外6 襄陵奇遇记（二）");
        println!(
            "[②] 目录 {} 章，首章「{}」末章「{}」",
            chapters.len(),
            chapters[0].title,
            last.title
        );

        // ③ 第 1 章正文（3 页 + CSS 注入合并）。抓包/实网真值：3 页、合计约 2800 汉字。
        let (title, body) = client
            .chapter_content(592874, "122773716")
            .expect("③ 正文拉取失败");
        assert!(title.starts_with("第1章"));
        let cjk = body
            .chars()
            .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
            .count();
        assert!(cjk > 2000, "③ 第 1 章正文应约 2800 汉字，实际 {cjk}");
        assert!(!body.contains("::after"), "③ style 块不应残留");
        assert!(!body.contains("<span"), "③ span 标记不应残留");
        println!("[③] 第1章正文 {cjk} 汉字");

        // ④ 末章（第 98 章）正文。实网真值：3 页、约 3200 汉字。
        let (title4, body4) = client
            .chapter_content(592874, "122773813")
            .expect("④ 末章拉取失败");
        assert!(title4.starts_with("第98章"));
        let cjk4 = body4
            .chars()
            .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
            .count();
        assert!(cjk4 > 2000, "④ 末章正文应约 3200 汉字，实际 {cjk4}");
        println!("[④] 第98章正文 {cjk4} 汉字");

        // ⑤ 真实下载（临时目录，仅前 3 章）：计划准备 → 工作池下载 → BookManager 落盘。
        let mut cfg = Config::default();
        let tmp = std::env::temp_dir().join(format!("undl_hen0_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("创建临时目录");
        cfg.save_path = tmp.to_string_lossy().to_string();

        let hint = BookMeta {
            book_name: Some(detail.book_name.clone()),
            ..Default::default()
        };
        let plan = prepare_hen0_plan(&cfg, "bx:592874", hint).expect("⑤ 下载计划失败");
        assert_eq!(plan.chapters.len(), 98);
        assert_eq!(plan.meta.author.as_deref(), Some("星棘"));

        let chosen: Vec<ChapterRef> = plan.chapters.iter().take(3).cloned().collect();
        let pending: Vec<ChapterRef> = chosen.clone();
        let mut manager = crate::book_parser::book_manager::BookManager::new(
            cfg.clone(),
            "bx:592874",
            plan.meta
                .book_name
                .as_deref()
                .unwrap_or("救了病娇后他总想杀我[穿书]"),
        )
        .expect("BookManager 创建失败");
        let mut reporter = crate::download::progress::make_reporter(&cfg, &chosen, &pending, None);
        let result = download_hen0_into_manager(
            &cfg,
            "bx:592874",
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
        .expect("⑤ 下载执行失败");
        println!(
            "[⑤] 下载 {} 章：成功 {}，失败 {}（临时目录 {}）",
            pending.len(),
            result.success,
            result.failed,
            tmp.display()
        );
        assert_eq!(result.success, 3);
        assert_eq!(result.failed, 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    #[ignore = "实网 bench：hen0 下载 5 章（并行分页）"]
    fn bench_download_five_chapters() {
        let mut cfg = Config::default();
        let workers = std::env::var("HEN0_BENCH_WORKERS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(4);
        cfg.max_workers = workers;
        cfg.hen0_workers = workers;
        let tmp = std::env::temp_dir().join(format!("undl_hen0_bench_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("创建临时目录");
        cfg.save_path = tmp.to_string_lossy().to_string();

        let hint = BookMeta {
            book_name: Some("不断作死后我成了白月光".to_string()),
            ..Default::default()
        };
        let t_plan = std::time::Instant::now();
        let plan = prepare_hen0_plan(&cfg, "bx:586264", hint).expect("下载计划失败");
        println!(
            "[bench] 计划 {:?}，总章 {}",
            t_plan.elapsed(),
            plan.chapters.len()
        );

        let chapter_count = std::env::var("HEN0_BENCH_CHAPTERS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(5)
            .clamp(1, plan.chapters.len());
        let chosen: Vec<ChapterRef> = plan.chapters.iter().take(chapter_count).cloned().collect();
        let pending: Vec<ChapterRef> = chosen.clone();
        let name = plan.meta.book_name.as_deref().unwrap_or("bench");
        let mut manager =
            crate::book_parser::book_manager::BookManager::new(cfg.clone(), "bx:586264", name)
                .expect("BookManager 创建失败");
        let mut reporter = crate::download::progress::make_reporter(&cfg, &chosen, &pending, None);
        let t = std::time::Instant::now();
        let result = download_hen0_into_manager(
            &cfg,
            "bx:586264",
            name,
            &mut manager,
            &chosen,
            &pending,
            &mut reporter,
            None,
        )
        .expect("下载执行失败");
        println!(
            "[bench] 下载 5 章耗时 {:?}，成功={}，失败={}",
            t.elapsed(),
            result.success,
            result.failed
        );
        assert_eq!(result.success, chapter_count as u32);
        assert_eq!(result.failed, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
