//! 书籍 ID/链接解析与规范化。

use regex::Regex;
use std::sync::OnceLock;

static RE_URL: OnceLock<Regex> = OnceLock::new();
static RE_QS: OnceLock<Regex> = OnceLock::new();
static RE_PAGE: OnceLock<Regex> = OnceLock::new();
static RE_SHORT_LINK: OnceLock<Regex> = OnceLock::new();
static RE_ZLINK: OnceLock<Regex> = OnceLock::new();
static HTTP_CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();

/// Known domains that issue short-link share URLs of the form `/t/<token>`
/// or `/<token>` (zlink). The token is URL-safe and may contain `_` / `-`
/// in addition to letters and digits.
/// Only these hosts are followed during redirect resolution to prevent SSRF.
const ALLOWED_SHORT_LINK_HOSTS: &[&str] = &[
    "changdunovel.com",
    "www.changdunovel.com",
    "fanqienovel.com",
    "www.fanqienovel.com",
    "fqnovel.com",
    "www.fqnovel.com",
    "zlink.fqnovel.com",
];

fn re_url() -> &'static Regex {
    RE_URL.get_or_init(|| Regex::new(r"https?://\S+").expect("compile RE_URL"))
}

fn re_qs() -> &'static Regex {
    RE_QS.get_or_init(|| Regex::new(r"(?i)(book_id|bookId)=([0-9]+)").expect("compile RE_QS"))
}

fn re_page() -> &'static Regex {
    RE_PAGE.get_or_init(|| Regex::new(r"/page/(\d+)").expect("compile RE_PAGE"))
}

fn re_short_link() -> &'static Regex {
    RE_SHORT_LINK.get_or_init(|| {
        Regex::new(r"(?i)^https?://[^/\s]+/t/[A-Za-z0-9_-]+/?(?:[?#][^\s]*)?$")
            .expect("compile RE_SHORT_LINK")
    })
}

/// Matches zlink-style short URLs: `https://zlink.fqnovel.com/<token>`
/// (path is just `/<token>` without the `/t/` prefix used by other hosts).
fn re_zlink() -> &'static Regex {
    RE_ZLINK.get_or_init(|| {
        Regex::new(r"(?i)^https?://[^/\s]+/[A-Za-z0-9_-]+/?(?:[?#][^\s]*)?$")
            .expect("compile RE_ZLINK")
    })
}

fn http_client() -> &'static reqwest::blocking::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("build HTTP client for short-link resolution")
    })
}

/// Extracts the host (without port) from a URL string, lowercased.
fn url_host(url: &str) -> Option<String> {
    let after_scheme = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))?;
    let host_and_rest = after_scheme.split('/').next()?;
    // Strip port if present
    let host = host_and_rest.split(':').next()?;
    Some(host.to_lowercase())
}

pub fn parse_book_id(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Some(trimmed.to_string());
    }

    // 书旗（Shuqi）识别：sq: 前缀或 shuqi.com URL
    #[cfg(feature = "shuqi")]
    if let Some(id) = crate::shuqi::normalize_book_input(trimmed) {
        return Some(id);
    }

    // If user pasted extra text around the URL, try to extract URL first.
    let target = re_url()
        .find(trimmed)
        .map(|m| m.as_str())
        .unwrap_or(trimmed);

    if let Some(caps) = re_qs().captures(target) {
        return caps.get(2).map(|m| m.as_str().to_string());
    }

    if let Some(caps) = re_page().captures(target) {
        return caps.get(1).map(|m| m.as_str().to_string());
    }

    None
}

/// Returns `true` if `input` contains a short-redirect share link from a
/// known allowed domain (e.g. `https://changdunovel.com/t/E_HDbOHpMJA/`
/// or `https://zlink.fqnovel.com/dhVGe`).
pub fn is_short_link(input: &str) -> bool {
    let trimmed = input.trim();
    let target = re_url()
        .find(trimmed)
        .map(|m| m.as_str())
        .unwrap_or(trimmed);
    let host = match url_host(target) {
        Some(h) => h,
        None => return false,
    };
    if !ALLOWED_SHORT_LINK_HOSTS.contains(&host.as_str()) {
        return false;
    }
    // Standard short link: /t/<token>
    if re_short_link().is_match(target) {
        return true;
    }
    // zlink-style short link: /<token> (only for zlink.fqnovel.com)
    if host == "zlink.fqnovel.com" && re_zlink().is_match(target) {
        return true;
    }
    false
}

/// Like [`parse_book_id`], but also handles short-redirect share links by
/// following the HTTP redirect and parsing the resolved URL.
///
/// Only short links from [`ALLOWED_SHORT_LINK_HOSTS`] are followed to
/// prevent SSRF.  This function performs a blocking network request when
/// `input` is a short link.  Call it from a blocking context (e.g. inside
/// `tokio::task::spawn_blocking`) when used from async code.
pub fn resolve_book_id(input: &str) -> Option<String> {
    if let Some(id) = parse_book_id(input) {
        return Some(id);
    }

    let trimmed = input.trim();
    let url = re_url().find(trimmed).map(|m| m.as_str())?;

    if !is_short_link(url) {
        return None;
    }

    let response = match http_client().get(url).send() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(url = %url, error = %e, "短链接跳转失败");
            return None;
        }
    };
    let final_url = response.url().to_string();

    let book_id = parse_book_id(&final_url);
    if book_id.is_none() {
        tracing::warn!(url = %url, final_url = %final_url, "短链接跳转后仍无法解析 book_id");
    }
    book_id
}

#[cfg(test)]
mod tests {
    use super::{is_short_link, parse_book_id};

    #[test]
    fn parse_plain_numeric_book_id() {
        assert_eq!(
            parse_book_id("7423591956359416856"),
            Some("7423591956359416856".into())
        );
    }

    #[test]
    fn parse_book_id_from_share_page_url() {
        let url = "https://changdunovel.com/ug/pages/book-share?share_type=11&aid=1967&book_id=7423591956359416856";
        assert_eq!(parse_book_id(url), Some("7423591956359416856".into()));
    }

    #[test]
    fn parse_book_id_from_full_share_link_with_encoded_params() {
        // Real-world share URL with URL-encoded parameters (encrypt_did, zlink, etc.)
        let url = "https://changdunovel.com/ug/pages/book-share?share_type=11&aid=1967&book_id=7612464554961800216&encrypt_did=MDIEDNvz53FgTSz0b4iFggQQTfts0%2BVFRTP1m3%2BV2oelgQQQktgNmnytAcSL4HnxpajjRQ%3D%3D&ver=v2&share_genre=read&user_id=d232ab2c3b5d29e7bc819a6063c5d4d2&did=e219c3c6f9f1f6563ef9342091f1dd04&entrance=&zlink=https%3A%2F%2Fzlink.fqnovel.com%2FdhVGe&gd_label=click_schema_lhft_share_novelapp_android";
        assert_eq!(parse_book_id(url), Some("7612464554961800216".into()));
    }

    #[test]
    fn recognize_short_link_with_underscore_token() {
        assert!(is_short_link("https://changdunovel.com/t/E_HDbOHpMJA/"));
    }

    #[test]
    fn recognize_short_link_with_dash_token() {
        assert!(is_short_link("https://changdunovel.com/t/AbC-Def_123/"));
    }

    #[test]
    fn recognize_zlink_short_link() {
        assert!(is_short_link("https://zlink.fqnovel.com/dhVGe"));
    }

    #[test]
    fn reject_zlink_style_from_non_zlink_host() {
        // /<token> without /t/ prefix is only valid for zlink.fqnovel.com
        assert!(!is_short_link("https://fanqienovel.com/dhVGe"));
    }

    #[test]
    fn reject_short_link_from_unknown_host() {
        assert!(!is_short_link("https://example.com/t/E_HDbOHpMJA/"));
    }
}
