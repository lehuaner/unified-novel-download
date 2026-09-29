pub(crate) const INDEX_HTML_RAW: &str = include_str!("templates/index.html");
pub(crate) const APP_JS: &str = include_str!("templates/app.js");
pub(crate) const APP_CSS: &str = include_str!("templates/app.css");
pub(crate) const APP_FAVICON_ICO: &[u8] = include_bytes!("../../../img/app.ico");

// 各搜索源图标（用于结果/下载库卡片右上角的来源标志）。
// 文件名按真实字节类型命名：fqnovel.webp / qmnovel.webp 为 WEBP，sqnovel.png 为 PNG（Content-Type 见 routes/index.rs）。
pub(crate) const ICON_FQNOVEL: &[u8] = include_bytes!("../../../img/fqnovel.webp");
pub(crate) const ICON_SQNOVEL: &[u8] = include_bytes!("../../../img/sqnovel.png");
pub(crate) const ICON_QMNOVEL: &[u8] = include_bytes!("../../../img/qmnovel.webp");

/// 仅当启用 official-api feature 时才注入免费声明。
/// 未启用时展开为空串，占位符从 HTML 中抹除。
#[cfg(feature = "official-api")]
pub(crate) const FREE_NOTICE_HTML: &str = concat!(
    r#"<div class="free-notice">"#,
    "本程序完全免费 &middot; ",
    r#"<a href="https://github.com/lehuaner/unified-novel-download" "#,
    r#"target="_blank" rel="noopener">开源仓库</a><br />"#,
    "若发现收费渠道，请勿上当受骗！",
    "</div>",
);

#[cfg(not(feature = "official-api"))]
pub(crate) const FREE_NOTICE_HTML: &str = "";

/// 移动端插入到状态页底部的免费声明（仅 official-api feature 启用时非空）。
#[cfg(feature = "official-api")]
pub(crate) const FREE_NOTICE_MOBILE_HTML: &str = concat!(
    r#"<div class="free-notice free-notice-mobile">"#,
    "本程序完全免费 &middot; ",
    r#"<a href="https://github.com/lehuaner/unified-novel-download" "#,
    r#"target="_blank" rel="noopener">开源仓库</a><br />"#,
    "若发现收费渠道，请勿上当受骗！",
    "</div>",
);

#[cfg(not(feature = "official-api"))]
pub(crate) const FREE_NOTICE_MOBILE_HTML: &str = "";
