//! 笔趣阁（二站，www.biquga.com）小说源适配。
//!
//! 接口清单（依据 examples/笔趣阁2_解析结果 抓包 2026-11-06 + 非重放实网 e2e 2026-11-06）：
//! - 搜索：`POST /search.html`，表单 `s=<UTF-8 urlencode>`；结果
//!   `ul.txt-list.txt-list-row5` 的 `li`：`span.s1` 分类、`span.s2` 书名链接 `/{aid}_{bid}/`、
//!   `span.s3` 作者、`span.s4` 最新章节、`span.s5` 日期。单页无翻页。
//! - 详情：`GET /{aid}_{bid}/`，`og:novel:*` meta +
//!   `ul.section-list.fix`（最新章节 12 条）+ `ul.fix.section-list`（章节列表 100 条）。
//! - 正文：`GET /{aid}_{bid}/{cid}.html`，`div.word_read` 内正文以
//!   `<script>document.writeln(qsbs.bb('<base64>'));</script>` 逐段输出（标准 Base64 +
//!   UTF-8），需解码后按 `<p>` 提取；章内分页 `{cid}_{p}.html`，由页内
//!   `var kkehvov='…'` 指向下一页。
//!
//! **已知限制（实网确认，非适配缺陷）**：站点的完整目录页 `index_{n}.html` 已被站群污染，
//! 返回的是「三一中文」的另一本书（404/串页），`?page=n` 也不生效。因此本适配器只能取得
//! 详情页暴露的「章节列表前 100 章 + 最新章节 12 条」。返回的目录会在日志与元数据中如实反映，
//! 不做逐章爬取补全（1000+ 章的站点负载不可接受）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use crossbeam_channel as channel;
use regex::Regex;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::base_system::book_paths;
use crate::base_system::context::Config;
use crate::download::downloader::chunk_chapters;
use crate::download::models::{
    BookMeta, ChapterRef, DownloadPlan, DownloadResult, merge_meta_prefer_hint_name,
};

// ── 常量 ──────────────────────────────────────────────────────

const SITE_BASE: &str = "https://www.biquga.com";

const SEARCH_MIN_GAP: Duration = Duration::from_secs(2);
const CHAPTER_MAX_PAGES: u32 = 50;
const WORKER_LIMIT: usize = 4;

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36 Edg/156.0.0.0";

// ── book_id 识别与规范化 ──────────────────────────────────────

/// `bg:` 前缀即笔趣阁（二站）book_id，形如 `bg:171_171712`。
pub fn is_biquga_book_id(book_id: &str) -> bool {
    book_id.starts_with("bg:")
}

pub fn strip_bg_prefix(book_id: &str) -> &str {
    book_id.strip_prefix("bg:").unwrap_or(book_id)
}

/// 从用户输入识别：`bg:171_171712` 或 biquga.com 书籍/章节页 URL。
pub fn normalize_book_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("bg:")
        && valid_book_id(rest)
    {
        return Some(format!("bg:{rest}"));
    }
    re_book_url()
        .captures(trimmed)
        .map(|c| format!("bg:{}_{}", &c[1], &c[2]))
}

fn valid_book_id(s: &str) -> bool {
    let mut parts = s.split('_');
    let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !a.is_empty()
        && !b.is_empty()
        && a.chars().all(|c| c.is_ascii_digit())
        && b.chars().all(|c| c.is_ascii_digit())
}

fn parse_book_id(book_id: &str) -> Result<(String, String)> {
    let rest = strip_bg_prefix(book_id);
    if !valid_book_id(rest) {
        return Err(anyhow!("非法笔趣阁（二站）book_id: {book_id:?}"));
    }
    let (aid, bid) = rest.split_once('_').expect("validated");
    Ok((aid.to_string(), bid.to_string()))
}

/// 详情页/目录页路径段 `{aid}_{bid}`。
fn book_path(book_id: &str) -> Result<String> {
    let (aid, bid) = parse_book_id(book_id)?;
    Ok(format!("{aid}_{bid}"))
}

fn sanitize_keyword(raw: &str) -> Result<String> {
    let kw = raw.trim();
    if kw.is_empty() {
        return Err(anyhow!("搜索关键字不能为空"));
    }
    Ok(kw.to_string())
}

// ── 正则缓存 ──────────────────────────────────────────────────

fn re_book_url() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)biquga\.com/(\d+)_(\d+)(?:/\d+\.html)?").expect("compile re_book_url")
    })
}

fn re_search_list() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<ul class="txt-list txt-list-row5">([\s\S]*?)</ul>"#)
            .expect("compile re_search_list")
    })
}

fn re_search_li() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?s)<li><span class="s1">\[(.*?)\]</span><span class="s2"><a href="/(\d+)_(\d+)/"[^>]*>(.*?)</a></span><span class="s3"><a[^>]*>(.*?)</a></span><span class="s4"><a[^>]*>(.*?)</a></span><span class="s5">(.*?)</span></li>"#,
        )
        .expect("compile re_search_li")
    })
}

fn re_meta_og() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<meta property="(og:[^"]+)" content="([^"]*)""#)
            .expect("compile re_meta_og")
    })
}

fn re_section_list() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<ul class="fix section-list">([\s\S]*?)</ul>"#)
            .expect("compile re_section_list")
    })
}

fn re_latest_list() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<ul class="section-list fix">([\s\S]*?)</ul>"#)
            .expect("compile re_latest_list")
    })
}

fn re_chapter_li() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<li><a href="/(\d+_\d+)/(\d+)\.html">(.*?)</a></li>"#)
            .expect("compile re_chapter_li")
    })
}

fn re_word_read() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?s)<div class="word_read">([\s\S]*?)(?:<script>eval\(atob|<div class="pc-footer")"#,
        )
        .expect("compile re_word_read")
    })
}

fn re_qsbs() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"qsbs\.bb\('([A-Za-z0-9+/=]+)'\)"#).expect("compile re_qsbs"))
}

fn re_p() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?is)<p[^>]*>([\s\S]*?)</p>"#).expect("compile re_p"))
}

fn re_br() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)<br\s*/?>").expect("compile re_br"))
}

fn re_chapter_title() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<h3>(.*?)</h3>").expect("compile re_chapter_title"))
}

fn re_page_suffix() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[\(（]\s*第?\s*\d+\s*页\s*[\)）]").expect("compile re_page_suffix")
    })
}

fn re_next_var() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"var kkehvov='([^']*)'"#).expect("compile re_next_var"))
}

fn re_page_url() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/\d+_\d+/(\d+)_(\d+)\.html").expect("compile re_page_url"))
}

// ── 文本工具 ──────────────────────────────────────────────────

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

fn clean_text(s: &str) -> String {
    let unescaped = crate::book_parser::html_utils::unescape_basic_entities(s);
    strip_tags(&unescaped)
        .replace('\u{a0}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 规范化封面 URL（详情页 og:image 为站内相对路径 `/img/171712.jpg`）。
fn normalize_cover_url(src: &str) -> Option<String> {
    let s = src.trim();
    if s.is_empty() || s.contains("nocover") {
        return None;
    }
    if let Some(rest) = s.strip_prefix("//") {
        return Some(format!("https:{rest}"));
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        return Some(s.to_string());
    }
    if s.starts_with('/') {
        return Some(format!("{SITE_BASE}{s}"));
    }
    Some(format!("{SITE_BASE}/{s}"))
}

fn og_value(html: &str, key: &str) -> Option<String> {
    for cap in re_meta_og().captures_iter(html) {
        if cap[1].eq_ignore_ascii_case(key) {
            return Some(cap[2].to_string());
        }
    }
    None
}

// ── 数据结构 ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BiqugaSearchItem {
    pub aid: String,
    pub bid: String,
    pub title: String,
    pub author: String,
    pub latest_chapter: String,
    pub update_date: String,
    pub category: String,
}

#[derive(Debug, Clone)]
pub struct BiqugaBookDetail {
    pub book_name: String,
    pub author: String,
    pub description: String,
    pub cover_url: String,
    pub category: String,
    pub finished: Option<bool>,
    /// 详情页暴露的章节（前 100 章 + 最新章节，按章节 id 升序去重）。
    pub chapters: Vec<ChapterRef>,
}

// ── 客户端 ────────────────────────────────────────────────────

pub struct BiqugaClient {
    http: reqwest::blocking::Client,
    last_search: Mutex<Option<Instant>>,
}

impl BiqugaClient {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(5)))
            .user_agent(USER_AGENT)
            .build()
            .context("build biquga HTTP client")?;
        Ok(Self {
            http,
            last_search: Mutex::new(None),
        })
    }

    fn throttle_gap(&self) {
        let wait = {
            let prev = self.last_search.lock().unwrap_or_else(|e| e.into_inner());
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
        *self.last_search.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    pub(crate) fn get_text(&self, url: &str) -> Result<String> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..2u32 {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(300));
            }
            match self.http.get(url).send() {
                Ok(resp) => {
                    let status = resp.status();
                    let bytes = resp.bytes().context("读取笔趣阁（二站）响应失败")?;
                    if status.is_success() {
                        return Ok(String::from_utf8_lossy(&bytes).into_owned());
                    }
                    return Err(anyhow!("笔趣阁（二站）请求 HTTP {status}：{url}"));
                }
                Err(e) => {
                    debug!(target: "biquga", url, attempt, error = %e, "笔趣阁（二站）请求传输错误，将重试");
                    last_err = Some(
                        anyhow::Error::new(e).context(format!("笔趣阁（二站）请求失败：{url}")),
                    );
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("笔趣阁（二站）请求失败：{url}")))
    }

    pub fn search(&self, raw_keyword: &str) -> Result<Vec<BiqugaSearchItem>> {
        let keyword = sanitize_keyword(raw_keyword)?;
        self.throttle_gap();
        let url = format!("{SITE_BASE}/search.html");
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Origin", SITE_BASE)
            .header("Referer", format!("{SITE_BASE}/"))
            .body(format!("s={}", form_encode(&keyword)))
            .send()
            .context("笔趣阁（二站）搜索请求失败")?;
        let status = resp.status();
        let html = resp
            .bytes()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .context("笔趣阁（二站）搜索响应读取失败")?;
        if !status.is_success() {
            return Err(anyhow!("笔趣阁（二站）搜索 HTTP {status}"));
        }
        if re_search_list().captures(&html).is_none() {
            return Err(anyhow!("笔趣阁（二站）搜索返回异常（无结果列表）"));
        }
        Ok(parse_search_result(&html))
    }

    pub fn book_detail(&self, book_id: &str) -> Result<BiqugaBookDetail> {
        let path = book_path(book_id)?;
        let url = format!("{SITE_BASE}/{path}/");
        let html = self.get_text(&url)?;
        parse_book_detail(&html)
            .ok_or_else(|| anyhow!("笔趣阁（二站）详情页解析失败（book_id={book_id}）"))
    }

    /// 章节正文（含章内分页）。返回 (标题, `<p>` 段落 HTML)。
    pub fn chapter_content(&self, book_id: &str, chapter_id: &str) -> Result<(String, String)> {
        let path = book_path(book_id)?;
        let mut paragraphs: Vec<String> = Vec::new();
        let mut title = String::new();
        let mut cid = chapter_id.to_string();
        let mut guard = 0u32;
        loop {
            guard += 1;
            if guard > CHAPTER_MAX_PAGES {
                return Err(anyhow!(
                    "笔趣阁（二站）章内分页超过保护上限（{CHAPTER_MAX_PAGES} 页）：cid={chapter_id}"
                ));
            }
            let url = format!("{SITE_BASE}/{path}/{cid}.html");
            let html = self.get_text(&url)?;
            let (page_title, page_paras, next) = parse_chapter_page(&html, chapter_id);
            if guard == 1 {
                title = page_title;
            }
            if guard == 1 && page_paras.is_empty() {
                return Err(anyhow!("笔趣阁（二站）章节正文为空（cid={chapter_id}）"));
            }
            paragraphs.extend(page_paras);
            match next {
                Some(c) => cid = c,
                None => break,
            }
        }
        if paragraphs.is_empty() {
            return Err(anyhow!("笔趣阁（二站）章节正文为空（cid={chapter_id}）"));
        }
        Ok((title, paragraphs.join("\n")))
    }
}

fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── 解析 ──────────────────────────────────────────────────────

fn parse_search_result(html: &str) -> Vec<BiqugaSearchItem> {
    let Some(list) = re_search_list().captures(html) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for cap in re_search_li().captures_iter(&list[1]) {
        let title = clean_text(&cap[4]);
        if title.is_empty() {
            continue;
        }
        items.push(BiqugaSearchItem {
            aid: cap[2].to_string(),
            bid: cap[3].to_string(),
            title,
            author: clean_text(&cap[5]),
            latest_chapter: clean_text(&cap[6]),
            update_date: clean_text(&cap[7]),
            category: clean_text(&cap[1]),
        });
    }
    items
}

fn parse_chapter_list(html: &str, re: &Regex) -> Vec<ChapterRef> {
    let Some(block) = re.captures(html) else {
        return Vec::new();
    };
    re_chapter_li()
        .captures_iter(&block[1])
        .filter_map(|c| {
            let title = clean_text(&c[3]);
            if title.is_empty() {
                return None;
            }
            Some(ChapterRef {
                id: c[2].to_string(),
                title,
            })
        })
        .collect()
}

fn parse_book_detail(html: &str) -> Option<BiqugaBookDetail> {
    let book_name = og_value(html, "og:novel:book_name")
        .or_else(|| og_value(html, "og:title"))
        .map(|s| clean_text(&s))
        .filter(|s| !s.is_empty())?;
    let author = og_value(html, "og:novel:author")
        .map(|s| clean_text(&s))
        .unwrap_or_default();
    let description = og_value(html, "og:description")
        .map(|s| clean_text(&s))
        .unwrap_or_default();
    let cover_url = og_value(html, "og:image")
        .and_then(|s| normalize_cover_url(&s))
        .unwrap_or_default();
    let category = og_value(html, "og:novel:category")
        .map(|s| clean_text(&s))
        .unwrap_or_default();
    let finished = og_value(html, "og:novel:status").map(|s| {
        let s = clean_text(&s);
        s.contains("完") || s.contains("完结")
    });

    let mut chapters = parse_chapter_list(html, re_section_list());
    let latest = parse_chapter_list(html, re_latest_list());
    let mut seen: std::collections::HashSet<String> =
        chapters.iter().map(|c| c.id.clone()).collect();
    for ch in latest {
        if seen.insert(ch.id.clone()) {
            chapters.push(ch);
        }
    }
    // 章节 id 随发布递增，升序即阅读顺序；中间缺失章由站点目录页缺陷导致（见模块文档）。
    chapters.sort_by_key(|c| c.id.parse::<u64>().unwrap_or(0));

    Some(BiqugaBookDetail {
        book_name,
        author,
        description,
        cover_url,
        category,
        finished,
        chapters,
    })
}

/// 解析章节页：返回 (标题, 段落列表, 同章下一页 URL 中的 cid)。
fn parse_chapter_page(html: &str, chapter_id: &str) -> (String, Vec<String>, Option<String>) {
    let title = re_chapter_title()
        .captures(html)
        .map(|c| clean_text(&c[1]))
        .map(|s| re_page_suffix().replace_all(&s, "").trim().to_string())
        .unwrap_or_default();

    let mut paragraphs = Vec::new();
    // 主形态：Base64 脚本段落（整段 HTML）。
    let block = re_word_read().captures(html).map(|c| c[1].to_string());
    let source = block.as_deref().unwrap_or(html);
    let mut decoded_any = false;
    for cap in re_qsbs().captures_iter(source) {
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&cap[1]) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        decoded_any = true;
        if text.contains("<p") {
            for p in re_p().captures_iter(&text) {
                push_paragraph(&mut paragraphs, &p[1]);
            }
        } else {
            for line in re_br().split(&text) {
                push_paragraph(&mut paragraphs, line);
            }
        }
    }
    // 兜底形态：普通 `<p>` 段落（短章节/站点切换编码时）。
    if !decoded_any {
        for p in re_p().captures_iter(source) {
            let text = clean_text(&p[1]);
            if text.is_empty() || is_noise_paragraph(&text) {
                continue;
            }
            paragraphs.push(format!("<p>{text}</p>"));
        }
    }

    // 下一页：kkehvov 指向同章 `{cid}_{p}.html` 时继续；否则结束。
    let next = re_next_var()
        .captures(html)
        .map(|c| c[1].to_string())
        .and_then(|u| {
            let c = re_page_url().captures(&u)?;
            (c[1].to_string() == chapter_id && c[2].parse::<u32>().ok()? >= 1)
                .then(|| format!("{}_{}", &c[1], &c[2]))
        });
    (title, paragraphs, next)
}

fn push_paragraph(out: &mut Vec<String>, raw: &str) {
    let text = clean_text(raw);
    if text.is_empty() || is_noise_paragraph(&text) {
        return;
    }
    out.push(format!("<p>{text}</p>"));
}

fn is_noise_paragraph(s: &str) -> bool {
    s.contains("请关闭浏览器阅读模式") || s.starts_with("相邻推荐") || s.starts_with("相邻推荐:")
}

// ── 下载计划 ──────────────────────────────────────────────────

pub fn prepare_biquga_plan(
    config: &Config,
    book_id: &str,
    meta_hint: BookMeta,
) -> Result<DownloadPlan> {
    info!(target: "download", book_id, "准备笔趣阁（二站）下载计划");

    let path = book_path(book_id)?;
    let timeout = config.request_timeout.max(5);
    let client = BiqugaClient::new(timeout).context("init BiqugaClient")?;

    let detail = client
        .book_detail(book_id)
        .context("fetch biquga book detail")?;
    let chapters = detail.chapters.clone();
    if chapters.is_empty() {
        return Err(anyhow!("笔趣阁（二站）目录为空（book_id={book_id}）"));
    }
    let chapter_count = chapters.len();
    if chapter_count <= 112 {
        warn!(
            target: "download",
            book_id,
            path,
            "笔趣阁（二站）目录受站点限制，仅取得前 100 章 + 最新章节（共 {} 章）",
            chapter_count
        );
    }

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

    if let Some(cover_url) = merged.cover_url.as_ref() {
        let folder = book_paths::book_folder_path(config, book_id, merged.book_name.as_deref());
        let _ = std::fs::create_dir_all(&folder);
        let cover_path = book_paths::canonical_cover_path(&folder, "jpg");
        if !cover_path.exists()
            && let Err(e) = download_cover(&client.http, cover_url, &cover_path)
        {
            debug!(target: "download", error = %e, "笔趣阁（二站）封面下载失败（忽略）");
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

#[allow(clippy::too_many_arguments)]
pub fn download_biquga_into_manager(
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
    let client = Arc::new(BiqugaClient::new(timeout).context("init BiqugaClient")?);
    let book_id = book_id.to_string();

    let worker_count = config.biquga_workers.clamp(1, WORKER_LIMIT);
    let (tx_jobs, rx_jobs) = channel::unbounded::<Vec<ChapterRef>>();
    let (tx_res, rx_res) = channel::unbounded::<Result<(Vec<ChapterRef>, Vec<(String, String)>)>>();

    for group in chunk_chapters(pending_chapters, 8) {
        tx_jobs.send(group.to_vec()).ok();
    }
    drop(tx_jobs);

    for _ in 0..worker_count {
        let rx = rx_jobs.clone();
        let tx = tx_res.clone();
        let client = Arc::clone(&client);
        let book_id = book_id.clone();
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
                    match client.chapter_content(&book_id, &ch.id) {
                        Ok((_title, html)) if !html.trim().is_empty() => {
                            contents.push((ch.id.clone(), html));
                        }
                        Ok(_) => {
                            warn!(target: "download", chapter_id = %ch.id, "笔趣阁（二站）章节正文为空：{}", ch.title);
                        }
                        Err(e) => {
                            warn!(target: "download", chapter_id = %ch.id, error = %e, "笔趣阁（二站）章节下载失败：{}", ch.title);
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
        "笔趣阁（二站）下载完成：{} ({} 章，成功 {}，失败 {})",
        book_name,
        pending_chapters.len(),
        result.success,
        result.failed
    );
    Ok(result)
}

// ── 搜索（Web UI / TUI）───────────────────────────────────────

pub fn search_items(client: &BiqugaClient, keyword: &str, page: u32) -> Result<(Vec<Value>, bool)> {
    if page > 1 {
        return Ok((Vec::new(), false));
    }
    let items = client.search(keyword)?;
    let out = items
        .iter()
        .map(|it| {
            json!({
                "book_id": format!("bg:{}_{}", it.aid, it.bid),
                "title": it.title,
                "author": it.author,
                "latest_chapter": it.latest_chapter,
                "update_date": it.update_date,
                "raw": Value::Null,
                "description": Value::Null,
                "cover_url": Value::Null,
                "category": (!it.category.is_empty()).then(|| it.category.clone()),
                "finished": Value::Null,
                "word_count": Value::Null,
                "chapter_count": Value::Null,
                "score": Value::Null,
            })
        })
        .collect();
    Ok((out, false))
}

// ── BookProvider 适配器 ────────────────────────────────────────

pub struct BiqugaProvider;

impl crate::provider::BookProvider for BiqugaProvider {
    fn id(&self) -> &'static str {
        "biquga"
    }

    fn display_name(&self) -> &'static str {
        "笔趣阁二站"
    }

    fn owns_book_id(&self, book_id: &str) -> bool {
        is_biquga_book_id(book_id)
    }

    fn normalize_book_input(&self, input: &str) -> Option<String> {
        normalize_book_input(input)
    }

    fn search(
        &self,
        opts: &crate::provider::SearchOptions,
    ) -> anyhow::Result<(Vec<serde_json::Value>, bool)> {
        let client = BiqugaClient::new(opts.timeout_secs)?;
        search_items(&client, &opts.keyword, opts.page)
    }

    fn prepare_plan(
        &self,
        config: &crate::base_system::context::Config,
        book_id: &str,
        meta_hint: crate::download::models::BookMeta,
    ) -> anyhow::Result<crate::download::models::DownloadPlan> {
        prepare_biquga_plan(config, book_id, meta_hint)
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
        download_biquga_into_manager(
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
        let client = BiqugaClient::new(timeout_secs).ok()?;
        let detail = client.book_detail(book_id).ok()?;
        Some(BookMeta {
            book_name: Some(detail.book_name),
            author: (!detail.author.is_empty()).then_some(detail.author),
            cover_url: (!detail.cover_url.is_empty()).then_some(detail.cover_url),
            category: (!detail.category.is_empty()).then_some(detail.category),
            finished: detail.finished,
            chapter_count: Some(detail.chapters.len()),
            ..Default::default()
        })
    }

    fn fetch_update_state(
        &self,
        book_id: &str,
        _book_name: Option<&str>,
    ) -> Option<(usize, Option<bool>)> {
        let client = BiqugaClient::new(crate::provider::UPDATE_FETCH_TIMEOUT_SECS).ok()?;
        let detail = client.book_detail(book_id).ok()?;
        (!detail.chapters.is_empty()).then_some((detail.chapters.len(), detail.finished))
    }
}

// ── 离线解析回归 ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/src/biquga/testdata/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("read fixture")
    }

    #[test]
    fn normalize_book_input_variants() {
        assert_eq!(
            normalize_book_input("bg:171_171712").as_deref(),
            Some("bg:171_171712")
        );
        assert_eq!(
            normalize_book_input("https://www.biquga.com/171_171712/").as_deref(),
            Some("bg:171_171712")
        );
        assert_eq!(
            normalize_book_input("https://www.biquga.com/171_171712/69677852.html").as_deref(),
            Some("bg:171_171712")
        );
        assert_eq!(normalize_book_input("bg:171"), None);
    }

    #[test]
    fn search_result_matches_capture() {
        let html = fixture("search_result.html");
        let items = parse_search_result(&html);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].aid, "171");
        assert_eq!(items[0].bid, "171712");
        assert_eq!(items[0].title, "黑莲花攻略手册[穿书]");
        assert_eq!(items[0].author, "白羽摘雕弓");
        assert_eq!(items[0].category, "都市小说");
        assert_eq!(items[0].latest_chapter, "崩坏脑洞2.0——十五年（下）");
    }

    #[test]
    fn book_detail_collects_section_and_latest() {
        let html = fixture("book_detail.html");
        let d = parse_book_detail(&html).expect("detail parsed");
        assert_eq!(d.book_name, "黑莲花攻略手册[穿书]");
        assert_eq!(d.author, "白羽摘雕弓");
        assert_eq!(d.finished, Some(false));
        // 抓包：章节列表 100 条 + 最新章节 12 条（末尾与章节列表不重叠时合计 112）。
        assert!(d.chapters.len() >= 100);
        assert_eq!(d.chapters[0].id, "69677852");
        assert_eq!(d.chapters[0].title, "替嫁一修");
    }

    #[test]
    fn chapter_base64_paragraphs() {
        let html = fixture("chapter_69677852.html");
        let (title, paras, next) = parse_chapter_page(&html, "69677852");
        assert!(title.starts_with("替嫁一修"), "title={title}");
        assert!(paras.len() > 10);
        assert!(paras.iter().any(|p| p.contains("红衣少女坐在妆台前")));
        assert_eq!(next.as_deref(), Some("69677852_1"));
    }

    #[test]
    fn chapter_second_page_parses() {
        let html = fixture("chapter_69677852_p2.html");
        let (_title, paras, _next) = parse_chapter_page(&html, "69677852");
        assert!(!paras.is_empty());
    }

    #[test]
    fn form_encode_matches_browser() {
        assert_eq!(form_encode("黑莲花"), "%E9%BB%91%E8%8E%B2%E8%8A%B1");
    }

    /// 实网基准：30 章，串行 vs 4 线程。
    #[test]
    #[ignore]
    fn bench_download_workers() {
        let mut cfg = Config::default();
        let tmp = std::env::temp_dir().join(format!("undl_bg_bench_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        cfg.save_path = tmp.to_string_lossy().to_string();

        let plan = prepare_biquga_plan(
            &cfg,
            "bg:2_2369",
            BookMeta {
                book_name: Some("诡秘之主".into()),
                ..Default::default()
            },
        )
        .expect("目录准备失败");
        let chosen: Vec<ChapterRef> = plan.chapters.iter().take(30).cloned().collect();

        for workers in [1usize, 4] {
            let dir = tmp.join(format!("w{workers}"));
            std::fs::create_dir_all(&dir).unwrap();
            let mut c = cfg.clone();
            c.biquga_workers = workers;
            c.save_path = dir.to_string_lossy().to_string();
            let mut manager =
                crate::book_parser::book_manager::BookManager::new(c.clone(), "bg:2_2369", "bench")
                    .expect("manager");
            let mut reporter = crate::download::progress::make_reporter(&c, &chosen, &chosen, None);
            let t = Instant::now();
            let r = download_biquga_into_manager(
                &c,
                "bg:2_2369",
                "bench",
                &mut manager,
                &chosen,
                &chosen,
                &mut reporter,
                None,
            )
            .expect("下载失败");
            let elapsed = t.elapsed();
            println!(
                "[bench] workers={} {}章 {:.2}s => {:.2} 章/s, {:.0} ms/章, 成功{} 失败{}",
                workers,
                chosen.len(),
                elapsed.as_secs_f32(),
                chosen.len() as f32 / elapsed.as_secs_f32(),
                elapsed.as_secs_f32() * 1000.0 / chosen.len() as f32,
                r.success,
                r.failed
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
    static LIVE_LOCK: Mutex<()> = Mutex::new(());

    /// 非重放实网 e2e：关键词/书目均与抓包不同（抓包为「黑莲花攻略手册」）。
    /// 运行：`cargo test biquga::tests::e2e_live_full_chain -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn e2e_live_full_chain() {
        let _guard = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let client = BiqugaClient::new(20).expect("init live client");

        // ① 搜索
        let t0 = Instant::now();
        let items = client.search("诡秘之主").expect("① 搜索失败");
        println!(
            "[①] 「诡秘之主」命中 {} 条，耗时 {:.2}s",
            items.len(),
            t0.elapsed().as_secs_f32()
        );
        assert!(!items.is_empty());
        let hit = items
            .iter()
            .find(|it| it.title == "诡秘之主" && it.aid == "2")
            .expect("目标书 bg:2_2369《诡秘之主》应在结果中");
        println!(
            "    bg:{}_{}《{}》/ {}",
            hit.aid, hit.bid, hit.title, hit.author
        );
        let book_id = format!("bg:{}_{}", hit.aid, hit.bid);

        // ② 详情 + 目录（站点限制：100 章 + 最新章节 12）
        let detail = client.book_detail(&book_id).expect("② 详情失败");
        assert_eq!(detail.book_name, "诡秘之主");
        assert_eq!(detail.author, "爱潜水的乌贼");
        assert_eq!(detail.finished, Some(true));
        println!(
            "[②] 详情《{}》/{}，站点可见章节 {} 条（前100+最新章节）",
            detail.book_name,
            detail.author,
            detail.chapters.len()
        );
        assert!(detail.chapters.len() >= 100);

        // ③ 正文（Base64 脚本形态 + 章内分页）
        let first = detail.chapters.first().expect("first chapter");
        let (title, body) = client
            .chapter_content(&book_id, &first.id)
            .expect("③ 正文失败");
        assert_eq!(title, "第一章 绯红");
        assert!(body.contains("周明瑞"), "正文应含实网首章文本");
        assert!(!body.contains("qsbs.bb"), "Base64 应已解码");
        assert!(!body.contains("请关闭浏览器阅读模式"));
        println!(
            "[③] 首章正文 {} 字（Base64 解码 + 分页合并）",
            body.chars().count()
        );

        // ④ 真实下载 2 章
        let mut cfg = Config::default();
        let tmp = std::env::temp_dir().join(format!("undl_biquga_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("创建临时目录");
        cfg.save_path = tmp.to_string_lossy().to_string();

        let plan = prepare_biquga_plan(
            &cfg,
            &book_id,
            BookMeta {
                book_name: Some(detail.book_name.clone()),
                ..Default::default()
            },
        )
        .expect("④ 下载计划失败");
        assert_eq!(plan.meta.author.as_deref(), Some("爱潜水的乌贼"));

        let chosen: Vec<ChapterRef> = plan.chapters.iter().take(2).cloned().collect();
        let pending = chosen.clone();
        let mut manager = crate::book_parser::book_manager::BookManager::new(
            cfg.clone(),
            &book_id,
            plan.meta.book_name.as_deref().unwrap_or("诡秘之主"),
        )
        .expect("BookManager 创建失败");
        let mut reporter = crate::download::progress::make_reporter(&cfg, &chosen, &pending, None);
        let result = download_biquga_into_manager(
            &cfg,
            &book_id,
            plan.meta.book_name.as_deref().unwrap_or("诡秘之主"),
            &mut manager,
            &chosen,
            &pending,
            &mut reporter,
            None,
        )
        .expect("④ 下载执行失败");
        println!(
            "[④] 下载 {} 章：成功 {}，失败 {}（临时目录 {}）",
            pending.len(),
            result.success,
            result.failed,
            tmp.display()
        );
        assert_eq!(result.success, 2);
        assert_eq!(result.failed, 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
