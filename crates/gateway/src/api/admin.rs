//! 管理面 API。
//!
//! 一次写入 = 一个事务 + 一次快照重建，**保存即生效，无需重启**。
//! 因此这里不缓存任何配置副本，也不做「读自己的缓存」这种会漂移的事。
//!
//! 鉴权走 [`AdminAuthed`]：口令换来的会话 token，放在 `HttpOnly` Cookie 里。
//! 网关面（API Key）与管理面是两套凭证，互不通用。

use axum::extract::{ConnectInfo, Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::sync::Arc;

use crate::api::auth::{self, AdminAuthed};
use crate::billing::{ledger, usage};
use crate::domain::node::RateLimitScope;
use crate::domain::validate;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

type Shared = State<Arc<AppState>>;

fn now() -> i64 {
    crate::keys::unix_now()
}

fn created() -> StatusCode {
    StatusCode::CREATED
}

// ── 登录 / 会话 ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LoginBody {
    pub username: String,
    pub password: String,
}

/// 请求是否经过 HTTPS。
///
/// 仓库不做 TLS 终止，所以正常部署下这个恒为 false——**这正是问题**：
/// 套了 Nginx/Caddy 之后网关自己并不知道会话走在加密链路上，
/// 于是 Cookie 少了 `Secure`，被降级到 http 时照样发。
/// 采信 `x-forwarded-proto` 在这里是安全的：伪造它只能让 Cookie
/// 变得更严（浏览器不再走 http），不可能更松。
fn request_is_https(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

pub async fn login(
    State(s): Shared,
    // `Option<Extension<…>>` 而不是 `Option<ConnectInfo<…>>`：axum 0.8
    // 没给 `ConnectInfo` 实现 `OptionalFromRequestParts`，少了扩展就得
    // 直接报错——单测里没有 connect info 时登录会整个挂掉。
    peer: Option<Extension<ConnectInfo<std::net::SocketAddr>>>,
    headers: axum::http::HeaderMap,
    Json(b): Json<LoginBody>,
) -> ApiResult<Response> {
    // 认不出对端就退化成单一来源：宁可所有人都被一起限流，
    // 也不能因为拿不到 IP 就完全不限流。
    let who = match peer {
        Some(Extension(ConnectInfo(addr))) => addr.ip().to_string(),
        None => "unknown".to_string(),
    };

    if let Some(secs) = s.login_throttle.blocked_for(&who) {
        return Err(
            ApiError::too_many_requests(format!("登录尝试过于频繁，{secs} 秒后再试"))
                .with_retry_after(secs),
        );
    }

    let row = sqlx::query("SELECT * FROM admin_user WHERE username = ?")
        .bind(&b.username)
        .fetch_optional(&s.pool)
        .await?;

    let Some(row) = row else {
        s.login_throttle.fail(&who);
        return Err(ApiError::unauthorized("用户名或口令不对"));
    };

    let hash: String = row.get("password_hash");
    // 口令不对也要走完校验再返回，避免用响应时间探测用户名
    if !auth::verify_password(&b.password, &hash) {
        s.login_throttle.fail(&who);
        return Err(ApiError::unauthorized("用户名或口令不对"));
    }
    s.login_throttle.succeed(&who);

    let token = auth::new_session_token();
    let expires = now() + 7 * 24 * 3600;
    sqlx::query("INSERT INTO session (token, expires_at, created_at) VALUES (?,?,?)")
        .bind(&token)
        .bind(expires)
        .bind(now())
        .execute(&s.pool)
        .await?;

    // https 才加 Secure。见 `request_is_https`。
    let secure = if request_is_https(&headers) {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "hac_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{secure}",
        7 * 24 * 3600
    );
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("set-cookie", cookie)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({ "token": token, "expires_at": expires }).to_string(),
        ))
        .unwrap())
}

/// 登出删的就是这一行。
///
/// 单独抽出来是为了能直接测。原来这里传的是 `Some(String::new())`，
/// 于是执行的是 `DELETE … WHERE token = ''`——一行都删不掉，而浏览器
/// 那边的 Cookie 已经清了，「已登出」的凭证还能继续用满 7 天。
async fn delete_session(pool: &sqlx::SqlitePool, token: &str) -> anyhow::Result<u64> {
    Ok(sqlx::query("DELETE FROM session WHERE token = ?")
        .bind(token)
        .execute(pool)
        .await?
        .rows_affected())
}

/// 登出。
///
/// 关键是**真的把 `session` 里那一行删掉**：只清浏览器 Cookie 的话，
/// 那把 token 仍然有效到过期为止，而它登录时就随响应体一起回给了前端。
pub async fn logout(
    State(s): Shared,
    headers: axum::http::HeaderMap,
    _a: AdminAuthed,
) -> ApiResult<Response> {
    if let Some(t) = auth::session_token(&headers) {
        delete_session(&s.pool, &t).await?;
    }
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(
            "set-cookie",
            "hac_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        )
        .header("content-type", "application/json")
        .body(axum::body::Body::from("{}"))
        .unwrap())
}

pub async fn me(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let row = sqlx::query("SELECT id, username, created_at FROM admin_user WHERE id = 1")
        .fetch_one(&s.pool)
        .await?;
    Ok(Json(json!({
        "id": row.get::<i64, _>("id"),
        "username": row.get::<String, _>("username"),
        "created_at": row.get::<i64, _>("created_at"),
    })))
}

#[derive(Deserialize)]
pub struct PasswordBody {
    pub old_password: String,
    pub new_password: String,
}

pub async fn change_password(
    State(s): Shared,
    _a: AdminAuthed,
    Json(b): Json<PasswordBody>,
) -> ApiResult<Json<Value>> {
    validate::password(&b.new_password)?;
    let row = sqlx::query("SELECT password_hash FROM admin_user WHERE id = 1")
        .fetch_one(&s.pool)
        .await?;
    if !auth::verify_password(&b.old_password, &row.get::<String, _>("password_hash")) {
        return Err(ApiError::unauthorized("原口令不对"));
    }
    sqlx::query("UPDATE admin_user SET password_hash = ?, updated_at = ? WHERE id = 1")
        .bind(auth::hash_password(&b.new_password)?)
        .bind(now())
        .execute(&s.pool)
        .await?;
    // 改完口令作废所有会话
    sqlx::query("DELETE FROM session")
        .execute(&s.pool)
        .await
        .ok();
    Ok(Json(json!({ "ok": true })))
}

// ── 账号 ────────────────────────────────────────────────────────────────────
//
// 账号只管**钱**。key 和并发额度在下面的 `api_key` 区。
// 一个账号可以有多把 key，余额是它们共用的。

pub async fn list_accounts(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let rows = sqlx::query("SELECT * FROM account ORDER BY id")
        .fetch_all(&s.pool)
        .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "name": r.get::<String, _>("name"),
                "enabled": r.get::<i64, _>("enabled") != 0,
                "balance_micro": r.get::<i64, _>("balance_micro"),
                "held_micro": r.get::<i64, _>("held_micro"),
                "available_micro": r.get::<i64, _>("balance_micro") - r.get::<i64, _>("held_micro"),
                "created_at": r.get::<i64, _>("created_at"),
            })
        })
        .collect();
    // 列表里带上各自的 key，界面上不用再发一轮请求
    let keys = load_keys(&s, None).await?;
    Ok(Json(json!({
        "items": items,
        "keys": keys,
        // 建账号时的默认额度，新建的那把「默认」key 就用这套值
        "default_quota": {
            "llm_max_concurrency": DEFAULT_LLM_CONCURRENCY,
            "llm_max_queue": DEFAULT_LLM_QUEUE,
            "image_max_concurrency": DEFAULT_IMAGE_CONCURRENCY,
            "image_max_queue": DEFAULT_IMAGE_QUEUE,
        }
    })))
}

#[derive(Deserialize)]
pub struct AccountBody {
    pub name: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    pub balance_micro: Option<i64>,
    /// 建账号时给第一把 key 的额度；之后每把 key 各自独立。
    pub llm_max_concurrency: Option<i64>,
    pub llm_max_queue: Option<i64>,
    pub image_max_concurrency: Option<i64>,
    pub image_max_queue: Option<i64>,
}

/// 建账号。明文 key **只在这一次响应里出现**，库里只存 SHA-256。
///
/// 必然同时建一把叫「默认」的 key：建出来却没有任何 key 的账号
/// 是没法用的状态，与其让用户自己想起来补，不如当场给一把。
pub async fn create_account(
    State(s): Shared,
    _a: AdminAuthed,
    Json(b): Json<AccountBody>,
) -> ApiResult<Response> {
    validate::validate_name(&b.name)?;
    let raw = auth::generate_api_key();
    let now = now();
    let mut tx = s.pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO account (name, enabled, balance_micro, held_micro, created_at, updated_at)
         VALUES (?,?,?,0,?,?)",
    )
    .bind(&b.name)
    .bind(b.enabled.unwrap_or(true) as i64)
    .bind(b.balance_micro.unwrap_or(0))
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        if matches!(&e, sqlx::Error::Database(d) if d.is_unique_violation()) {
            ApiError::bad_request("账号名已存在")
        } else {
            e.into()
        }
    })?
    .last_insert_rowid();

    let key_id = insert_key(
        &mut tx,
        id,
        "默认",
        &raw,
        b.llm_max_concurrency,
        b.llm_max_queue,
        b.image_max_concurrency,
        b.image_max_queue,
        now,
    )
    .await?;
    tx.commit().await?;

    Ok((
        created(),
        Json(json!({
            "id": id,
            "key_id": key_id,
            "key_name": "默认",
            // 明文只此一次
            "api_key": raw,
        })),
    )
        .into_response())
}

pub async fn update_account(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
    Json(b): Json<AccountBody>,
) -> ApiResult<Json<Value>> {
    validate::validate_name(&b.name)?;
    // 额度不在这里改：它在 key 上。传了也当没传，否则用户以为改了其实没改。
    if b.llm_max_concurrency.is_some()
        || b.llm_max_queue.is_some()
        || b.image_max_concurrency.is_some()
        || b.image_max_queue.is_some()
    {
        return Err(ApiError::bad_request(
            "并发额度是每把 API Key 独立的，请在「API Key」里改",
        ));
    }
    let res = sqlx::query(
        "UPDATE account SET name = ?, enabled = COALESCE(?, enabled), updated_at = ?
         WHERE id = ?",
    )
    .bind(&b.name)
    .bind(b.enabled.map(|v| v as i64))
    .bind(now())
    .bind(id)
    .execute(&s.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found("账号不存在"));
    }
    s.reload().await.ok();
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_account(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    // 明细表对 account 有 ON DELETE CASCADE，删账号即删明细。
    // 这是刻意的：留着没有归属的明细只会污染统计。
    sqlx::query("DELETE FROM account WHERE id = ?")
        .bind(id)
        .execute(&s.pool)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

// ── API Key ─────────────────────────────────────────────────────────────────

const DEFAULT_LLM_CONCURRENCY: i64 = 2;
const DEFAULT_LLM_QUEUE: i64 = 8;
const DEFAULT_IMAGE_CONCURRENCY: i64 = 1;
const DEFAULT_IMAGE_QUEUE: i64 = 4;

fn key_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64, _>("id"),
        "account_id": r.get::<i64, _>("account_id"),
        "name": r.get::<String, _>("name"),
        // 只给前缀。明文永远不落库，界面上靠前缀认人。
        "key_prefix": r.get::<String, _>("key_prefix"),
        "enabled": r.get::<i64, _>("enabled") != 0,
        "llm_max_concurrency": r.get::<i64, _>("llm_max_concurrency"),
        "llm_max_queue": r.get::<i64, _>("llm_max_queue"),
        "image_max_concurrency": r.get::<i64, _>("image_max_concurrency"),
        "image_max_queue": r.get::<i64, _>("image_max_queue"),
        "created_at": r.get::<i64, _>("created_at"),
    })
}

/// `account_id = None` 时返回全部 key（账号列表页要用）。
async fn load_keys(s: &AppState, account_id: Option<i64>) -> ApiResult<Vec<Value>> {
    let rows = match account_id {
        Some(a) => {
            sqlx::query("SELECT * FROM api_key WHERE account_id = ? ORDER BY id")
                .bind(a)
                .fetch_all(&s.pool)
                .await?
        }
        None => {
            sqlx::query("SELECT * FROM api_key ORDER BY id")
                .fetch_all(&s.pool)
                .await?
        }
    };
    Ok(rows.iter().map(key_json).collect())
}

#[allow(clippy::too_many_arguments)]
async fn insert_key(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: i64,
    name: &str,
    raw: &str,
    llm_conc: Option<i64>,
    llm_queue: Option<i64>,
    img_conc: Option<i64>,
    img_queue: Option<i64>,
    now: i64,
) -> ApiResult<i64> {
    Ok(sqlx::query(
        "INSERT INTO api_key (account_id, name, key_hash, key_prefix, enabled,
                              llm_max_concurrency, llm_max_queue,
                              image_max_concurrency, image_max_queue, created_at, updated_at)
         VALUES (?,?,?,?,1,?,?,?,?,?,?)",
    )
    .bind(account_id)
    .bind(name)
    .bind(auth::hash_key(raw))
    .bind(auth::key_prefix(raw))
    .bind(llm_conc.unwrap_or(DEFAULT_LLM_CONCURRENCY).clamp(1, 1024))
    .bind(llm_queue.unwrap_or(DEFAULT_LLM_QUEUE).max(0))
    .bind(img_conc.unwrap_or(DEFAULT_IMAGE_CONCURRENCY).clamp(1, 1024))
    .bind(img_queue.unwrap_or(DEFAULT_IMAGE_QUEUE).max(0))
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(|e| {
        if matches!(&e, sqlx::Error::Database(d) if d.is_unique_violation()) {
            ApiError::bad_request("该账号下已有同名 key")
        } else {
            e.into()
        }
    })?
    .last_insert_rowid())
}

#[derive(Deserialize)]
pub struct ApiKeyBody {
    pub name: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    pub llm_max_concurrency: Option<i64>,
    pub llm_max_queue: Option<i64>,
    pub image_max_concurrency: Option<i64>,
    pub image_max_queue: Option<i64>,
}

/// 在账号下再建一把 key。明文同样只出现一次。
pub async fn create_api_key(
    State(s): Shared,
    _a: AdminAuthed,
    Path(account_id): Path<i64>,
    Json(b): Json<ApiKeyBody>,
) -> ApiResult<Response> {
    validate::validate_name(&b.name)?;
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM account WHERE id = ?")
        .bind(account_id)
        .fetch_one(&s.pool)
        .await?;
    if exists == 0 {
        return Err(ApiError::not_found("账号不存在"));
    }
    let raw = auth::generate_api_key();
    let mut tx = s.pool.begin().await?;
    let id = insert_key(
        &mut tx,
        account_id,
        &b.name,
        &raw,
        b.llm_max_concurrency,
        b.llm_max_queue,
        b.image_max_concurrency,
        b.image_max_queue,
        now(),
    )
    .await?;
    tx.commit().await?;

    Ok((created(), Json(json!({ "id": id, "api_key": raw }))).into_response())
}

pub async fn update_api_key(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
    Json(b): Json<ApiKeyBody>,
) -> ApiResult<Json<Value>> {
    validate::validate_name(&b.name)?;
    let res = sqlx::query(
        "UPDATE api_key SET name = ?, enabled = COALESCE(?, enabled),
                llm_max_concurrency = COALESCE(?, llm_max_concurrency),
                llm_max_queue = COALESCE(?, llm_max_queue),
                image_max_concurrency = COALESCE(?, image_max_concurrency),
                image_max_queue = COALESCE(?, image_max_queue),
                updated_at = ?
         WHERE id = ?",
    )
    .bind(&b.name)
    .bind(b.enabled.map(|v| v as i64))
    .bind(b.llm_max_concurrency.map(|v| v.clamp(1, 1024)))
    .bind(b.llm_max_queue.map(|v| v.max(0)))
    .bind(b.image_max_concurrency.map(|v| v.clamp(1, 1024)))
    .bind(b.image_max_queue.map(|v| v.max(0)))
    .bind(now())
    .bind(id)
    .execute(&s.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found("key 不存在"));
    }
    // 额度变了要立刻唤醒这把 key 的排队者，否则调大额度后
    // 已在队列里排着的请求会一直等到超时。
    if let Some(c) = key_llm_concurrency(&s, id).await? {
        s.key_gate(id).await.nudge(c).await;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn key_llm_concurrency(s: &AppState, id: i64) -> ApiResult<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT llm_max_concurrency FROM api_key WHERE id = ?")
            .bind(id)
            .fetch_optional(&s.pool)
            .await?,
    )
}

/// 轮换某一把 key：只有这把失效，账号下别的 key 不受影响。
///
/// 这是与旧行为的唯一区别 —— 以前账号只有一把 key，「轮换」等于换掉账号的全部凭据。
pub async fn rotate_api_key(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let raw = auth::generate_api_key();
    let res =
        sqlx::query("UPDATE api_key SET key_hash = ?, key_prefix = ?, updated_at = ? WHERE id = ?")
            .bind(auth::hash_key(&raw))
            .bind(auth::key_prefix(&raw))
            .bind(now())
            .bind(id)
            .execute(&s.pool)
            .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found("key 不存在"));
    }
    Ok(Json(json!({ "api_key": raw })))
}

pub async fn delete_api_key(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    // request_log 上是 ON DELETE SET NULL：删 key 不删别人的历史账，
    // 那笔账照样是发生过的，只是查不出是哪把 key 发的。
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM account")
        .fetch_one(&s.pool)
        .await?;
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM api_key WHERE account_id =
           (SELECT account_id FROM api_key WHERE id = ?)",
    )
    .bind(id)
    .fetch_one(&s.pool)
    .await
    .unwrap_or(0);
    if remaining <= 1 {
        return Err(ApiError::bad_request(format!(
            "这是账号下最后一把 key，删了账号就没法用了（现有账号 {n} 个）。\
             停用它（enabled=false）比删掉更安全"
        )));
    }
    sqlx::query("DELETE FROM api_key WHERE id = ?")
        .bind(id)
        .execute(&s.pool)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

/// 每把 key 的用量：rpm / tpm / 出图张数。
///
/// **只统计不拦截。** 限流是并发闸门的事（`key_gate`），这里是给人和调用方
/// 看的读数。想按分钟看趋势就带 `minutes`，不带则只要今日汇总。
pub async fn api_key_stats(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let minutes: i64 = q
        .get("minutes")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .clamp(0, 1440);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    let today_row: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(requests),0), COALESCE(SUM(prompt_tokens+completion_tokens),0),
                COALESCE(SUM(image_count),0), COALESCE(SUM(cost_micro),0), COALESCE(SUM(errors),0)
         FROM usage_daily WHERE api_key_id = ? AND day = ?",
    )
    .bind(id)
    .bind(&today)
    .fetch_one(&s.pool)
    .await
    .unwrap_or((0, 0, 0, 0, 0));

    // 按分钟分桶。`created_at` 是秒，除 60 向下取整即分钟序号。
    let series: Vec<(i64, i64, i64, i64)> = if minutes > 0 {
        sqlx::query_as(
            "SELECT created_at / 60, COUNT(*),
                    COALESCE(SUM(prompt_tokens+completion_tokens),0), COALESCE(SUM(image_count),0)
             FROM request_log
             WHERE api_key_id = ?
               AND created_at >= strftime('%s','now') - ?
             GROUP BY created_at / 60 ORDER BY 1",
        )
        .bind(id)
        .bind(minutes * 60)
        .fetch_all(&s.pool)
        .await
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    Ok(Json(json!({
        "key_id": id,
        "today": {
            "day": today,
            "requests": today_row.0,
            "tokens": today_row.1,
            "images": today_row.2,
            "cost_micro": today_row.3,
            "errors": today_row.4,
        },
        // 每分钟一根柱子：requests=该分钟请求数，tokens=该分钟 token 数
        "series": series.iter().map(|(m, r, t, i)| json!({
            "minute": m * 60,
            "requests": r,
            "tokens": t,
            "images": i,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
pub struct TopupBody {
    pub amount_micro: i64,
    #[serde(default)]
    pub note: String,
}

pub async fn topup(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
    Json(b): Json<TopupBody>,
) -> ApiResult<Json<Value>> {
    if b.amount_micro == 0 {
        return Err(ApiError::bad_request("金额不能为 0"));
    }
    let note = if b.note.trim().is_empty() {
        "充值".to_string()
    } else {
        b.note.clone()
    };
    let bal = ledger::adjust(&s.pool, id, b.amount_micro, &note).await?;
    Ok(Json(json!({ "balance_micro": bal })))
}

pub async fn account_txns(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let rows =
        sqlx::query("SELECT * FROM balance_txn WHERE account_id = ? ORDER BY id DESC LIMIT 200")
            .bind(id)
            .fetch_all(&s.pool)
            .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "kind": r.get::<String, _>("kind"),
                "amount_micro": r.get::<i64, _>("amount_micro"),
                "balance_after": r.get::<i64, _>("balance_after"),
                "note": r.get::<Option<String>, _>("note"),
                "created_at": r.get::<i64, _>("created_at"),
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

// ── LLM 节点 ────────────────────────────────────────────────────────────────

pub async fn list_nodes(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let snap = s.snapshot();
    let health = s.health.snapshot().await;
    let mut items = Vec::new();
    for n in &snap.nodes {
        let g = s.node_gate(n.id).await;
        let st = g.snapshot(n.max_concurrency).await;
        let h = health.get(&n.id);
        items.push(json!({
            "id": n.id,
            "name": n.name,
            "kind": n.kind,
            "base_url": n.base_url,
            "lan_base_url": n.lan_base_url,
            "max_concurrency": n.max_concurrency,
            "default_max_output_tokens": n.default_max_output_tokens,
            "enabled": n.enabled,
            "sort_order": n.sort_order,
            "disabled_start": n.window.start,
            "disabled_end": n.window.end,
            "disabled_timezone": n.window.timezone,
            "in_disabled_window": n.window.in_disabled_hours(),
            "extra_headers": n.extra_headers,
            "extra_body": n.extra_body,
            "active": st.active,
            "waiting": st.waiting,
            "usable_keys": n.keys.iter().filter(|k| k.is_usable_at(now())).count(),
            "total_keys": n.keys.len(),
            "healthy": h.map(|x| x.healthy).unwrap_or(true),
            "cooldown_secs_remaining": h.map(|x| x.cooldown_secs_remaining).unwrap_or(0),
            "last_error": h.and_then(|x| x.last_error.clone()),
            // 内网优先是否正在走公网兜底。不露出来的话这个回退是隐形的：
            // 用户只会觉得「怎么变慢了」，不知道网关已经绕了一圈。
            "lan_configured": n
                .lan_base_url
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty()),
            "lan_down": s.health.is_lan_down(n.id).await,
            "lan_retry_after_secs": s.health.lan_down_remaining(n.id).await,
        }));
    }
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct NodeBody {
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    pub base_url: String,
    pub lan_base_url: Option<String>,
    pub max_concurrency: i64,
    pub default_max_output_tokens: Option<i64>,
    #[serde(default)]
    pub enabled: Option<bool>,
    pub sort_order: Option<i64>,
    pub disabled_start: Option<u8>,
    pub disabled_end: Option<u8>,
    pub disabled_timezone: Option<String>,
    pub extra_headers: Option<Value>,
    pub extra_body: Option<Value>,
}

pub async fn create_node(State(s): Shared, _a: AdminAuthed, Json(b): Json<NodeBody>) -> ApiResult<Response> {
    validate::validate_name(&b.name)?;
    validate::base_url(&b.base_url)?;
    validate::window(b.disabled_start, b.disabled_end)?;
    validate::max_concurrency(b.max_concurrency)?;
    let now = now();
    let id = sqlx::query(
        "INSERT INTO llm_node (name, kind, base_url, lan_base_url, max_concurrency,
                               default_max_output_tokens, enabled, sort_order,
                               disabled_start, disabled_end, disabled_timezone,
                               extra_headers, extra_body, created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&b.name)
    .bind(b.kind.as_deref().unwrap_or("openai"))
    .bind(validate::trim_url(&b.base_url))
    .bind(b.lan_base_url.as_deref().map(validate::trim_url))
    .bind(b.max_concurrency)
    .bind(b.default_max_output_tokens)
    .bind(b.enabled.unwrap_or(true) as i64)
    .bind(b.sort_order.unwrap_or(0))
    .bind(b.disabled_start.map(|v| v as i64))
    .bind(b.disabled_end.map(|v| v as i64))
    .bind(b.disabled_timezone.as_deref())
    .bind(json_str(&b.extra_headers))
    .bind(json_str(&b.extra_body))
    .bind(now)
    .bind(now)
    .execute(&s.pool)
    .await
    .map_err(|e| {
        if matches!(&e, sqlx::Error::Database(d) if d.is_unique_violation()) {
            ApiError::bad_request("节点名已存在")
        } else {
            e.into()
        }
    })?
    .last_insert_rowid();
    s.reload().await?;
    Ok((created(), Json(json!({ "id": id }))).into_response())
}

pub async fn update_node(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
    Json(b): Json<NodeBody>,
) -> ApiResult<Json<Value>> {
    validate::validate_name(&b.name)?;
    validate::base_url(&b.base_url)?;
    validate::window(b.disabled_start, b.disabled_end)?;
    validate::max_concurrency(b.max_concurrency)?;
    let res = sqlx::query(
        "UPDATE llm_node SET name=?, base_url=?, lan_base_url=?, max_concurrency=?,
                default_max_output_tokens=?, enabled=COALESCE(?, enabled), sort_order=COALESCE(?, sort_order),
                disabled_start=?, disabled_end=?, disabled_timezone=?,
                extra_headers=?, extra_body=?, updated_at=?
         WHERE id = ?",
    )
    .bind(&b.name)
    .bind(validate::trim_url(&b.base_url))
    .bind(b.lan_base_url.as_deref().map(validate::trim_url))
    .bind(b.max_concurrency)
    .bind(b.default_max_output_tokens)
    .bind(b.enabled.map(|v| v as i64))
    .bind(b.sort_order)
    .bind(b.disabled_start.map(|v| v as i64))
    .bind(b.disabled_end.map(|v| v as i64))
    .bind(b.disabled_timezone.as_deref())
    .bind(json_str(&b.extra_headers))
    .bind(json_str(&b.extra_body))
    .bind(now())
    .bind(id)
    .execute(&s.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found("节点不存在"));
    }
    // 改了配置就清掉内网不可达标记：标记按节点记，运营刚换上的新内网
    // 地址会被上一条地址的旧标记压住，最长白等一个冷却周期。
    s.health.mark_lan_up(id).await;
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_node(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    // llm_node_key / llm_route 都是 CASCADE
    sqlx::query("DELETE FROM llm_node WHERE id = ?")
        .bind(id)
        .execute(&s.pool)
        .await?;
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

// ── 节点 API Key ────────────────────────────────────────────────────────────

pub async fn list_keys(
    State(s): Shared,
    _a: AdminAuthed,
    Path(node_id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let rows = sqlx::query("SELECT * FROM llm_node_key WHERE node_id = ? ORDER BY sort_order, id")
        .bind(node_id)
        .fetch_all(&s.pool)
        .await?;
    // **不回明文**，只回 label 与末 4 位
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let secret: String = r.get("secret");
            json!({
                "id": r.get::<i64, _>("id"),
                "node_id": r.get::<i64, _>("node_id"),
                "label": r.get::<String, _>("label"),
                "masked": mask(&secret),
                "sort_order": r.get::<i64, _>("sort_order"),
                "enabled": r.get::<i64, _>("enabled") != 0,
                "state": r.get::<String, _>("state"),
                "cooldown_until": r.get::<Option<i64>, _>("cooldown_until"),
                "cooldown_remaining": r
                    .get::<Option<i64>, _>("cooldown_until")
                    .map(|t| (t - now()).max(0)),
                "reset_source": r.get::<Option<String>, _>("reset_source"),
                "quota_class": r.get::<Option<String>, _>("quota_class"),
                "matched_rule": r.get::<Option<String>, _>("matched_rule"),
                "matched_signal": r.get::<Option<String>, _>("matched_signal"),
                "rate_limit_scope": r.get::<String, _>("rate_limit_scope"),
                "soft_cap_window_ms": r.get::<Option<i64>, _>("soft_cap_window_ms"),
                "soft_cap_tokens": r.get::<Option<i64>, _>("soft_cap_tokens"),
                "window_tokens_used": r.get::<i64, _>("window_tokens_used"),
                "count_429": r.get::<i64, _>("count_429"),
                "count_rotations": r.get::<i64, _>("count_rotations"),
                "last_used_at": r.get::<Option<i64>, _>("last_used_at"),
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

fn mask(secret: &str) -> String {
    if secret.len() <= 4 {
        "****".into()
    } else {
        format!(
            "{}{}",
            &secret[..2],
            "*".repeat(secret.len().saturating_sub(6))
        )
    }
}

#[derive(Deserialize)]
pub struct KeyBody {
    /// 建 key 时必填；更新时留空表示不改明文。
    ///
    /// 这里**不能**是必填的 `String`：管理台的编辑弹窗不展示明文，
    /// 提交上来的 body 里没有这个字段，serde 会直接 422，
    /// 结果就是「改个标签都存不进去」。
    pub secret: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    pub sort_order: Option<i64>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub rate_limit_scope: Option<String>,
    pub soft_cap_window_ms: Option<i64>,
    pub soft_cap_tokens: Option<i64>,
}

pub async fn create_key(
    State(s): Shared,
    _a: AdminAuthed,
    Path(node_id): Path<i64>,
    Json(b): Json<KeyBody>,
) -> ApiResult<Response> {
    let secret = b.secret.as_deref().unwrap_or_default().trim();
    if secret.len() < 8 {
        return Err(ApiError::bad_request("API Key 太短"));
    }
    let id = sqlx::query(
        "INSERT INTO llm_node_key (node_id, label, secret, sort_order, enabled,
                                    rate_limit_scope, soft_cap_window_ms, soft_cap_tokens,
                                    created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(node_id)
    .bind(b.label.as_deref().unwrap_or("key"))
    .bind(secret)
    .bind(b.sort_order.unwrap_or(0))
    .bind(b.enabled.unwrap_or(true) as i64)
    .bind(RateLimitScope::parse(b.rate_limit_scope.as_deref().unwrap_or("perKey")).as_str())
    .bind(b.soft_cap_window_ms)
    .bind(b.soft_cap_tokens)
    .bind(now())
    .bind(now())
    .execute(&s.pool)
    .await?
    .last_insert_rowid();
    s.reload().await?;
    Ok((created(), Json(json!({ "id": id }))).into_response())
}

pub async fn update_key(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
    Json(b): Json<KeyBody>,
) -> ApiResult<Json<Value>> {
    // 空串 = 不改明文。这样「只改标签」和「顺手换 key」都能走同一个入口。
    let res = sqlx::query(
        "UPDATE llm_node_key SET label=COALESCE(?, label), sort_order=COALESCE(?, sort_order),
                enabled=COALESCE(?, enabled), rate_limit_scope=COALESCE(?, rate_limit_scope),
                secret=COALESCE(NULLIF(TRIM(?), ''), secret),
                soft_cap_window_ms=?, soft_cap_tokens=?, updated_at=?
         WHERE id = ?",
    )
    .bind(b.label.as_deref())
    .bind(b.sort_order)
    .bind(b.enabled.map(|v| v as i64))
    .bind(
        b.rate_limit_scope
            .as_deref()
            .map(RateLimitScope::parse)
            .map(RateLimitScope::as_str),
    )
    .bind(b.secret.as_deref().map(str::trim))
    .bind(b.soft_cap_window_ms)
    .bind(b.soft_cap_tokens)
    .bind(now())
    .bind(id)
    .execute(&s.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found("Key 不存在"));
    }
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_key(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    sqlx::query("DELETE FROM llm_node_key WHERE id = ?")
        .bind(id)
        .execute(&s.pool)
        .await?;
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

/// 人工解除冷却。5 小时限额是「等」，但 key 泄露/欠费是「修」——
/// 后者要人点一下才能恢复。
pub async fn clear_key_cooldown(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    s.keys.clear_cooldown(id).await?;
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn key_events(
    State(s): Shared,
    _a: AdminAuthed,
    Path(key_id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let rows =
        sqlx::query("SELECT * FROM key_rotation_event WHERE key_id = ? ORDER BY id DESC LIMIT 50")
            .bind(key_id)
            .fetch_all(&s.pool)
            .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "request_id": r.get::<Option<String>, _>("request_id"),
                "quota_class": r.get::<String, _>("quota_class"),
                "matched_rule": r.get::<String, _>("matched_rule"),
                "matched_signal": r.get::<Option<String>, _>("matched_signal"),
                "cooldown_ms": r.get::<i64, _>("cooldown_ms"),
                "reset_source": r.get::<String, _>("reset_source"),
                "created_at": r.get::<i64, _>("created_at"),
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

// ── 模型路由 ────────────────────────────────────────────────────────────────

pub async fn list_routes(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let rows = sqlx::query(
        "SELECT r.*, n.name AS node_name FROM llm_route r
         JOIN llm_node n ON n.id = r.node_id ORDER BY r.model_name, r.priority, r.id",
    )
    .fetch_all(&s.pool)
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "model_name": r.get::<String, _>("model_name"),
                "node_id": r.get::<i64, _>("node_id"),
                "node_name": r.get::<String, _>("node_name"),
                "upstream_model": r.get::<String, _>("upstream_model"),
                "enabled": r.get::<i64, _>("enabled") != 0,
                "priority": r.get::<i64, _>("priority"),
            })
        })
        .collect();
    Ok(Json(
        json!({ "items": items, "models": s.snapshot().models() }),
    ))
}

#[derive(Deserialize)]
pub struct RouteBody {
    pub model_name: String,
    pub node_id: i64,
    pub upstream_model: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    pub priority: Option<i64>,
}

/// 建路由用 upsert：同一个 (模型, 节点) 重复提交是常见操作，
/// 让它变成更新而不是报唯一键冲突。
pub async fn upsert_route(
    State(s): Shared,
    _a: AdminAuthed,
    Json(b): Json<RouteBody>,
) -> ApiResult<Json<Value>> {
    if b.model_name.trim().is_empty() || b.upstream_model.trim().is_empty() {
        return Err(ApiError::bad_request("模型名不能为空"));
    }
    sqlx::query(
        "INSERT INTO llm_route (model_name, node_id, upstream_model, enabled, priority)
         VALUES (?,?,?,?,?)
         ON CONFLICT(model_name, node_id) DO UPDATE SET
           upstream_model = excluded.upstream_model,
           enabled = excluded.enabled,
           priority = excluded.priority",
    )
    .bind(&b.model_name)
    .bind(b.node_id)
    .bind(&b.upstream_model)
    .bind(b.enabled.unwrap_or(true) as i64)
    .bind(b.priority.unwrap_or(0))
    .execute(&s.pool)
    .await
    .map_err(|e| {
        if matches!(&e, sqlx::Error::Database(d) if d.is_foreign_key_violation()) {
            ApiError::bad_request("节点不存在")
        } else {
            e.into()
        }
    })?;
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_route(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    sqlx::query("DELETE FROM llm_route WHERE id = ?")
        .bind(id)
        .execute(&s.pool)
        .await?;
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

// ── 定价 ────────────────────────────────────────────────────────────────────

pub async fn list_prices(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let rows = sqlx::query("SELECT * FROM model_price ORDER BY model_name")
        .fetch_all(&s.pool)
        .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "model_name": r.get::<String, _>("model_name"),
                "input_micro_per_1k": r.get::<i64, _>("input_micro_per_1k"),
                "output_micro_per_1k": r.get::<i64, _>("output_micro_per_1k"),
                "cached_input_micro_per_1k": r.get::<Option<i64>, _>("cached_input_micro_per_1k"),
            })
        })
        .collect();
    let snap = s.snapshot();
    Ok(Json(json!({
        "items": items,
        "default_model": snap.prices.default_model,
        "default_image_micro": snap.prices.default_image_micro,
        "models_without_price": snap
            .models()
            .into_iter()
            .filter(|m| snap.prices.model_price(m).is_none())
            .collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
pub struct PriceBody {
    pub model_name: String,
    pub input_micro_per_1k: i64,
    pub output_micro_per_1k: i64,
    pub cached_input_micro_per_1k: Option<i64>,
}

pub async fn upsert_price(
    State(s): Shared,
    _a: AdminAuthed,
    Json(b): Json<PriceBody>,
) -> ApiResult<Json<Value>> {
    // 单价必须是正数或 0：负单价会变成「用得越多返现越多」
    if b.input_micro_per_1k < 0 || b.output_micro_per_1k < 0 {
        return Err(ApiError::bad_request("单价不能为负"));
    }
    if let Some(c) = b.cached_input_micro_per_1k {
        if c < 0 {
            return Err(ApiError::bad_request("缓存单价不能为负"));
        }
    }
    sqlx::query(
        "INSERT INTO model_price (model_name, input_micro_per_1k, output_micro_per_1k, cached_input_micro_per_1k)
         VALUES (?,?,?,?)
         ON CONFLICT(model_name) DO UPDATE SET
           input_micro_per_1k = excluded.input_micro_per_1k,
           output_micro_per_1k = excluded.output_micro_per_1k,
           cached_input_micro_per_1k = excluded.cached_input_micro_per_1k",
    )
    .bind(&b.model_name)
    .bind(b.input_micro_per_1k)
    .bind(b.output_micro_per_1k)
    .bind(b.cached_input_micro_per_1k)
    .execute(&s.pool)
    .await?;
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_price(
    State(s): Shared,
    _a: AdminAuthed,
    Path(model): Path<String>,
) -> ApiResult<Json<Value>> {
    sqlx::query("DELETE FROM model_price WHERE model_name = ?")
        .bind(&model)
        .execute(&s.pool)
        .await?;
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

// ── ComfyUI 端点 ────────────────────────────────────────────────────────────

pub async fn list_comfy_nodes(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let snap = s.snapshot();
    let mut items = Vec::new();
    for n in &snap.comfy_nodes {
        let st = s.comfy_gate(n.id).await.snapshot(n.max_concurrency).await;
        items.push(json!({
            "id": n.id,
            "name": n.name,
            "base_url": n.base_url,
            "lan_base_url": n.lan_base_url,
            "has_password": !n.password.is_empty(),
            "max_concurrency": n.max_concurrency,
            "enabled": n.enabled,
            "sort_order": n.sort_order,
            "disabled_start": n.window.start,
            "disabled_end": n.window.end,
            "in_disabled_window": n.window.in_disabled_hours(),
            "active": st.active,
            "waiting": st.waiting,
        }));
    }
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct ComfyNodeBody {
    pub name: String,
    pub base_url: String,
    pub lan_base_url: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    /// 留空表示不改密码（更新时用）
    pub password: Option<String>,
    pub max_concurrency: i64,
    #[serde(default)]
    pub enabled: Option<bool>,
    pub sort_order: Option<i64>,
    pub disabled_start: Option<u8>,
    pub disabled_end: Option<u8>,
    pub disabled_timezone: Option<String>,
}

pub async fn create_comfy_node(
    State(s): Shared,
    _a: AdminAuthed,
    Json(b): Json<ComfyNodeBody>,
) -> ApiResult<Response> {
    validate::validate_name(&b.name)?;
    validate::base_url(&b.base_url)?;
    validate::max_concurrency(b.max_concurrency)?;
    validate::window(b.disabled_start, b.disabled_end)?;
    let now = now();
    let id = sqlx::query(
        "INSERT INTO comfy_node (name, base_url, lan_base_url, username, password,
                                 max_concurrency, enabled, sort_order,
                                 disabled_start, disabled_end, disabled_timezone, created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&b.name)
    .bind(validate::trim_url(&b.base_url))
    .bind(b.lan_base_url.as_deref().map(validate::trim_url))
    .bind(b.username.as_deref().unwrap_or(""))
    .bind(b.password.as_deref().unwrap_or(""))
    .bind(b.max_concurrency)
    .bind(b.enabled.unwrap_or(true) as i64)
    .bind(b.sort_order.unwrap_or(0))
    .bind(b.disabled_start.map(|v| v as i64))
    .bind(b.disabled_end.map(|v| v as i64))
    .bind(b.disabled_timezone.as_deref())
    .bind(now)
    .bind(now)
    .execute(&s.pool)
    .await?
    .last_insert_rowid();
    s.reload().await?;
    Ok((created(), Json(json!({ "id": id }))).into_response())
}

pub async fn update_comfy_node(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
    Json(b): Json<ComfyNodeBody>,
) -> ApiResult<Json<Value>> {
    validate::validate_name(&b.name)?;
    validate::base_url(&b.base_url)?;
    validate::max_concurrency(b.max_concurrency)?;
    let res = sqlx::query(
        "UPDATE comfy_node SET name=?, base_url=?, lan_base_url=?, username=COALESCE(?, username),
                password=COALESCE(?, password), max_concurrency=?, enabled=COALESCE(?, enabled),
                sort_order=COALESCE(?, sort_order), disabled_start=?, disabled_end=?,
                disabled_timezone=?, updated_at=?
         WHERE id = ?",
    )
    .bind(&b.name)
    .bind(validate::trim_url(&b.base_url))
    .bind(b.lan_base_url.as_deref().map(validate::trim_url))
    .bind(b.username.as_deref())
    .bind(b.password.as_deref())
    .bind(b.max_concurrency)
    .bind(b.enabled.map(|v| v as i64))
    .bind(b.sort_order)
    .bind(b.disabled_start.map(|v| v as i64))
    .bind(b.disabled_end.map(|v| v as i64))
    .bind(b.disabled_timezone.as_deref())
    .bind(now())
    .bind(id)
    .execute(&s.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found("端点不存在"));
    }
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_comfy_node(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    sqlx::query("DELETE FROM comfy_node WHERE id = ?")
        .bind(id)
        .execute(&s.pool)
        .await?;
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

// ── 工作流 ──────────────────────────────────────────────────────────────────

pub async fn list_workflows(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let rows = sqlx::query("SELECT * FROM workflow ORDER BY id")
        .fetch_all(&s.pool)
        .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let comfy: Value = serde_json::from_str(&r.get::<String, _>("comfy_workflow"))
                .unwrap_or(Value::Object(Default::default()));
            // 模型名是**推**出来的，前端要能看见推导结果——改 checkpoint
            // 等于改 model id，只藏在内核里的话没人会发现调用方会 404。
            let (model, model_err) = match crate::domain::workflow::derive_model(&comfy) {
                Ok(m) => (m, String::new()),
                Err(e) => (String::new(), e),
            };
            json!({
                "id": r.get::<i64, _>("id"),
                "name": r.get::<String, _>("name"),
                "mode": r.get::<String, _>("mode"),
                "model": model,
                "model_error": model_err,
                "node_ids": serde_json::from_str::<Vec<i64>>(&r.get::<String, _>("node_ids"))
                    .unwrap_or_default(),
                "price_micro": r.get::<i64, _>("price_micro"),
                "enabled": r.get::<i64, _>("enabled") != 0,
                // 参数面是全局固定的，列表里直接告诉前端能传什么
                "allowed_params": crate::domain::workflow::allowed_slots_json(),
                "sizes": crate::domain::workflow::size_presets_json(),
                "has_workflow_json": comfy != Value::Object(Default::default()),
            })
        })
        .collect();
    Ok(Json(json!({
        "items": items,
        "default_size": crate::domain::workflow::DEFAULT_SIZE,
    })))
}

#[derive(Deserialize)]
pub struct WorkflowBody {
    pub name: String,
    #[serde(default)]
    pub mode: Option<String>,
    pub node_ids: Option<Vec<i64>>,
    pub comfy_workflow: Option<String>,
    pub price_micro: Option<i64>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

pub async fn create_workflow(
    State(s): Shared,
    _a: AdminAuthed,
    Json(b): Json<WorkflowBody>,
) -> ApiResult<Response> {
    validate::validate_name(&b.name)?;
    if b.price_micro.unwrap_or(0) < 0 {
        return Err(ApiError::bad_request("单价不能为负"));
    }
    let id = sqlx::query(
        "INSERT INTO workflow (name, mode, node_ids, comfy_workflow,
                               price_micro, enabled, created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(&b.name)
    .bind(b.mode.as_deref().unwrap_or("template"))
    .bind(
        serde_json::to_string(&b.node_ids.clone().unwrap_or_default())
            .unwrap_or_else(|_| "[]".into()),
    )
    .bind(b.comfy_workflow.clone().unwrap_or_else(|| "{}".into()))
    .bind(b.price_micro.unwrap_or(0))
    .bind(b.enabled.unwrap_or(true) as i64)
    .bind(now())
    .bind(now())
    .execute(&s.pool)
    .await?
    .last_insert_rowid();
    s.reload().await?;
    Ok((created(), Json(json!({ "id": id }))).into_response())
}

pub async fn get_workflow(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let r = sqlx::query("SELECT * FROM workflow WHERE id = ?")
        .bind(id)
        .fetch_optional(&s.pool)
        .await?
        .ok_or_else(|| ApiError::not_found("工作流不存在"))?;
    Ok(Json(json!({
        "id": r.get::<i64, _>("id"),
        "name": r.get::<String, _>("name"),
        "mode": r.get::<String, _>("mode"),
        "node_ids": r.get::<String, _>("node_ids"),
        "comfy_workflow": r.get::<String, _>("comfy_workflow"),
        "price_micro": r.get::<i64, _>("price_micro"),
        "enabled": r.get::<i64, _>("enabled") != 0,
        "allowed_params": crate::domain::workflow::allowed_slots_json(),
    })))
}

pub async fn update_workflow(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
    Json(b): Json<WorkflowBody>,
) -> ApiResult<Json<Value>> {
    validate::validate_name(&b.name)?;
    if b.price_micro.unwrap_or(0) < 0 {
        return Err(ApiError::bad_request("单价不能为负"));
    }
    let res = sqlx::query(
        "UPDATE workflow SET name=?, mode=COALESCE(?, mode), node_ids=COALESCE(?, node_ids),
                comfy_workflow=COALESCE(?, comfy_workflow),
                price_micro=COALESCE(?, price_micro), enabled=COALESCE(?, enabled),
                updated_at=?
         WHERE id = ?",
    )
    .bind(&b.name)
    .bind(b.mode.as_deref())
    .bind(
        b.node_ids
            .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "[]".into())),
    )
    .bind(b.comfy_workflow.as_deref())
    .bind(b.price_micro)
    .bind(b.enabled.map(|v| v as i64))
    .bind(now())
    .bind(id)
    .execute(&s.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found("工作流不存在"));
    }
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_workflow(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    sqlx::query("DELETE FROM workflow WHERE id = ?")
        .bind(id)
        .execute(&s.pool)
        .await?;
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct DryRunBody {
    #[serde(default)]
    pub params: Value,
}

/// 模板试跑：只做填充，不提交。改工作流时先在这里验证参数。
pub async fn dry_run_workflow(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
    Json(b): Json<DryRunBody>,
) -> ApiResult<Json<Value>> {
    let r = sqlx::query("SELECT comfy_workflow FROM workflow WHERE id = ?")
        .bind(id)
        .fetch_optional(&s.pool)
        .await?
        .ok_or_else(|| ApiError::not_found("工作流不存在"))?;
    let tpl: Value = serde_json::from_str(&r.get::<String, _>("comfy_workflow"))
        .map_err(|e| ApiError::bad_request(format!("工作流 JSON 解析失败：{e}")))?;
    // 参数校验与 size 展开要和真实提交路径**走同一个函数**，否则试跑通过、
    // 实际调用却被拒，排障时会怀疑是别的地方坏了。
    let params = b.params.as_object().cloned().unwrap_or_default();
    let slots = match crate::upstream::workflow::prepare_slots(&tpl, &params) {
        Ok(s) => s,
        Err(e) => return Ok(Json(json!({ "ok": false, "error": e.0 }))),
    };

    match crate::upstream::workflow::render(&tpl, &slots) {
        Ok((v, used, unresolved)) => Ok(Json(json!({
            "ok": true,
            "used": used,
            // 调用方只能传固定那几个，所以模板里剩下的占位符永远填不上。
            // 不报出来的话，`TextEncode` 会把那串字面量当提示词用，图废了还不报错。
            "unresolved": unresolved,
            "workflow": v,
        }))),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e.0 }))),
    }
}

// ── 用量与明细 ──────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct UsageQuery {
    pub account_id: Option<i64>,
    pub api_key_id: Option<i64>,
    pub target: Option<String>,
    pub status: Option<String>,
    pub protocol: Option<String>,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

pub async fn list_logs(
    State(s): Shared,
    _a: AdminAuthed,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Value>> {
    let page = q.page.unwrap_or(1).max(1);
    let size = q.page_size.unwrap_or(50).clamp(1, 500);
    let (items, total) = usage::list(
        &s.pool,
        usage::LogFilter {
            account_id: q.account_id,
            api_key_id: q.api_key_id,
            target: q.target.as_deref(),
            status: q.status.as_deref(),
            protocol: q.protocol.as_deref(),
            from: q.from,
            to: q.to,
            limit: size,
            offset: (page - 1) * size,
        },
    )
    .await?;
    Ok(Json(
        json!({ "items": items, "total": total, "page": page, "page_size": size }),
    ))
}

#[derive(Deserialize)]
pub struct SummaryQuery {
    pub from_day: Option<String>,
    pub to_day: Option<String>,
    pub account_id: Option<i64>,
}

pub async fn usage_summary(
    State(s): Shared,
    _a: AdminAuthed,
    Query(q): Query<SummaryQuery>,
) -> ApiResult<Json<Value>> {
    let rows = usage::summary(
        &s.pool,
        q.from_day.as_deref(),
        q.to_day.as_deref(),
        q.account_id,
    )
    .await?;
    Ok(Json(json!({ "items": rows })))
}

/// 总览卡片：今天 / 本月 / 余额 / 在线节点。
pub async fn overview(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let month = chrono::Local::now().format("%Y-%m").to_string();

    let day: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(cost_micro),0), COALESCE(SUM(requests),0),
                COALESCE(SUM(errors),0), COALESCE(SUM(image_count),0)
         FROM usage_daily WHERE day = ?",
    )
    .bind(&today)
    .fetch_one(&s.pool)
    .await?;

    let mon: (i64, i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(cost_micro),0), COALESCE(SUM(requests),0), COALESCE(SUM(prompt_tokens+completion_tokens),0)
         FROM usage_daily WHERE day LIKE ? || '%'",
    )
    .bind(&month)
    .fetch_one(&s.pool)
    .await?;

    let totals: (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(balance_micro),0), COALESCE(SUM(held_micro),0) FROM account WHERE enabled = 1",
    )
    .fetch_one(&s.pool)
    .await?;

    let snap = s.snapshot();
    let health = s.health.snapshot().await;
    // 分母是节点总数：没有健康记录的节点就是健康的（还没失败过）。
    // 只数 health 里的条目会让「一个节点都没出问题」显示成 0 个健康。
    let healthy = snap
        .nodes
        .iter()
        .filter(|n| health.get(&n.id).map(|h| h.healthy).unwrap_or(true))
        .count();
    let cooling = health
        .values()
        .filter(|h| h.cooldown_secs_remaining > 0)
        .count();

    Ok(Json(json!({
        "today": { "day": today, "cost_micro": day.0, "requests": day.1, "errors": day.2, "images": day.3 },
        "month": { "month": month, "cost_micro": mon.0, "requests": mon.1, "tokens": mon.2 },
        "balance_micro": totals.0,
        "held_micro": totals.1,
        "nodes": { "total": snap.nodes.len(), "healthy": healthy, "cooling": cooling },
        "comfy_nodes": snap.comfy_nodes.len(),
        "models": snap.models().len(),
        "workflows": snap.workflow_names().len(),
    })))
}

/// 运行时面板：闸门实时占用。
pub async fn runtime(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let snap = s.snapshot();
    let health = s.health.snapshot().await;

    let mut nodes = Vec::new();
    for n in &snap.nodes {
        let st = s.node_gate(n.id).await.snapshot(n.max_concurrency).await;
        nodes.push(json!({
            "id": n.id, "name": n.name, "enabled": n.enabled,
            "max_concurrency": n.max_concurrency, "active": st.active, "waiting": st.waiting,
            "in_disabled_window": n.window.in_disabled_hours(),
            "usable_keys": n.keys.iter().filter(|k| k.is_usable_at(now())).count(),
            "total_keys": n.keys.len(),
            "healthy": health.get(&n.id).map(|h| h.healthy).unwrap_or(true),
            "cooldown_secs_remaining": health.get(&n.id).map(|h| h.cooldown_secs_remaining).unwrap_or(0),
            "last_error": health.get(&n.id).and_then(|h| h.last_error.clone()),
        }));
    }
    let mut comfy = Vec::new();
    for n in &snap.comfy_nodes {
        let st = s.comfy_gate(n.id).await.snapshot(n.max_concurrency).await;
        comfy.push(json!({
            "id": n.id, "name": n.name, "enabled": n.enabled,
            "max_concurrency": n.max_concurrency, "active": st.active, "waiting": st.waiting,
        }));
    }
    // 闸门是 **key 级**的，所以这里列的也是 key。账号层面没有闸门——
    // 建 3 把 key 就是 3 份并发，额度全在 key 上。
    let mut key_slots = Vec::new();
    for r in sqlx::query(
        "SELECT k.id, k.name, k.enabled, a.id AS account_id, a.name AS account_name
         FROM api_key k JOIN account a ON a.id = k.account_id
         WHERE a.enabled = 1 ORDER BY a.id, k.id",
    )
    .fetch_all(&s.pool)
    .await?
    {
        let kid = r.get::<i64, _>("id");
        let st = s.key_gate(kid).await.snapshot(i64::MAX).await;
        key_slots.push(json!({
            "id": kid,
            "name": r.get::<String, _>("name"),
            "account_id": r.get::<i64, _>("account_id"),
            "account_name": r.get::<String, _>("account_name"),
            "enabled": r.get::<i64, _>("enabled") != 0,
            "active": st.active, "waiting": st.waiting,
        }));
    }
    Ok(Json(
        json!({ "nodes": nodes, "comfy_nodes": comfy, "keys": key_slots }),
    ))
}

// ── 设置 ────────────────────────────────────────────────────────────────────

pub async fn get_settings(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let rows = sqlx::query("SELECT key, value FROM setting ORDER BY key")
        .fetch_all(&s.pool)
        .await?;
    let mut m = serde_json::Map::new();
    for r in rows {
        let k: String = r.get("key");
        let raw: String = r.get("value");
        m.insert(k, serde_json::from_str(&raw).unwrap_or(Value::Null));
    }
    Ok(Json(Value::Object(m)))
}

pub async fn put_settings(
    State(s): Shared,
    _a: AdminAuthed,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let obj = body
        .as_object()
        .ok_or_else(|| ApiError::bad_request("设置必须是对象"))?;
    let mut tx = s.pool.begin().await?;
    for (k, v) in obj {
        sqlx::query(
            "INSERT INTO setting (key, value) VALUES (?,?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(k)
        .bind(v.to_string())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    s.reload().await?;
    Ok(Json(json!({ "ok": true })))
}

// ── 备份 ────────────────────────────────────────────────────────────────────

pub async fn list_backups(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let rows = sqlx::query("SELECT * FROM backup_record ORDER BY id DESC LIMIT 50")
        .fetch_all(&s.pool)
        .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "path": r.get::<String, _>("path"),
                "size_bytes": r.get::<i64, _>("size_bytes"),
                "status": r.get::<String, _>("status"),
                "error": r.get::<Option<String>, _>("error"),
                "started_at": r.get::<i64, _>("started_at"),
                "finished_at": r.get::<Option<i64>, _>("finished_at"),
            })
        })
        .collect();
    Ok(Json(
        json!({ "items": items, "dir": s.cfg.backup_dir().display().to_string() }),
    ))
}

/// 立刻备份一次。手动触发与每日任务走**同一条路径**，
/// 免得「手动的好使、自动的不好使」这种诡异情况。
pub async fn backup_now(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let r = crate::tasks::backup::run_once(&s).await?;
    Ok(Json(json!({
        "ok": r.ok,
        "path": r.path,
        "size_bytes": r.size_bytes,
        "error": r.error,
    })))
}

pub async fn backup_retention(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let removed = crate::tasks::backup::purge(&s, 7).await?;
    Ok(Json(json!({ "removed": removed })))
}

// ── 内部工具 ────────────────────────────────────────────────────────────────

/// `extra_headers` / `extra_body` 在 schema 里是
/// `NOT NULL DEFAULT '{}'`，绑 NULL 会直接触发约束失败。
/// 读路径本来就容忍非法 JSON（回退空对象），所以这里给 "{}" 即可。
fn json_str(v: &Option<Value>) -> String {
    v.as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn session_pool() -> sqlx::SqlitePool {
        let dir = tempfile::tempdir().expect("建临时目录");
        let path = dir.path().join("a.db");
        let pool = crate::db::connect(&path).await.expect("建库");
        crate::db::migrate::MIGRATOR
            .run(&pool)
            .await
            .expect("跑迁移");
        std::mem::forget(dir);
        pool
    }

    /// 登出必须真的删掉**那把** token 对应的那一行。
    ///
    /// 这是本次修的 bug 的回归守卫：原来删的是 `token = ''`，
    /// 库里一行不少，而 Cookie 已经清了——凭证照样能用满 7 天。
    #[tokio::test]
    async fn logout_deletes_only_its_own_session() {
        let pool = session_pool().await;
        let now = now();
        for t in ["keep-me", "kill-me"] {
            sqlx::query("INSERT INTO session (token, expires_at, created_at) VALUES (?,?,?)")
                .bind(t)
                .bind(now + 3600)
                .bind(now)
                .execute(&pool)
                .await
                .expect("插入会话");
        }

        let n = delete_session(&pool, "kill-me").await.expect("删会话");
        assert_eq!(n, 1, "应当正好删掉一行");

        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session WHERE token = 'kill-me'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0, "被登出的那把不该还在库里");

        // 别连累别人：同一时刻可能有别的设备还登着
        let others: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM session WHERE token = 'keep-me'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(others, 1, "别的会话不该被一起删掉");
    }

    /// token 对不上时不能误删。这条曾经是反过来的：删了个空串，
    /// 看起来「成功」了，实际一行没动。
    #[tokio::test]
    async fn logout_with_an_unknown_token_deletes_nothing() {
        let pool = session_pool().await;
        sqlx::query("INSERT INTO session (token, expires_at, created_at) VALUES ('live',?,?)")
            .bind(now() + 3600)
            .bind(now())
            .execute(&pool)
            .await
            .expect("插入会话");
        let n = delete_session(&pool, "not-a-real-token")
            .await
            .expect("删会话");
        assert_eq!(n, 0);
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 1, "会话不该被动过");
    }

    /// 套了 TLS 之后会话 cookie 必须带 `Secure`。
    #[test]
    fn secure_flag_follows_the_forwarded_proto() {
        let mut h = axum::http::HeaderMap::new();
        assert!(
            !request_is_https(&h),
            "明文直连不能加 Secure，否则浏览器根本不发"
        );
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(request_is_https(&h));
        h.insert("x-forwarded-proto", "https, http".parse().unwrap());
        assert!(request_is_https(&h), "多跳时看第一跳");
        h.insert("x-forwarded-proto", "http".parse().unwrap());
        assert!(!request_is_https(&h));
    }

    /// 管理台的编辑弹窗不展示明文，提交上来的 body 里没有 `secret`。
    /// 这个 body 必须能反序列化，否则「改个标签」永远 422。
    #[test]
    fn key_body_accepts_an_edit_without_the_secret() {
        let b: KeyBody = serde_json::from_value(json!({
            "label": "备用",
            "sort_order": 1,
            "enabled": false
        }))
        .expect("编辑 key 的请求体不该要求 secret");
        assert_eq!(b.secret, None);
        assert_eq!(b.label.as_deref(), Some("备用"));
    }

    #[test]
    fn key_body_still_carries_the_secret_on_create() {
        let b: KeyBody = serde_json::from_value(json!({"secret": "sk-abcdefgh"}))
            .expect("建 key 必须能带上明文");
        assert_eq!(b.secret.as_deref(), Some("sk-abcdefgh"));
    }
}
