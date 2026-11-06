//! 付费平台「官方源」抽象（OfficialSource）。
//!
//! 付费平台（如晋江）与免费平台不同：官方站只提供**搜索与元数据**，
//! 内容获取由平台池中的资源站承担。官方源因此不是 [`crate::provider::BookProvider`]，
//! 不参与下载路由，只用于聚合层的归属判定。
//!
//! 新增一个付费平台的接线成本：
//! 1. 在本模块下新增实现（如 `jjwxc.rs`）；
//! 2. 在 [`build_registry`] 中 push；
//! 3. 在 [`crate::pool`] 中声明该平台使用的池（可复用共享池）。

use std::sync::OnceLock;

use anyhow::Result;

#[cfg(feature = "jjwxc")]
pub mod jjwxc;

/// 官方站的单本书元数据（用于与资源站条目做匹配度判定）。
///
/// `tags` / `cover_url` / `url` 供后续展示与调试使用（当前判定只用书名/作者/简介），
/// 保留完整元数据避免各调用点二次请求。
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct OfficialBook {
    /// 官方站内部 ID（晋江为 novelid）。
    pub official_id: String,
    pub title: String,
    pub author: String,
    /// 搜索列表里的短简介（可能被截断）。
    pub intro_short: String,
    /// 详情接口补全后的完整简介。
    pub intro_full: Option<String>,
    pub tags: Option<String>,
    pub cover_url: Option<String>,
    /// 分类（如 `原创-言情-架空历史-奇幻-女主`）。
    pub category: Option<String>,
    /// 总字数（详情接口 `novelSize`，已去掉千分位）。
    pub word_count: Option<u64>,
    /// 章节数（详情接口 `novelChapterCount`）。
    pub chapter_count: Option<u32>,
    /// 是否完结（详情接口 `novelStep`：1=连载，2=完结）。
    pub finished: Option<bool>,
    /// 评分（详情接口 `novelReviewScore`，如 9.1）。
    pub score: Option<f64>,
    /// 官方站书籍页 URL（仅用于展示/调试，不作为下载入口）。
    pub url: String,
}

impl OfficialBook {
    /// 判定用简介：优先完整简介，其次短简介。
    pub fn intro_for_match(&self) -> &str {
        match self.intro_full.as_deref() {
            Some(s) if !s.trim().is_empty() => s,
            _ => &self.intro_short,
        }
    }
}

/// 付费平台官方源接口。
///
/// `timeout_secs` 由调用方（聚合层）传入，便于按平台池的预算动态收紧。
pub trait OfficialSource: Send + Sync {
    /// 源标识（配置/聚合层使用，如 `"jjwxc"`）。
    fn id(&self) -> &'static str;

    /// 平台显示名（如 `"晋江"`）。
    #[allow(dead_code)]
    fn display_name(&self) -> &'static str;

    /// 搜索官方站，返回 `(结果列表, 是否还有更多)`。
    fn search(
        &self,
        keyword: &str,
        page: u32,
        timeout_secs: u64,
    ) -> Result<(Vec<OfficialBook>, bool)>;

    /// 拉取单本书的完整元数据（简介补全等）。
    fn fetch_detail(&self, official_id: &str, timeout_secs: u64) -> Result<OfficialBook>;
}

static REGISTRY: OnceLock<Vec<Box<dyn OfficialSource>>> = OnceLock::new();

#[allow(clippy::vec_init_then_push)]
fn build_registry() -> Vec<Box<dyn OfficialSource>> {
    #[allow(unused_mut)]
    let mut v: Vec<Box<dyn OfficialSource>> = Vec::new();
    #[cfg(feature = "jjwxc")]
    v.push(Box::new(jjwxc::JjwxcOfficial));
    v
}

/// 已启用的官方源（按 feature 编译期裁剪）。
pub fn sources() -> &'static [Box<dyn OfficialSource>] {
    REGISTRY.get_or_init(build_registry)
}

/// 按 id 查找官方源。
pub fn find(id: &str) -> Option<&'static dyn OfficialSource> {
    sources().iter().find(|s| s.id() == id).map(|b| &**b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_ids_are_unique() {
        let mut ids: Vec<_> = sources().iter().map(|s| s.id()).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "官方源 id 不得重复");
    }
}
