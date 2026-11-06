//! 官术网（guanshuya.net，杰奇系模板）小说源适配。
//!
//! 接口清单（依据 examples/官术网_解析结果 抓包，2026-11-06 实测）：
//! - 搜索：`GET https://www.guanshuya.net/search.php?keyword=<关键词>`，结果 `div.item`。
//!   **该路径受 Cloudflare 托管挑战保护**，必须携带浏览器生成的 `cf_clearance` Cookie
//!   与同一浏览器 UA（见 config.yml 的 `guanshuya_cookie` / `guanshuya_user_agent`）。
//! - 详情/目录/正文：优先 `www` + 已配置的 Cookie（与搜索同一 cf_clearance）；
//!   无 Cookie 时回退移动站 `m.guanshuya.net`（移动 UA）。实测移动站对 curl 开放，
//!   但 reqwest 的 TLS 指纹可能被 Cloudflare 挑战，因此正常链路统一走 www+Cookie。
//! - 详情：`/html/{sid}/{bid}/`，`og:novel:*` + `#intro` + 目录 `option` 分页。
//! - 目录：`/html/{sid}/{bid}_{page}/`，每页 20 章；页面同时含「最新章节」预览块，需跳过。
//! - 正文：`/html/{sid}/{bid}/{cid}.html`，章内分页 `{cid}_{n}.html`；
//!   容器 `div#chaptercontent`，段落以 `<br>` 分隔。
//!
//! 正文尾部固定推广语（实测 4 页样本完全一致）：
//! `《<a ...>书名</a>》情节跌宕起伏、扣人心弦，是一本情节与文笔俱佳的女生频道，`
//! `笔趣阁转载收集...最新章节。`
//! 以及 `本站所有小说为转载作品…` / `Copyright …` / `欢迎收藏本站…`，统一剔除。
//!
//! 站点按固定长度切页，句末无标点时下一页首段与上一页末段直接拼接
//! （与 hen0 的处理一致：`琼` + `瑶”。`、`但嫁` + `娶之事…`）。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

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

/// 搜索走 www（受 Cloudflare 保护，必须带 Cookie）。
const SEARCH_BASE: &str = "https://www.guanshuya.net";
/// 无 Cookie 时的兜底读取站（移动 UA）。
const READ_BASE: &str = "https://m.guanshuya.net";

/// 与抓包 Cookie 匹配的默认 UA（Edge 156 / Windows）。
/// 若用户从其他浏览器复制 Cookie，需同时配置 `guanshuya_user_agent`。
const DEFAULT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36 Edg/156.0.0.0";
/// 无 Cookie 时走移动站，使用移动 UA（桌面 UA 会被 Cloudflare 判定为异常）。
const MOBILE_UA: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36";

const KEYWORD_MIN_CHARS: usize = 2;
const KEYWORD_MAX_CHARS: usize = 50;
const CHAPTER_MAX_PAGES: u32 = 30;
const TOC_MAX_PAGES: u32 = 500;
const TOC_PAGE_GAP: Duration = Duration::from_millis(120);
const CHAPTER_PAGE_WORKERS: usize = 4;

// ── book_id 识别与规范化 ──────────────────────────────────────

/// `gs:` 前缀即官术网 book_id。
pub fn is_guanshuya_book_id(book_id: &str) -> bool {
    book_id.starts_with("gs:")
}

/// 去掉 `gs:` 前缀，返回纯数字部分（无前缀则原样返回）。
pub fn strip_gs_prefix(book_id: &str) -> &str {
    book_id.strip_prefix("gs:").unwrap_or(book_id)
}

/// 从用户输入识别官术网 book_id：`gs:108891` 前缀，或 guanshuya.net 的书籍/章节页 URL。
/// 返回带前缀的规范化 book_id（如 `gs:108891`），非本源输入返回 None。
pub fn normalize_book_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("gs:")
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return Some(format!("gs:{rest}"));
    }
    re_book_url()
        .captures(trimmed)
        .map(|c| format!("gs:{}", &c[1]))
}

/// 从 `gs:` book_id 提取纯数字 ID。
fn parse_book_id(book_id: &str) -> Result<u64> {
    let rest = strip_gs_prefix(book_id);
    rest.parse::<u64>()
        .with_context(|| format!("非法官术网 book_id: {book_id:?}"))
}

/// 站点 URL 中的 `{sid}` 固定为 `bid / 1000`（抓包样本：108891→108、71247→71、256→0）。
fn sid_of(bid: u64) -> u64 {
    bid / 1000
}

fn sanitize_keyword(raw: &str) -> Result<String> {
    let kw = raw.trim();
    if kw.chars().count() < KEYWORD_MIN_CHARS {
        return Err(anyhow!("搜索关键字过短（至少 {KEYWORD_MIN_CHARS} 个字符）"));
    }
    let mut out: String = kw.chars().take(KEYWORD_MAX_CHARS).collect();
    if out.chars().count() < kw.chars().count() {
        info!(
            target: "guanshuya",
            raw_chars = kw.chars().count(),
            truncated_chars = out.chars().count(),
            "搜索关键字超过站点上限，已截断"
        );
    }
    out = out.trim().to_string();
    Ok(out)
}

// ── 正则缓存 ──────────────────────────────────────────────────

fn re_book_url() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)guanshuya\.net/html/\d+/(\d+)(?:/|_|$)").expect("compile gs re_book_url")
    })
}

fn re_search_item() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<div class="item">(.*?)<div class="clear">"#)
            .expect("compile gs re_search_item")
    })
}

fn re_item_book() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"/html/(\d+)/(\d+)/"#).expect("compile gs re_item_book"))
}

fn re_item_title() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<dt>\s*<a[^>]*title="([^"]*)""#).expect("compile gs re_item_title")
    })
}

fn re_item_author() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<a href="/author/\d+/" title="([^"]*)""#).expect("compile gs re_item_author")
    })
}

fn re_item_cover() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"data-original="([^"]+)""#).expect("compile gs re_item_cover"))
}

fn re_item_desc() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?s)<dd>(.*?)</dd>"#).expect("compile gs re_item_desc"))
}

fn re_item_words() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<em class="orange[^"]*">([^<]+)</em>"#).expect("compile gs re_item_words")
    })
}

fn re_chapter_title() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<h1 class="bookname">(.*?)</h1>"#).expect("compile gs re_chapter_title")
    })
}

fn re_page_marker() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[（(](\d+)\s*/\s*(\d+)[）)]").expect("compile gs re_page_marker")
    })
}

fn re_promo_tail() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?s)《\s*<a\s+href="[^"]*guanshuya\.net/html/[^"]*"[^>]*>.*?</a>\s*》情节跌宕起伏.*$"#,
        )
        .expect("compile gs re_promo_tail")
    })
}

fn re_toc_page_option() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"/html/\d+/\d+_(\d+)/"#).expect("compile gs re_toc_page_option"))
}

fn re_option_text() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<option[^>]*>(.*?)</option>"#).expect("compile gs re_option_text")
    })
}

fn re_option_chapter_number() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"第\s*(\d+)\s*章").expect("compile gs re_option_chapter_number"))
}

fn re_script_or_style() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<script[^>]*>.*?</script>|<style[^>]*>.*?</style>")
            .expect("compile gs re_script_or_style")
    })
}

// ── HTML 工具 ─────────────────────────────────────────────────

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
    out
}

fn clean_text(s: &str) -> String {
    let unescaped = crate::book_parser::html_utils::unescape_basic_entities(s);
    strip_tags(unescaped.as_ref())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn og_value(html: &str, prop: &str) -> Option<String> {
    let marker = format!(r#"<meta property="{prop}" content=""#);
    let start = html.find(&marker)? + marker.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    Some(clean_text(&rest[..end]))
}

fn parse_word_count(text: &str) -> Option<usize> {
    let s = text.replace([',', '，'], "");
    let s = s.trim_end_matches('字');
    if let Some(v) = s.strip_suffix('万') {
        let n: f64 = v.trim().parse().ok()?;
        Some((n * 10_000.0).round() as usize)
    } else {
        let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
        digits.parse::<usize>().ok().filter(|n| *n > 0)
    }
}

fn parse_finished(status: &str) -> Option<bool> {
    if status.contains('完') {
        Some(true)
    } else if status.contains("连载") || status.contains("连在") {
        Some(false)
    } else {
        None
    }
}

/// 去掉正文尾部固定推广语（4 页样本 + 跨章样本一致）。
fn strip_chapter_promo(raw: &str) -> String {
    let mut s = raw.to_string();
    if let Some(m) = re_promo_tail().find(&s) {
        s.truncate(m.start());
    }
    for marker in [
        "本站所有小说为转载作品",
        "Copyright &copy;",
        "Copyright ©",
        "欢迎收藏本站",
    ] {
        if let Some(pos) = s.find(marker) {
            s.truncate(pos);
        }
    }
    s
}

/// 将 HTML 正文转成纯文本段落：`<br>` 与 `</p>` 都视为换行。
fn html_to_paragraphs(raw: &str) -> Vec<String> {
    let no_script = re_script_or_style().replace_all(raw, "");
    let normalized = no_script
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("<br>", "\n")
        .replace("<br >", "\n")
        .replace("</p>", "\n")
        .replace("<p>", "\n")
        .replace("<p ", "\n<p ");
    let text = crate::book_parser::html_utils::unescape_basic_entities(&normalized);
    let text = strip_tags(text.as_ref());
    let mut out = Vec::new();
    for line in text.split('\n') {
        let cleaned = line
            .replace('\u{3000}', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let cleaned = cleaned.trim();
        if !cleaned.is_empty() {
            out.push(cleaned.to_string());
        }
    }
    out
}

fn ends_with_terminal(s: &str) -> bool {
    let t = s.trim_end_matches(['”', '」', '』', '）', ')', '…', ' ']);
    t.chars()
        .last()
        .map(|c| matches!(c, '。' | '！' | '？' | '；' | '.' | '!' | '?' | ';'))
        .unwrap_or(false)
}

/// 追加一页：上一页末段没有句末标点时，说明句子跨页截断，直接拼接。
fn append_page(paragraphs: &mut Vec<String>, page: Vec<String>, join_first: bool) {
    let mut first = true;
    for para in page {
        if first
            && join_first
            && let Some(last) = paragraphs.last_mut()
            && !ends_with_terminal(last)
        {
            last.push_str(&para);
        } else {
            paragraphs.push(para);
        }
        first = false;
    }
}

// ── 数据结构 ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GuanshuyaSearchItem {
    pub book_id: u64,
    pub title: String,
    pub author: String,
    pub description: String,
    pub cover_url: String,
    pub word_count: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct GuanshuyaBookDetail {
    pub book_name: String,
    pub author: String,
    pub description: String,
    pub cover_url: String,
    pub category: String,
    pub finished: Option<bool>,
    pub word_count: Option<usize>,
    /// 目录分页数（每页 20 章）。
    pub toc_pages: u32,
    /// 从目录 option 文本推断的总章节数（如「第121章 - 第122章」→ 122）。
    pub chapter_count: Option<u32>,
}

/// 单页章节解析结果。
struct ChapterPage {
    title: String,
    #[allow(dead_code)]
    page_no: Option<u32>,
    total_pages: Option<u32>,
    paragraphs: Vec<String>,
}

// ── 客户端 ────────────────────────────────────────────────────

/// 进程级 Cookie 兜底：`fetch_update_state` / `fetch_chapter_count` / `preview_summary`
/// 的 trait 签名不带 Config，搜索或启动时注入一次，后续轻量读取复用同一 Cookie。
static DEFAULT_COOKIE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn cookie_slot() -> &'static Mutex<Option<String>> {
    DEFAULT_COOKIE.get_or_init(|| Mutex::new(None))
}

/// 启动/保存配置时注入 Cookie（留空则清空）。
pub fn set_default_cookie(cookie: &str) {
    let value = cookie.trim();
    let mut slot = cookie_slot().lock().unwrap_or_else(|e| e.into_inner());
    *slot = (!value.is_empty()).then(|| value.to_string());
}

fn default_cookie() -> Option<String> {
    cookie_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub struct GuanshuyaClient {
    http: reqwest::blocking::Client,
    /// 无 Cookie 时读取移动站用的客户端（移动 UA）。
    read_http: reqwest::blocking::Client,
    cookie: Option<String>,
    user_agent: String,
}

impl GuanshuyaClient {
    pub fn new(timeout_secs: u64, cookie: Option<&str>, user_agent: Option<&str>) -> Result<Self> {
        let ua = user_agent
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_UA)
            .to_string();
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(5)))
            .user_agent(&ua)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .context("build guanshuya HTTP client")?;
        let cookie = cookie
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if cookie.is_some() {
            set_default_cookie(cookie.as_deref().unwrap_or(""));
        }
        let cookie = cookie.or_else(default_cookie);
        let read_http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(5)))
            .user_agent(MOBILE_UA)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .context("build guanshuya read HTTP client")?;
        Ok(Self {
            http,
            read_http,
            cookie,
            user_agent: ua,
        })
    }

    fn is_cloudflare_challenge(body: &str) -> bool {
        body.contains("Just a moment") || body.contains("cf-chl") || body.contains("__cf_chl")
    }

    fn get_search_text(&self, url: &str) -> Result<String> {
        let cookie = self.cookie.as_deref().ok_or_else(|| {
            anyhow!(
                "官术网搜索需要 Cloudflare Cookie：请在 config.yml 填写 guanshuya_cookie \
                 （浏览器中的 cf_clearance），并确保 guanshuya_user_agent 与获取 Cookie 的浏览器一致"
            )
        })?;
        let resp = self
            .http
            .get(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::REFERER, format!("{SEARCH_BASE}/"))
            .send()
            .context("官术网搜索请求失败")?;
        let status = resp.status();
        let body = resp.text().context("官术网搜索响应读取失败")?;
        if Self::is_cloudflare_challenge(&body) {
            return Err(anyhow!(
                "官术网搜索被 Cloudflare 拦截（HTTP {status}）：请更新 config.yml 的 \
                 guanshuya_cookie / guanshuya_user_agent（需与浏览器一致）"
            ));
        }
        if !status.is_success() {
            return Err(anyhow!("官术网搜索返回 HTTP {status}"));
        }
        Ok(body)
    }

    /// 详情/目录/正文：有 Cookie 时走 www（复用 cf_clearance）；否则走移动站（移动 UA）。
    fn get_read_text(&self, path: &str) -> Result<String> {
        let use_www = self.cookie.is_some();
        let url = if use_www {
            format!("{SEARCH_BASE}{path}")
        } else {
            format!("{READ_BASE}{path}")
        };
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..2u32 {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(250));
            }
            let mut req = if use_www {
                self.http.get(&url)
            } else {
                self.read_http.get(&url)
            };
            if use_www {
                req = req.header(reqwest::header::REFERER, format!("{SEARCH_BASE}/"));
                if let Some(cookie) = self.cookie.as_deref() {
                    req = req.header(reqwest::header::COOKIE, cookie);
                }
            } else {
                req = req.header(reqwest::header::REFERER, format!("{READ_BASE}/"));
            }
            match req.send() {
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().context("官术网响应读取失败")?;
                    if Self::is_cloudflare_challenge(&body) {
                        return Err(anyhow!(
                            "官术网页面被 Cloudflare 拦截（{url}）：请更新 guanshuya_cookie / guanshuya_user_agent"
                        ));
                    }
                    if status.is_success() {
                        return Ok(body);
                    }
                    return Err(anyhow!("官术网请求 HTTP {status}：{url}"));
                }
                Err(e) => {
                    debug!(target: "guanshuya", url, attempt, error = %e, "官术网传输错误，将重试");
                    last_err = Some(
                        anyhow::Error::new(e)
                            .context(format!("官术网请求失败：{url}（UA={}）", self.user_agent)),
                    );
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("官术网请求失败：{url}")))
    }

    /// 搜索（www，Cloudflare Cookie）。
    pub fn search(&self, raw_keyword: &str) -> Result<Vec<GuanshuyaSearchItem>> {
        let keyword = sanitize_keyword(raw_keyword)?;
        let mut url = reqwest::Url::parse(&format!("{SEARCH_BASE}/search.php"))
            .context("build guanshuya search url")?;
        url.query_pairs_mut().append_pair("keyword", &keyword);
        let html = self.get_search_text(url.as_str())?;
        Ok(parse_search_result(&html))
    }

    /// 详情（有 Cookie 走 www，否则回退移动站）。
    pub fn book_detail(&self, bid: u64) -> Result<GuanshuyaBookDetail> {
        let path = format!("/html/{}/{bid}/", sid_of(bid));
        let html = self.get_read_text(&path)?;
        parse_book_detail(&html, bid).ok_or_else(|| anyhow!("官术网详情页解析失败（bid={bid}）"))
    }

    /// 目录单页。
    fn toc_page(&self, bid: u64, page: u32) -> Result<Vec<ChapterRef>> {
        let path = format!("/html/{}/{bid}_{page}/", sid_of(bid));
        let html = self.get_read_text(&path)?;
        let chapters = parse_toc_page(&html, bid);
        if chapters.is_empty() {
            return Err(anyhow!(
                "官术网目录第 {page} 页为空（bid={bid}），可能已失效"
            ));
        }
        Ok(chapters)
    }

    /// 全量目录：按详情页 option 页数逐页拉取。
    pub fn fetch_all_chapters(
        &self,
        bid: u64,
        detail: &GuanshuyaBookDetail,
    ) -> Result<Vec<ChapterRef>> {
        let pages = detail.toc_pages.clamp(1, TOC_MAX_PAGES);
        let mut chapters = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for page in 1..=pages {
            if page > 1 {
                thread::sleep(TOC_PAGE_GAP);
            }
            let page_chapters = self.toc_page(bid, page)?;
            for ch in page_chapters {
                if seen.insert(ch.id.clone()) {
                    chapters.push(ch);
                }
            }
        }
        if chapters.is_empty() {
            return Err(anyhow!("官术网目录为空（bid={bid}）"));
        }
        Ok(chapters)
    }

    /// 单章正文：抓第 1 页，按标题 `（1/N）` 并行抓剩余页；跨页断句直接拼接。
    pub fn chapter_content(&self, bid: u64, chapter_id: &str) -> Result<(String, String)> {
        let first_path = format!("/html/{}/{bid}/{chapter_id}.html", sid_of(bid));
        let first_html = self.get_read_text(&first_path)?;
        let first = parse_chapter_page(&first_html)
            .ok_or_else(|| anyhow!("官术网章节页解析失败（chapter_id={chapter_id}）"))?;
        if first.paragraphs.is_empty() {
            return Err(anyhow!("官术网章节正文为空（chapter_id={chapter_id}）"));
        }
        let total = first.total_pages.unwrap_or(1).min(CHAPTER_MAX_PAGES);
        let mut pages = vec![first.paragraphs.clone()];
        if total > 1 {
            let rest = (2..=total).collect::<Vec<_>>();
            for chunk in rest.chunks(CHAPTER_PAGE_WORKERS) {
                let results: Vec<Option<Vec<String>>> = thread::scope(|scope| {
                    let handles: Vec<_> = chunk
                        .iter()
                        .map(|p| {
                            let path = format!("/html/{}/{bid}/{chapter_id}_{p}.html", sid_of(bid));
                            scope.spawn(move || {
                                self.get_read_text(&path)
                                    .ok()
                                    .and_then(|html| parse_chapter_page(&html))
                                    .map(|page| page.paragraphs)
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().ok().flatten())
                        .collect()
                });
                for page in results {
                    pages.push(page.ok_or_else(|| {
                        anyhow!("官术网章内分页抓取失败（chapter_id={chapter_id}）")
                    })?);
                }
            }
        }

        let mut paragraphs = pages.remove(0);
        for page in pages {
            append_page(&mut paragraphs, page, true);
        }
        if paragraphs.is_empty() {
            return Err(anyhow!("官术网章节正文为空（chapter_id={chapter_id}）"));
        }
        let title = first.title;
        Ok((title, paragraphs.join("\n")))
    }

    fn download_cover(&self, url: &str, path: &std::path::Path) -> Result<()> {
        let resp = self.http.get(url).send().context("cover request")?;
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
}

// ── 解析 ──────────────────────────────────────────────────────

pub fn parse_search_result(html: &str) -> Vec<GuanshuyaSearchItem> {
    let mut out = Vec::new();
    for block in re_search_item().captures_iter(html) {
        let seg = &block[1];
        let Some(book) = re_item_book().captures(seg) else {
            continue;
        };
        let book_id = book[2].parse::<u64>().unwrap_or(0);
        if book_id == 0 {
            continue;
        }
        let title = re_item_title()
            .captures(seg)
            .map(|c| clean_text(&c[1]))
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let author = re_item_author()
            .captures(seg)
            .map(|c| clean_text(&c[1]))
            .unwrap_or_default();
        let description = re_item_desc()
            .captures(seg)
            .map(|c| clean_text(&c[1]))
            .unwrap_or_default();
        let cover_url = re_item_cover()
            .captures(seg)
            .map(|c| c[1].to_string())
            .filter(|s| !s.contains("nocover"))
            .map(|s| {
                if s.starts_with("http") {
                    s
                } else {
                    format!("{SEARCH_BASE}{s}")
                }
            })
            .unwrap_or_default();
        let word_count = re_item_words()
            .captures(seg)
            .and_then(|c| parse_word_count(&c[1]));
        out.push(GuanshuyaSearchItem {
            book_id,
            title,
            author,
            description,
            cover_url,
            word_count,
        });
    }
    out
}

fn parse_book_detail(html: &str, _bid: u64) -> Option<GuanshuyaBookDetail> {
    let book_name = og_value(html, "og:novel:book_name").or_else(|| {
        Regex::new(r#"(?s)<div id="info">\s*<h1>(.*?)</h1>"#)
            .ok()
            .and_then(|re| re.captures(html))
            .map(|c| clean_text(&c[1]))
    })?;
    if book_name.is_empty() {
        return None;
    }
    let author = og_value(html, "og:novel:author").unwrap_or_default();
    let category = og_value(html, "og:novel:category").unwrap_or_default();
    let cover_url = og_value(html, "og:image")
        .or_else(|| {
            Regex::new(r#"data-original="([^"]+)""#)
                .ok()
                .and_then(|re| re.captures(html))
                .map(|c| c[1].to_string())
        })
        .filter(|s| !s.contains("nocover"))
        .map(|s| {
            if s.starts_with("http") {
                s
            } else {
                format!("{SEARCH_BASE}{s}")
            }
        })
        .unwrap_or_default();

    let status = Regex::new(r"状态：([^<]+)")
        .ok()
        .and_then(|re| re.captures(html))
        .map(|c| clean_text(&c[1]))
        .unwrap_or_default();
    let word_count = Regex::new(r"字数：([^<]+)")
        .ok()
        .and_then(|re| re.captures(html))
        .and_then(|c| parse_word_count(&c[1]));

    let description = html
        .find(r#"<div id="intro">"#)
        .and_then(|pos| {
            let start = pos + r#"<div id="intro">"#.len();
            let rest = &html[start..];
            let end = rest.find("</div>")?;
            Some(clean_text(rest[..end].trim_start_matches("小说简介：")))
        })
        .unwrap_or_default();

    let toc_pages = re_toc_page_option()
        .captures_iter(html)
        .filter_map(|c| c[1].parse::<u32>().ok())
        .max()
        .unwrap_or(1)
        .max(1);
    let chapter_count = re_option_text()
        .captures_iter(html)
        .filter_map(|c| {
            re_option_chapter_number()
                .captures_iter(&c[1])
                .filter_map(|n| n[1].parse::<u32>().ok())
                .max()
        })
        .max();

    Some(GuanshuyaBookDetail {
        book_name,
        author,
        description,
        cover_url,
        category,
        finished: parse_finished(&status),
        word_count,
        toc_pages,
        chapter_count,
    })
}

fn parse_toc_page(html: &str, bid: u64) -> Vec<ChapterRef> {
    // 页面顶部有「最新章节」预览块，第二个 dt 才是完整目录；只解析其后内容。
    let seg = html
        .split_once("章节目录</dt>")
        .map(|(_, rest)| rest)
        .unwrap_or(html);
    let re = Regex::new(&format!(
        r#"(?s)<a[^>]*href="[^"]*/html/\d+/{bid}/(\d+)\.html"[^>]*>\s*<dd>(.*?)</dd>"#
    ))
    .expect("compile gs toc link");
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for cap in re.captures_iter(seg) {
        let id = cap[1].to_string();
        let title = clean_text(&cap[2]);
        if id.is_empty() || title.is_empty() {
            continue;
        }
        if seen.insert(id.clone()) {
            out.push(ChapterRef { id, title });
        }
    }
    out
}

fn parse_chapter_page(html: &str) -> Option<ChapterPage> {
    let title = re_chapter_title()
        .captures(html)
        .map(|c| clean_text(&c[1]))
        .unwrap_or_default();
    let (page_no, total_pages) = re_page_marker()
        .captures(&title)
        .map(|c| {
            (
                c[1].parse::<u32>().ok(),
                c[2].parse::<u32>().ok().filter(|n| *n > 0),
            )
        })
        .unwrap_or((None, None));
    let title = re_page_marker().replace(&title, "").trim().to_string();

    let start = html.find(r#"id="chaptercontent""#)?;
    let start = start + html[start..].find('>')? + 1;
    let rest = &html[start..];
    let end = rest
        .find(r#"<div class="bottem2""#)
        .or_else(|| rest.find(r#"<div class="bottem"#))
        .unwrap_or(rest.len());
    let raw = strip_chapter_promo(&rest[..end]);
    let paragraphs = html_to_paragraphs(&raw);
    Some(ChapterPage {
        title,
        page_no,
        total_pages,
        paragraphs,
    })
}

// ── 搜索 JSON / 下载计划 ──────────────────────────────────────

pub fn search_items(
    client: &GuanshuyaClient,
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
                "book_id": format!("gs:{}", it.book_id),
                "title": it.title,
                "author": it.author,
                "raw": it.description,
                "description": it.description,
                "cover_url": if it.cover_url.is_empty() { Value::Null } else { json!(it.cover_url) },
                "category": Value::Null,
                "finished": Value::Null,
                "word_count": it.word_count,
                "chapter_count": Value::Null,
                "score": Value::Null,
            })
        })
        .collect();
    Ok((out, false))
}

pub fn prepare_gs_plan(
    config: &Config,
    book_id: &str,
    meta_hint: BookMeta,
) -> Result<DownloadPlan> {
    info!(target: "download", book_id, "准备官术网下载计划");
    let bid = parse_book_id(book_id)?;
    let timeout = config.request_timeout.max(5);
    let client = GuanshuyaClient::new(
        timeout,
        Some(config.guanshuya_cookie.as_str()),
        Some(config.guanshuya_user_agent.as_str()),
    )?;
    let detail = client.book_detail(bid).context("fetch guanshuya detail")?;
    let chapters = client
        .fetch_all_chapters(bid, &detail)
        .context("fetch guanshuya catalog")?;
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

    if let Some(cover_url) = merged.cover_url.as_ref() {
        let folder = book_paths::book_folder_path(config, book_id, merged.book_name.as_deref());
        let _ = std::fs::create_dir_all(&folder);
        let cover_path = book_paths::canonical_cover_path(&folder, "jpg");
        if !cover_path.exists()
            && let Err(e) = client.download_cover(cover_url, &cover_path)
        {
            debug!(target: "download", error = %e, "官术网封面下载失败（忽略）");
        }
    }

    Ok(DownloadPlan {
        book_id: book_id.to_string(),
        meta: merged,
        chapters,
        _raw: Value::Null,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn download_gs_into_manager(
    config: &Config,
    book_id: &str,
    book_name: &str,
    manager: &mut crate::book_parser::book_manager::BookManager,
    _chosen_chapters: &[ChapterRef],
    pending_chapters: &[ChapterRef],
    reporter: &mut crate::download::progress::ProgressReporter,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<DownloadResult> {
    let bid = parse_book_id(book_id)?;
    let timeout = config.request_timeout.max(5);
    let client = Arc::new(GuanshuyaClient::new(
        timeout,
        Some(config.guanshuya_cookie.as_str()),
        Some(config.guanshuya_user_agent.as_str()),
    )?);

    let worker_count = config.guanshuya_workers.clamp(1, 4);
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
                        Ok((_title, text)) if !text.trim().is_empty() => {
                            contents.push((ch.id.clone(), text));
                        }
                        Ok(_) => warn!(
                            target: "download",
                            chapter_id = %ch.id,
                            "官术网章节正文为空：{} ({})",
                            ch.title, ch.id
                        ),
                        Err(e) => warn!(
                            target: "download",
                            chapter_id = %ch.id,
                            error = %e,
                            "官术网章节下载失败：{} ({})",
                            ch.title, ch.id
                        ),
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
                Some(text) if !text.is_empty() => {
                    manager.save_chapter(&ch.id, &ch.title, text);
                    manager.append_downloaded_chapter(&ch.id, &ch.title, text);
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
        "官术网下载完成：{} ({} 章，成功 {}，失败 {})",
        book_name,
        pending_chapters.len(),
        result.success,
        result.failed
    );
    Ok(result)
}

// ── BookProvider 适配器 ──────────────────────────────────────

pub struct GuanshuyaProvider;

impl crate::provider::BookProvider for GuanshuyaProvider {
    fn id(&self) -> &'static str {
        "guanshuya"
    }

    fn display_name(&self) -> &'static str {
        "官术网"
    }

    fn owns_book_id(&self, book_id: &str) -> bool {
        is_guanshuya_book_id(book_id)
    }

    fn normalize_book_input(&self, input: &str) -> Option<String> {
        normalize_book_input(input)
    }

    fn search(
        &self,
        opts: &crate::provider::SearchOptions,
    ) -> anyhow::Result<(Vec<serde_json::Value>, bool)> {
        let client = GuanshuyaClient::new(
            opts.timeout_secs,
            opts.cookie.as_deref(),
            opts.user_agent.as_deref(),
        )?;
        search_items(&client, &opts.keyword, opts.page)
    }

    fn prepare_plan(
        &self,
        config: &crate::base_system::context::Config,
        book_id: &str,
        meta_hint: crate::download::models::BookMeta,
    ) -> anyhow::Result<crate::download::models::DownloadPlan> {
        prepare_gs_plan(config, book_id, meta_hint)
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
        download_gs_into_manager(
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
        let bid = strip_gs_prefix(book_id).parse::<u64>().ok()?;
        let client =
            GuanshuyaClient::new(crate::provider::UPDATE_FETCH_TIMEOUT_SECS, None, None).ok()?;
        let detail = client.book_detail(bid).ok()?;
        let total = detail.chapter_count.map(|n| n as usize).unwrap_or(0);
        if total == 0 {
            return None;
        }
        Some((total, detail.finished))
    }

    fn fetch_chapter_count(&self, book_id: &str, timeout_secs: u64) -> Option<u32> {
        let bid = strip_gs_prefix(book_id).parse::<u64>().ok()?;
        let client = GuanshuyaClient::new(timeout_secs, None, None).ok()?;
        client.book_detail(bid).ok().and_then(|d| d.chapter_count)
    }

    fn preview_summary(&self, book_id: &str, timeout_secs: u64) -> Option<BookMeta> {
        let bid = strip_gs_prefix(book_id).parse::<u64>().ok()?;
        let client = GuanshuyaClient::new(timeout_secs, None, None).ok()?;
        let detail = client.book_detail(bid).ok()?;
        Some(BookMeta {
            book_name: Some(detail.book_name),
            author: (!detail.author.is_empty()).then_some(detail.author),
            description: (!detail.description.is_empty()).then_some(detail.description),
            tags: Vec::new(),
            cover_url: (!detail.cover_url.is_empty()).then_some(detail.cover_url),
            detail_cover_url: None,
            finished: detail.finished,
            chapter_count: detail.chapter_count.map(|n| n as usize),
            word_count: detail.word_count,
            score: None,
            read_count: None,
            read_count_text: None,
            book_short_name: None,
            original_book_name: None,
            first_chapter_title: None,
            last_chapter_title: None,
            category: (!detail.category.is_empty()).then_some(detail.category),
            cover_primary_color: None,
        })
    }
}

// ── 离线解析回归（夹具来自抓包，防解析回归）────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::BookProvider;

    #[test]
    fn normalize_book_input_variants() {
        assert_eq!(
            normalize_book_input("gs:108891").as_deref(),
            Some("gs:108891")
        );
        assert_eq!(
            normalize_book_input("GS:108891").as_deref(),
            Some("gs:108891")
        );
        assert_eq!(
            normalize_book_input("https://www.guanshuya.net/html/108/108891/").as_deref(),
            Some("gs:108891")
        );
        assert_eq!(
            normalize_book_input("https://m.guanshuya.net/html/108/108891/224510.html").as_deref(),
            Some("gs:108891")
        );
        assert_eq!(
            normalize_book_input("https://m.guanshuya.net/html/108/108891_2/").as_deref(),
            Some("gs:108891")
        );
        assert_eq!(normalize_book_input("108891"), None);
        assert_eq!(normalize_book_input("bx:108891"), None);
    }

    #[test]
    fn sid_matches_capture_urls() {
        assert_eq!(sid_of(108891), 108);
        assert_eq!(sid_of(71247), 71);
        assert_eq!(sid_of(256), 0);
        assert_eq!(sid_of(136968), 136);
    }

    #[test]
    fn parse_search_result_matches_capture() {
        let html = include_str!("testdata/search_result.html");
        let items = parse_search_result(html);
        assert_eq!(items.len(), 50, "搜索页固定 50 条");
        assert_eq!(items[0].book_id, 108891);
        assert_eq!(items[0].title, "（穿书）黑莲花攻略手册");
        assert_eq!(items[0].author, "白羽摘雕弓");
        assert_eq!(items[0].word_count, Some(450000));
        assert!(items[0].cover_url.ends_with("/img/108/108891.jpg"));
        assert!(!items[0].description.is_empty());
    }

    #[test]
    fn parse_book_detail_matches_capture() {
        let html = include_str!("testdata/book_detail.html");
        let detail = parse_book_detail(html, 108891).expect("详情应可解析");
        assert_eq!(detail.book_name, "（穿书）黑莲花攻略手册");
        assert_eq!(detail.author, "白羽摘雕弓");
        assert_eq!(detail.category, "女生频道");
        assert_eq!(detail.finished, Some(false));
        assert_eq!(detail.word_count, Some(450000));
        assert_eq!(detail.toc_pages, 7);
        assert_eq!(detail.chapter_count, Some(122));
        assert!(detail.description.starts_with("实体书预售已出"));
    }

    #[test]
    fn parse_toc_skips_latest_block_and_dedupes() {
        let html = include_str!("testdata/book_detail.html");
        let chapters = parse_toc_page(html, 108891);
        assert_eq!(chapters.len(), 20, "目录第一页 20 章");
        assert_eq!(chapters[0].id, "224510");
        assert_eq!(chapters[0].title, "第 1 章 替嫁（一）修");
        assert_eq!(chapters[19].id, "224529");
        assert!(
            !chapters.iter().any(|c| c.id == "224631"),
            "最新章节预览块不应混入目录"
        );
    }

    #[test]
    fn parse_chapter_strips_page_marker_and_promo() {
        let html = include_str!("testdata/chapter_1.html");
        let page = parse_chapter_page(html).expect("章节应可解析");
        assert_eq!(page.title, "第 1 章 替嫁（一）修");
        assert_eq!(page.page_no, Some(1));
        assert_eq!(page.total_pages, Some(3));
        assert!(page.paragraphs[0].contains("红衣少女坐在妆台前"));
        assert!(!page.paragraphs.iter().any(|p| p.contains("本站所有小说")));
        assert!(!page.paragraphs.iter().any(|p| p.contains("情节跌宕起伏")));
        assert!(!page.paragraphs.iter().any(|p| p.contains("笔趣阁")));
        assert!(!page.paragraphs.iter().any(|p| p.contains("Copyright")));
        assert!(!page.paragraphs.iter().any(|p| p.contains("欢迎收藏本站")));
        // 最后一页正文句尾不应被推广语吞掉。
        assert!(page.paragraphs.last().is_some_and(|p| p.contains("琼")));
    }

    #[test]
    fn parse_chapter_second_page_starts_mid_sentence() {
        let html = include_str!("testdata/chapter_2.html");
        let page = parse_chapter_page(html).expect("章节应可解析");
        assert_eq!(page.page_no, Some(2));
        assert_eq!(page.total_pages, Some(3));
        assert!(page.paragraphs[0].starts_with("瑶”。"));
    }

    #[test]
    fn cross_page_join_uses_terminal_punctuation() {
        let mut paragraphs = vec!["感情线十分“琼".to_string()];
        append_page(&mut paragraphs, vec!["瑶”。".to_string()], true);
        assert_eq!(paragraphs[0], "感情线十分“琼瑶”。");

        let mut paragraphs = vec!["句子完整。".to_string()];
        append_page(&mut paragraphs, vec!["下一段".to_string()], true);
        assert_eq!(paragraphs.len(), 2);
        assert_eq!(paragraphs[1], "下一段");
    }

    #[test]
    fn promo_tail_removed_across_all_capture_pages() {
        for html in [
            include_str!("testdata/chapter_1.html"),
            include_str!("testdata/chapter_2.html"),
            include_str!("testdata/chapter_3.html"),
            include_str!("testdata/chapter_next.html"),
        ] {
            let page = parse_chapter_page(html).expect("章节应可解析");
            let text = page.paragraphs.join("\n");
            assert!(!text.contains("本站所有小说"));
            assert!(!text.contains("情节跌宕起伏"));
            assert!(!text.contains("笔趣阁"));
            assert!(!text.contains("Copyright"));
            assert!(!text.contains("欢迎收藏本站"));
        }
    }

    #[test]
    fn full_chapter_joins_three_pages() {
        let first = parse_chapter_page(include_str!("testdata/chapter_1.html")).unwrap();
        let second = parse_chapter_page(include_str!("testdata/chapter_2.html")).unwrap();
        let third = parse_chapter_page(include_str!("testdata/chapter_3.html")).unwrap();
        let mut paragraphs = first.paragraphs;
        append_page(&mut paragraphs, second.paragraphs, true);
        append_page(&mut paragraphs, third.paragraphs, true);
        let text = paragraphs.join("\n");
        assert!(text.contains("琼瑶"));
        assert!(text.contains("但嫁娶之事乃是寻常"));
        assert!(!text.contains("但嫁\n娶"));
    }

    #[test]
    fn word_count_parses_units() {
        assert_eq!(parse_word_count("45万字"), Some(450000));
        assert_eq!(parse_word_count("1,234字"), Some(1234));
        assert_eq!(parse_word_count("未知"), None);
    }

    /// 实网全链路：搜索 → 详情 → 全量目录 → 三页正文拼接。
    /// 需要 `GUANSHUYA_COOKIE`（cf_clearance）与可选 `GUANSHUYA_UA`。
    /// 运行：`cargo test --bin unified-novel-downloader live_guanshuya -- --ignored --nocapture`
    #[test]
    #[ignore = "真实网络请求：需要 GUANSHUYA_COOKIE"]
    fn live_guanshuya_chain() {
        let cookie = std::env::var("GUANSHUYA_COOKIE").expect("需要 GUANSHUYA_COOKIE");
        let ua = std::env::var("GUANSHUYA_UA").ok();
        let client = GuanshuyaClient::new(20, Some(&cookie), ua.as_deref()).unwrap();

        let items = client.search("黑莲花攻略手册").unwrap();
        assert!(!items.is_empty(), "搜索应有结果");
        let hit = items
            .iter()
            .find(|i| i.book_id == 108891)
            .expect("应命中 108891");
        assert_eq!(hit.author, "白羽摘雕弓");

        let detail = client.book_detail(108891).unwrap();
        assert_eq!(detail.chapter_count, Some(122));

        // 无 Cookie 的轻量读取（章节数排序/预览）走移动站。
        let mobile = GuanshuyaClient::new(20, None, None).unwrap();
        let mobile_detail = mobile.book_detail(108891).unwrap();
        assert_eq!(mobile_detail.book_name, "（穿书）黑莲花攻略手册");

        // trait 轻量接口：启动/搜索时注入的 Cookie 应可复用。
        let provider = GuanshuyaProvider;
        assert_eq!(provider.fetch_chapter_count("gs:108891", 20), Some(122));
        let (total, finished) = provider.fetch_update_state("gs:108891", None).unwrap();
        assert_eq!(total, 122);
        assert_eq!(finished, Some(false));
        let chapters = client.fetch_all_chapters(108891, &detail).unwrap();
        assert_eq!(chapters.len(), 122, "全量目录应为 122 章");

        let (title, text) = client.chapter_content(108891, "224510").unwrap();
        assert_eq!(title, "第 1 章 替嫁（一）修");
        assert!(text.contains("红衣少女坐在妆台前"));
        assert!(text.contains("琼瑶"));
        assert!(text.contains("但嫁娶之事乃是寻常"));
        for junk in [
            "本站所有小说",
            "情节跌宕起伏",
            "笔趣阁",
            "Copyright",
            "欢迎收藏本站",
        ] {
            assert!(!text.contains(junk), "正文不应含推广语：{junk}");
        }
        println!(
            "title={title} chars={} chapters={}",
            text.chars().count(),
            chapters.len()
        );

        // 真实下载：准备计划（全量 122 章）→ 下载前 3 章 → BookManager 落盘。
        let mut cfg = Config::default();
        cfg.guanshuya_cookie = cookie.clone();
        cfg.guanshuya_user_agent = ua.clone().unwrap_or_default();
        let tmp = std::env::temp_dir().join(format!("undl_gs_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("创建临时目录");
        cfg.save_path = tmp.to_string_lossy().to_string();

        let hint = BookMeta {
            book_name: Some("（穿书）黑莲花攻略手册".to_string()),
            ..Default::default()
        };
        let plan = prepare_gs_plan(&cfg, "gs:108891", hint).expect("官术网下载计划失败");
        assert_eq!(plan.chapters.len(), 122);
        assert_eq!(plan.chapters[0].id, "224510");

        let chosen: Vec<ChapterRef> = plan.chapters.iter().take(3).cloned().collect();
        let pending = chosen.clone();
        let mut manager = crate::book_parser::book_manager::BookManager::new(
            cfg.clone(),
            "gs:108891",
            plan.meta.book_name.as_deref().unwrap_or("官术网测试"),
        )
        .expect("BookManager 创建失败");
        let mut reporter = crate::download::progress::make_reporter(&cfg, &chosen, &pending, None);
        let result = download_gs_into_manager(
            &cfg,
            "gs:108891",
            plan.meta.book_name.as_deref().unwrap_or("官术网测试"),
            &mut manager,
            &chosen,
            &pending,
            &mut reporter,
            None,
        )
        .expect("官术网下载执行失败");
        println!(
            "[download] 成功 {}，失败 {}（临时目录 {}）",
            result.success,
            result.failed,
            tmp.display()
        );
        assert_eq!(result.success, 3);
        assert_eq!(result.failed, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 多样本正文清洗：不同书的第 1 章都应无固定推广语。
    #[test]
    #[ignore = "真实网络请求：需要 GUANSHUYA_COOKIE"]
    fn live_guanshuya_diverse_books() {
        let cookie = std::env::var("GUANSHUYA_COOKIE").expect("需要 GUANSHUYA_COOKIE");
        let ua = std::env::var("GUANSHUYA_UA").ok();
        let client = GuanshuyaClient::new(20, Some(&cookie), ua.as_deref()).unwrap();
        for bid in [108891_u64, 136968, 101714, 100298] {
            let detail = client.book_detail(bid).expect("详情失败");
            assert!(!detail.book_name.is_empty());
            let chapters = client.toc_page(bid, 1).expect("目录失败");
            let first = chapters.first().expect("目录为空");
            let (title, text) = client.chapter_content(bid, &first.id).expect("正文失败");
            let chars = text.chars().count();
            println!("bid={bid} title={title} chars={chars}");
            assert!(chars > 300, "bid={bid} 正文过短");
            for junk in [
                "本站所有小说",
                "情节跌宕起伏",
                "笔趣阁",
                "Copyright",
                "欢迎收藏本站",
            ] {
                assert!(!text.contains(junk), "bid={bid} 正文含推广语：{junk}");
            }
        }
    }
}
