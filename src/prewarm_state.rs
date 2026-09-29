//! IID 注册失败信息记录。
//!
//! 启动期的 IID 预热已随“官方 API 分支”一起移除（当前唯一构建方案不在启动时注册 IID），
//! 这里只保留失败文案与错误暂存，供 TUI / 老 CLI / Web UI 在拉取内容失败时给出可操作提示。

use std::sync::{Mutex, OnceLock};

static PREWARM_ERROR: OnceLock<Mutex<Option<String>>> = OnceLock::new();

#[allow(dead_code)]
const IID_BLOCK_HINT: &str = "IID 注册需要访问 https://log.snssdk.com/service/2/device_register/。番茄把该域名用于设备注册/广告分发，因此它经常会被公司/校园网、DNS 过滤、代理规则或 AdGuard/uBlock 等反广告插件拦截。请把 log.snssdk.com 加入放行列表，或临时关闭相关拦截后重试。";

#[allow(dead_code)]
pub fn mark_prewarm_failed(err: impl Into<String>) {
    set_prewarm_error(Some(format_iid_register_failure(err.into())));
}

pub fn prewarm_error() -> Option<String> {
    PREWARM_ERROR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[allow(dead_code)]
pub fn format_iid_register_failure(err: impl AsRef<str>) -> String {
    let err = err.as_ref().trim();
    if err.contains("log.snssdk.com") && err.contains("AdGuard") {
        return format!("IID 注册失败：{err}");
    }
    format!("IID 注册失败：{err}\n\n{IID_BLOCK_HINT}")
}

fn set_prewarm_error(err: Option<String>) {
    *PREWARM_ERROR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = err;
}
