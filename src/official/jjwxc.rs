//! 晋江（jjwxc.net）官方源。
//!
//! 基于 `examples/晋江搜索_解析结果` 抓包分析实现，刻意**不使用** App 端
//! `NovelSearch/keywordSearch`（请求体为应用层密文，需逆向加密），改用两条
//! 公开、无加密的通路：
//!
//! - 搜索：`GET https://www.jjwxc.net/search.php?kw=...&t=1`（UTF-8 为主、GBK 兜底 HTML）
//! - 详情：`GET https://app-cdn.jjwxc.net/androidapi/novelbasicinfo?novelId=...`（JSON）
//!
//! 两条通路均已真实请求验证可用。官方源只提供搜索/元数据，不参与下载。

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use serde_json::Value;

use super::{OfficialBook, OfficialSource};

const SEARCH_URL: &str = "https://www.jjwxc.net/search.php";
const DETAIL_URL: &str = "https://app-cdn.jjwxc.net/androidapi/novelbasicinfo";
const BOOK_URL: &str = "https://www.jjwxc.net/onebook.php?novelid=";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";

/// 官方站是公开网页，保守限速 1 req/s（进程内串行）。
fn throttle() {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    if let Ok(mut guard) = LAST.lock() {
        if let Some(prev) = *guard {
            let elapsed = prev.elapsed();
            if elapsed < Duration::from_secs(1) {
                std::thread::sleep(Duration::from_secs(1) - elapsed);
            }
        }
        *guard = Some(Instant::now());
    }
}

fn http_client(timeout_secs: u64) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs.max(5)))
        .user_agent(UA)
        .build()
        .context("创建晋江官方源 HTTP 客户端失败")
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for &b in bytes {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urlencode(input: &str) -> String {
    percent_encode(input.as_bytes())
}

/// 晋江搜索页的 `kw` 参数按 **GBK** 解码（实测：UTF-8 传参会被解成乱码，
/// 搜索结果完全不相干）。GBK 无法表示的字符才退回 UTF-8 字节。
fn urlencode_query(input: &str) -> String {
    let (gbk, _, had_errors) = encoding_rs::GBK.encode(input);
    if had_errors {
        urlencode(input)
    } else {
        percent_encode(&gbk)
    }
}

fn decode_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// 去掉 HTML 标签、解码实体并压平空白。
fn strip_html(input: &str) -> String {
    static TAG: OnceLock<Regex> = OnceLock::new();
    let tag = TAG.get_or_init(|| Regex::new(r"(?s)<[^>]*>").expect("valid html tag regex"));
    // 先解码实体（晋江简介里的换行是 `&lt;br/&gt;`，解码后才成为真正的标签），
    // 再去除标签，否则 `<br/>` 会残留在正文里。
    let decoded = decode_entities(input);
    let no_tags = tag.replace_all(&decoded, " ");
    no_tags.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 典型乱码特征：GBK 字节被当成 UTF-8 解码后，会大量落在拉丁扩展/希腊/西里尔/
/// 组合附加符号区。合法书名里的 `·`、`—`、`…`、全角标点等不在此列。
fn looks_like_mojibake(s: &str) -> bool {
    s.chars().any(|c| {
        let u = c as u32;
        ((0x0080..=0x02FF).contains(&u) && u != 0x00B7) // · 是合法书名分隔符
            || (0x0300..=0x036F).contains(&u) // 组合附加符号
            || (0x0370..=0x03FF).contains(&u) // 希腊字母
            || (0x0400..=0x04FF).contains(&u) // 西里尔字母
    })
}

/// 解码一个非 ASCII 连续字节片段。
///
/// 判定顺序：
/// 1. 严格 UTF-8 且无乱码特征 → 直接用；
/// 2. 严格 UTF-8 失败但**大部分字节有效**（如页面个别字符被污染成 `?`）→
///    用 UTF-8 lossy 保住可读前缀，避免整段误判为 GBK 后变成乱码；
/// 3. 其余情况按 GBK 解码。
fn decode_text_run(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) if !looks_like_mojibake(s) => return s.to_string(),
        Ok(_) => {}
        Err(e) => {
            let valid_ratio = e.valid_up_to() as f64 / bytes.len().max(1) as f64;
            let lossy = String::from_utf8_lossy(bytes).into_owned();
            let has_cjk = lossy
                .chars()
                .any(|c| (0x4E00..=0x9FFF).contains(&(c as u32)));
            if valid_ratio >= 0.6 && has_cjk {
                return lossy;
            }
        }
    }
    let (gbk, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if !had_errors {
        return gbk.into_owned();
    }
    // 两者都不理想时退回 UTF-8 lossy（至少保留可读部分）。
    String::from_utf8_lossy(bytes).into_owned()
}

/// 混合编码解码：晋江搜索页实测「书名 UTF-8 + 作者/简介/模板 GBK」混排，
/// 按非 ASCII 连续片段逐段选择编码（HTML 标签本身是 ASCII，天然作为分隔符）。
pub(crate) fn decode_mixed_html(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut run: Vec<u8> = Vec::new();
    for &b in bytes {
        if b.is_ascii() {
            if !run.is_empty() {
                out.push_str(&decode_text_run(&run));
                run.clear();
            }
            out.push(b as char);
        } else {
            run.push(b);
        }
    }
    if !run.is_empty() {
        out.push_str(&decode_text_run(&run));
    }
    out
}

/// 解析晋江搜索页 HTML（已解码为 UTF-8 的字符串）。
///
/// 页面结构（来自真实抓包）：
/// ```html
/// <h3 class="title"><a ... href="http://www.jjwxc.net/onebook.php?novelid=7668975">书名</a>...</h3>
/// <div class="intro">简介</div>
/// <div class="info">作者：<a href=".../oneauthor.php?authorid=5370394"><span>作者名</span></a> ...</div>
/// ```
pub(crate) fn parse_search_html(html: &str) -> Vec<OfficialBook> {
    static ITEM: OnceLock<Regex> = OnceLock::new();
    static INTRO: OnceLock<Regex> = OnceLock::new();
    static AUTHOR: OnceLock<Regex> = OnceLock::new();

    let item_re = ITEM.get_or_init(|| {
        Regex::new(
            r#"(?is)<h3[^>]*class="title"[^>]*>\s*<a\b[^>]*?href="([^"]*onebook\.php\?novelid=(\d+)[^"]*)"[^>]*>(.*?)</a>"#,
        )
        .expect("valid jjwxc item regex")
    });
    let intro_re = INTRO.get_or_init(|| {
        Regex::new(r#"(?is)<div[^>]*class="intro"[^>]*>(.*?)</div>"#).expect("intro")
    });
    let author_re = AUTHOR.get_or_init(|| {
        Regex::new(r#"(?is)oneauthor\.php\?authorid=\d+"[^>]*>(.*?)</a>"#).expect("author")
    });

    let anchors: Vec<(usize, String, String)> = item_re
        .captures_iter(html)
        .filter_map(|caps| {
            let start = caps.get(0)?.start();
            let id = caps.get(2)?.as_str().to_string();
            let raw_title = caps.get(3)?.as_str();
            Some((start, id, strip_html(raw_title)))
        })
        .collect();

    let mut out: Vec<OfficialBook> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (idx, (start, id, title)) in anchors.iter().enumerate() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let end = anchors
            .get(idx + 1)
            .map(|(next, _, _)| *next)
            .unwrap_or(html.len());
        let block = &html[*start..end];
        let intro = intro_re
            .captures(block)
            .map(|c| strip_html(c.get(1).map(|m| m.as_str()).unwrap_or("")))
            .unwrap_or_default();
        let author = author_re
            .captures(block)
            .map(|c| strip_html(c.get(1).map(|m| m.as_str()).unwrap_or("")))
            .unwrap_or_default();
        out.push(OfficialBook {
            official_id: id.clone(),
            title: title.clone(),
            author,
            intro_short: intro,
            intro_full: None,
            tags: None,
            cover_url: None,
            category: None,
            word_count: None,
            chapter_count: None,
            finished: None,
            score: None,
            url: format!("{BOOK_URL}{id}"),
        });
    }
    out
}

/// 解析详情接口 JSON。
pub(crate) fn parse_detail_json(v: &Value) -> Result<OfficialBook> {
    let id = v
        .get("novelId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("详情响应缺少 novelId"))?
        .to_string();
    let intro_full = v
        .get("novelIntro")
        .and_then(Value::as_str)
        .map(strip_html)
        .filter(|s| !s.trim().is_empty());

    let digits = |s: &str| -> Option<u64> {
        s.chars()
            .filter(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u64>()
            .ok()
    };

    Ok(OfficialBook {
        url: format!("{BOOK_URL}{id}"),
        official_id: id,
        title: v
            .get("novelName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        author: v
            .get("authorName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        intro_short: v
            .get("novelIntroShort")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        intro_full,
        tags: v
            .get("novelTags")
            .and_then(Value::as_str)
            .map(str::to_string),
        cover_url: v
            .get("novelCover")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string),
        category: v
            .get("novelClass")
            .and_then(Value::as_str)
            .map(str::to_string),
        word_count: v.get("novelSize").and_then(Value::as_str).and_then(digits),
        chapter_count: v
            .get("novelChapterCount")
            .and_then(Value::as_str)
            .and_then(digits)
            .and_then(|n| u32::try_from(n).ok()),
        finished: v
            .get("novelStep")
            .and_then(Value::as_str)
            .map(|s| s.trim() == "2"),
        score: v
            .get("novelReviewScore")
            .and_then(Value::as_str)
            .and_then(|s| s.trim().trim_end_matches('分').trim().parse::<f64>().ok()),
    })
}

/// 执行一次官方搜索。`page` 从 1 起；晋江结果页分页参数为 `p`。
fn search_impl(keyword: &str, page: u32, timeout_secs: u64) -> Result<(Vec<OfficialBook>, bool)> {
    let page = page.max(1);
    let mut url = format!("{SEARCH_URL}?kw={}&t=1", urlencode_query(keyword));
    if page > 1 {
        url.push_str(&format!("&p={page}"));
    }
    throttle();
    let resp = http_client(timeout_secs)?
        .get(&url)
        .send()
        .context("请求晋江搜索页失败")?;
    if !resp.status().is_success() {
        return Err(anyhow!("晋江搜索页返回 HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().context("读取晋江搜索页失败")?;
    // 页面 meta 声明 gb18030，但实测为混合编码（书名 UTF-8、作者/简介 GBK），
    // 逐文本片段自适应解码（见 decode_mixed_html 注释）。
    let text = decode_mixed_html(&bytes);
    let books = parse_search_html(&text);
    // 结果页每页约 20 条；只有整页结果才认为可能有下一页。
    let has_more = books.len() >= 20;
    Ok((books, has_more))
}

/// 拉取单本书详情（完整简介/标签/封面）。
fn fetch_detail_impl(official_id: &str, timeout_secs: u64) -> Result<OfficialBook> {
    let url = format!("{DETAIL_URL}?novelId={}", urlencode(official_id));
    throttle();
    let resp = http_client(timeout_secs)?
        .get(&url)
        .send()
        .context("请求晋江详情接口失败")?;
    if !resp.status().is_success() {
        return Err(anyhow!("晋江详情接口返回 HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().context("读取晋江详情响应失败")?;
    // 详情接口为 UTF-8 JSON（抓包实测 content-type: text/plain;charset=utf-8）。
    let text = String::from_utf8_lossy(&bytes);
    let value: Value = serde_json::from_str(&text).context("解析晋江详情 JSON 失败")?;
    parse_detail_json(&value)
}

/// 晋江官方源（零大小适配器）。
pub struct JjwxcOfficial;

impl OfficialSource for JjwxcOfficial {
    fn id(&self) -> &'static str {
        "jjwxc"
    }

    fn display_name(&self) -> &'static str {
        "晋江"
    }

    fn search(
        &self,
        keyword: &str,
        page: u32,
        timeout_secs: u64,
    ) -> Result<(Vec<OfficialBook>, bool)> {
        search_impl(keyword, page, timeout_secs)
    }

    fn fetch_detail(&self, official_id: &str, timeout_secs: u64) -> Result<OfficialBook> {
        fetch_detail_impl(official_id, timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        <h3 class="title"><a data-recommendInfo='{"x":1}' class="track" href="http://www.jjwxc.net/onebook.php?novelid=7668975"
             target="_blank">故国秋&nbsp;色</a><font size='-2' color='gray'>(2023-01-02)</font></h3>
        <div class="intro">
            在一个架空的王朝，落魄公子的复仇计划和与帝王的虐恋情深
        </div>
        <div class="info">
            作者：<a href="http://www.jjwxc.net/oneauthor.php?authorid=5370394" target="_blank">
                <span style="color: #3B9D3B; ">霍息</span>
            </a> 类型：原创-纯爱-架空历史-爱情
        </div>
        <h3 class="title"><a class="track" href="https://www.jjwxc.net/onebook.php?novelid=4449074" target="_blank">救了病娇后他总想杀我[穿书]</a></h3>
        <div class="intro">病娇残忍嗜血但很乖</div>
        <div class="info">作者：<a href="http://www.jjwxc.net/oneauthor.php?authorid=2337435"><span>星棘</span></a></div>
    "#;

    #[test]
    fn parse_search_html_extracts_fields() {
        let books = parse_search_html(SAMPLE);
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].official_id, "7668975");
        assert_eq!(books[0].title, "故国秋 色");
        assert_eq!(books[0].author, "霍息");
        assert!(books[0].intro_short.contains("复仇计划"));
        assert_eq!(books[1].official_id, "4449074");
        assert_eq!(books[1].author, "星棘");
    }

    #[test]
    fn decode_mixed_handles_utf8_title_and_gbk_author() {
        // 真实页面混排：书名 UTF-8，作者/简介 GBK。
        let mut bytes = Vec::new();
        bytes.extend_from_slice("救了病娇".as_bytes());
        bytes.push(b'<');
        let (gbk_author, _, _) = encoding_rs::GBK.encode("星棘");
        bytes.extend_from_slice(&gbk_author);
        bytes.push(b'>');
        assert_eq!(
            decode_mixed_html(&bytes),
            "救了病娇<星棘>",
            "混合编码片段应分别用 UTF-8 / GBK 正确解码"
        );
    }

    #[test]
    fn parse_detail_json_decodes_escaped_intro() {
        let v: Value = serde_json::from_str(
            r#"{"novelId":"4449074","novelName":"救了他","authorName":"星棘",
                "novelIntro":"A&lt;br/&gt;B&lt;br/&gt;C","novelTags":"甜文,穿书",
                "novelCover":"https://x/cover.jpg"}"#,
        )
        .unwrap();
        let b = parse_detail_json(&v).unwrap();
        assert_eq!(b.official_id, "4449074");
        assert_eq!(b.title, "救了他");
        assert!(b.intro_for_match().contains("A B C"));
        assert_eq!(b.tags.as_deref(), Some("甜文,穿书"));
    }

    // ── 真实端到端（非重放）用例：默认 ignore，需网络时手动跑 ──
    #[test]
    #[ignore = "真实网络请求：cargo test --bins jjwxc_live_search -- --ignored"]
    fn jjwxc_live_search() {
        let (books, _more) = search_impl("救了病娇", 1, 20).expect("晋江搜索真实请求应成功");
        assert!(!books.is_empty(), "应至少解析出一条晋江结果");
        assert!(
            books
                .iter()
                .any(|b| !b.title.is_empty() && !b.author.is_empty()),
            "解析结果应包含书名与作者"
        );
    }

    #[test]
    #[ignore = "真实网络请求：cargo test --bins jjwxc_live_detail -- --ignored"]
    fn jjwxc_live_detail() {
        let b = fetch_detail_impl("4449074", 20).expect("晋江详情真实请求应成功");
        assert_eq!(b.official_id, "4449074");
        assert!(!b.intro_for_match().is_empty(), "详情应包含简介");
        assert!(b.tags.is_some(), "详情应包含标签");
    }
}
