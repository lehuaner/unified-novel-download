//! 书源统一抽象（BookProvider）。
//!
//! 此前新增一个书源需要在 `book_id.rs` / `plan.rs` / `downloader.rs` /
//! `novel_updates.rs` / `web routes/search.rs` / `preview.rs` / `tui/mod.rs`
//! 等 10 余处逐点添加 `#[cfg(feature = "...")] if/else` 分支（见
//! `zishiba_适配报告.md` 的"接线清单(14 处)"）。
//!
//! 本模块把这些分发点收敛为一个 trait + 编译期注册表：
//! - 每个书源模块内定义一个零大小适配器（如 `ShuqiProvider`）实现本 trait，
//!   委托给模块内既有的自由函数（保持旧测试与调用不变）；
//! - [`providers`] 返回按 feature 裁剪后的注册表（`OnceLock`，惰性初始化）；
//! - 各接线点改为遍历注册表或按 id / 归属查找，新增书源只需：
//!   1. 新建 `src/<源>/mod.rs`（含 Provider 适配器实现）；
//!   2. `Cargo.toml` 加 feature；
//!   3. `main.rs` 加 `#[cfg(feature = "...")] mod <源>;`；
//!   4. 在 [`build_registry`] 中 `v.push(Box::new(<源>::XxxProvider));`。
//!
//! 番茄不进注册表：它是默认回退源（无前缀、无 feature 门控、通路与外部源
//! 差异大），保持各处"注册表未命中即番茄"的原语义。

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use serde_json::{Value, json};

use crate::base_system::context::Config;
use crate::download::models::{BookMeta, ChapterRef, DownloadPlan, DownloadResult};
use crate::download::progress::ProgressReporter;

/// 更新检测抓取远端状态时的统一超时（秒）。
// 无任何外部源 feature 时无人调用，避免退化构建的 dead_code 警告。
#[cfg_attr(
    not(any(
        feature = "shuqi",
        feature = "qimao",
        feature = "zishiba",
        feature = "hen0",
        feature = "guanshuya",
        feature = "biqubee",
        feature = "biquga",
        feature = "dongtan"
    )),
    allow(dead_code)
)]
pub const UPDATE_FETCH_TIMEOUT_SECS: u64 = 15;

/// 搜索参数（选项袋）：把各源 `search_items` 的差异参数收拢，
/// 源不识别的字段自行忽略。字段全部持有所有权，方便 `spawn_blocking` 直接 move。
// 同上：无任何外部源 feature 时字段无读取方。
#[cfg_attr(
    not(any(
        feature = "shuqi",
        feature = "qimao",
        feature = "zishiba",
        feature = "hen0",
        feature = "guanshuya",
        feature = "biqubee",
        feature = "biquga",
        feature = "dongtan"
    )),
    allow(dead_code)
)]
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// 搜索关键字。
    pub keyword: String,
    /// 页码（从 1 起）。
    pub page: u32,
    /// 客户端超时（秒）。
    pub timeout_secs: u64,
    /// 分类 tab（七猫/番茄用，其余源忽略）。
    pub tab: Option<i64>,
    /// 筛选器选中项，逗号分隔的 selector_item_id（七猫用，其余源忽略）。
    pub selected_items: Option<String>,
    /// 资源站所需的 Cookie（官术网 Cloudflare `cf_clearance`，其余源忽略）。
    pub cookie: Option<String>,
    /// 资源站所需的 User-Agent（官术网需与 Cookie 获取浏览器一致，其余源忽略）。
    pub user_agent: Option<String>,
}

impl SearchOptions {
    pub fn new(keyword: impl Into<String>, page: u32, timeout_secs: u64) -> Self {
        Self {
            keyword: keyword.into(),
            page,
            timeout_secs,
            tab: None,
            selected_items: None,
            cookie: None,
            user_agent: None,
        }
    }
}

/// 书源 trait：统一「识别 / 归一化 / 搜索 / 下载计划 / 下载执行 / 更新检测」六类能力。
/// `Send + Sync`：注册表为 static，且各源在 `spawn_blocking` 闭包中跨线程调用。
pub trait BookProvider: Send + Sync {
    /// 源标识，与 Web API `provider` 参数及前端 `PROVIDER_REGISTRY` 一致（如 `"shuqi"`）。
    fn id(&self) -> &'static str;

    /// 中文显示名，用于错误提示（如 `"书旗"`）。
    fn display_name(&self) -> &'static str;

    /// 判断 book_id 是否属于本源（按前缀识别，如 `sq:`）。
    fn owns_book_id(&self, book_id: &str) -> bool;

    /// 用户输入归一化：`sq:123` / `https://www.shuqi.com/book/123` → `"sq:123"`。
    fn normalize_book_input(&self, input: &str) -> Option<String>;

    /// 纯数字 ID 是否允许回退到本源。书旗/七猫 ID 与番茄无区分度但实测可回退；
    /// 姿势吧/笔下文学 ID 无区分度，禁止回退（避免误下载）。
    fn allow_numeric_fallback(&self) -> bool {
        false
    }

    /// 纯数字 ID 回退：番茄 web 解析失败时，把纯数字 ID 按本源前缀补全后
    /// 再走一次下载计划。默认不支持（直接返回 Err）；书旗/七猫覆写。
    /// 前缀知识由各源模块内部持有，不外泄到分发点。
    fn prepare_numeric_fallback(
        &self,
        _config: &Config,
        _numeric_id: &str,
        _meta_hint: BookMeta,
    ) -> Result<DownloadPlan> {
        Err(anyhow::anyhow!("该源不支持纯数字 ID 回退"))
    }

    /// 搜索，返回 `(结果列表, 是否还有更多)`。结果为已带 `book_id`/`title` 等
    /// 字段的 JSON 项，与各源既有 `search_items` 输出同构。
    fn search(&self, opts: &SearchOptions) -> Result<(Vec<Value>, bool)>;

    /// 搜索能力元数据（分类 tab / 筛选器 / 是否还有更多），前端工具栏据此构建。
    /// 默认实现为「无 tab、无筛选器」，翻页型源直接可用。
    fn search_meta(&self, has_more: bool, _tab: i64) -> Value {
        json!({ "tabs": [], "selector": Value::Null, "has_more": has_more })
    }

    /// 轻量获取该书当前章节数（仅用于「同一本书多个下载源」的排序）。
    /// 默认复用更新检测（各源已实现）；不支持/失败返回 None。
    fn fetch_chapter_count(&self, book_id: &str, _timeout_secs: u64) -> Option<u32> {
        self.fetch_update_state(book_id, None)
            .and_then(|(total, _)| u32::try_from(total).ok())
    }

    /// 轻量预览摘要：只取详情 + 首章/末章/章节总数，不拉完整目录。
    ///
    /// 详情页并不展示完整章列表，完整目录留给真正下载时再拉。默认返回 None
    /// （调用方回退完整下载计划）。
    fn preview_summary(&self, _book_id: &str, _timeout_secs: u64) -> Option<BookMeta> {
        None
    }

    /// 筛选器定义（旧前端兼容字段）。默认无筛选器；七猫有同构 selector。
    fn selector(&self) -> Value {
        Value::Null
    }

    /// 准备下载计划（拉目录 + 元数据）。
    fn prepare_plan(
        &self,
        config: &Config,
        book_id: &str,
        meta_hint: BookMeta,
    ) -> Result<DownloadPlan>;

    /// 执行章节下载并写入 `BookManager`（签名与各源既有
    /// `download_*_into_manager` 完全一致）。
    #[allow(clippy::too_many_arguments)]
    fn download_into_manager(
        &self,
        config: &Config,
        book_id: &str,
        book_name: &str,
        manager: &mut crate::book_parser::book_manager::BookManager,
        chosen_chapters: &[ChapterRef],
        pending_chapters: &[ChapterRef],
        reporter: &mut ProgressReporter,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<DownloadResult>;

    /// 更新检测：抓取远端 `(章节总数, 服务器完结态)`。
    /// 抓不到有效总数返回 `None`，调用方据此保留旧缓存。
    fn fetch_update_state(
        &self,
        book_id: &str,
        book_name: Option<&str>,
    ) -> Option<(usize, Option<bool>)>;
}

// ── 注册表 ──────────────────────────────────────────────────────

static REGISTRY: OnceLock<Vec<Box<dyn BookProvider>>> = OnceLock::new();

/// 编译期装配注册表：顺序即各分发点的尝试顺序
///（书旗 → 七猫 → 姿势吧 → 笔下文学，与重构前 `plan.rs` 的顺序一致）。
// 注册项随 feature 条件裁剪，无法静态写成 vec![]，逐个 push 更清晰。
// 退化构建（无任何外部源 feature）下没有 push，`mut` 会告警。
#[cfg_attr(
    not(any(
        feature = "shuqi",
        feature = "qimao",
        feature = "zishiba",
        feature = "hen0",
        feature = "guanshuya",
        feature = "biqubee",
        feature = "biquga",
        feature = "dongtan"
    )),
    allow(unused_mut)
)]
#[allow(clippy::vec_init_then_push)]
fn build_registry() -> Vec<Box<dyn BookProvider>> {
    let mut v: Vec<Box<dyn BookProvider>> = Vec::new();
    #[cfg(feature = "shuqi")]
    v.push(Box::new(crate::shuqi::ShuqiProvider));
    #[cfg(feature = "qimao")]
    v.push(Box::new(crate::qimao::QimaoProvider));
    #[cfg(feature = "zishiba")]
    v.push(Box::new(crate::zishiba::ZishibaProvider));
    #[cfg(feature = "hen0")]
    v.push(Box::new(crate::hen0::Hen0Provider));
    #[cfg(feature = "guanshuya")]
    v.push(Box::new(crate::guanshuya::GuanshuyaProvider));
    #[cfg(feature = "biqubee")]
    v.push(Box::new(crate::biqubee::BiqubeeProvider));
    #[cfg(feature = "biquga")]
    v.push(Box::new(crate::biquga::BiqugaProvider));
    #[cfg(feature = "dongtan")]
    v.push(Box::new(crate::dongtan::DongtanProvider));
    v
}

/// 已启用书源的注册表（按 feature 编译期裁剪，`--no-default-features` 下为空）。
pub fn providers() -> &'static [Box<dyn BookProvider>] {
    REGISTRY.get_or_init(build_registry)
}

/// 按 id 查找书源。
pub fn find(id: &str) -> Option<&'static dyn BookProvider> {
    providers().iter().find(|p| p.id() == id).map(|b| &**b)
}

/// 按 book_id 前缀归属查找书源；未命中任何外部源即番茄（返回 `None`）。
pub fn owner_of(book_id: &str) -> Option<&'static dyn BookProvider> {
    providers()
        .iter()
        .find(|p| p.owns_book_id(book_id))
        .map(|b| &**b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_owns_prefix_matches() {
        let cases: &[(&str, &str)] = &[
            ("sq:9349463", "shuqi"),
            ("qm:195631", "qimao"),
            ("zs:592874", "zishiba"),
            ("bx:586264", "hen0"),
            ("gs:108891", "guanshuya"),
            ("bb:37940", "biqubee"),
            ("bg:171_171712", "biquga"),
            ("dt:80579", "dongtan"),
            ("7496026299845053465", ""),
        ];
        for (book_id, expected) in cases {
            let owner = owner_of(book_id).map(|p| p.id()).unwrap_or("");
            assert_eq!(owner, *expected, "book_id={book_id}");
        }
    }

    #[test]
    fn registry_ids_are_unique() {
        let mut ids: Vec<_> = providers().iter().map(|p| p.id()).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "注册表内 id 不得重复");
    }

    /// 实网验证各源详情封面（懒加载 `/api/book-cover` 的数据源）。
    /// 运行：`cargo test provider::tests::e2e_live_preview_cover -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn e2e_live_preview_cover() {
        for (src, book_id) in [
            ("biqubee", "bb:633308"),
            ("biquga", "bg:2_2369"),
            ("dongtan", "dt:50235"),
            ("hen0", "bx:592874"),
        ] {
            let Some(p) = find(src) else {
                println!("{src}: feature 未启用");
                continue;
            };
            let cover = p.preview_summary(book_id, 10).and_then(|m| m.cover_url);
            println!("{src} {book_id} => {cover:?}");
        }
    }
}
