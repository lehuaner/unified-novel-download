//! 平台池（PlatformPool）。
//!
//! 平台池是**付费平台**背后的资源站集合：晋江的池当前由「姿势吧 + 笔下文学 +
//! 官术网 + 笔趣阁 + 笔趣阁二站 + 东滩小说」组成。池成员可以被多个付费平台共享
//! （同一站点可同时服务于多个平台），
//! 因此健康度绑定在**站点**上，池只负责声明成员关系。
//!
//! 站点不稳定的应对：
//! - 三态熔断（Closed → Open → HalfOpen），连续失败 3 次打开；
//! - 冷却 5 分钟起，指数退避（×2），上限 30 分钟；
//! - 调度时 `allow()` 过掉 Open 的站点；HalfOpen 只放行一个探测请求。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// 一个池的静态定义。
pub struct PoolDef {
    pub id: &'static str,
    /// 展示名（日志/调试用）。
    #[allow(dead_code)]
    pub display_name: &'static str,
    /// 池成员（资源站 provider id，可共享）。
    pub members: &'static [&'static str],
}

/// 共享池 A：晋江当前使用的三个资源站。
///
/// 未来接入新的付费平台时，若这些站也收录其内容，直接复用本池即可
/// （在 [`crate::aggregate::paid_platform`] 中声明新的平台并引用 `shared_pool_a`）。
pub const SHARED_POOL_A: PoolDef = PoolDef {
    id: "shared_pool_a",
    display_name: "共享池A",
    members: &[
        "zishiba",
        "hen0",
        "guanshuya",
        "biqubee",
        "biquga",
        "dongtan",
    ],
};

static POOLS: &[PoolDef] = &[SHARED_POOL_A];

/// 按池 id 查找。
pub fn find(pool_id: &str) -> Option<&'static PoolDef> {
    POOLS.iter().find(|p| p.id == pool_id)
}

/// 连续失败达到该次数后熔断。
pub const FAILURE_THRESHOLD: u32 = 3;
/// 基础冷却时间。
pub const BASE_COOLDOWN: Duration = Duration::from_secs(300);
/// 冷却上限。
pub const MAX_COOLDOWN: Duration = Duration::from_secs(1800);

/// 站点级三态熔断器（纯状态机，便于单元测试）。
#[derive(Debug, Default, Clone)]
pub struct Breaker {
    failures: u32,
    open_until: Option<Instant>,
    backoff_steps: u32,
    probing: bool,
}

impl Breaker {
    /// 当前是否允许发起请求（Open 拒绝；冷却结束进入 HalfOpen 放行一次探测）。
    pub fn allow(&mut self, now: Instant) -> bool {
        match self.open_until {
            Some(until) if now < until => false,
            Some(_) => {
                if self.probing {
                    false
                } else {
                    self.probing = true;
                    true
                }
            }
            None => true,
        }
    }

    /// 记录一次请求结果。
    pub fn record(&mut self, now: Instant, ok: bool) {
        self.probing = false;
        if ok {
            self.failures = 0;
            self.open_until = None;
            self.backoff_steps = 0;
            return;
        }
        self.failures = self.failures.saturating_add(1);
        if self.failures >= FAILURE_THRESHOLD {
            let multiplier = 1u32 << self.backoff_steps.min(3); // 1,2,4,8
            let cooldown = BASE_COOLDOWN.saturating_mul(multiplier).min(MAX_COOLDOWN);
            self.open_until = Some(now + cooldown);
            self.backoff_steps = self.backoff_steps.saturating_add(1);
        }
    }

    /// 对外状态名。
    pub fn state(&self, now: Instant) -> &'static str {
        match self.open_until {
            Some(until) if now < until => "open",
            Some(_) => "half-open",
            None => "closed",
        }
    }
}

fn health_map() -> &'static Mutex<HashMap<String, Breaker>> {
    static MAP: OnceLock<Mutex<HashMap<String, Breaker>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 调度前询问：该站点本轮是否可请求。
pub fn allow(site: &str) -> bool {
    let mut map = health_map().lock().unwrap_or_else(|e| e.into_inner());
    map.entry(site.to_string())
        .or_default()
        .allow(Instant::now())
}

/// 记录一次站点搜索请求结果。
pub fn record(site: &str, ok: bool) {
    let mut map = health_map().lock().unwrap_or_else(|e| e.into_inner());
    map.entry(site.to_string())
        .or_default()
        .record(Instant::now(), ok);
}

/// 池状态快照（透出给前端 / 调试）：`{ "zishiba": "closed", "hen0": "open" }`。
pub fn snapshot() -> Value {
    let now = Instant::now();
    let map = health_map().lock().unwrap_or_else(|e| e.into_inner());
    let mut out = serde_json::Map::new();
    for pool in POOLS {
        for member in pool.members {
            let state = map.get(*member).map(|b| b.state(now)).unwrap_or("closed");
            out.insert((*member).to_string(), json!(state));
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_opens_after_three_failures_and_recovers() {
        let t0 = Instant::now();
        let mut b = Breaker::default();
        assert!(b.allow(t0));
        b.record(t0, false);
        b.record(t0, false);
        assert!(b.allow(t0), "两次失败仍应放行");
        b.record(t0, false);
        assert_eq!(b.state(t0), "open");
        assert!(!b.allow(t0), "熔断后应拒绝");

        // 冷却结束 → HalfOpen 只放行一次探测。
        let t1 = t0 + BASE_COOLDOWN + Duration::from_secs(1);
        assert_eq!(b.state(t1), "half-open");
        assert!(b.allow(t1));
        assert!(!b.allow(t1), "HalfOpen 期间只允许一个探测请求");

        // 探测成功 → 恢复。
        b.record(t1, true);
        assert_eq!(b.state(t1), "closed");
        assert!(b.allow(t1));
    }

    #[test]
    fn breaker_backoff_grows_and_caps() {
        let t0 = Instant::now();
        let mut b = Breaker::default();
        // 第一轮熔断：300s。
        for _ in 0..3 {
            b.record(t0, false);
        }
        assert!(!b.allow(t0));
        // 探测失败：冷却应当变长（600s）。
        let t1 = t0 + BASE_COOLDOWN + Duration::from_secs(1);
        assert!(b.allow(t1));
        b.record(t1, false);
        assert!(!b.allow(t1 + BASE_COOLDOWN + Duration::from_secs(1)));
        assert!(b.allow(t1 + BASE_COOLDOWN * 2 + Duration::from_secs(1)));
    }

    #[test]
    fn shared_pool_contains_all_resource_sites() {
        let pool = find("shared_pool_a").expect("共享池应存在");
        assert_eq!(
            pool.members,
            &[
                "zishiba",
                "hen0",
                "guanshuya",
                "biqubee",
                "biquga",
                "dongtan"
            ]
        );
    }
}
