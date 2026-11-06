use reqwest::blocking::Client;
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONNECTION, HeaderMap, HeaderValue, REFERER, USER_AGENT,
};
use std::sync::OnceLock;
use std::time::Duration;

/// 复用 HTTP Client，避免每次调用都重建导致连接池和 TLS 握手浪费。
fn shared_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| build_client(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        "https://fanqienovel.com/",
    ))
}

/// 移动端 UA 兜底客户端：部分站点（如姿势吧 zishiba.com）的 WAF 会直接**断开桌面 UA 的连接**
/// （连桌面浏览器访问也一样，见 `src/zishiba/mod.rs` 文档注释），只有移动端 UA 放行。
/// 因此首次（桌面 UA）请求失败时，用移动 UA 重试一次，两套指纹覆盖全部站点。
fn mobile_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| build_client(
        "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36",
        "https://fanqienovel.com/",
    ))
}

fn build_client(user_agent: &'static str, referer: &'static str) -> Client {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert(REFERER, HeaderValue::from_static(referer));
    headers.insert(USER_AGENT, HeaderValue::from_static(user_agent));
    Client::builder()
        .default_headers(headers)
        .build()
        .expect("failed to build shared HTTP client")
}

/// 晋江 CDN（i2-static.jjwxc.net 等）会拒绝番茄域名的 Referer（实测 403），
/// 单独使用晋江站内 Referer。
fn jjwxc_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| build_client(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        "https://www.jjwxc.net/",
    ))
}

pub(crate) fn fetch_bytes(url: &str, timeout: Duration) -> Option<Vec<u8>> {
    if url.trim().is_empty() {
        return None;
    }
    // 晋江封面走专用 Referer；其余站点保持原有桌面/移动 UA 双指纹策略。
    if url.contains("jjwxc.net") {
        return fetch_with(jjwxc_client(), url, timeout)
            .or_else(|| fetch_with(shared_client(), url, timeout))
            .or_else(|| fetch_with(mobile_client(), url, timeout));
    }
    fetch_with(shared_client(), url, timeout)
        // 桌面 UA 被站点 WAF 断连/拒绝时（如 zishiba.com），换移动 UA 重试一次。
        .or_else(|| fetch_with(mobile_client(), url, timeout))
}

fn fetch_with(client: &Client, url: &str, timeout: Duration) -> Option<Vec<u8>> {
    let resp = client.get(url).timeout(timeout).send().ok()?;
    let resp = resp.error_for_status().ok()?;
    let bytes = resp.bytes().ok()?;
    Some(bytes.to_vec())
}
