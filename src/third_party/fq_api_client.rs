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
    #[allow(dead_code)]
    fn get_decryption_key(&self) -> Result<(String, i64)> {
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
        // 让 sidecar 内部 nextDevice() 轮换到新设备。
        const CHUNK: usize = 8;
        const RETRY: usize = 3;

        let mut out = serde_json::Map::new();
        let mut last_err: Option<String> = None;
        for part in ids.chunks(CHUNK) {
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

    /// 搜索书籍（通过 sidecar 的 `/api/fqsearch/books` 端点）。
    ///
    /// 返回 `Vec<Value>`，每项包含 `book_id`、`title`、`author`、`raw` 字段，
    /// 与 official-api 的 `SearchClient::search_books` 返回格式兼容。
    pub(crate) fn search_books(&self, query: &str) -> Result<Vec<Value>> {
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
