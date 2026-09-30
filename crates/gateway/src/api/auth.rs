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

pub async fn authenticate(state: &AppState, headers: &axum::http::HeaderMap) -> ApiResult<Account> {
    let raw = extract_raw_key(headers).ok_or_else(|| {
        ApiError::unauthorized("缺少 API Key：请用 Authorization: Bearer <key> 或 x-api-key 头")
    })?;
    let hash = hash_key(&raw);

    let row = sqlx::query("SELECT * FROM account WHERE api_key_hash = ? LIMIT 1")
        .bind(&hash)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::unauthorized("API Key 无效"))?;

    if row.get::<i64, _>("enabled") == 0 {
        return Err(ApiError::unauthorized("账号已停用"));
    }

    Ok(Account {
        id: row.get("id"),
        name: row.get("name"),
        enabled: true,
        balance_micro: row.get("balance_micro"),
        held_micro: row.get("held_micro"),
        llm_max_concurrency: row.get("llm_max_concurrency"),
        llm_max_queue: row.get("llm_max_queue"),
        image_max_concurrency: row.get("image_max_concurrency"),
        image_max_queue: row.get("image_max_queue"),
        rpm_limit: row.get("rpm_limit"),
    })
}

/// axum 提取器：把鉴权后的账号放进请求扩展。
pub struct Authed(pub Account);

impl FromRequestParts<std::sync::Arc<AppState>> for Authed {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let acc = authenticate(state, &parts.headers).await?;
        Ok(Authed(acc))
    }
}

/// 管理员会话守卫。
pub struct AdminAuthed;

impl FromRequestParts<std::sync::Arc<AppState>> for AdminAuthed {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get("x-admin-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| {
                parts
                    .headers
                    .get(axum::http::header::COOKIE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|c| {
                        c.split(';')
                            .filter_map(|kv| kv.trim().split_once('='))
                            .find(|(k, _)| *k == "hac_session")
                            .map(|(_, v)| v.to_string())
                    })
            })
            .ok_or_else(|| ApiError::unauthorized("未登录管理台"))?;

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
