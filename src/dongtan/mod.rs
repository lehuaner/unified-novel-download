//! 东滩小说（www.dongtanxs.com，杰奇系采集站）小说源适配。
//!
//! 接口清单（依据 examples/东滩小说_解析结果 抓包 2026-11-06 + 非重放实网 e2e 2026-11-06）：
//! - 搜索：`GET /s.php?ie=utf-8&q=<UTF-8 urlencode>`，结果 `div.bookbox`：
//!   `h4.bookname > a`（`/dong{id}/`）、`div.cat`、`div.author`、`div.update`（最新章节）。
//!   页面标注「最多显示100条」，无翻页。
//! - 详情/目录：`GET /dong{id}/`（**详情路径无 `tan`**），`og:novel:*` meta +
//!   `dl` 内两个 `dt`：「最新章节列表」预览（尾部倒序）与「正文卷」（全量、正序）。
//! - 正文：`GET /dongtan{id}/{cid}.html`（**正文路径含 `tan`**），
//!   `div#content.showtxt`，段落以 `<br />` 分隔，无章内分页。
//!
//! 编码：UTF-8（meta 与响应一致）。站点为 HTTP（https 不可用）。

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
use crate::download::downloader::chunk_chapters;
use crate::download::models::{
    BookMeta, ChapterRef, DownloadPlan, DownloadResult, merge_meta_prefer_hint_name,
};

// ── 常量 ──────────────────────────────────────────────────────

const SITE_BASE: &str = "http://www.dongtanxs.com";

const SEARCH_MIN_GAP: Duration = Duration::from_secs(2);
const KEYWORD_MAX_CHARS: usize = 50;

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36 Edg/156.0.0.0";

// ── book_id 识别与规范化 ──────────────────────────────────────

/// `dt:` 前缀即东滩小说 book_id。
pub fn is_dongtan_book_id(book_id: &str) -> bool {
    book_id.starts_with("dt:")
}

pub fn strip_dt_prefix(book_id: &str) -> &str {
    book_id.strip_prefix("dt:").unwrap_or(book_id)
}

/// 从用户输入识别：`dt:80579` 或 dongtanxs.com 的书籍/章节页 URL。
pub fn normalize_book_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("dt:")
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return Some(format!("dt:{rest}"));
    }
    re_book_url()
        .captures(trimmed)
        .map(|c| format!("dt:{}", &c[1]))
}

fn parse_book_numeric_id(book_id: &str) -> Result<u64> {
    strip_dt_prefix(book_id)
        .parse::<u64>()
        .with_context(|| format!("非法东滩小说 book_id: {book_id:?}"))
}

fn sanitize_keyword(raw: &str) -> Result<String> {
    let kw = raw.trim();
    if kw.is_empty() {
        return Err(anyhow!("搜索关键字不能为空"));
    }
    if kw.chars().count() <= KEYWORD_MAX_CHARS {
        return Ok(kw.to_string());
    }
    let truncated: String = kw.chars().take(KEYWORD_MAX_CHARS).collect();
    info!(target: "dongtan", truncated = truncated.as_str(), "搜索关键字超长，已截断");
    Ok(truncated)
}

// ── 正则缓存 ──────────────────────────────────────────────────

fn re_book_url() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)dongtanxs\.com/(?:dongtan|dong)(\d+)/").expect("compile re_book_url")
    })
}

fn re_bookbox() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<div class="bookbox">([\s\S]*?)</div></div></div>"#)
            .expect("compile re_bookbox")
    })
}

fn re_search_title() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<h4 class="bookname"><a href="/dong(\d+)/">(.*?)</a></h4>"#)
            .expect("compile re_search_title")
    })
}

fn re_cat() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<div class="cat">分类：(.*?)</div>"#).expect("compile re_cat")
    })
}

fn re_author() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<div class="author">作者：(.*?)</div>"#).expect("compile re_author")
    })
}

fn re_latest() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<div class="update"><span>最新章节：</span><a[^>]*>(.*?)</a></div>"#)
            .expect("compile re_latest")
    })
}

fn re_cover() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?s)<img src="([^"]+)""#).expect("compile re_cover"))
}

fn re_meta_og() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<meta property="(og:[^"]+)" content="([^"]*)""#)
            .expect("compile re_meta_og")
    })
}

fn re_dt() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<dt>(.*?)</dt>").expect("compile re_dt"))
}

fn re_chapter_dd() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<dd><a href ="/dongtan(\d+)/(\d+)\.html">(.*?)</a></dd>"#)
            .expect("compile re_chapter_dd")
    })
}

fn re_chapter_title() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<h1>(.*?)</h1>").expect("compile re_chapter_title"))
}

fn re_br() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)<br\s*/?>").expect("compile re_br"))
}

fn re_script() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<script[\s\S]*?</script>").expect("compile re_script"))
}

fn re_page_suffix() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[\(（]\s*第?\s*\d+\s*页\s*[\)）]").expect("compile re_page_suffix")
    })
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

/// 规范化封面 URL：搜索/详情模板里的封面是站内相对路径，前端只接受绝对 URL。
fn normalize_cover_url(src: &str) -> Option<String> {
    let s = src.trim();
    if s.is_empty() || s.contains("nocover") {
        return None;
    }
    if let Some(rest) = s.strip_prefix("//") {
        return Some(format!("http:{rest}"));
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
pub struct DongtanSearchItem {
    pub book_id: u64,
    pub title: String,
    pub author: String,
    pub latest_chapter: String,
    pub category: String,
    pub cover_url: String,
}

#[derive(Debug, Clone)]
pub struct DongtanBookDetail {
    pub book_name: String,
    pub author: String,
    pub description: String,
    pub cover_url: String,
    pub category: String,
    pub finished: Option<bool>,
    pub chapters: Vec<ChapterRef>,
}

// ── 客户端 ────────────────────────────────────────────────────

pub struct DongtanClient {
    http: reqwest::blocking::Client,
    last_search: Mutex<Option<Instant>>,
}

impl DongtanClient {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        // 站点在 Cloudflare 后：缺少浏览器常规头时会被判定为机器人并返回
        // `403 Just a moment...`（实测 rustls 客户端；补齐 Accept/Accept-Language/
        // Connection 后放行，与 UA 无关）。
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
            ),
        );
        headers.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            reqwest::header::HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
        );
        headers.insert(
            reqwest::header::CONNECTION,
            reqwest::header::HeaderValue::from_static("keep-alive"),
        );
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(5)))
            .user_agent(USER_AGENT)
            .default_headers(headers)
            // Cloudflare 的机器人判定会识别 HTTP/1 头名大小写（小写头被挑战，
            // 标题式头与浏览器/curl 一致后放行；实测同一 IP/UA 差异仅此一项）。
            .http1_title_case_headers()
            .build()
            .context("build dongtan HTTP client")?;
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
                    let bytes = resp.bytes().context("读取东滩小说响应失败")?;
                    if status.is_success() {
                        return Ok(String::from_utf8_lossy(&bytes).into_owned());
                    }
                    return Err(anyhow!("东滩小说请求 HTTP {status}：{url}"));
                }
                Err(e) => {
                    debug!(target: "dongtan", url, attempt, error = %e, "东滩小说请求传输错误，将重试");
                    last_err =
                        Some(anyhow::Error::new(e).context(format!("东滩小说请求失败：{url}")));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("东滩小说请求失败：{url}")))
    }

    pub fn search(&self, raw_keyword: &str) -> Result<Vec<DongtanSearchItem>> {
        let keyword = sanitize_keyword(raw_keyword)?;
        self.throttle_gap();
        let url = format!("{SITE_BASE}/s.php");
        let resp = self
            .http
            .get(&url)
            .query(&[("ie", "utf-8"), ("q", keyword.as_str())])
            .send()
            .context("东滩小说搜索请求失败")?;
        let status = resp.status();
        let html = resp
            .bytes()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .context("东滩小说搜索响应读取失败")?;
        if !status.is_success() {
            return Err(anyhow!("东滩小说搜索 HTTP {status}"));
        }
        if !html.contains("bookbox") && !html.contains("没有") {
            return Err(anyhow!("东滩小说搜索返回异常（无结果区块）"));
        }
        Ok(parse_search_result(&html))
    }

    pub fn book_detail(&self, book_id: u64) -> Result<DongtanBookDetail> {
        let url = format!("{SITE_BASE}/dong{book_id}/");
        let html = self.get_text(&url)?;
        parse_book_detail(&html, book_id)
            .ok_or_else(|| anyhow!("东滩小说详情页解析失败（book_id={book_id}）"))
    }

    pub fn chapter_content(&self, book_id: u64, chapter_id: &str) -> Result<(String, String)> {
        let url = format!("{SITE_BASE}/dongtan{book_id}/{chapter_id}.html");
        let html = self.get_text(&url)?;
        parse_chapter_page(&html)
            .ok_or_else(|| anyhow!("东滩小说章节页解析失败（cid={chapter_id}）"))
    }
}

// ── 解析 ──────────────────────────────────────────────────────

fn parse_search_result(html: &str) -> Vec<DongtanSearchItem> {
    let mut items = Vec::new();
    for cap in re_bookbox().captures_iter(html) {
        let block = &cap[1];
        let Some(t) = re_search_title().captures(block) else {
            continue;
        };
        let Ok(book_id) = t[1].parse::<u64>() else {
            continue;
        };
        let title = clean_text(&t[2]);
        if title.is_empty() {
            continue;
        }
        items.push(DongtanSearchItem {
            book_id,
            title,
            author: re_author()
                .captures(block)
                .map(|c| clean_text(&c[1]))
                .unwrap_or_default(),
            latest_chapter: re_latest()
                .captures(block)
                .map(|c| clean_text(&c[1]))
                .unwrap_or_default(),
            category: re_cat()
                .captures(block)
                .map(|c| clean_text(&c[1]))
                .unwrap_or_default(),
            cover_url: re_cover()
                .captures(block)
                .and_then(|c| normalize_cover_url(&c[1]))
                .unwrap_or_default(),
        });
    }
    items
}

fn parse_book_detail(html: &str, book_id: u64) -> Option<DongtanBookDetail> {
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

    // 「正文卷」dt 之后为全量正序目录；缺失时退回解析全部 dd（去重）。
    let body_start = re_dt()
        .captures_iter(html)
        .find(|c| clean_text(&c[1]).contains("正文卷"))
        .map(|c| c.get(0).map(|m| m.end()).unwrap_or(0))
        .unwrap_or(0);
    let bid = book_id.to_string();
    let mut chapters = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in re_chapter_dd().captures_iter(html) {
        if body_start > 0 && c.get(0).map(|m| m.start()).unwrap_or(0) < body_start {
            continue;
        }
        if c[1] != bid {
            continue;
        }
        let title = clean_text(&c[3]);
        if title.is_empty() {
            continue;
        }
        let id = c[2].to_string();
        if seen.insert(id.clone()) {
            chapters.push(ChapterRef { id, title });
        }
    }
    if chapters.is_empty() {
        return None;
    }
    Some(DongtanBookDetail {
        book_name,
        author,
        description,
        cover_url,
        category,
        finished,
        chapters,
    })
}

fn parse_chapter_page(html: &str) -> Option<(String, String)> {
    let title = re_chapter_title()
        .captures(html)
        .map(|c| clean_text(&c[1]))
        .map(|s| re_page_suffix().replace_all(&s, "").trim().to_string())
        .unwrap_or_default();

    let pos = html.find(r#"id="content""#)?;
    let open_end = html[pos..].find('>')? + pos + 1;
    let seg_end = html[open_end..]
        .find("</div>")
        .map(|p| p + open_end)
        .unwrap_or(html.len());
    let seg = re_script().replace_all(&html[open_end..seg_end], "");
    let mut paragraphs = Vec::new();
    for line in re_br().split(&seg) {
        let text = clean_text(line);
        if text.is_empty() || is_noise_line(&text) {
            continue;
        }
        paragraphs.push(format!("<p>{text}</p>"));
    }
    if paragraphs.is_empty() {
        return None;
    }
    Some((title, paragraphs.join("\n")))
}

fn is_noise_line(s: &str) -> bool {
    s.contains("东滩小说小说推荐阅读")
        || s.contains("请收藏本站")
        || s == "东滩小说"
        || s.contains("（本章完）")
}

// ── 下载计划 ──────────────────────────────────────────────────

pub fn prepare_dongtan_plan(
    config: &Config,
    book_id: &str,
    meta_hint: BookMeta,
) -> Result<DownloadPlan> {
    info!(target: "download", book_id, "准备东滩小说下载计划");

    let bid = parse_book_numeric_id(book_id)?;
    let timeout = config.request_timeout.max(5);
    let client = DongtanClient::new(timeout).context("init DongtanClient")?;

    let detail = client
        .book_detail(bid)
        .context("fetch dongtan book detail")?;
    let chapters = detail.chapters.clone();
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

    if let Some(cover_url) = merged.cover_url.as_ref() {
        let folder = book_paths::book_folder_path(config, book_id, merged.book_name.as_deref());
        let _ = std::fs::create_dir_all(&folder);
        let cover_path = book_paths::canonical_cover_path(&folder, "jpg");
        if !cover_path.exists()
            && let Err(e) = download_cover(&client.http, cover_url, &cover_path)
        {
            debug!(target: "download", error = %e, "东滩小说封面下载失败（忽略）");
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
pub fn download_dongtan_into_manager(
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
    let client = Arc::new(DongtanClient::new(timeout).context("init DongtanClient")?);

    let worker_count = config.dongtan_workers.clamp(1, 2);
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
                        Ok((_title, html)) if !html.trim().is_empty() => {
                            contents.push((ch.id.clone(), html));
                        }
                        Ok(_) => {
                            warn!(target: "download", chapter_id = %ch.id, "东滩小说章节正文为空：{}", ch.title);
                        }
                        Err(e) => {
                            warn!(target: "download", chapter_id = %ch.id, error = %e, "东滩小说章节下载失败：{}", ch.title);
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
        "东滩小说下载完成：{} ({} 章，成功 {}，失败 {})",
        book_name,
        pending_chapters.len(),
        result.success,
        result.failed
    );
    Ok(result)
}

// ── 搜索（Web UI / TUI）───────────────────────────────────────

pub fn search_items(
    client: &DongtanClient,
    keyword: &str,
    page: u32,
) -> Result<(Vec<Value>, bool)> {
    if page > 1 {
        return Ok((Vec::new(), false));
    }
    let items = client.search(keyword)?;
    let out = items
        .iter()
        .map(|it| {
            json!({
                "book_id": format!("dt:{}", it.book_id),
                "title": it.title,
                "author": it.author,
                "latest_chapter": it.latest_chapter,
                "raw": Value::Null,
                "description": Value::Null,
                "cover_url": (!it.cover_url.is_empty()).then(|| it.cover_url.clone()),
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

pub struct DongtanProvider;

impl crate::provider::BookProvider for DongtanProvider {
    fn id(&self) -> &'static str {
        "dongtan"
    }

    fn display_name(&self) -> &'static str {
        "东滩小说"
    }

    fn owns_book_id(&self, book_id: &str) -> bool {
        is_dongtan_book_id(book_id)
    }

    fn normalize_book_input(&self, input: &str) -> Option<String> {
        normalize_book_input(input)
    }

    fn search(
        &self,
        opts: &crate::provider::SearchOptions,
    ) -> anyhow::Result<(Vec<serde_json::Value>, bool)> {
        let client = DongtanClient::new(opts.timeout_secs)?;
        search_items(&client, &opts.keyword, opts.page)
    }

    fn prepare_plan(
        &self,
        config: &crate::base_system::context::Config,
        book_id: &str,
        meta_hint: crate::download::models::BookMeta,
    ) -> anyhow::Result<crate::download::models::DownloadPlan> {
        prepare_dongtan_plan(config, book_id, meta_hint)
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
        download_dongtan_into_manager(
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
        let bid = parse_book_numeric_id(book_id).ok()?;
        let client = DongtanClient::new(timeout_secs).ok()?;
        let detail = client.book_detail(bid).ok()?;
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
        let client = DongtanClient::new(crate::provider::UPDATE_FETCH_TIMEOUT_SECS).ok()?;
        let bid = strip_dt_prefix(book_id).parse::<u64>().ok()?;
        let detail = client.book_detail(bid).ok()?;
        (!detail.chapters.is_empty()).then_some((detail.chapters.len(), detail.finished))
    }
}

// ── 离线解析回归 ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/src/dongtan/testdata/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("read fixture")
    }

    #[test]
    fn normalize_book_input_variants() {
        assert_eq!(
            normalize_book_input("dt:80579").as_deref(),
            Some("dt:80579")
        );
        assert_eq!(
            normalize_book_input("http://www.dongtanxs.com/dong80579/").as_deref(),
            Some("dt:80579")
        );
        assert_eq!(
            normalize_book_input("http://www.dongtanxs.com/dongtan80579/43951572.html").as_deref(),
            Some("dt:80579")
        );
    }

    #[test]
    fn search_result_matches_capture() {
        let html = fixture("search_result.html");
        let items = parse_search_result(&html);
        assert!(!items.is_empty());
        let first = &items[0];
        assert_eq!(first.book_id, 19181);
        assert_eq!(first.title, "大佬不断作死后成了白月光");
        assert_eq!(first.author, "小柚皮皮");
        assert_eq!(first.category, "科幻灵异");
        assert!(first.latest_chapter.contains("第2079章"));
    }

    #[test]
    fn book_detail_collects_volume_chapters() {
        let html = fixture("book_detail.html");
        let d = parse_book_detail(&html, 80579).expect("detail parsed");
        assert_eq!(d.book_name, "（穿书）黑莲花攻略手册");
        assert_eq!(d.author, "白羽摘雕弓");
        assert_eq!(d.finished, Some(false));
        assert_eq!(d.chapters.len(), 122);
        assert_eq!(d.chapters[0].id, "43951572");
        assert_eq!(d.chapters[0].title, "1 替嫁（一）修");
        assert_eq!(d.chapters[121].title, "122 崩坏脑洞2.0——十五年（下）");
    }

    #[test]
    fn chapter_content_matches_capture() {
        let html = fixture("chapter_43951572.html");
        let (title, body) = parse_chapter_page(&html).expect("chapter parsed");
        assert_eq!(title, "1 替嫁（一）修");
        assert!(body.contains("红衣少女坐在妆台前"));
        assert!(!body.contains("<script"));
        assert!(!body.contains("东滩小说小说推荐阅读"));
    }

    /// 实网基准：同一本书 50 章，串行 vs 4 线程。用于量化下载吞吐。
    /// 运行：`cargo test dongtan::tests::bench_download_workers -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_download_workers() {
        let mut cfg = Config::default();
        let tmp = std::env::temp_dir().join(format!("undl_dt_bench_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        cfg.save_path = tmp.to_string_lossy().to_string();

        let plan = prepare_dongtan_plan(
            &cfg,
            "dt:50235",
            BookMeta {
                book_name: Some("诡秘之主".into()),
                ..Default::default()
            },
        )
        .expect("目录准备失败");
        let chosen: Vec<ChapterRef> = plan.chapters.iter().take(30).cloned().collect();

        for workers in [1usize, 2] {
            let dir = tmp.join(format!("w{workers}"));
            std::fs::create_dir_all(&dir).unwrap();
            let mut c = cfg.clone();
            c.dongtan_workers = workers;
            c.save_path = dir.to_string_lossy().to_string();
            let mut manager =
                crate::book_parser::book_manager::BookManager::new(c.clone(), "dt:50235", "bench")
                    .expect("manager");
            let mut reporter = crate::download::progress::make_reporter(&c, &chosen, &chosen, None);
            let t = Instant::now();
            let r = download_dongtan_into_manager(
                &c,
                "dt:50235",
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
    /// 运行：`cargo test dongtan::tests::e2e_live_full_chain -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn e2e_live_full_chain() {
        let _guard = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let client = DongtanClient::new(20).expect("init live client");

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
            .find(|it| it.title == "诡秘之主" && it.author == "爱潜水的乌贼")
            .expect("目标书《诡秘之主》应在结果中");
        println!("    dt:{}《{}》/ {}", hit.book_id, hit.title, hit.author);
        let book_id = format!("dt:{}", hit.book_id);

        // ② 详情 + 全量目录
        let detail = client.book_detail(hit.book_id).expect("② 详情失败");
        assert_eq!(detail.book_name, "诡秘之主");
        assert_eq!(detail.author, "爱潜水的乌贼");
        println!(
            "[②] 详情《{}》/{}，目录 {} 章",
            detail.book_name,
            detail.author,
            detail.chapters.len()
        );
        assert!(detail.chapters.len() > 1000, "长篇目录应完整展开");

        // ③ 正文
        let first = detail.chapters.first().expect("first chapter");
        let (title, body) = client
            .chapter_content(hit.book_id, &first.id)
            .expect("③ 正文失败");
        assert_eq!(title, "第一章 绯红");
        assert!(body.contains("周明瑞"), "正文应含实网首章文本");
        assert!(!body.contains("东滩小说小说推荐阅读"));
        println!("[③] 首章正文 {} 字", body.chars().count());

        // ④ 真实下载 2 章
        let mut cfg = Config::default();
        let tmp = std::env::temp_dir().join(format!("undl_dongtan_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("创建临时目录");
        cfg.save_path = tmp.to_string_lossy().to_string();

        let plan = prepare_dongtan_plan(
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
        let result = download_dongtan_into_manager(
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
