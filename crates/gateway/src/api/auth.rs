//! 鉴权。
//!
//! 网关面：调用方的 API Key（`Authorization: Bearer` 或 Anthropic 的 `x-api-key`）。
//! 管理面：管理员口令换来的会话 token（`HttpOnly` Cookie）。
//!
//! 账号 key **只存哈希**：数据库被读走也无法直接冒用。存哈希用 SHA-256
//! 而非 Argon2——网关每请求都要验一次，Argon2 的参数是刻意选来慢的。

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::domain::account::Account;
use crate::domain::api_key::ApiKey;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn hash_key(raw: &str) -> String {
    let mut h = Sha256::new();
    h.update(raw.as_bytes());
    format!("{:x}", h.finalize())
}

pub fn key_prefix(raw: &str) -> String {
    raw.chars().take(8).collect()
}

/// 生成一个可读性好的调用 key。
pub fn generate_api_key() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    let body: String = (0..40)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect();
    format!("sk-hac-{body}")
}

/// 提取调用方的原始 key。Anthropic 客户端发 `x-api-key`，OpenAI 发
/// `Authorization: Bearer`——两种都收。
fn extract_raw_key(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    let auth = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer "))?;
    let t = token.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

pub async fn authenticate(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> ApiResult<(Account, ApiKey)> {
    let raw = extract_raw_key(headers).ok_or_else(|| {
        ApiError::unauthorized("缺少 API Key：请用 Authorization: Bearer <key> 或 x-api-key 头")
    })?;
    let hash = hash_key(&raw);

    // 一次 JOIN 拿到两样东西：key 定并发额度，account 定余额。
    // 分两次查的话，中间可能有人轮换了 key，两个对象对不上号。
    let row = sqlx::query(
        "SELECT k.id AS key_id, k.account_id, k.name AS key_name, k.key_hash, k.key_prefix,
                k.enabled AS key_enabled, k.llm_max_concurrency, k.llm_max_queue,
                k.image_max_concurrency, k.image_max_queue,
                a.name, a.balance_micro, a.held_micro, a.enabled AS account_enabled
         FROM api_key k JOIN account a ON a.id = k.account_id
         WHERE k.key_hash = ? LIMIT 1",
    )
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::unauthorized("API Key 无效"))?;

    if row.get::<i64, _>("account_enabled") == 0 {
        return Err(ApiError::unauthorized("账号已停用"));
    }
    if row.get::<i64, _>("key_enabled") == 0 {
        return Err(ApiError::unauthorized(format!(
            "API Key「{}」已停用",
            row.get::<String, _>("key_name")
        )));
    }

    let account = Account {
        id: row.get("account_id"),
        name: row.get("name"),
        enabled: true,
        balance_micro: row.get("balance_micro"),
        held_micro: row.get("held_micro"),
    };
    let key = ApiKey {
        id: row.get("key_id"),
        account_id: row.get("account_id"),
        name: row.get("key_name"),
        key_hash: row.get("key_hash"),
        key_prefix: row.get("key_prefix"),
        enabled: true,
        llm_max_concurrency: row.get("llm_max_concurrency"),
        llm_max_queue: row.get("llm_max_queue"),
        image_max_concurrency: row.get("image_max_concurrency"),
        image_max_queue: row.get("image_max_queue"),
    };
    Ok((account, key))
}

/// axum 提取器：把鉴权后的账号与 key 放进请求扩展。
///
/// 两个都要：额度看 key，钱看账号。调用方拿 `.0` / `.1` 有点难记，
/// 所以各给一个具名取值方法。
pub struct Authed {
    pub account: Account,
    pub key: ApiKey,
}

impl Authed {
    pub fn account(&self) -> &Account {
        &self.account
    }
    pub fn key(&self) -> &ApiKey {
        &self.key
    }
}

impl FromRequestParts<std::sync::Arc<AppState>> for Authed {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let (account, key) = authenticate(state, &parts.headers).await?;
        Ok(Authed { account, key })
    }
}

/// 从请求里取会话 token：先看 `x-admin-token`，再看 `hac_session` Cookie。
///
/// **登出也用它**：守卫验完「这 token 有效」之后，还得靠它把**同一把**
/// token 从 `session` 表里删掉。以前登出只清了浏览器 Cookie，库里的会话
/// 还在，token 又会随登录响应体一起回给前端——于是「已登出」的凭证
/// 还能再用满 7 天。
pub fn session_token(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(t) = headers
        .get("x-admin-token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return Some(t.to_string());
    }
    headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';')
                .filter_map(|kv| kv.trim().split_once('='))
                .find(|(k, _)| *k == "hac_session")
                .map(|(_, v)| v.to_string())
        })
        .filter(|t| !t.is_empty())
}

/// 管理员会话守卫。
pub struct AdminAuthed;

impl FromRequestParts<std::sync::Arc<AppState>> for AdminAuthed {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let token =
            session_token(&parts.headers).ok_or_else(|| ApiError::unauthorized("未登录管理台"))?;

        let now = crate::keys::unix_now();
        let ok: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session WHERE token = ? AND expires_at > ?",
        )
        .bind(&token)
        .bind(now)
        .fetch_one(&state.pool)
        .await?;
        if ok == 0 {
            return Err(ApiError::unauthorized("会话已过期，请重新登录"));
        }
        Ok(AdminAuthed)
    }
}

/// 口令哈希（管理员登录是低频操作，用 Argon2 是对的）。
pub fn hash_password(pw: &str) -> anyhow::Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};
    let salt = SaltString::generate(&mut OsRng);
    argon2::Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("口令哈希失败: {e}"))
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    match PasswordHash::new(hash) {
        Ok(p) => argon2::Argon2::default()
            .verify_password(pw.as_bytes(), &p)
            .is_ok(),
        Err(_) => false,
    }
}

pub fn new_session_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..48)
        .map(|_| format!("{:02x}", rng.gen_range(0..=255u8)))
        .collect()
}

/// 首次启动：用环境变量里的 token 初始化管理员。
pub async fn ensure_admin(pool: &sqlx::SqlitePool, token: Option<&str>) -> anyhow::Result<()> {
    let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_user")
        .fetch_one(pool)
        .await?;
    if existing > 0 {
        return Ok(());
    }
    let Some(tok) = token else {
        tracing::warn!("尚未初始化管理员：请设置 HOME_AI_ADMIN_TOKEN 环境变量后重启");
        return Ok(());
    };
    let now = crate::keys::unix_now();
    sqlx::query("INSERT INTO admin_user (id, username, password_hash, created_at, updated_at) VALUES (1,'admin',?,?,?)")
        .bind(hash_password(tok)?)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;
    tracing::info!("已用 HOME_AI_ADMIN_TOKEN 初始化管理员账号");
    Ok(())
}

// ── 登录节流 ─────────────────────────────────────────────────────────────────

/// 连续失败多少次开始退让。
const LOCK_AFTER_FAILS: u32 = 5;
/// 第一次锁多久（秒），之后翻倍。
const BASE_LOCK_SECS: i64 = 60;
/// 锁的上限（秒）。**必须有上限**：否则一次成功的爆破尝试会把管理员
/// 永久锁在门外——那比口令弱一点好不了多少。
const MAX_LOCK_SECS: i64 = 300;
/// 多久没动静就忘掉这个来源。兼作内存回收：家里不会有无限多个来源。
const FORGET_AFTER_SECS: i64 = 3600;

#[derive(Debug, Clone, Copy)]
struct Attempt {
    fails: u32,
    locked_until: i64,
    last: i64,
}

/// 登录失败节流。
///
/// 口令是管理面的**唯一**屏障，Argon2 只把爆破速度从「毫秒一次」压到
/// 「每秒几次」——家里任何一台设备都能把 8 位口令试完。这里按来源计数，
/// 连续失败就退让，退让时间随失败次数翻倍但有上限。
///
/// **只按对端 IP 计数，不采信 `x-forwarded-for`**：那个头是谁都能填，
/// 采信它等于节流可以被一个请求头绕开。代价是网关在反代后面时所有登录
/// 都算同一个来源——单管理员的家庭网关里，这恰好是想要的行为。
#[derive(Debug, Default)]
pub struct LoginThrottle {
    inner: std::sync::Mutex<std::collections::HashMap<String, Attempt>>,
}

impl LoginThrottle {
    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Attempt>> {
        // 中毒不致命：这里存的不是需要保护的数据，宁可继续计数
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 还在退让期内就返回剩余秒数。
    pub fn blocked_for(&self, who: &str) -> Option<u64> {
        self.blocked_at(who, crate::keys::unix_now())
    }

    pub fn fail(&self, who: &str) {
        self.fail_at(who, crate::keys::unix_now());
    }

    /// 登录成功即清零。这一步不能省：不然管理员自己手滑输错三次之后，
    /// 还得等退让过去才能登进去。
    pub fn succeed(&self, who: &str) {
        self.lock().remove(who);
    }

    fn blocked_at(&self, who: &str, now: i64) -> Option<u64> {
        let a = *self.lock().get(who)?;
        (a.locked_until > now).then_some((a.locked_until - now) as u64)
    }

    fn fail_at(&self, who: &str, now: i64) {
        let mut m = self.lock();
        // 顺手回收：否则表会随扫到的来源数无上限地长
        m.retain(|_, a| now - a.last < FORGET_AFTER_SECS);
        let a = m.entry(who.to_string()).or_insert(Attempt {
            fails: 0,
            locked_until: 0,
            last: now,
        });
        a.last = now;
        a.fails = a.fails.saturating_add(1);
        if a.fails >= LOCK_AFTER_FAILS {
            let extra = (a.fails - LOCK_AFTER_FAILS).min(8) as u64;
            a.locked_until = now + BASE_LOCK_SECS.saturating_mul(1 << extra).min(MAX_LOCK_SECS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    /// 登出要靠它拿到**那把** token 去删库里的行。
    #[test]
    fn session_token_reads_the_cookie() {
        let h = headers(&[("cookie", "other=1; hac_session=abc123; more=2")]);
        assert_eq!(session_token(&h).as_deref(), Some("abc123"));
    }

    #[test]
    fn session_token_prefers_the_header() {
        let h = headers(&[
            ("x-admin-token", "from-header"),
            ("cookie", "hac_session=from-cookie"),
        ]);
        assert_eq!(session_token(&h).as_deref(), Some("from-header"));
    }

    /// 空 Cookie / 没有 Cookie 都必须当成「没登录」，不能当空串去查库。
    #[test]
    fn empty_session_token_is_not_a_token() {
        assert!(session_token(&headers(&[("cookie", "hac_session=")])).is_none());
        assert!(session_token(&headers(&[("cookie", "other=1")])).is_none());
        assert!(session_token(&headers(&[("x-admin-token", "   ")])).is_none());
    }

    /// 连续失败到阈值才开始拦，之前的失败不该影响正常登录。
    #[test]
    fn few_failures_do_not_lock() {
        let t = LoginThrottle::default();
        for _ in 0..(LOCK_AFTER_FAILS - 1) {
            t.fail_at("192.0.2.1", 1000);
        }
        assert_eq!(t.blocked_at("192.0.2.1", 1000), None);
    }

    #[test]
    fn reaching_the_threshold_locks() {
        let t = LoginThrottle::default();
        for _ in 0..LOCK_AFTER_FAILS {
            t.fail_at("192.0.2.1", 1000);
        }
        let left = t.blocked_at("192.0.2.1", 1000).expect("应当被拦");
        assert!(left > 0 && left <= MAX_LOCK_SECS as u64, "{left}");
    }

    /// 退让会自己过期。没有过期就成了永久锁门，那是比口令弱的东西。
    #[test]
    fn the_lock_expires_on_its_own() {
        let t = LoginThrottle::default();
        for _ in 0..LOCK_AFTER_FAILS {
            t.fail_at("192.0.2.1", 1000);
        }
        assert!(t.blocked_at("192.0.2.1", 1000 + MAX_LOCK_SECS).is_none());
    }

    /// 翻倍但封顶：不能让一次尝试把管理员锁到明天。
    #[test]
    fn the_lock_is_capped() {
        let t = LoginThrottle::default();
        for _ in 0..40 {
            t.fail_at("192.0.2.1", 1000);
        }
        assert!(t.blocked_at("192.0.2.1", 1000).unwrap() <= MAX_LOCK_SECS as u64);
    }

    /// 不同来源各算各的，否则一台设备能锁掉全屋。
    #[test]
    fn sources_are_independent() {
        let t = LoginThrottle::default();
        for _ in 0..LOCK_AFTER_FAILS {
            t.fail_at("192.0.2.1", 1000);
        }
        assert!(t.blocked_at("192.0.2.1", 1000).is_some());
        assert_eq!(t.blocked_at("192.0.2.9", 1000), None);
    }

    /// 管理员自己手滑输错几次，之后要能直接登进去。
    #[test]
    fn success_clears_the_counter() {
        let t = LoginThrottle::default();
        for _ in 0..(LOCK_AFTER_FAILS - 1) {
            t.fail_at("192.0.2.1", 1000);
        }
        t.succeed("192.0.2.1");
        for _ in 0..(LOCK_AFTER_FAILS - 1) {
            t.fail_at("192.0.2.1", 1000);
        }
        assert_eq!(t.blocked_at("192.0.2.1", 1000), None);
    }

    /// 长时间不来的来源要被忘掉，兼作内存回收。
    #[test]
    fn stale_sources_are_forgotten() {
        let t = LoginThrottle::default();
        for _ in 0..LOCK_AFTER_FAILS {
            t.fail_at("192.0.2.1", 1000);
        }
        // 换一个来源触发清理
        t.fail_at("192.0.2.2", 1000 + FORGET_AFTER_SECS + 1);
        assert_eq!(t.lock().len(), 1, "老来源应当被清掉");
    }
}
