//! 笔趣阁（www.biqubee.com）小说源适配。
//!
//! 接口清单（依据 examples/笔趣阁_解析结果 抓包 2026-11-06 + 非重放实网 e2e 2026-11-06）：
//! - 搜索：`GET /search.html?bi=<UTF-8 urlencode>`，结果 `table.search-list` 的 `tr`：
//!   `td.wap_none` 分类、`td` 书名链接 `/bi_{id}/`、`td.wap_none` 最新章节、`td` 作者。
//!   单页封顶 ~20 条、无翻页；**服务端有 10 秒搜索冷却**（页面文案「频率过快，请10秒后再试」）。
//! - 详情：`GET /bi_{id}/`，`og:novel:*` meta 全套 + `dl#chpbi`。
//!   第 1 页的 `dd` = 「最新章节」预览（最新若干章，倒序）+「正文卷」（第 1~100 章，正序）。
//! - 目录：`GET /bi_{id}/?page={n}`（n 从 2 起，100 章/页，正序）；末页不足 100 章。
//! - 正文：`GET /bi_{id}/{seq}.html`，seq **从 0 起**（0.html 即第 1 章）；
//!   容器 `div#htmlContent`，段落以 `<br>` 分隔；章内分页 `{seq}_{p}.html`（p 从 2 起），
//!   `a.Readpage_down` 指向同章下一页或下一章。
//!
//! 关键 ID 关系：URL 书籍 ID（如 37940）即 book_id；章节文件序号 = 章节序号 - 1。
//!
//! 搜索冷却对策（实网定维 + 前置排队，不做规则规避）：
//!
//! - **维度确认**：抓包中所有搜索请求均无 Cookie、响应也无 `Set-Cookie`；实网连续 3 次
//!   不同关键词（间隔 1s）前 2 次成功、第 3 次被拒；换 `m.biqubee.com` 移动站同样被拒。
//!   → 冷却按 **IP + 全站** 计数，姿势吧/笔下文学 那种「净身请求」无法绕过。
//! - **窗口满立即跳过**：进程级滑动窗口（跨 client 实例共享，容量 2/10s）。窗口已满时
//!   直接返回空结果，**不排队、不重试** —— 搜索路由的总时长以番茄为上限，撞窗口的源
//!   必须立即让位，不能拖慢搜索框。
//! - **同词缓存**：10s 内重复搜索直接复用结果，不消耗站点窗口（聚合搜索/重试常触发）。
//! - 站点仍返回冷却页时同样直接跳过（不等待重试）。
//! - 明确不做：代理轮换、IP 伪造、`X-Forwarded-For` 等规避手段。

use std::collections::{BTreeMap, VecDeque};
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

const SITE_BASE: &str = "https://www.biqubee.com";

/// 服务端 IP 维度的滑动窗口长度。
///
/// 实网实测（2026-11-06，连续 3 次不同关键词、间隔 1s）：前 2 次成功、第 3 次返回
/// 「频率过快」；换 `m.biqubee.com` 移动站同样被拒，说明**冷却按 IP + 全站计数**，
/// 且抓包中的搜索请求本来就没有 Cookie、响应也无 `Set-Cookie` —— 净身请求无法绕过。
const SEARCH_WINDOW: Duration = Duration::from_secs(10);
/// 滑动窗口内允许的搜索次数（实网令牌桶容量）。
const SEARCH_BURST: usize = 2;
/// 同关键词结果缓存时长（窗口内重复搜索不再发请求）。
const SEARCH_CACHE_TTL: Duration = Duration::from_secs(10);
/// 同关键词结果缓存条数上限。
const SEARCH_CACHE_MAX: usize = 64;
/// 目录翻页间隔。
const TOC_PAGE_GAP: Duration = Duration::from_millis(300);
/// 目录页数保护上限（100 章/页）。
const TOC_MAX_PAGES: u32 = 200;
/// 章内分页保护上限。
const CHAPTER_MAX_PAGES: u32 = 50;
/// 目录每页条数（第 2 页起）。
const TOC_PAGE_SIZE: usize = 100;
/// 关键字最长字符数（防御性截断）。
const KEYWORD_MAX_CHARS: usize = 50;
/// 下载并发上限（个人站，保持克制）。
const WORKER_LIMIT: usize = 4;

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36 Edg/156.0.0.0";

// ── book_id 识别与规范化 ──────────────────────────────────────

/// `bb:` 前缀即笔趣阁 book_id。
pub fn is_biqubee_book_id(book_id: &str) -> bool {
    book_id.starts_with("bb:")
}

/// 去掉 `bb:` 前缀，返回纯数字部分。
pub fn strip_bb_prefix(book_id: &str) -> &str {
    book_id.strip_prefix("bb:").unwrap_or(book_id)
}

/// 从用户输入识别笔趣阁 book_id：`bb:37940` 前缀，或 biqubee.com 的书籍/章节页 URL。
pub fn normalize_book_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("bb:")
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return Some(format!("bb:{rest}"));
    }
    re_book_url()
        .captures(trimmed)
        .map(|c| format!("bb:{}", &c[1]))
}

fn parse_book_numeric_id(book_id: &str) -> Result<u64> {
    strip_bb_prefix(book_id)
        .parse::<u64>()
        .with_context(|| format!("非法笔趣阁 book_id: {book_id:?}"))
}

/// 关键字预处理：去首尾空白，超长按字符边界截断。
fn sanitize_keyword(raw: &str) -> Result<String> {
    let kw = raw.trim();
    if kw.is_empty() {
        return Err(anyhow!("搜索关键字不能为空"));
    }
    if kw.chars().count() <= KEYWORD_MAX_CHARS {
        return Ok(kw.to_string());
    }
    let truncated: String = kw.chars().take(KEYWORD_MAX_CHARS).collect();
    info!(target: "biqubee", truncated = truncated.as_str(), "搜索关键字超长，已截断");
    Ok(truncated)
}

// ── 正则缓存 ──────────────────────────────────────────────────

fn re_book_url() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)biqubee\.com/bi_(\d+)(?:/\d+\.html)?").expect("compile re_book_url")
    })
}

fn re_search_table() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<table class="search-list">([\s\S]*?)</table>"#)
            .expect("compile re_search_table")
    })
}

fn re_tr() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<tr[^>]*>([\s\S]*?)</tr>").expect("compile re_tr"))
}

fn re_search_title() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<td><a href="/bi_(\d+)/"[^>]*>(.*?)</a></td>"#)
            .expect("compile re_search_title")
    })
}

fn re_td_wap_none() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<td class="wap_none">(.*?)</td>"#).expect("compile re_td_wap_none")
    })
}

fn re_td_plain() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<td>(.*?)</td>").expect("compile re_td_plain"))
}

fn re_chapter_dd() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<dd><a href ="[^"]*?/bi_(\d+)/(\d+)\.html">(.*?)</a></dd>"#)
            .expect("compile re_chapter_dd")
    })
}

fn re_meta_og() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<meta property="(og:[^"]+)" content="([^"]*)""#)
            .expect("compile re_meta_og")
    })
}

fn re_chapter_title() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<h1[^>]*>(.*?)</h1>").expect("compile re_chapter_title"))
}

fn re_br() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)<br\s*/?>").expect("compile re_br"))
}

fn re_script() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<script[\s\S]*?</script>").expect("compile re_script"))
}

fn re_any_a() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?is)<a\s([^>]*)>"#).expect("compile re_any_a"))
}

fn re_href() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"href\s*=\s*"([^"]+)""#).expect("compile re_href"))
}

fn re_page_suffix() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\(（]\s*\d+\s*页\s*[\)）]").expect("compile re_page_suffix"))
}

fn re_chapter_seq_url() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(\d+)_(\d+)\.html").expect("compile re_chapter_seq_url"))
}

// ── 文本工具 ──────────────────────────────────────────────────

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn clean_text(s: &str) -> String {
    let unescaped = crate::book_parser::html_utils::unescape_basic_entities(s);
    let text = strip_tags(&unescaped).replace('\u{a0}', " ");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
pub struct BiqubeeSearchItem {
    pub book_id: u64,
    pub title: String,
    pub author: String,
    pub latest_chapter: String,
    pub category: String,
}

#[derive(Debug, Clone)]
pub struct BiqubeeBookDetail {
    pub book_id: u64,
    pub book_name: String,
    pub author: String,
    pub description: String,
    pub cover_url: String,
    pub category: String,
    pub finished: Option<bool>,
    pub latest_seq: Option<u32>,
    /// 详情页第 1 页正文卷（已剔除「最新章节」预览块）。
    pub toc_first_page: Vec<ChapterRef>,
}

// ── 客户端 ────────────────────────────────────────────────────

/// 进程级滑动窗口：跨 `BiqubeeClient` 实例共享（聚合搜索/连续搜索常会新建 client，
/// 实例内节流会失效，必须放在全局）。
static SEARCH_TIMES: Mutex<VecDeque<Instant>> = Mutex::new(VecDeque::new());

type SearchCache = Mutex<std::collections::HashMap<String, (Instant, Vec<BiqubeeSearchItem>)>>;

fn search_cache() -> &'static SearchCache {
    static CACHE: OnceLock<SearchCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// 尝试取得一个搜索配额。**窗口已满时立即返回 false（不排队）**：
/// 搜索路由有统一的总时长上限，撞窗口的源应直接跳过，绝不能把搜索框拖慢。
fn try_acquire_search_slot() -> bool {
    let now = Instant::now();
    let mut q = SEARCH_TIMES.lock().unwrap_or_else(|e| e.into_inner());
    while q
        .front()
        .is_some_and(|t| now.duration_since(*t) >= SEARCH_WINDOW)
    {
        q.pop_front();
    }
    if q.len() < SEARCH_BURST {
        q.push_back(now);
        true
    } else {
        false
    }
}
fn cache_get(keyword: &str) -> Option<Vec<BiqubeeSearchItem>> {
    let map = search_cache().lock().unwrap_or_else(|e| e.into_inner());
    map.get(keyword)
        .filter(|(at, _)| at.elapsed() <= SEARCH_CACHE_TTL)
        .map(|(_, items)| items.clone())
}

fn cache_put(keyword: &str, items: &[BiqubeeSearchItem]) {
    let mut map = search_cache().lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, (at, _)| at.elapsed() <= SEARCH_CACHE_TTL);
    if map.len() >= SEARCH_CACHE_MAX {
        map.clear();
    }
    map.insert(keyword.to_string(), (Instant::now(), items.to_vec()));
}

#[cfg(test)]
fn reset_search_window() {
    SEARCH_TIMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    search_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

pub struct BiqubeeClient {
    http: reqwest::blocking::Client,
}

impl BiqubeeClient {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(5)))
            .user_agent(USER_AGENT)
            .build()
            .context("build biqubee HTTP client")?;
        Ok(Self { http })
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
                    let bytes = resp.bytes().context("读取笔趣阁响应失败")?;
                    if status.is_success() {
                        return Ok(String::from_utf8_lossy(&bytes).into_owned());
                    }
                    return Err(anyhow!("笔趣阁请求 HTTP {status}：{url}"));
                }
                Err(e) => {
                    debug!(target: "biqubee", url, attempt, error = %e, "笔趣阁请求传输错误，将重试");
                    last_err =
                        Some(anyhow::Error::new(e).context(format!("笔趣阁请求失败：{url}")));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("笔趣阁请求失败：{url}")))
    }

    pub fn search(&self, raw_keyword: &str) -> Result<Vec<BiqubeeSearchItem>> {
        let keyword = sanitize_keyword(raw_keyword)?;
        if let Some(items) = cache_get(&keyword) {
            info!(
                target: "biqubee",
                keyword = keyword.as_str(),
                count = items.len(),
                "笔趣阁搜索命中同词缓存，未消耗站点窗口"
            );
            return Ok(items);
        }
        match self.attempt_search(&keyword) {
            Ok(items) => {
                cache_put(&keyword, &items);
                Ok(items)
            }
            // 撞窗口的源不参与本轮搜索，也不排队/重试：搜索路由的总时长以番茄为上限，
            // 慢源必须立即让位，绝不能把搜索框拖住。
            Err(SearchError::WindowBusy) => {
                info!(
                    target: "biqubee",
                    keyword = keyword.as_str(),
                    "笔趣阁搜索窗口已满，本轮跳过（不排队、不重试）"
                );
                Ok(Vec::new())
            }
            Err(SearchError::Cooldown) => {
                info!(
                    target: "biqubee",
                    keyword = keyword.as_str(),
                    "笔趣阁站点返回「频率过快」，本轮跳过（不等待重试）"
                );
                Ok(Vec::new())
            }
            Err(SearchError::Other(e)) => Err(e),
        }
    }

    fn attempt_search(
        &self,
        keyword: &str,
    ) -> std::result::Result<Vec<BiqubeeSearchItem>, SearchError> {
        if !try_acquire_search_slot() {
            return Err(SearchError::WindowBusy);
        }
        let url = format!("{SITE_BASE}/search.html");
        let resp = self
            .http
            .get(&url)
            .query(&[("bi", keyword)])
            .send()
            .context("笔趣阁搜索请求失败")
            .map_err(SearchError::Other)?;
        let status = resp.status();
        let html = resp
            .bytes()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .context("笔趣阁搜索响应读取失败")
            .map_err(SearchError::Other)?;
        if html.contains("频率过快") {
            return Err(SearchError::Cooldown);
        }
        if !status.is_success() {
            return Err(SearchError::Other(anyhow!("笔趣阁搜索 HTTP {status}")));
        }
        if re_search_table().captures(&html).is_none() {
            return Err(SearchError::Other(anyhow!(
                "笔趣阁搜索返回异常（无结果表格，可能与站点结构变化有关）"
            )));
        }
        Ok(parse_search_result(&html))
    }

    pub fn book_detail(&self, book_id: u64) -> Result<BiqubeeBookDetail> {
        let url = format!("{SITE_BASE}/bi_{book_id}/");
        let html = self.get_text(&url)?;
        parse_book_detail(&html, book_id)
            .ok_or_else(|| anyhow!("笔趣阁详情页解析失败（book_id={book_id}）"))
    }

    fn toc_page(&self, book_id: u64, page: u32) -> Result<Vec<(u32, ChapterRef)>> {
        let url = if page <= 1 {
            format!("{SITE_BASE}/bi_{book_id}/")
        } else {
            format!("{SITE_BASE}/bi_{book_id}/?page={page}")
        };
        let html = self.get_text(&url)?;
        Ok(parse_toc_rows(&html, book_id))
    }

    /// 全量目录：第 1 页（跳过「最新章节」预览块）+ `?page=n` 翻页。
    pub fn fetch_all_chapters(
        &self,
        book_id: u64,
        detail: &BiqubeeBookDetail,
    ) -> Result<Vec<ChapterRef>> {
        let mut map: BTreeMap<u32, String> = BTreeMap::new();
        for ch in &detail.toc_first_page {
            if let Ok(seq) = ch.id.parse::<u32>() {
                map.insert(seq, ch.title.clone());
            }
        }
        let mut page = 2u32;
        loop {
            if page > TOC_MAX_PAGES {
                return Err(anyhow!("笔趣阁目录翻页超过保护上限（{TOC_MAX_PAGES} 页）"));
            }
            // 第 1 页不足一页时无需翻页。
            if page == 2 && map.len() < TOC_PAGE_SIZE {
                break;
            }
            thread::sleep(TOC_PAGE_GAP);
            let rows = self.toc_page(book_id, page)?;
            let count = rows.len();
            for (seq, ch) in rows {
                map.entry(seq).or_insert(ch.title);
            }
            if count < TOC_PAGE_SIZE {
                break;
            }
            page += 1;
        }
        let chapters: Vec<ChapterRef> = map
            .into_iter()
            .map(|(seq, title)| ChapterRef {
                id: seq.to_string(),
                title,
            })
            .collect();
        if chapters.is_empty() {
            return Err(anyhow!("笔趣阁目录为空（book_id={book_id}）"));
        }
        Ok(chapters)
    }

    /// 章节正文（含章内分页）。返回 (标题, `<p>` 段落 HTML)。
    pub fn chapter_content(&self, book_id: u64, chapter_id: &str) -> Result<(String, String)> {
        let seq = chapter_id
            .parse::<u32>()
            .with_context(|| format!("非法笔趣阁章节序号: {chapter_id}"))?;
        let first_url = format!("{SITE_BASE}/bi_{book_id}/{seq}.html");
        let first_html = self.get_text(&first_url)?;
        let (title, first_paras, mut next) = parse_chapter_page(&first_html, seq)
            .ok_or_else(|| anyhow!("笔趣阁章节页解析失败（seq={seq}，可能已失效）"))?;
        let mut paragraphs = first_paras;
        let mut guard = 1u32;
        while let Some(p) = next {
            guard += 1;
            if guard > CHAPTER_MAX_PAGES {
                return Err(anyhow!(
                    "笔趣阁章内分页超过保护上限（{CHAPTER_MAX_PAGES} 页）：seq={seq}"
                ));
            }
            let url = format!("{SITE_BASE}/bi_{book_id}/{seq}_{p}.html");
            let html = self.get_text(&url)?;
            let (_, page_paras, next_page) = parse_chapter_page(&html, seq)
                .ok_or_else(|| anyhow!("笔趣阁章内分页解析失败（seq={seq}_{p}）"))?;
            paragraphs.extend(page_paras);
            next = next_page;
        }
        if paragraphs.is_empty() {
            return Err(anyhow!("笔趣阁章节正文为空（seq={seq}）"));
        }
        Ok((title, paragraphs.join("\n")))
    }
}

enum SearchError {
    /// 本地滑动窗口已满（本轮未发请求）。
    WindowBusy,
    /// 站点返回「频率过快」（请求已发出但被拒）。
    Cooldown,
    Other(anyhow::Error),
}

// ── 解析 ──────────────────────────────────────────────────────

fn parse_search_result(html: &str) -> Vec<BiqubeeSearchItem> {
    let Some(table) = re_search_table().captures(html) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for tr in re_tr().captures_iter(&table[1]) {
        let row = &tr[1];
        let Some(title_cap) = re_search_title().captures(row) else {
            continue;
        };
        let Ok(book_id) = title_cap[1].parse::<u64>() else {
            continue;
        };
        let title = clean_text(&title_cap[2]);
        if title.is_empty() {
            continue;
        }
        let wap: Vec<String> = re_td_wap_none()
            .captures_iter(row)
            .map(|c| clean_text(&c[1]))
            .collect();
        let category = wap
            .first()
            .map(|s| s.trim_matches(['[', ']']).to_string())
            .unwrap_or_default();
        let latest_chapter = wap.get(1).cloned().unwrap_or_default();
        let author = re_td_plain()
            .captures_iter(row)
            .last()
            .map(|c| clean_text(&c[1]))
            .unwrap_or_default();
        items.push(BiqubeeSearchItem {
            book_id,
            title,
            author,
            latest_chapter,
            category,
        });
    }
    items
}

fn parse_toc_rows(html: &str, book_id: u64) -> Vec<(u32, ChapterRef)> {
    let bid = book_id.to_string();
    re_chapter_dd()
        .captures_iter(html)
        .filter(|c| &c[1] == bid.as_str())
        .filter_map(|c| {
            let seq = c[2].parse::<u32>().ok()?;
            let title = clean_text(&c[3]);
            if title.is_empty() {
                return None;
            }
            Some((
                seq,
                ChapterRef {
                    id: seq.to_string(),
                    title,
                },
            ))
        })
        .collect()
}

fn parse_book_detail(html: &str, book_id: u64) -> Option<BiqubeeBookDetail> {
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
    let cover_url = og_value(html, "og:image").unwrap_or_default();
    let category = og_value(html, "og:novel:category")
        .map(|s| clean_text(&s))
        .unwrap_or_default();
    let finished = og_value(html, "og:novel:status").map(|s| {
        let s = clean_text(&s);
        s.contains("完") || s.contains("完结")
    });
    let latest_seq = og_value(html, "og:novel:latest_chapter_url").and_then(|u| {
        Regex::new(&format!(r"/bi_{book_id}/(\d+)\.html"))
            .ok()?
            .captures(&u)
            .and_then(|c| c[1].parse::<u32>().ok())
    });

    // 第 1 页：dd 顺序 = 「最新章节」预览（尾部、倒序）+「正文卷」（0..99、正序）。
    // 预览块的 seq 必然落在正文 100 章之后（短书则与正文重复，交给去重）。
    let toc_first_page: Vec<ChapterRef> = parse_toc_rows(html, book_id)
        .into_iter()
        .filter(|(seq, _)| (*seq as usize) < TOC_PAGE_SIZE)
        .map(|(_, ch)| ch)
        .collect();

    Some(BiqubeeBookDetail {
        book_id,
        book_name,
        author,
        description,
        cover_url,
        category,
        finished,
        latest_seq,
        toc_first_page,
    })
}

/// 解析章节页：返回 (标题, 段落列表, 同章下一页号)。
fn parse_chapter_page(html: &str, seq: u32) -> Option<(String, Vec<String>, Option<u32>)> {
    let title = re_chapter_title()
        .captures(html)
        .map(|c| clean_text(&c[1]))
        .map(|s| re_page_suffix().replace_all(&s, "").trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();

    let pos = html.find(r#"id="htmlContent""#)?;
    let open_end = html[pos..].find('>')? + pos + 1;
    let seg_end = html[open_end..]
        .find("</div>")
        .map(|p| p + open_end)
        .unwrap_or(html.len());
    let seg = re_script().replace_all(&html[open_end..seg_end], "");
    let mut paragraphs = Vec::new();
    for line in re_br().split(&seg) {
        let text = clean_text(line);
        if text.is_empty() {
            continue;
        }
        paragraphs.push(format!("<p>{text}</p>"));
    }

    // 同章下一页：Readpage_down 的 href 形如 `{seq}_{p}.html`。
    let mut next = None;
    for cap in re_any_a().captures_iter(html) {
        let attrs = &cap[1];
        if !attrs.contains("Readpage_down") {
            continue;
        }
        let Some(href) = re_href().captures(attrs).map(|c| c[1].to_string()) else {
            continue;
        };
        let file = href.rsplit('/').next().unwrap_or(&href);
        if let Some(c) = re_chapter_seq_url().captures(file)
            && c[1].parse::<u32>().ok() == Some(seq)
        {
            next = c[2].parse::<u32>().ok();
        }
    }
    Some((title, paragraphs, next))
}

// ── 下载计划 ──────────────────────────────────────────────────

pub fn prepare_biqubee_plan(
    config: &Config,
    book_id: &str,
    meta_hint: BookMeta,
) -> Result<DownloadPlan> {
    info!(target: "download", book_id, "准备笔趣阁下载计划");

    let bid = parse_book_numeric_id(book_id)?;
    let timeout = config.request_timeout.max(5);
    let client = BiqubeeClient::new(timeout).context("init BiqubeeClient")?;

    let detail = client
        .book_detail(bid)
        .context("fetch biqubee book detail")?;
    if detail.book_id != bid {
        return Err(anyhow!(
            "笔趣阁详情页 book_id 与请求不一致（{}/{bid}）",
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
            debug!(target: "download", error = %e, "笔趣阁封面下载失败（忽略）");
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
pub fn download_biqubee_into_manager(
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
    let client = Arc::new(BiqubeeClient::new(timeout).context("init BiqubeeClient")?);

    let worker_count = config.biqubee_workers.clamp(1, WORKER_LIMIT);
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
                            warn!(target: "download", chapter_id = %ch.id, "笔趣阁章节正文为空：{}", ch.title);
                        }
                        Err(e) => {
                            warn!(target: "download", chapter_id = %ch.id, error = %e, "笔趣阁章节下载失败：{}", ch.title);
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
        "笔趣阁下载完成：{} ({} 章，成功 {}，失败 {})",
        book_name,
        pending_chapters.len(),
        result.success,
        result.failed
    );
    Ok(result)
}

// ── 搜索（Web UI / TUI）───────────────────────────────────────

pub fn search_items(
    client: &BiqubeeClient,
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
                "book_id": format!("bb:{}", it.book_id),
                "title": it.title,
                "author": it.author,
                "latest_chapter": it.latest_chapter,
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

pub struct BiqubeeProvider;

impl crate::provider::BookProvider for BiqubeeProvider {
    fn id(&self) -> &'static str {
        "biqubee"
    }

    fn display_name(&self) -> &'static str {
        "笔趣阁"
    }

    fn owns_book_id(&self, book_id: &str) -> bool {
        is_biqubee_book_id(book_id)
    }

    fn normalize_book_input(&self, input: &str) -> Option<String> {
        normalize_book_input(input)
    }

    fn search(
        &self,
        opts: &crate::provider::SearchOptions,
    ) -> anyhow::Result<(Vec<serde_json::Value>, bool)> {
        let client = BiqubeeClient::new(opts.timeout_secs)?;
        search_items(&client, &opts.keyword, opts.page)
    }

    fn prepare_plan(
        &self,
        config: &crate::base_system::context::Config,
        book_id: &str,
        meta_hint: crate::download::models::BookMeta,
    ) -> anyhow::Result<crate::download::models::DownloadPlan> {
        prepare_biqubee_plan(config, book_id, meta_hint)
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
        download_biqubee_into_manager(
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
        let client = BiqubeeClient::new(timeout_secs).ok()?;
        let detail = client.book_detail(bid).ok()?;
        Some(BookMeta {
            book_name: Some(detail.book_name),
            author: (!detail.author.is_empty()).then_some(detail.author),
            cover_url: (!detail.cover_url.is_empty()).then_some(detail.cover_url),
            category: (!detail.category.is_empty()).then_some(detail.category),
            finished: detail.finished,
            chapter_count: detail.latest_seq.map(|s| s as usize + 1),
            ..Default::default()
        })
    }

    fn fetch_update_state(
        &self,
        book_id: &str,
        _book_name: Option<&str>,
    ) -> Option<(usize, Option<bool>)> {
        let client = BiqubeeClient::new(crate::provider::UPDATE_FETCH_TIMEOUT_SECS).ok()?;
        let bid = strip_bb_prefix(book_id).parse::<u64>().ok()?;
        let detail = client.book_detail(bid).ok()?;
        // 章节文件序号从 0 起，最新章节 seq+1 即章节总数。
        let total = detail.latest_seq? as usize + 1;
        (total > 0).then_some((total, detail.finished))
    }
}

// ── 离线解析回归 ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/src/biqubee/testdata/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("read fixture")
    }

    #[test]
    fn normalize_book_input_variants() {
        assert_eq!(
            normalize_book_input("bb:37940").as_deref(),
            Some("bb:37940")
        );
        assert_eq!(
            normalize_book_input("BB:37940").as_deref(),
            Some("bb:37940")
        );
        assert_eq!(
            normalize_book_input("https://www.biqubee.com/bi_37940/").as_deref(),
            Some("bb:37940")
        );
        assert_eq!(
            normalize_book_input("https://www.biqubee.com/bi_37940/121.html").as_deref(),
            Some("bb:37940")
        );
        assert_eq!(normalize_book_input("sq:1"), None);
    }

    #[test]
    fn search_result_matches_capture() {
        let html = fixture("search_result.html");
        let items = parse_search_result(&html);
        assert_eq!(items.len(), 3, "抓包成功结果页 3 条");
        assert_eq!(items[0].book_id, 37940);
        assert_eq!(items[0].title, "黑莲花攻略手册[穿书]");
        assert_eq!(items[0].author, "白羽摘雕弓");
        assert_eq!(items[0].category, "仙侠");
        assert_eq!(items[1].book_id, 185198);
    }

    #[test]
    fn throttled_page_is_detected() {
        let html = fixture("search_throttled.html");
        assert!(html.contains("频率过快"));
        assert!(parse_search_result(&html).is_empty());
    }

    #[test]
    fn book_detail_matches_capture() {
        let html = fixture("book_detail.html");
        let d = parse_book_detail(&html, 37940).expect("detail parsed");
        assert_eq!(d.book_name, "黑莲花攻略手册[穿书]");
        assert_eq!(d.author, "白羽摘雕弓");
        assert_eq!(d.category, "仙侠修真");
        assert_eq!(d.latest_seq, Some(121));
        assert_eq!(d.finished, Some(false));
        assert_eq!(d.toc_first_page.len(), 100, "第 1 页正文卷 1~100 章");
        assert_eq!(d.toc_first_page[0].id, "0");
        assert_eq!(d.toc_first_page[99].id, "99");
    }

    #[test]
    fn toc_page2_is_next_hundred_chapters() {
        // 实测夹具：/bi_633308/?page=2 → seq 100..199
        let html = fixture("toc_page2.html");
        let rows = parse_toc_rows(&html, 633308);
        assert_eq!(rows.len(), 100);
        assert_eq!(rows[0].0, 100);
        assert_eq!(rows[99].0, 199);
        assert_eq!(rows[0].1.title, "第101章 巧遇与祈祷");
    }

    #[test]
    fn chapter_first_page_parsed() {
        let html = fixture("chapter_0_p1.html");
        let (title, paras, next) = parse_chapter_page(&html, 0).expect("chapter parsed");
        assert_eq!(title, "1、替嫁（一）修");
        assert!(paras.len() > 3);
        assert!(paras[0].starts_with("<p>"));
        assert!(paras.iter().any(|p| p.contains("红衣少女坐在妆台前")));
        // 抓包此章有章内第 2 页。
        assert_eq!(next, Some(2));
    }

    #[test]
    fn chapter_second_page_continues() {
        let html = fixture("chapter_0_p2.html");
        let (title, paras, next) = parse_chapter_page(&html, 0).expect("chapter parsed");
        assert_eq!(title, "1、替嫁（一）修");
        assert!(!paras.is_empty());
        assert_eq!(next, None);
    }

    /// 实网基准：30 章，串行 vs 4 线程。
    #[test]
    #[ignore]
    fn bench_download_workers() {
        let mut cfg = Config::default();
        let tmp = std::env::temp_dir().join(format!("undl_bb_bench_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        cfg.save_path = tmp.to_string_lossy().to_string();

        let plan = prepare_biqubee_plan(
            &cfg,
            "bb:633308",
            BookMeta {
                book_name: Some("诡秘：秩序之主".into()),
                ..Default::default()
            },
        )
        .expect("目录准备失败");
        let chosen: Vec<ChapterRef> = plan.chapters.iter().take(30).cloned().collect();

        for workers in [1usize, 4] {
            let dir = tmp.join(format!("w{workers}"));
            std::fs::create_dir_all(&dir).unwrap();
            let mut c = cfg.clone();
            c.biqubee_workers = workers;
            c.save_path = dir.to_string_lossy().to_string();
            let mut manager =
                crate::book_parser::book_manager::BookManager::new(c.clone(), "bb:633308", "bench")
                    .expect("manager");
            let mut reporter = crate::download::progress::make_reporter(&c, &chosen, &chosen, None);
            let t = Instant::now();
            let r = download_biqubee_into_manager(
                &c,
                "bb:633308",
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

    #[test]
    fn search_window_full_returns_busy_without_waiting() {
        reset_search_window();
        assert!(try_acquire_search_slot(), "窗口空闲应取得配额");
        assert!(try_acquire_search_slot(), "容量 2，第二次仍应取得");
        assert!(
            !try_acquire_search_slot(),
            "窗口满时必须立即返回 false（不排队）"
        );
        reset_search_window();
    }

    /// 实网延时测量：窗口空闲时单次搜索不应有任何额外等待；同词重搜走缓存。
    /// 运行：`cargo test biqubee::tests::e2e_live_search_latency -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn e2e_live_search_latency() {
        let _guard = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_search_window();
        let client = BiqubeeClient::new(20).expect("init live client");

        let t = Instant::now();
        let items = match client.search("剑来") {
            Ok(v) => v,
            Err(e) => {
                // 站点在高频触犯后会临时收紧 IP 窗口，惩罚期内不把测量当失败。
                println!("[跳过] 站点当前处于动态惩罚期：{e}");
                return;
            }
        };
        let single = t.elapsed();
        println!(
            "[单次] 「剑来」命中 {} 条，耗时 {:.2}s（窗口空闲，无额外等待）",
            items.len(),
            single.as_secs_f32()
        );

        let t = Instant::now();
        let cached = client.search("剑来").expect("同词重搜失败");
        let cache = t.elapsed();
        println!(
            "[同词] 重搜 {} 条，耗时 {:.3}s（缓存命中，不消耗窗口）",
            cached.len(),
            cache.as_secs_f32()
        );
        assert_eq!(cached.len(), items.len());
        assert!(cache < Duration::from_millis(200));

        // 同进程复用连接后的第 2 次搜索（等过站点窗口），用于区分「首次建连」
        // 与「站点窗口」：若本次远快于首搜，说明首搜慢在连接初始化或站点窗口。
        thread::sleep(SEARCH_WINDOW + Duration::from_secs(2));
        let t = Instant::now();
        match client.search("雪中悍刀行") {
            Ok(items2) if !items2.is_empty() => println!(
                "[复用连接] 「雪中悍刀行」命中 {} 条，耗时 {:.2}s",
                items2.len(),
                t.elapsed().as_secs_f32()
            ),
            Ok(_) => println!(
                "[复用连接] 「雪中悍刀行」本轮被窗口跳过（耗时 {:.2}s，不排队）",
                t.elapsed().as_secs_f32()
            ),
            Err(e) => println!("[复用连接] 第 2 次搜索失败：{e}"),
        }
    }

    /// 实网验证：本地窗口满时第 3 次搜索应立即返回空结果（不排队）。
    #[test]
    #[ignore]
    fn e2e_live_window_busy_returns_fast() {
        let _guard = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_search_window();
        let client = BiqubeeClient::new(20).expect("init live client");

        // 前两次占用本地配额（无论站点是否拒绝，都不影响本地窗口）。
        let _ = client.search("剑来");
        let _ = client.search("雪中悍刀行");

        let t = Instant::now();
        let third = client
            .search("凡人修仙传")
            .expect("窗口满应返回空结果而不是错误");
        let elapsed = t.elapsed();
        println!(
            "[窗口满] 第 3 次返回 {} 条，耗时 {:.3}s（立即跳过，不排队）",
            third.len(),
            elapsed.as_secs_f32()
        );
        assert!(third.is_empty(), "窗口满应返回空结果");
        assert!(elapsed < Duration::from_secs(2), "窗口满不得排队等待");
        reset_search_window();
    }
    /// 非重放实网 e2e：关键词/书目均与抓包不同（抓包为「黑莲花攻略手册」）。
    /// 运行：`cargo test biqubee::tests::e2e_live_full_chain -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn e2e_live_full_chain() {
        let _guard = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_search_window();
        let client = BiqubeeClient::new(20).expect("init live client");

        // ① 搜索（非重放关键词）
        let t0 = Instant::now();
        let items = client.search("诡秘之主").expect("① 搜索失败");
        println!(
            "[①] 「诡秘之主」命中 {} 条，耗时 {:.2}s",
            items.len(),
            t0.elapsed().as_secs_f32()
        );
        assert!(!items.is_empty(), "搜索结果不应为空");
        let hit = items
            .iter()
            .find(|it| it.title == "诡秘：秩序之主")
            .expect("目标书「诡秘：秩序之主」应在结果中");
        println!("    bb:{}《{}》/ {}", hit.book_id, hit.title, hit.author);

        // ② 详情
        let detail = client.book_detail(hit.book_id).expect("② 详情失败");
        assert_eq!(detail.book_name, "诡秘：秩序之主");
        println!(
            "[②] 详情《{}》/{}，最新章节序号 {:?}，完结 {:?}",
            detail.book_name, detail.author, detail.latest_seq, detail.finished
        );

        // ③ 目录跨页（本书 582 章，需 ?page=2..6）
        let chapters = client
            .fetch_all_chapters(hit.book_id, &detail)
            .expect("③ 目录失败");
        assert!(
            chapters.len() >= 500,
            "目录应跨页覆盖 500+ 章，实际 {}",
            chapters.len()
        );
        assert_eq!(chapters[0].id, "0");
        assert_eq!(chapters[0].title, "第1章 起源");
        println!(
            "[③] 目录 {} 章，首「{}」末「{}」",
            chapters.len(),
            chapters[0].title,
            chapters.last().unwrap().title
        );

        // ④ 正文（首章有章内分页，应自动合并）
        let (title, body) = client
            .chapter_content(hit.book_id, "0")
            .expect("④ 正文失败");
        assert_eq!(title, "第1章 起源");
        assert!(body.contains("暗金色雾气"), "正文应含实网首段文本");
        assert!(!body.contains("<script"), "正文不应残留 script");
        println!("[④] 首章正文 {} 字（章内分页已合并）", body.chars().count());

        // ⑤ 真实下载 2 章
        let mut cfg = Config::default();
        let tmp = std::env::temp_dir().join(format!("undl_biqubee_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("创建临时目录");
        cfg.save_path = tmp.to_string_lossy().to_string();

        let book_id = format!("bb:{}", hit.book_id);
        let plan = prepare_biqubee_plan(
            &cfg,
            &book_id,
            BookMeta {
                book_name: Some(detail.book_name.clone()),
                ..Default::default()
            },
        )
        .expect("⑤ 下载计划失败");
        assert_eq!(plan.meta.author.as_deref(), Some("大白不是熊"));

        let chosen: Vec<ChapterRef> = plan.chapters.iter().take(2).cloned().collect();
        let pending = chosen.clone();
        let mut manager = crate::book_parser::book_manager::BookManager::new(
            cfg.clone(),
            &book_id,
            plan.meta.book_name.as_deref().unwrap_or("诡秘：秩序之主"),
        )
        .expect("BookManager 创建失败");
        let mut reporter = crate::download::progress::make_reporter(&cfg, &chosen, &pending, None);
        let result = download_biqubee_into_manager(
            &cfg,
            &book_id,
            plan.meta.book_name.as_deref().unwrap_or("诡秘：秩序之主"),
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
        assert_eq!(result.success, 2);
        assert_eq!(result.failed, 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
