pub(crate) const INDEX_HTML_RAW: &str = include_str!("templates/index.html");
pub(crate) const APP_JS: &str = include_str!("templates/app.js");
pub(crate) const APP_CSS: &str = include_str!("templates/app.css");
pub(crate) const APP_FAVICON_ICO: &[u8] = include_bytes!("../../../img/app.ico");

// 各搜索源图标（用于结果/下载库卡片右上角的来源标志）。
// 文件名按真实字节类型命名：fqnovel.webp / qmnovel.webp 为 WEBP，sqnovel.png 为 PNG（Content-Type 见 routes/index.rs）。
pub(crate) const ICON_FQNOVEL: &[u8] = include_bytes!("../../../img/fqnovel.webp");
pub(crate) const ICON_SQNOVEL: &[u8] = include_bytes!("../../../img/sqnovel.png");
pub(crate) const ICON_QMNOVEL: &[u8] = include_bytes!("../../../img/qmnovel.webp");

/// 底部横条的免费声明文案（外层 `<footer class="notice-bar">` 由 index.html 提供，
/// 桌面与移动端都固定在页面最底部）。“开源仓库”前内联 GitHub octicon（16x16）。
/// 注意：`d` 属性必须单段写完——分段拼接曾导致闭合引号与 `.67` 一段路径数据丢失。
pub(crate) const FREE_NOTICE_HTML: &str = concat!(
    "本程序完全免费 &middot; ",
    r#"<a class="repo-link" href="https://github.com/lehuaner/unified-novel-download" target="_blank" rel="noopener">"#,
    r#"<svg class="repo-icon" viewBox="0 0 16 16" width="15" height="15" aria-hidden="true" focusable="false">"#,
    r#"<path fill="currentColor" d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.15.55-.17.55-.38 0-.19-.01-.68-.02-1.34-2.22.48-2.69-1.07-2.69-1.07-.36-.92-.89-1.17-.89-1.17-.73-.5.06-.49.06-.49.8.06 1.22.82 1.22.82.71 1.22 1.87.87 2.33.67.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2.82 2.2.82.44 1.1.16 1.92.08 2.12.51.56.82 1.28.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0 0 16 8c0-4.42-3.58-8-8-8z"/>"#,
    "</svg>",
    "开源仓库</a> &middot; ",
    "若发现收费渠道，请勿上当受骗！",
);
