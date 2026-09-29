//! 基础设施模块。
//!
//! 统一放置配置、日志、重试、路径与通用工具，供下载/解析/UI 层复用。

pub mod app_update;
pub mod book_id;
pub mod book_paths;
pub mod config;
pub mod context;
pub mod download_history;
pub mod file_cleaner;
pub mod json_extract;
pub mod logging;
pub mod novel_updates;
pub mod search_history;
pub mod self_update;

/// 按顺序返回第一个“存在且非空”的环境变量值。
///
/// 用于“新名优先、旧名兼容”：项目改名后环境变量已换成 `UNIFIED_*`，
/// 但读取侧仍接受历史的 `TOMATO_*` 前缀，避开旧部署 / Docker 编排直接失效。
pub fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// 布尔型环境变量读取（`1` / `true` / `yes` / `on` 视为真）。
pub fn env_bool_first(names: &[&str]) -> bool {
    env_first(names)
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}
