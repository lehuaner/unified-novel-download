use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::base_system::context::Config;
use crate::download::downloader::{BookNameOption, ProgressSnapshot};

/// Web 层只读配置快照。目前仅番茄搜索会读 `unidbg_signer_url`；
/// 其余字段随状态页与 `/api/status` 一同下线。
#[derive(Clone, Debug)]
pub(crate) struct ConfigView {
    pub(crate) unidbg_signer_url: String,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) config_view: Arc<ConfigView>,
    pub(crate) config: Arc<Mutex<Config>>, // allow runtime updates via Web UI
    pub(crate) config_path: Arc<PathBuf>,
    pub(crate) library_root: Arc<PathBuf>,
    pub(crate) jobs: Arc<JobStore>,
    pub(crate) self_update: Arc<SelfUpdateStore>,
    pub(crate) library_scan: Arc<LibraryScanStore>,
    pub(crate) update_scan: Arc<UpdateScanStore>,
    pub(crate) cover_cache: Arc<CoverThumbCache>,
    pub(crate) auth: Option<AuthState>,
    /// 限制同时访问上游 API（search / preview）的并发数，防止 WebUI 被用作多用户 API 代理。
    pub(crate) api_semaphore: Arc<tokio::sync::Semaphore>,
}

// ── 搜索结果封面缩略图内存缓存 ───────────────────────────────────

const COVER_CACHE_MAX_ENTRIES: usize = 200;

#[derive(Debug, Default)]
pub(crate) struct CoverThumbCache {
    inner: Mutex<HashMap<String, Arc<Vec<u8>>>>,
}

impl CoverThumbCache {
    pub(crate) fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    pub(crate) fn put(&self, key: String, data: Vec<u8>) -> Arc<Vec<u8>> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() >= COVER_CACHE_MAX_ENTRIES {
            // 简单淘汰：删除最早插入的条目
            if let Some(first_key) = g.keys().next().cloned() {
                g.remove(&first_key);
            }
        }
        let arc = Arc::new(data);
        g.insert(key, arc.clone());
        arc
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SelfUpdateState {
    Idle,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SelfUpdateInfo {
    pub(crate) state: SelfUpdateState,
    pub(crate) stage: String,
    pub(crate) percent: u8,
    pub(crate) message: String,
    pub(crate) updated_ms: u64,
}

#[derive(Debug)]
pub(crate) struct SelfUpdateStore {
    running: AtomicBool,
    inner: Mutex<SelfUpdateInfo>,
}

impl Default for SelfUpdateStore {
    fn default() -> Self {
        Self {
            running: AtomicBool::new(false),
            inner: Mutex::new(SelfUpdateInfo {
                state: SelfUpdateState::Idle,
                stage: "idle".to_string(),
                percent: 0,
                message: "尚未开始".to_string(),
                updated_ms: now_ms(),
            }),
        }
    }
}

impl SelfUpdateStore {
    pub(crate) fn try_start(&self) -> bool {
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        self.set(SelfUpdateState::Running, "prepare", 2, "准备开始自更新…");
        true
    }

    pub(crate) fn snapshot(&self) -> SelfUpdateInfo {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn set(
        &self,
        state: SelfUpdateState,
        stage: impl Into<String>,
        percent: u8,
        message: impl Into<String>,
    ) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.state = state;
        g.stage = stage.into();
        g.percent = percent.min(100);
        g.message = message.into();
        g.updated_ms = now_ms();
    }

    pub(crate) fn tick_running(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(g.state, SelfUpdateState::Running) {
            return;
        }
        if g.percent < 92 {
            g.percent = (g.percent + 1).min(92);
            g.updated_ms = now_ms();
        }
    }

    pub(crate) fn finish_done(&self, stage: impl Into<String>, message: impl Into<String>) {
        self.set(SelfUpdateState::Done, stage, 100, message);
        self.running.store(false, Ordering::SeqCst);
    }

    pub(crate) fn finish_failed(&self, stage: impl Into<String>, message: impl Into<String>) {
        self.set(SelfUpdateState::Failed, stage, 100, message);
        self.running.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LibraryScanRow {
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) rel_path: String,
    pub(crate) ext: String,
    pub(crate) size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) file_count: Option<u64>,
    pub(crate) modified_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LibraryScanInfo {
    pub(crate) path: String,
    pub(crate) running: bool,
    pub(crate) scanned: usize,
    pub(crate) items: Vec<LibraryScanRow>,
    pub(crate) error: Option<String>,
    pub(crate) started_ms: u64,
    pub(crate) updated_ms: u64,
}

#[derive(Debug, Default)]
struct LibraryScanState {
    infos: HashMap<String, LibraryScanInfo>,
    running: HashSet<String>,
}

#[derive(Debug, Default)]
pub(crate) struct LibraryScanStore {
    inner: Mutex<LibraryScanState>,
}

impl LibraryScanStore {
    pub(crate) fn try_start(&self, path: String) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !g.running.insert(path.clone()) {
            return false;
        }

        let now = now_ms();
        g.infos.insert(
            path.clone(),
            LibraryScanInfo {
                path,
                running: true,
                scanned: 0,
                items: Vec::new(),
                error: None,
                started_ms: now,
                updated_ms: now,
            },
        );
        true
    }

    pub(crate) fn push_item(&self, path: &str, item: LibraryScanRow, scanned: usize) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(info) = g.infos.get_mut(path) {
            info.running = true;
            info.scanned = scanned;
            info.items.push(item);
            info.updated_ms = now_ms();
        }
    }

    pub(crate) fn finish(&self, path: &str, mut items: Vec<LibraryScanRow>) {
        items.sort_by(|a, b| {
            b.modified_ms
                .cmp(&a.modified_ms)
                .then_with(|| a.name.cmp(&b.name))
        });

        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.running.remove(path);
        let now = now_ms();
        let started_ms = g.infos.get(path).map(|info| info.started_ms).unwrap_or(now);
        g.infos.insert(
            path.to_string(),
            LibraryScanInfo {
                path: path.to_string(),
                running: false,
                scanned: items.len(),
                items,
                error: None,
                started_ms,
                updated_ms: now,
            },
        );
    }

    pub(crate) fn finish_failed(&self, path: &str, error: String) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.running.remove(path);
        let now = now_ms();
        let info = g.infos.entry(path.to_string()).or_insert(LibraryScanInfo {
            path: path.to_string(),
            running: false,
            scanned: 0,
            items: Vec::new(),
            error: None,
            started_ms: now,
            updated_ms: now,
        });
        info.running = false;
        info.error = Some(error);
        info.updated_ms = now;
    }

    pub(crate) fn snapshot(&self, path: &str) -> Option<LibraryScanInfo> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .infos
            .get(path)
            .cloned()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpdateScanRow {
    pub(crate) book_id: String,
    pub(crate) book_name: String,
    pub(crate) folder: String,
    pub(crate) local_total: usize,
    pub(crate) local_failed: usize,
    pub(crate) remote_total: usize,
    pub(crate) new_count: usize,
    pub(crate) has_update: bool,
    pub(crate) is_ignored: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpdateScanInfo {
    pub(crate) running: bool,
    pub(crate) scanned: usize,
    pub(crate) total: usize,
    pub(crate) save_dir: String,
    pub(crate) updates: Vec<UpdateScanRow>,
    pub(crate) no_updates: Vec<UpdateScanRow>,
    pub(crate) error: Option<String>,
    pub(crate) started_ms: u64,
    pub(crate) updated_ms: u64,
}

#[derive(Debug)]
pub(crate) struct UpdateScanStore {
    running: AtomicBool,
    inner: Mutex<UpdateScanInfo>,
}

impl Default for UpdateScanStore {
    fn default() -> Self {
        Self {
            running: AtomicBool::new(false),
            inner: Mutex::new(UpdateScanInfo {
                running: false,
                scanned: 0,
                total: 0,
                save_dir: String::new(),
                updates: Vec::new(),
                no_updates: Vec::new(),
                error: None,
                started_ms: 0,
                updated_ms: now_ms(),
            }),
        }
    }
}

impl UpdateScanStore {
    pub(crate) fn try_start(&self, save_dir: String) -> bool {
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }

        let now = now_ms();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g = UpdateScanInfo {
            running: true,
            scanned: 0,
            total: 0,
            save_dir,
            updates: Vec::new(),
            no_updates: Vec::new(),
            error: None,
            started_ms: now,
            updated_ms: now,
        };
        true
    }

    pub(crate) fn push_progress(&self, row: UpdateScanRow, scanned: usize, total: usize) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.running = true;
        g.scanned = scanned;
        g.total = total;
        g.updated_ms = now_ms();
        if row.is_ignored || !row.has_update {
            g.no_updates.push(row);
        } else {
            g.updates.push(row);
        }
    }

    pub(crate) fn finish(
        &self,
        save_dir: String,
        updates: Vec<UpdateScanRow>,
        no_updates: Vec<UpdateScanRow>,
    ) {
        let total = updates.len() + no_updates.len();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.running = false;
        g.scanned = total;
        g.total = total;
        g.save_dir = save_dir;
        g.updates = updates;
        g.no_updates = no_updates;
        g.error = None;
        g.updated_ms = now_ms();
        self.running.store(false, Ordering::SeqCst);
    }

    pub(crate) fn finish_failed(&self, error: String) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.running = false;
        g.error = Some(error);
        g.updated_ms = now_ms();
        self.running.store(false, Ordering::SeqCst);
    }

    pub(crate) fn snapshot(&self) -> UpdateScanInfo {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[derive(Clone)]
pub(crate) struct AuthState {
    pub(crate) password_sha256: [u8; 32],
    pub(crate) session_secret: [u8; 32],
    cookie_secure: bool,
    login_attempts: Arc<Mutex<HashMap<IpAddr, LoginAttemptState>>>,
}

const SESSION_TTL_SECS: u64 = 24 * 60 * 60;
/// 受信设备 Cookie 有效期：一旦在某设备上登录过一次，长期免密放行（跨浏览器会话与服务重启）。
const DEVICE_TTL_SECS: u64 = 180 * 24 * 60 * 60;
const LOGIN_RATE_WINDOW_SECS: u64 = 1;
const LOGIN_RATE_MAX_ATTEMPTS: usize = 5;
const LOGIN_LOCK_AFTER_FAILURES: u32 = 10;
const LOGIN_LOCK_SECS: u64 = 5 * 60;

/// 文件名：持久化的会话签名密钥（放在数据目录，保证重启后已登录设备的 Cookie 仍有效）。
pub(crate) const SESSION_SECRET_FILE: &str = "web_session_secret.key";

#[derive(Debug, Default)]
struct LoginAttemptState {
    recent_attempts: VecDeque<u64>,
    failed_count: u32,
    locked_until: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginLimitDecision {
    Allowed,
    RateLimited { retry_after_secs: u64 },
    Locked { retry_after_secs: u64 },
}

impl AuthState {
    /// 从密码 + 已持久化的会话密钥构造 AuthState。
    /// `session_secret` 应由 [`load_or_create_session_secret`] 提供，保证跨服务重启稳定。
    pub(crate) fn from_password(
        password: &str,
        cookie_secure: bool,
        session_secret: [u8; 32],
    ) -> Self {
        let mut h = Sha256::new();
        h.update(password.as_bytes());
        let out = h.finalize();
        let mut password_sha256 = [0u8; 32];
        password_sha256.copy_from_slice(&out);

        Self {
            password_sha256,
            session_secret,
            cookie_secure,
            login_attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn check_login_allowed(&self, ip: IpAddr) -> LoginLimitDecision {
        let now = now_secs();
        let mut attempts = self
            .login_attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = attempts.entry(ip).or_default();

        if let Some(locked_until) = state.locked_until {
            if locked_until > now {
                return LoginLimitDecision::Locked {
                    retry_after_secs: locked_until.saturating_sub(now).max(1),
                };
            }
            state.locked_until = None;
            state.failed_count = 0;
            state.recent_attempts.clear();
        }

        let cutoff = now.saturating_sub(LOGIN_RATE_WINDOW_SECS);
        while state
            .recent_attempts
            .front()
            .map(|ts| *ts <= cutoff)
            .unwrap_or(false)
        {
            state.recent_attempts.pop_front();
        }

        if state.recent_attempts.len() >= LOGIN_RATE_MAX_ATTEMPTS {
            let oldest = *state.recent_attempts.front().unwrap_or(&now);
            let retry_after_secs = oldest
                .saturating_add(LOGIN_RATE_WINDOW_SECS)
                .saturating_sub(now)
                .max(1);
            return LoginLimitDecision::RateLimited { retry_after_secs };
        }

        state.recent_attempts.push_back(now);
        LoginLimitDecision::Allowed
    }

    pub(crate) fn record_login_failure(&self, ip: IpAddr) -> Option<u64> {
        let now = now_secs();
        let mut attempts = self
            .login_attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = attempts.entry(ip).or_default();
        state.failed_count = state.failed_count.saturating_add(1);
        if state.failed_count >= LOGIN_LOCK_AFTER_FAILURES {
            let locked_until = now.saturating_add(LOGIN_LOCK_SECS);
            state.locked_until = Some(locked_until);
            state.recent_attempts.clear();
            Some(LOGIN_LOCK_SECS)
        } else {
            None
        }
    }

    pub(crate) fn record_login_success(&self, ip: IpAddr) {
        let mut attempts = self
            .login_attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        attempts.remove(&ip);
    }

    pub(crate) fn issue_session_token(&self) -> String {
        self.issue_token(SESSION_TTL_SECS)
    }

    /// 签发长效“受信设备”令牌：登录成功时下发，使该设备长期免密。
    pub(crate) fn issue_device_token(&self) -> String {
        self.issue_token(DEVICE_TTL_SECS)
    }

    fn issue_token(&self, ttl_secs: u64) -> String {
        let exp = now_secs().saturating_add(ttl_secs);
        let nonce = Uuid::new_v4().simple().to_string();
        let payload = format!("{exp}.{nonce}");
        let sig = self.sign_payload(&payload);
        format!("{payload}.{sig}")
    }

    pub(crate) fn verify_session_token(&self, token: &str) -> bool {
        let mut parts = token.split('.');
        let Some(exp_raw) = parts.next() else {
            return false;
        };
        let Some(nonce_raw) = parts.next() else {
            return false;
        };
        let Some(sig_raw) = parts.next() else {
            return false;
        };
        if parts.next().is_some() {
            return false;
        }

        let Ok(exp) = exp_raw.parse::<u64>() else {
            return false;
        };
        if now_secs() > exp {
            return false;
        }

        let payload = format!("{exp_raw}.{nonce_raw}");
        let expected = self.sign_payload(&payload);
        constant_time_eq(sig_raw.as_bytes(), expected.as_bytes())
    }

    pub(crate) fn session_ttl_secs(&self) -> u64 {
        SESSION_TTL_SECS
    }

    pub(crate) fn device_ttl_secs(&self) -> u64 {
        DEVICE_TTL_SECS
    }

    pub(crate) fn cookie_secure(&self) -> bool {
        self.cookie_secure
    }

    fn sign_payload(&self, payload: &str) -> String {
        let mut h = Sha256::new();
        h.update(self.session_secret);
        h.update(payload.as_bytes());
        hex::encode(h.finalize())
    }
}

/// 从数据目录读取持久化的会话签名密钥；不存在则生成随机密钥并落盘。
/// 保证服务重启后已登录设备（含长效 device cookie）的令牌仍可校验，无需重新输入密码。
pub(crate) fn load_or_create_session_secret(dir: &std::path::Path) -> [u8; 32] {
    let file = dir.join(SESSION_SECRET_FILE);
    if let Ok(bytes) = std::fs::read(&file)
        && bytes.len() == 32
    {
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&bytes);
        return secret;
    }

    let now = now_secs();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut h = Sha256::new();
    h.update(a.as_bytes());
    h.update(b.as_bytes());
    h.update(now.to_le_bytes());
    let digest = h.finalize();
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&digest);

    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::write(&file, secret);
    }
    secret
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use std::net::IpAddr;

    fn test_secret() -> [u8; 32] {
        let mut s = [0u8; 32];
        s[0] = 0xAB;
        s
    }

    #[test]
    fn session_secret_persists_across_calls() {
        let dir = std::env::temp_dir().join(format!("tnd_secret_test_{}", now_ms()));
        let a = load_or_create_session_secret(&dir);
        let b = load_or_create_session_secret(&dir);
        assert_eq!(a, b, "同一目录应返回相同密钥（已落盘）");
        // 同一密钥签发的令牌可被校验
        let auth1 = AuthState::from_password("pw", false, a);
        let auth2 = AuthState::from_password("pw", false, b);
        let tok = auth1.issue_device_token();
        assert!(auth2.verify_session_token(&tok));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn device_token_has_long_ttl_and_verifies() {
        let auth = AuthState::from_password("secret", false, test_secret());
        let tok = auth.issue_device_token();
        assert!(auth.verify_session_token(&tok));
        assert!(auth.device_ttl_secs() > auth.session_ttl_secs());
    }

    #[test]
    fn login_rate_limit_allows_five_attempts_per_second() {
        let auth = AuthState::from_password("secret", false, test_secret());
        let ip = IpAddr::from([127, 0, 0, 1]);

        for _ in 0..LOGIN_RATE_MAX_ATTEMPTS {
            assert_eq!(auth.check_login_allowed(ip), LoginLimitDecision::Allowed);
        }
        assert!(matches!(
            auth.check_login_allowed(ip),
            LoginLimitDecision::RateLimited { .. }
        ));
    }

    #[test]
    fn repeated_failures_lock_ip_and_success_resets_state() {
        let auth = AuthState::from_password("secret", false, test_secret());
        let ip = IpAddr::from([127, 0, 0, 2]);

        for _ in 1..LOGIN_LOCK_AFTER_FAILURES {
            assert_eq!(auth.record_login_failure(ip), None);
        }
        assert_eq!(auth.record_login_failure(ip), Some(LOGIN_LOCK_SECS));
        assert!(matches!(
            auth.check_login_allowed(ip),
            LoginLimitDecision::Locked { .. }
        ));

        auth.record_login_success(ip);
        assert_eq!(auth.check_login_allowed(ip), LoginLimitDecision::Allowed);
    }

    #[test]
    fn session_defaults_are_short_lived_and_secure_flag_is_configurable() {
        let insecure = AuthState::from_password("secret", false, test_secret());
        let secure = AuthState::from_password("secret", true, test_secret());

        assert_eq!(insecure.session_ttl_secs(), 24 * 60 * 60);
        assert!(!insecure.cookie_secure());
        assert!(secure.cookie_secure());
        assert!(secure.verify_session_token(&secure.issue_session_token()));
    }
}

pub(crate) const RECENT_DONE_JOB_RETENTION_MS: u64 = 2 * 60 * 60 * 1000;

/// 任务表纪元（本进程首次对外提供任务同步的时刻，进程内恒定）。
/// 前端比对自身记录的纪元，一旦不一致即说明内存任务表已换过一批，
/// 需丢弃本地增量缓存全量重同步（否则旧任务卡片会成为幽灵）。
pub(crate) fn jobs_epoch_ms() -> u64 {
    static EPOCH: OnceLock<u64> = OnceLock::new();
    *EPOCH.get_or_init(now_ms)
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Canceled,
}

impl JobState {
    fn is_auto_prunable(self) -> bool {
        matches!(self, JobState::Done)
    }
}

/// 任务自身携带的书籍元数据，供下载库“进行中卡片”与成品卡同构渲染。
/// 数据来源只有两处：提交任务时卡片携带的封面、本任务 prepare_download_plan 拿到的上游元数据；
/// 不从其它接口补分，字段缺失即为 None（前端不渲染该项）。
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct JobBookMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cover_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) word_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) chapter_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) finished: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) read_count_text: Option<String>,
}

/// 任务静态视图（生命周期内基本不变）：与动态进度分开传输，避免 1.5s 轮询重复拉取相同数据。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct JobStaticView {
    pub(crate) book_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) author: Option<String>,
    pub(crate) created_ms: u64,
    pub(crate) meta: JobBookMeta,
}

/// 增量同步结果：变化项 + 自游标后被移除的任务 id（墓碑）+ 新游标。
pub(crate) struct JobSync {
    pub(crate) changed: Vec<JobInfo>,
    pub(crate) removed_ids: Vec<u64>,
    pub(crate) cursor: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JobInfo {
    pub(crate) id: u64,
    pub(crate) book_id: String,
    pub(crate) title: Option<String>,
    pub(crate) author: Option<String>,
    pub(crate) state: JobState,
    pub(crate) progress: Option<ProgressSnapshot>,
    pub(crate) message: Option<String>,
    pub(crate) book_name_options: Option<Vec<BookNameOption>>,
    pub(crate) format_options: Option<Vec<BookNameOption>>,
    pub(crate) meta: Option<JobBookMeta>,
    /// 上游元数据是否已写入（prepare 完成）。
    /// 不能用 meta.is_some() 代替：提交时播种的封面会让 meta 提前存在，
    /// 前端会过早拉一次静态层并永久标记“已拉”，永远拿不到书名/简介。
    pub(crate) meta_ready: bool,
    pub(crate) created_ms: u64,
    pub(crate) updated_ms: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct JobHandle {
    pub(crate) id: u64,
    pub(crate) cancel: Arc<AtomicBool>,
}

#[derive(Debug)]
struct JobEntry {
    info: JobInfo,
    cancel: Arc<AtomicBool>,
    book_name_sender: Option<std::sync::mpsc::Sender<Option<String>>>,
    format_sender: Option<std::sync::mpsc::Sender<Option<String>>>,
}

#[derive(Debug, Default)]
pub(crate) struct JobStore {
    next_id: AtomicU64,
    inner: Mutex<HashMap<u64, JobEntry>>,
    /// 墓碑 (job_id, 移除时刻)：增量轮询靠它告知客户端删除条目，否则前端 Map 会残留。
    removed: Mutex<Vec<(u64, u64)>>,
}

impl JobStore {
    pub(crate) fn create(&self, book_id: String, cover_hint: Option<String>) -> JobHandle {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let now = now_ms();
        let cancel = Arc::new(AtomicBool::new(false));

        // 提交时卡片带来的封面立刻入库：排队/解析阶段即可显示，无需等 plan。
        let meta = cover_hint.map(|c| JobBookMeta {
            cover_url: Some(c),
            ..Default::default()
        });

        let info = JobInfo {
            id,
            book_id,
            title: None,
            author: None,
            state: JobState::Queued,
            progress: None,
            message: None,
            book_name_options: None,
            format_options: None,
            meta,
            meta_ready: false,
            created_ms: now,
            updated_ms: now,
        };

        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.insert(
            id,
            JobEntry {
                info,
                cancel: cancel.clone(),
                book_name_sender: None,
                format_sender: None,
            },
        );

        JobHandle { id, cancel }
    }

    pub(crate) fn list(&self) -> Vec<JobInfo> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<JobInfo> = g.values().map(|e| e.info.clone()).collect();
        v.sort_by(|a, b| {
            b.updated_ms
                .cmp(&a.updated_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        v
    }

    pub(crate) fn prune_done_older_than(&self, retention_ms: u64) {
        let cutoff = now_ms().saturating_sub(retention_ms);
        let mut dead: Vec<u64> = Vec::new();
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.retain(|id, e| {
                let keep = !e.info.state.is_auto_prunable() || e.info.updated_ms >= cutoff;
                if !keep {
                    dead.push(*id);
                }
                keep
            });
        }
        self.record_removed(dead);
    }

    /// 增量同步：只返回 updated_ms 严格大于游标的任务，外加该时刻后被移除的任务（墓碑）。
    /// 无变化时 items 为空，1.5s 轮询的 payload 接近零，不重复传输相同数据。
    pub(crate) fn sync_since(&self, since_ms: u64) -> JobSync {
        let now = now_ms();
        let changed: Vec<JobInfo> = {
            let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let mut v: Vec<JobInfo> = g
                .values()
                .filter(|e| e.info.updated_ms > since_ms)
                .map(|e| e.info.clone())
                .collect();
            v.sort_by(|a, b| {
                b.updated_ms
                    .cmp(&a.updated_ms)
                    .then_with(|| b.id.cmp(&a.id))
            });
            v
        };
        let removed_ids = {
            let mut r = self.removed.lock().unwrap_or_else(|e| e.into_inner());
            r.retain(|(_, ms)| now.saturating_sub(*ms) <= RECENT_DONE_JOB_RETENTION_MS);
            r.iter()
                .filter(|(_, ms)| *ms > since_ms)
                .map(|(id, _)| *id)
                .collect()
        };
        JobSync {
            changed,
            removed_ids,
            cursor: now,
        }
    }

    /// 批量取任务静态视图：仅返回元数据已就绪（meta 已写入）的任务，
    /// 未就绪的不返回，前端下个周期再取（因此不会重复拉到相同内容）。
    pub(crate) fn static_views(&self, ids: &[u64]) -> Vec<(u64, JobStaticView)> {
        if ids.is_empty() {
            return Vec::new();
        }
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        ids.iter()
            .filter_map(|id| {
                let info = &g.get(id)?.info;
                // 未就绪（只有播种封面）时不返回，前端下个周期再取。
                if !info.meta_ready {
                    return None;
                }
                let meta = info.meta.clone()?;
                Some((
                    *id,
                    JobStaticView {
                        book_id: info.book_id.clone(),
                        title: info.title.clone(),
                        author: info.author.clone(),
                        created_ms: info.created_ms,
                        meta,
                    },
                ))
            })
            .collect()
    }

    fn record_removed(&self, ids: Vec<u64>) {
        if ids.is_empty() {
            return;
        }
        let now = now_ms();
        let mut r = self.removed.lock().unwrap_or_else(|e| e.into_inner());
        r.extend(ids.into_iter().map(|id| (id, now)));
    }

    /// 返回当前处于 Queued 或 Running 状态的任务数量，用于并发限制。
    pub(crate) fn count_active(&self) -> usize {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.values()
            .filter(|e| matches!(e.info.state, JobState::Queued | JobState::Running))
            .count()
    }

    pub(crate) fn set_running(&self, id: u64) {
        self.update(id, |j| {
            j.state = JobState::Running;
            j.message = None;
            j.book_name_options = None;
            j.format_options = None;
        });
    }

    pub(crate) fn set_meta(&self, id: u64, title: Option<String>, author: Option<String>) {
        self.update(id, |j| {
            j.title = title;
            j.author = author;
        });
    }

    /// 写入本任务上游元数据；已有封面（提交时卡片携带）且新值无封面时保留原封面，
    /// 避免出现“解析完反而没封面”的回退。
    pub(crate) fn set_book_meta(&self, id: u64, meta: JobBookMeta) {
        self.update(id, |j| {
            j.meta_ready = true;
            j.meta = Some(match j.meta.take() {
                Some(prev) if prev.cover_url.is_some() && meta.cover_url.is_none() => JobBookMeta {
                    cover_url: prev.cover_url,
                    ..meta
                },
                _ => meta,
            });
        });
    }

    pub(crate) fn set_progress(&self, id: u64, snap: ProgressSnapshot) {
        self.update(id, |j| {
            j.progress = Some(snap);
        });
    }

    pub(crate) fn set_done(&self, id: u64) {
        self.update(id, |j| {
            j.state = JobState::Done;
            j.message = None;
            j.book_name_options = None;
            j.format_options = None;
        });
    }

    pub(crate) fn set_failed(&self, id: u64, msg: String) {
        self.update(id, |j| {
            j.state = JobState::Failed;
            j.message = Some(msg);
            j.book_name_options = None;
            j.format_options = None;
        });
    }

    pub(crate) fn request_cancel(&self, id: u64) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(e) = g.get_mut(&id) else {
            return false;
        };
        e.cancel.store(true, Ordering::Relaxed);
        e.info.state = JobState::Canceled;
        e.info.message = Some("cancel requested".to_string());
        if let Some(tx) = e.book_name_sender.take() {
            let _ = tx.send(None);
        }
        if let Some(tx) = e.format_sender.take() {
            let _ = tx.send(None);
        }
        e.info.book_name_options = None;
        e.info.format_options = None;
        e.info.updated_ms = now_ms();
        true
    }

    pub(crate) fn request_cancel_and_remove(&self, id: u64) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut e) = g.remove(&id) else {
            return false;
        };
        drop(g);
        e.cancel.store(true, Ordering::Relaxed);
        if let Some(tx) = e.book_name_sender.take() {
            let _ = tx.send(None);
        }
        if let Some(tx) = e.format_sender.take() {
            let _ = tx.send(None);
        }
        self.record_removed(vec![id]);
        true
    }

    pub(crate) fn remove(&self, id: u64) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut e) = g.remove(&id) else {
            return false;
        };
        drop(g);
        if let Some(tx) = e.book_name_sender.take() {
            let _ = tx.send(None);
        }
        if let Some(tx) = e.format_sender.take() {
            let _ = tx.send(None);
        }
        self.record_removed(vec![id]);
        true
    }

    pub(crate) fn set_book_name_options(
        &self,
        id: u64,
        options: Vec<BookNameOption>,
        sender: std::sync::mpsc::Sender<Option<String>>,
    ) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(e) = g.get_mut(&id) else {
            return;
        };
        e.info.book_name_options = Some(options);
        e.info.message = Some("等待选择书名".to_string());
        e.book_name_sender = Some(sender);
        e.info.updated_ms = now_ms();
    }

    pub(crate) fn submit_book_name_choice(&self, id: u64, choice: Option<String>) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(e) = g.get_mut(&id) else {
            return false;
        };
        if let Some(tx) = e.book_name_sender.take() {
            let _ = tx.send(choice);
            e.info.book_name_options = None;
            e.info.message = None;
            e.info.updated_ms = now_ms();
            return true;
        }
        false
    }

    pub(crate) fn set_format_options(
        &self,
        id: u64,
        options: Vec<BookNameOption>,
        sender: std::sync::mpsc::Sender<Option<String>>,
    ) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(e) = g.get_mut(&id) else {
            return;
        };
        e.info.format_options = Some(options);
        e.info.message = Some("等待选择输出格式".to_string());
        e.format_sender = Some(sender);
        e.info.updated_ms = now_ms();
    }

    pub(crate) fn submit_format_choice(&self, id: u64, choice: Option<String>) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(e) = g.get_mut(&id) else {
            return false;
        };
        if let Some(tx) = e.format_sender.take() {
            let _ = tx.send(choice);
            e.info.format_options = None;
            e.info.message = None;
            e.info.updated_ms = now_ms();
            return true;
        }
        false
    }

    fn update<F: FnOnce(&mut JobInfo)>(&self, id: u64, f: F) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(e) = g.get_mut(&id) else {
            return;
        };
        f(&mut e.info);
        e.info.updated_ms = now_ms();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
