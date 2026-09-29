//! 番茄小说直连 API 客户端（签名-only sidecar 模式）。
//!
//! 流程：
//! 1. Rust 构建 URL + headers（设备指纹参数）
//! 2. 调 unidbg sidecar 生成签名头（X-Helios / X-Medusa 等）
//! 3. Rust 直接请求 `api5-normal-sinfonlineb.fqnovel.com`
//! 4. Rust 解密响应（AES-128-CBC + gzip）
//!
//! 需要用户先启动 unidbg-boot-server（端口 8099）。

use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::fq_crypto;
use super::unidbg_signer::UnidbgSigner;
use crate::base_system::json_extract::to_public_jpeg_cover;

/// API 基地址。
const API_BASE: &str = "https://api5-normal-sinfonlineb.fqnovel.com";

// ── 设备指纹（与 unidbg 端 FQApiProperties 默认值一致）──────────────────────

const AID: &str = "1967";
const VERSION_CODE: &str = "68132";
const VERSION_NAME: &str = "6.8.1.32";
const INSTALL_ID: &str = "933935730456617";
const DEVICE_ID: &str = "933935730452521";
const CDID: &str = "17f05006-423a-4172-be4b-7d26a42f2f4a";
const DEVICE_TYPE: &str = "OnePlus11";
const DEVICE_BRAND: &str = "OnePlus";
const ROM_VERSION: &str = "V291IR+release-keys";
const RESOLUTION: &str = "3200*1440";
const DPI: &str = "640";
const HOST_ABI: &str = "arm64-v8a";
const USER_AGENT: &str = "com.dragon.read.oversea.gp/68132 (Linux; U; Android 10; zh_CN; OnePlus11; Build/V291IR;tt-ok/3.12.13.4-tiktok)";
const COOKIE: &str = "store-region=cn-zj; store-region-src=did; install_id=933935730456617";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 单批正文请求的章节数，可用 `UNIFIED_FQ_BATCH_CHUNK` 覆盖（限制 1..=50）。
/// 默认 25：与 `build_dynamic_chapter_groups` 的组上限(MAX_DYNAMIC_GROUP_SIZE)对齐，
/// 使「一个下载组 = 一次 sidecar 往返」，尽量减少 HTTP/签名往返次数。
/// 调小可让设备轮换/失败重试的粒度更细，但会增加往返数。
fn fq_batch_chunk() -> usize {
    crate::base_system::env_first(&["UNIFIED_FQ_BATCH_CHUNK", "TOMATO_FQ_BATCH_CHUNK"])
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|v| v.clamp(1, 50))
        .unwrap_or(25)
}

/// 构建通用 API 参数（30+ 个设备指纹参数）。
fn build_common_params() -> Vec<(String, String)> {
    let rticket = now_ms().to_string();
    vec![
        ("iid".into(), INSTALL_ID.into()),
        ("device_id".into(), DEVICE_ID.into()),
        ("ac".into(), "wifi".into()),
        ("channel".into(), "googleplay".into()),
        ("aid".into(), AID.into()),
        ("app_name".into(), "novelapp".into()),
        ("version_code".into(), VERSION_CODE.into()),
        ("version_name".into(), VERSION_NAME.into()),
        ("device_platform".into(), "android".into()),
        ("os".into(), "android".into()),
        ("ssmix".into(), "a".into()),
        ("device_type".into(), DEVICE_TYPE.into()),
        ("device_brand".into(), DEVICE_BRAND.into()),
        ("language".into(), "zh".into()),
        ("os_api".into(), "32".into()),
        ("os_version".into(), "10".into()),
        ("manifest_version_code".into(), VERSION_CODE.into()),
        ("resolution".into(), RESOLUTION.into()),
        ("dpi".into(), DPI.into()),
        ("update_version_code".into(), VERSION_CODE.into()),
        ("_rticket".into(), rticket),
        ("host_abi".into(), HOST_ABI.into()),
        ("dragon_device_type".into(), "phone".into()),
        ("pv_player".into(), VERSION_CODE.into()),
        ("compliance_status".into(), "0".into()),
        ("need_personal_recommend".into(), "1".into()),
        ("player_so_load".into(), "1".into()),
        ("is_android_pad_screen".into(), "0".into()),
        ("rom_version".into(), ROM_VERSION.into()),
        ("cdid".into(), CDID.into()),
    ]
}

/// 构建通用请求头。
fn build_common_headers() -> HashMap<String, String> {
    let now = now_ms();
    let mut h = HashMap::new();
    h.insert("Cookie".into(), COOKIE.into());
    h.insert("User-Agent".into(), USER_AGENT.into());
    h.insert(
        "Accept".into(),
        "application/json; charset=utf-8,application/x-protobuf".into(),
    );
    h.insert("Accept-Encoding".into(), "gzip".into());
    h.insert("x-xs-from-web".into(), "0".into());
    h.insert("x-ss-req-ticket".into(), now.to_string());
    // x-reading-request = timestamp + "-" + random int
    let rand_part = (now % 2_000_000_000) as i32;
    h.insert("x-reading-request".into(), format!("{}-{}", now, rand_part));
    h.insert("x-vc-bdturing-sdk-version".into(), "3.7.2.cn".into());
    h.insert("lc".into(), "101".into());
    h.insert("sdk-version".into(), "2".into());
    h.insert("passport-sdk-version".into(), "50564".into());
    h.insert("x-tt-store-region".into(), "cn-zj".into());
    h.insert("x-tt-store-region-src".into(), "did".into());
    h
}

/// 将参数列表拼接为 query string（不对值做 URL 编码，与 Java 端 buildUrlWithParams 一致）。
fn join_params(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// 简单 URL 编码（与 Java URLEncoder.encode 一致，空格→+）。
#[allow(dead_code)]
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 番茄直连客户端。
pub(crate) struct FqApiClient {
    http: Client,
    signer: UnidbgSigner,
    // 缓存 registerkey 结果
    cached_key: Mutex<Option<(String, i64, u64)>>, // (key_hex, keyver, timestamp_ms)
}

impl FqApiClient {
    pub(crate) fn new(signer_url: &str, timeout_ms: u64) -> Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_millis(timeout_ms.max(100)))
            .gzip(true)
            .build()?;
        let signer = UnidbgSigner::new(signer_url, timeout_ms)?;
        Ok(Self {
            http,
            signer,
            cached_key: Mutex::new(None),
        })
    }

    /// 获取 registerkey 解密密钥（带缓存，5 分钟过期）。
    pub(crate) fn get_decryption_key(&self) -> Result<(String, i64)> {
        {
            let cache = self.cached_key.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((ref key, keyver, ts)) = *cache {
                let age = now_ms().saturating_sub(ts);
                if age < 300_000 {
                    // 5 min
                    return Ok((key.clone(), keyver));
                }
            }
        }
        let (key, keyver) = self.fetch_register_key()?;
        *self.cached_key.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((key.clone(), keyver, now_ms()));
        Ok((key, keyver))
    }

    /// 请求 registerkey API 并解密。
    pub(crate) fn fetch_register_key(&self) -> Result<(String, i64)> {
        let url = format!("{API_BASE}/reading/crypt/registerkey");
        let params = build_common_params();
        let full_url = format!("{}?{}", url, join_params(&params));

        let mut headers = build_common_headers();
        headers.insert("Content-Type".into(), "application/json".into());

        // 签名
        let sig = self.signer.sign(&full_url, &headers)?;
        headers.extend(sig);

        // POST body
        let content = fq_crypto::new_register_key_content(DEVICE_ID)?;
        let body = serde_json::json!({
            "content": content,
            "keyver": 1,
        });

        // 逐个设置签名+通用头
        let mut req = self.http.post(&full_url).json(&body);
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        let resp = req.send().context("registerkey request failed")?;
        if !resp.status().is_success() {
            return Err(anyhow!("registerkey HTTP {}", resp.status()));
        }
        let v: Value = resp.json().context("registerkey parse JSON failed")?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(anyhow!(
                "registerkey code={code}: {}",
                v.get("message").and_then(|m| m.as_str()).unwrap_or("")
            ));
        }
        let data = v
            .get("data")
            .ok_or_else(|| anyhow!("registerkey: no data field"))?;
        let encrypted_key = data
            .get("key")
            .and_then(|k| k.as_str())
            .ok_or_else(|| anyhow!("registerkey: no key field"))?;
        let keyver = data.get("keyver").and_then(|k| k.as_i64()).unwrap_or(0);

        let key_hex = fq_crypto::get_real_key(encrypted_key)?;
        tracing::info!("registerkey 成功: keyver={keyver}");
        Ok((key_hex, keyver))
    }

    /// 调用 batch_full API 获取章节内容（加密的），返回原始 JSON。
    #[allow(dead_code)]
    fn fetch_batch_full(&self, item_ids: &str, book_id: &str) -> Result<Value> {
        let url = format!("{API_BASE}/reading/reader/batch_full/v");
        let mut params = build_common_params();
        params.push(("item_ids".into(), item_ids.into()));
        params.push(("key_register_ts".into(), "0".into()));
        params.push(("book_id".into(), book_id.into()));
        params.push(("req_type".into(), "1".into()));
        let full_url = format!("{}?{}", url, join_params(&params));

        let headers = build_common_headers();
        let sig = self.signer.sign(&full_url, &headers)?;
        let mut req = self.http.get(&full_url);
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        for (k, v) in &sig {
            req = req.header(k, v);
        }
        let resp = req.send().context("batch_full request failed")?;
        if !resp.status().is_success() {
            return Err(anyhow!("batch_full HTTP {}", resp.status()));
        }
        let v: Value = resp.json().context("batch_full parse JSON failed")?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(anyhow!(
                "batch_full code={code}: {}",
                v.get("message").and_then(|m| m.as_str()).unwrap_or("")
            ));
        }
        Ok(v)
    }

    /// 通过 unidbg sidecar 的批量正文端点获取章节内容（设备轮换/解密由 sidecar 内部完成）。
    ///
    /// 返回格式与旧实现兼容：`{"data": {item_id: {"content": html, "title": title}, ...}}`。
    /// `item_ids` 逗号分隔。空内容/失败按设备轮换退避重试（给 sidecar 换设备的机会）。
    pub(crate) fn get_contents(&self, item_ids: &str, book_id: &str) -> Result<Value> {
        let ids: Vec<String> = item_ids
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        if ids.is_empty() {
            return Err(anyhow!("get_contents: empty item_ids"));
        }

        // 分块请求 sidecar，避免单请求章节过多；命中拒绝/空内容时退避重试，
        // 让 sidecar 内部 nextDevice() 轮换到新设备。单批章节数见 fq_batch_chunk()（默认 25）。
        let chunk = fq_batch_chunk();
        const RETRY: usize = 3;

        let mut out = serde_json::Map::new();
        let mut last_err: Option<String> = None;
        for part in ids.chunks(chunk) {
            let mut ok = false;
            for attempt in 0..RETRY {
                match self.fetch_chapters_via_sidecar(part, book_id) {
                    Ok(map) => {
                        for (k, v) in map {
                            out.insert(k, v);
                        }
                        ok = true;
                        break;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        tracing::warn!(
                            "sidecar 正文获取失败(第 {} 次, {} 章): {}",
                            attempt + 1,
                            part.len(),
                            msg
                        );
                        last_err = Some(msg);
                        std::thread::sleep(std::time::Duration::from_millis(
                            800 * (attempt as u64 + 1),
                        ));
                    }
                }
            }
            if !ok {
                return Err(match last_err {
                    Some(m) => anyhow!("sidecar 正文获取失败(已重试 {RETRY} 次): {m}"),
                    None => anyhow!("sidecar 正文获取失败"),
                });
            }
        }

        if out.is_empty() {
            return Err(anyhow!("sidecar 返回章节内容为空"));
        }
        Ok(serde_json::json!({"data": out}))
    }

    /// 调用 sidecar `POST /api/fqnovel/chapters/batch`，解析章节内容为
    /// `Map<item_id, {"content","title"}>`。设备轮换与解密在 sidecar 内部完成。
    fn fetch_chapters_via_sidecar(
        &self,
        ids: &[String],
        book_id: &str,
    ) -> Result<serde_json::Map<String, Value>> {
        let endpoint = format!("{}/api/fqnovel/chapters/batch", self.signer.base_url());
        let body = serde_json::json!({
            "bookId": book_id,
            "chapterIds": ids,
        });
        let resp = self
            .http
            .post(&endpoint)
            .json(&body)
            .send()
            .context("sidecar chapters request failed")?;
        if !resp.status().is_success() {
            return Err(anyhow!("sidecar chapters HTTP {}", resp.status()));
        }
        let v: Value = resp.json().context("sidecar chapters parse failed")?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(anyhow!(
                "sidecar chapters code={code}: {}",
                v.get("message").and_then(|m| m.as_str()).unwrap_or("")
            ));
        }
        let chapters = v
            .get("data")
            .and_then(|d| d.get("chapters"))
            .and_then(|c| c.as_object())
            .ok_or_else(|| anyhow!("sidecar chapters: no data.chapters"))?;

        let mut map = serde_json::Map::new();
        for (id, info) in chapters {
            let raw = info
                .get("rawContent")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let txt = info
                .get("txtContent")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let name = info
                .get("chapterName")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let content = if !raw.is_empty() { raw } else { txt };
            if content.is_empty() || content == "Invalid" {
                continue;
            }
            map.insert(
                id.clone(),
                serde_json::json!({"content": content, "title": name}),
            );
        }
        if map.is_empty() {
            return Err(anyhow!("sidecar chapters: all empty (device rejected?)"));
        }
        Ok(map)
    }

    /// 健康检查（signer 可达）。
    #[allow(dead_code)]
    pub(crate) fn health(&self) -> Result<bool> {
        self.signer.health()
    }

    /// 直连签名搜索（可指定分类 tab_type、筛选 selected_items、分页 offset），
    /// 返回 `{items, tabs, selector, has_more, next_offset, tab_type}`。
    ///
    /// 分类 tab 与筛选均为无状态单请求（实测无需会话 search_id），一次响应即包含
    /// 全部分类 tab 的元数据与筛选器（selector.rows），供前端构建工具栏。
    pub(crate) fn search_enriched(
        &self,
        query: &str,
        tab_type: i64,
        selected_items: Option<&str>,
        offset: usize,
    ) -> Result<Value> {
        let kw = query.trim();
        if kw.is_empty() {
            return Ok(json!({ "items": [], "tabs": [] }));
        }
        let tt = if tab_type == 0 { 1 } else { tab_type };
        let url = format!("{API_BASE}/reading/bookapi/search/tab/v");
        let mut params = build_common_params();
        params.extend([
            ("query".into(), url_encode(kw)),
            ("tab_name".into(), url_encode(tab_name(tt))),
            ("tab_type".into(), tt.to_string()),
            ("user_is_login".into(), "0".into()),
            (
                "bookstore_tab".into(),
                if tt == 3 { "0".into() } else { "2".into() },
            ),
            ("offset".into(), offset.to_string()),
            ("count".into(), "20".into()),
            ("search_source".into(), "1".into()),
            ("bookshelf_search_plan".into(), "4".into()),
        ]);
        if let Some(si) = selected_items.map(str::trim).filter(|s| !s.is_empty()) {
            params.push(("selected_items".into(), si.to_string()));
        }
        let full_url = format!("{}?{}", url, join_params(&params));

        let want_fallback = tt == 1
            && offset == 0
            && selected_items
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none();
        const RETRY: usize = 3;
        let mut last_err = String::new();
        for attempt in 0..RETRY {
            match self.try_signed_search(&full_url) {
                Ok(v) => {
                    let items = parse_tab_items(&v, tt);
                    // 综合无结果且允许回退时，跳出走 sidecar；否则直接返回（含空列表）。
                    if !items.is_empty() || !want_fallback {
                        let (has_more, next_offset) = tab_pagination(&v, tt);
                        return Ok(json!({
                            "items": items,
                            "tabs": build_tabs_meta(&v),
                            "selector": build_selector(&v),
                            "has_more": has_more,
                            "next_offset": next_offset,
                            "tab_type": tt,
                        }));
                    }
                    last_err = "综合直连返回空".to_string();
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                    tracing::warn!("番茄直连搜索失败(第 {} 次): {}", attempt + 1, last_err);
                    std::thread::sleep(std::time::Duration::from_millis(
                        600 * (attempt as u64 + 1),
                    ));
                }
            }
        }
        // 仅「综合、无筛选、首页」回退到 sidecar（sidecar 不支持分类/筛选）。
        if want_fallback {
            tracing::warn!("番茄直连综合搜索失败/空({last_err})，回退 sidecar");
            let items = self.search_books_via_sidecar(kw)?;
            return Ok(json!({
                "items": items,
                "tabs": [],
                "selector": Value::Null,
                "has_more": false,
                "next_offset": 0,
                "tab_type": 1,
            }));
        }
        Err(anyhow!("番茄直连搜索失败(已重试 {RETRY} 次): {last_err}"))
    }

    /// 签名 + 直连请求一次，返回原始 JSON。
    fn try_signed_search(&self, full_url: &str) -> Result<Value> {
        let headers = build_common_headers();
        let sig = self.signer.sign(full_url, &headers)?;
        let mut req = self.http.get(full_url);
        for (k, v) in headers.iter().chain(sig.iter()) {
            req = req.header(k, v);
        }
        let resp = req.send().context("search/tab request failed")?;
        if !resp.status().is_success() {
            return Err(anyhow!("search/tab HTTP {}", resp.status()));
        }
        let v: Value = resp.json().context("search/tab parse JSON failed")?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(anyhow!(
                "search/tab code={code}: {}",
                v.get("message").and_then(|m| m.as_str()).unwrap_or("")
            ));
        }
        Ok(v)
    }

    /// 通过已部署 sidecar 的 `/api/fqsearch/books` 搜索（字段较薄，无绝对封面）。
    fn search_books_via_sidecar(&self, query: &str) -> Result<Vec<Value>> {
        let endpoint = format!("{}/api/fqsearch/books", self.signer.base_url());
        let resp = self
            .http
            .get(&endpoint)
            .query(&[
                ("query", query),
                ("tabType", "1"),
                ("offset", "0"),
                ("count", "20"),
            ])
            .send()
            .context("search request failed")?;
        if !resp.status().is_success() {
            return Err(anyhow!("search HTTP {}", resp.status()));
        }
        let v: Value = resp.json().context("search parse JSON failed")?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(anyhow!(
                "search code={code}: {}",
                v.get("message").and_then(|m| m.as_str()).unwrap_or("")
            ));
        }
        let books = v
            .get("data")
            .and_then(|d| d.get("books"))
            .and_then(|b| b.as_array())
            .ok_or_else(|| anyhow!("search: no data.books array"))?;
        let items: Vec<Value> = books
            .iter()
            .map(|b| {
                json!({
                    "book_id": b.get("bookId").and_then(|v| v.as_str()).unwrap_or(""),
                    "title": b.get("bookName").and_then(|v| v.as_str()).unwrap_or(""),
                    "author": b.get("author").and_then(|v| v.as_str()).unwrap_or(""),
                    "raw": b,
                })
            })
            .collect();
        Ok(items)
    }
}

/// 从 cell 里取第一条 `book_data`（可能是数组或对象）。
fn first_book_data(cell: &Value) -> Option<&Value> {
    match cell.get("book_data") {
        Some(Value::Array(a)) => a.iter().find(|x| x.is_object()),
        Some(v) if v.is_object() => Some(v),
        _ => None,
    }
}

/// 从 book_data 构造浏览器可渲染的封面 URL：优先用 `thumb_uri` 拼公网免签 JPEG 镜像，
/// 否则回退到绝对 `thumb_url` 并把 HEIC 归一化为 JPEG。
fn fq_cover_url(bd: &Value) -> Option<String> {
    if let Some(uri) = bd.get("thumb_uri").and_then(|x| x.as_str()) {
        let uri = uri.trim().trim_matches('/');
        // thumb_uri 可能是 `novel-pic/<hash>` 或纯 `<hash>`（无目录前缀），
        // 两者在公网 byteimg 都有免签 JPEG 镜像，故不能要求必须含 '/'。
        if !uri.is_empty() && !uri.contains("//") && !uri.contains("..") {
            return Some(format!(
                "https://p6-novel.byteimg.com/{uri}~tplv-shrink:360:0.jpeg"
            ));
        }
    }
    bd.get("thumb_url")
        .and_then(|x| x.as_str())
        .map(to_public_jpeg_cover)
        .filter(|u| u.starts_with("http"))
}

/// tab_type → 请求时传的 tab_name（中文名，需百分号编码；综合用 store）。
fn tab_name(tt: i64) -> &'static str {
    match tt {
        1 => "store",
        2 => "听书",
        3 => "书籍",
        4 => "社区",
        5 => "全文",
        6 => "用户",
        8 => "漫画",
        11 => "短剧",
        13 => "买书",
        19 => "漫剧",
        _ => "store",
    }
}

/// 从 search_tabs 中找到目标 tab（按 tab_type；否则退化为首个有数据的 tab）。
fn find_tab(v: &Value, want: i64) -> Option<&Value> {
    let tabs = v.get("search_tabs").and_then(|x| x.as_array())?;
    tabs.iter()
        .find(|t| t.get("tab_type").and_then(|x| x.as_i64()) == Some(want))
        .or_else(|| {
            tabs.iter().find(|t| {
                t.get("data")
                    .and_then(|d| d.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
            })
        })
        .or_else(|| tabs.first())
}

/// 目标 tab 的书籍 cell → 富字段 item 列表。
fn parse_tab_items(v: &Value, want: i64) -> Vec<Value> {
    let Some(cells) = find_tab(v, want)
        .and_then(|t| t.get("data"))
        .and_then(|d| d.as_array())
    else {
        return Vec::new();
    };
    let mut items: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for cell in cells {
        // cell 有两种形态：网文/听书用 book_data，短剧/漫剧用 video_data（上游 tab=11/19 返回后者）。
        // 早期只认 book_data，导致短剧/漫剧 tab 明明各有 20 条数据却全被丢弃（表现为“0 条”）。
        let Some(bd) = first_book_data(cell) else {
            if let Some(vd) = first_video_data(cell) {
                push_video_item(&mut items, &mut seen, vd, cell, want);
            }
            continue;
        };
        let book_id = bd
            .get("book_id")
            .and_then(|x| x.as_str())
            .or_else(|| cell.get("book_id").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string();
        if book_id.is_empty() || !seen.insert(book_id.clone()) {
            continue;
        }
        let description = bd
            .get("abstract")
            .and_then(|x| x.as_str())
            .or_else(|| bd.get("book_abstract_v2").and_then(|x| x.as_str()))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let tags: Vec<String> = bd
            .get("tags")
            .and_then(|x| x.as_str())
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let finished = match json_i64(bd.get("creation_status")) {
            Some(0) => Some(true),
            Some(1) => Some(false),
            _ => None,
        };
        items.push(json!({
            "book_id": book_id,
            "title": bd.get("book_name").and_then(|x| x.as_str()).unwrap_or(""),
            "author": bd.get("author").and_then(|x| x.as_str()).unwrap_or(""),
            "cover_url": fq_cover_url(bd),
            "description": description,
            "score": bd.get("score").cloned().unwrap_or(Value::Null),
            "category": bd.get("category").and_then(|x| x.as_str()),
            "tags": tags,
            "word_count": json_u64(bd.get("word_number")),
            "chapter_count": json_u64(bd.get("serial_count")),
            "finished": finished,
            "read_count_text": bd.get("read_cnt_text").and_then(|x| x.as_str()),
            // 品类标识：book_type=="1" 为听书/音频（实测此类书在番茄 Web 站常无 /page/ 书页），
            // "0" 为网文小说。仅按上游字段分类，不做任何推断。
            "content_kind": content_kind_of(bd),
            "raw": bd,
        }));
    }
    items
}

/// book_data 的品类：book_type 可能为字符串或数字。
fn content_kind_of(bd: &Value) -> &'static str {
    let bt = match bd.get("book_type") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    if bt == "1" { "audio" } else { "novel" }
}

/// 从 cell 里取第一条 `video_data`（短剧/漫剧条目）。
fn first_video_data(cell: &Value) -> Option<&Value> {
    match cell.get("video_data") {
        Some(Value::Array(a)) => a.iter().find(|x| x.is_object()),
        Some(v) if v.is_object() => Some(v),
        _ => None,
    }
}

/// video_data（短剧/漫剧）→ 卡片 item。字段全部取自上游，缺失的一律 null、不推断：
/// video_data 无作者（`copyright` 是版权方不是作者）、无字数、无连载状态，故这些为 null；
/// `sub_title`（如“都市修真·复仇·全60集”）单独透出，不当作简介。
fn push_video_item(
    items: &mut Vec<Value>,
    seen: &mut std::collections::HashSet<String>,
    vd: &Value,
    cell: &Value,
    want: i64,
) {
    let book_id = vd
        .get("series_id")
        .and_then(|x| x.as_str())
        .or_else(|| cell.get("book_id").and_then(|x| x.as_str()))
        .unwrap_or("")
        .to_string();
    if book_id.is_empty() || !seen.insert(book_id.clone()) {
        return;
    }
    let title = vd
        .get("raw_book_name")
        .and_then(|x| x.as_str())
        .or_else(|| vd.get("title").and_then(|x| x.as_str()))
        .unwrap_or("");
    // 运营标签（“漫剧”/“热门”/“小说改编”）原样透出，不代表我们对其含义的判定。
    let tags: Vec<String> = vd
        .get("cover_tag_info_list")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("text").and_then(|x| x.as_str()))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let score = match vd.get("score") {
        Some(Value::Number(n)) => n.as_f64().map(|f| json!(f)).unwrap_or(Value::Null),
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .map(|f| json!(f))
            .unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let cover = vd
        .get("cover")
        .and_then(|x| x.as_str())
        .map(to_public_jpeg_cover)
        .filter(|u| u.starts_with("http"));
    // 品类中文名：只有从对应 tab 请求时才能确定（11=短剧、19=漫剧）；
    // 综合 tab 里混进的 video cell 无法区分，不下label，由前端给统一兑底文案。
    let kind_label = match want {
        11 => Some("短剧"),
        19 => Some("漫剧"),
        _ => None,
    };
    items.push(json!({
        "book_id": book_id,
        "title": title,
        "author": "",
        "cover_url": cover,
        "description": Value::Null,
        "sub_title": vd.get("sub_title").and_then(|x| x.as_str()),
        "score": score,
        "category": Value::Null,
        "tags": tags,
        "word_count": Value::Null,
        "chapter_count": json_u64(vd.get("episode_cnt")),
        "count_unit": "集",
        "finished": Value::Null,
        "read_count_text": vd.get("rec_text").and_then(|x| x.as_str()),
        "content_kind": "video",
        "kind_label": kind_label,
        "raw": vd,
    }));
}

/// 全部分类 tab 元数据：`[{tab_type,title,has_more,next_offset}]`。
fn build_tabs_meta(v: &Value) -> Vec<Value> {
    let Some(tabs) = v.get("search_tabs").and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    tabs.iter()
        .filter_map(|t| {
            let tt = t.get("tab_type").and_then(|x| x.as_i64())?;
            let title = t.get("title").and_then(|x| x.as_str()).unwrap_or("");
            Some(json!({
                "tab_type": tt,
                "title": title,
                "has_more": t.get("has_more").and_then(|x| x.as_bool()).unwrap_or(false),
                "next_offset": json_u64(t.get("next_offset")),
            }))
        })
        .collect()
}

/// 筛选器：优先取综合 tab（tab_type=1）的 selector，否则首个带 selector 的 tab。
fn build_selector(v: &Value) -> Option<Value> {
    let tabs = v.get("search_tabs").and_then(|x| x.as_array())?;
    let with = |t: &Value| t.get("selector").filter(|s| !s.is_null()).cloned();
    tabs.iter()
        .find(|t| t.get("tab_type").and_then(|x| x.as_i64()) == Some(1))
        .and_then(with)
        .or_else(|| tabs.iter().find_map(with))
}

/// 目标 tab 的分页信息 (has_more, next_offset)。
fn tab_pagination(v: &Value, want: i64) -> (bool, usize) {
    find_tab(v, want)
        .map(|t| {
            (
                t.get("has_more").and_then(|x| x.as_bool()).unwrap_or(false),
                json_u64(t.get("next_offset")).unwrap_or(0) as usize,
            )
        })
        .unwrap_or((false, 0))
}

/// 数字字段容错：上游可能以数字或字符串返回（如 `"1979646"`）。
fn json_u64(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn json_i64(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}
