//! 管理面 API。
//!
//! 一次写入 = 一个事务 + 一次快照重建，**保存即生效，无需重启**。
//! 因此这里不缓存任何配置副本，也不做「读自己的缓存」这种会漂移的事。
//!
//! 鉴权走 [`AdminAuthed`]：口令换来的会话 token，放在 `HttpOnly` Cookie 里。
//! 网关面（API Key）与管理面是两套凭证，互不通用。

use axum::extract::{Path, Query, State};
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

pub async fn login(State(s): Shared, Json(b): Json<LoginBody>) -> ApiResult<Response> {
    let row = sqlx::query("SELECT * FROM admin_user WHERE username = ?")
        .bind(&b.username)
        .fetch_optional(&s.pool)
        .await?
        .ok_or_else(|| ApiError::unauthorized("用户名或口令不对"))?;

    let hash: String = row.get("password_hash");
    // 口令不对也要走完校验再返回，避免用响应时间探测用户名
    if !auth::verify_password(&b.password, &hash) {
        return Err(ApiError::unauthorized("用户名或口令不对"));
    }

    let token = auth::new_session_token();
    let expires = now() + 7 * 24 * 3600;
    sqlx::query("INSERT INTO session (token, expires_at, created_at) VALUES (?,?,?)")
        .bind(&token)
        .bind(expires)
        .bind(now())
        .execute(&s.pool)
        .await?;

    let cookie = format!(
        "hac_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
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

pub async fn logout(State(s): Shared, _a: AdminAuthed) -> ApiResult<Response> {
    if let Some(t) = current_token(&s).await {
        sqlx::query("DELETE FROM session WHERE token = ?")
            .bind(t)
            .execute(&s.pool)
            .await?;
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

async fn current_token(s: &AppState) -> Option<String> {
    // 会话 token 已在守卫里验过，这里只需要拿到它做删除
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session WHERE expires_at > ?")
        .bind(now())
        .fetch_one(&s.pool)
        .await
        .unwrap_or(0);
    (n > 0).then(|| String::new())
}

// ── 账号 ────────────────────────────────────────────────────────────────────

pub async fn list_accounts(State(s): Shared, _a: AdminAuthed) -> ApiResult<Json<Value>> {
    let rows = sqlx::query("SELECT * FROM account ORDER BY id").fetch_all(&s.pool).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "name": r.get::<String, _>("name"),
                "api_key_prefix": r.get::<String, _>("api_key_prefix"),
                "enabled": r.get::<i64, _>("enabled") != 0,
                "balance_micro": r.get::<i64, _>("balance_micro"),
                "held_micro": r.get::<i64, _>("held_micro"),
                "available_micro": r.get::<i64, _>("balance_micro") - r.get::<i64, _>("held_micro"),
                "llm_max_concurrency": r.get::<i64, _>("llm_max_concurrency"),
                "llm_max_queue": r.get::<i64, _>("llm_max_queue"),
                "image_max_concurrency": r.get::<i64, _>("image_max_concurrency"),
                "image_max_queue": r.get::<i64, _>("image_max_queue"),
                "rpm_limit": r.get::<Option<i64>, _>("rpm_limit"),
                "created_at": r.get::<i64, _>("created_at"),
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct AccountBody {
    pub name: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    pub balance_micro: Option<i64>,
    pub llm_max_concurrency: Option<i64>,
    pub llm_max_queue: Option<i64>,
    pub image_max_concurrency: Option<i64>,
    pub image_max_queue: Option<i64>,
    pub rpm_limit: Option<Option<i64>>,
}

/// 建账号。明文 key **只在这一次响应里出现**，库里只存 SHA-256。
pub async fn create_account(
    State(s): Shared,
    _a: AdminAuthed,
    Json(b): Json<AccountBody>,
) -> ApiResult<Response> {
    validate::validate_name(&b.name)?;
    let raw = auth::generate_api_key();
    let now = now();
    let id = sqlx::query(
        "INSERT INTO account (name, api_key_hash, api_key_prefix, enabled, balance_micro,
                               llm_max_concurrency, llm_max_queue,
                               image_max_concurrency, image_max_queue, created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&b.name)
    .bind(auth::hash_key(&raw))
    .bind(auth::key_prefix(&raw))
    .bind(b.enabled.unwrap_or(true) as i64)
    .bind(b.balance_micro.unwrap_or(0))
    .bind(b.llm_max_concurrency.unwrap_or(2).clamp(1, 1024))
    .bind(b.llm_max_queue.unwrap_or(8).max(0))
    .bind(b.image_max_concurrency.unwrap_or(1).clamp(1, 1024))
    .bind(b.image_max_queue.unwrap_or(4).max(0))
    .bind(now)
    .bind(now)
    .execute(&s.pool)
    .await
    .map_err(|e| {
        if matches!(&e, sqlx::Error::Database(d) if d.is_unique_violation()) {
            ApiError::bad_request("账号名已存在")
        } else {
            e.into()
        }
    })?
    .last_insert_rowid();

    Ok((
        created(),
        Json(json!({ "id": id, "api_key": raw })),
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
    let now = now();
    let res = sqlx::query(
        "UPDATE account SET name = ?, enabled = COALESCE(?, enabled),
                llm_max_concurrency = COALESCE(?, llm_max_concurrency),
                llm_max_queue = COALESCE(?, llm_max_queue),
                image_max_concurrency = COALESCE(?, image_max_concurrency),
                image_max_queue = COALESCE(?, image_max_queue),
                rpm_limit = COALESCE(?, rpm_limit), updated_at = ?
         WHERE id = ?",
    )
    .bind(&b.name)
    .bind(b.enabled.map(|v| v as i64))
    .bind(b.llm_max_concurrency.map(|v| v.clamp(1, 1024)))
    .bind(b.llm_max_queue.map(|v| v.max(0)))
    .bind(b.image_max_concurrency.map(|v| v.clamp(1, 1024)))
    .bind(b.image_max_queue.map(|v| v.max(0)))
    .bind(b.rpm_limit.flatten())
    .bind(now)
    .bind(id)
    .execute(&s.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found("账号不存在"));
    }
    // 并发额度变了要立刻唤醒排队者
    if let Some(a) = load_account(&s, id).await? {
        let g = s.account_gate(id).await;
        g.nudge(a.llm_max_concurrency).await;
    }
    s.reload().await.ok();
    Ok(Json(json!({ "ok": true })))
}

async fn load_account(s: &AppState, id: i64) -> ApiResult<Option<crate::domain::account::Account>> {
    let r = sqlx::query("SELECT * FROM account WHERE id = ?")
        .bind(id)
        .fetch_optional(&s.pool)
        .await?;
    Ok(r.map(|r| crate::domain::account::Account {
        id: r.get("id"),
        name: r.get("name"),
        enabled: r.get::<i64, _>("enabled") != 0,
        balance_micro: r.get("balance_micro"),
        held_micro: r.get("held_micro"),
        llm_max_concurrency: r.get("llm_max_concurrency"),
        llm_max_queue: r.get("llm_max_queue"),
        image_max_concurrency: r.get("image_max_concurrency"),
        image_max_queue: r.get("image_max_queue"),
        rpm_limit: r.get("rpm_limit"),
    }))
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

#[derive(Deserialize)]
pub struct RotateKeyBody {
    #[serde(default)]
    pub name: Option<String>,
}

/// 换 key：旧 key 立刻失效。调用方需要同步更新客户端配置。
pub async fn rotate_account_key(
    State(s): Shared,
    _a: AdminAuthed,
    Path(id): Path<i64>,
    Json(b): Json<RotateKeyBody>,
) -> ApiResult<Json<Value>> {
    let raw = auth::generate_api_key();
    let prefix = if let Some(n) = b.name.as_deref().filter(|n| !n.is_empty()) {
        format!("{n}:{raw}")
    } else {
        raw
    };
    let res = sqlx::query(
        "UPDATE account SET api_key_hash = ?, api_key_prefix = ?, updated_at = ? WHERE id = ?",
    )
    .bind(auth::hash_key(&prefix))
    .bind(auth::key_prefix(&prefix))
    .bind(now())
    .bind(id)
    .execute(&s.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found("账号不存在"));
    }
    Ok(Json(json!({ "api_key": prefix })))
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
    let rows = sqlx::query(
        "SELECT * FROM balance_txn WHERE account_id = ? ORDER BY id DESC LIMIT 200",
    )
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
    /// 预留位：当前没有节点原生支持 `/v1/responses`，
    /// 但把它记下来，将来接的时候不用改 schema。
    pub capabilities: Option<Value>,
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
                               extra_headers, extra_body, capabilities, created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
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
    .bind(json_str(&b.capabilities))
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
        format!("{}{}", &secret[..2], "*".repeat(secret.len().saturating_sub(6)))
    }
}

#[derive(Deserialize)]
pub struct KeyBody {
    pub secret: String,
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
    let secret = b.secret.trim();
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
    let res = sqlx::query(
        "UPDATE llm_node_key SET label=COALESCE(?, label), sort_order=COALESCE(?, sort_order),
                enabled=COALESCE(?, enabled), rate_limit_scope=COALESCE(?, rate_limit_scope),
                soft_cap_window_ms=?, soft_cap_tokens=?, updated_at=?
         WHERE id = ?",
    )
    .bind(b.label.as_deref())
    .bind(b.sort_order)
    .bind(b.enabled.map(|v| v as i64))
    .bind(b.rate_limit_scope.as_deref().map(RateLimitScope::parse).map(RateLimitScope::as_str))
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
    let rows = sqlx::query(
        "SELECT * FROM key_rotation_event WHERE key_id = ? ORDER BY id DESC LIMIT 50",
    )
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
    Ok(Json(json!({ "items": items, "models": s.snapshot().models() })))
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
            json!({
                "id": r.get::<i64, _>("id"),
                "name": r.get::<String, _>("name"),
                "mode": r.get::<String, _>("mode"),
                "node_ids": serde_json::from_str::<Vec<i64>>(&r.get::<String, _>("node_ids"))
                    .unwrap_or_default(),
                "param_slots": serde_json::from_str::<Vec<String>>(&r.get::<String, _>("param_slots"))
                    .unwrap_or_default(),
                "price_micro": r.get::<i64, _>("price_micro"),
                "enabled": r.get::<i64, _>("enabled") != 0,
                "archive": r.get::<i64, _>("archive") != 0,
                "archive_retention_days": r.get::<Option<i64>, _>("archive_retention_days"),
                "has_workflow_json": r
                    .get::<String, _>("comfy_workflow")
                    .chars()
                    .count()
                    > 2,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct WorkflowBody {
    pub name: String,
    #[serde(default)]
    pub mode: Option<String>,
    pub node_ids: Option<Vec<i64>>,
    pub comfy_workflow: Option<String>,
    pub param_slots: Option<Vec<String>>,
    pub price_micro: Option<i64>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub archive: Option<bool>,
    pub archive_retention_days: Option<i64>,
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
        "INSERT INTO workflow (name, mode, node_ids, comfy_workflow, param_slots,
                               price_micro, enabled, archive, archive_retention_days,
                               created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&b.name)
    .bind(b.mode.as_deref().unwrap_or("template"))
    .bind(serde_json::to_string(&b.node_ids.clone().unwrap_or_default()).unwrap_or_else(|_| "[]".into()))
    .bind(b.comfy_workflow.clone().unwrap_or_else(|| "{}".into()))
    .bind(serde_json::to_string(&b.param_slots.clone().unwrap_or_default()).unwrap_or_else(|_| "[]".into()))
    .bind(b.price_micro.unwrap_or(0))
    .bind(b.enabled.unwrap_or(true) as i64)
    .bind(b.archive.unwrap_or(false) as i64)
    .bind(b.archive_retention_days)
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
        "param_slots": r.get::<String, _>("param_slots"),
        "price_micro": r.get::<i64, _>("price_micro"),
        "enabled": r.get::<i64, _>("enabled") != 0,
        "archive": r.get::<i64, _>("archive") != 0,
        "archive_retention_days": r.get::<Option<i64>, _>("archive_retention_days"),
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
                comfy_workflow=COALESCE(?, comfy_workflow), param_slots=COALESCE(?, param_slots),
                price_micro=COALESCE(?, price_micro), enabled=COALESCE(?, enabled),
                archive=COALESCE(?, archive),
                archive_retention_days=COALESCE(?, archive_retention_days), updated_at=?
         WHERE id = ?",
    )
    .bind(&b.name)
    .bind(b.mode.as_deref())
    .bind(b.node_ids.map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "[]".into())))
    .bind(b.comfy_workflow.as_deref())
    .bind(b.param_slots.map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "[]".into())))
    .bind(b.price_micro)
    .bind(b.enabled.map(|v| v as i64))
    .bind(b.archive.map(|v| v as i64))
    .bind(b.archive_retention_days)
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
    let r = sqlx::query("SELECT comfy_workflow, param_slots FROM workflow WHERE id = ?")
        .bind(id)
        .fetch_optional(&s.pool)
        .await?
        .ok_or_else(|| ApiError::not_found("工作流不存在"))?;
    let tpl: Value = serde_json::from_str(&r.get::<String, _>("comfy_workflow"))
        .map_err(|e| ApiError::bad_request(format!("工作流 JSON 解析失败：{e}")))?;
    // 槽位过滤要和真实提交路径**用同一套规则**，否则试跑通过、
    // 实际调用却被拒，排障时会怀疑是别的地方坏了。
    let params = b.params.as_object().cloned().unwrap_or_default();
    let declared: std::collections::HashSet<String> =
        serde_json::from_str::<Vec<String>>(&r.get::<String, _>("param_slots"))
            .unwrap_or_default()
            .into_iter()
            .collect();
    let unknown: Vec<&str> = params
        .keys()
        .map(String::as_str)
        .filter(|k| !declared.contains(*k))
        .collect();
    if !unknown.is_empty() {
        return Ok(Json(json!({
            "ok": false,
            "error": format!("工作流没有这些参数槽：{}", unknown.join(", ")),
        })));
    }

    match crate::upstream::workflow::render(&tpl, &params) {
        Ok((v, used)) => Ok(Json(json!({ "ok": true, "used": used, "workflow": v }))),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e.0 }))),
    }
}

// ── 用量与明细 ──────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct UsageQuery {
    pub account_id: Option<i64>,
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
        q.account_id,
        q.target.as_deref(),
        q.status.as_deref(),
        q.protocol.as_deref(),
        q.from,
        q.to,
        size,
        (page - 1) * size,
    )
    .await?;
    Ok(Json(json!({ "items": items, "total": total, "page": page, "page_size": size })))
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
    let cooling = health.values().filter(|h| h.cooldown_secs_remaining > 0).count();

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
    let mut accounts = Vec::new();
    for r in sqlx::query("SELECT id, name, balance_micro, held_micro FROM account WHERE enabled = 1 ORDER BY id")
        .fetch_all(&s.pool)
        .await?
    {
        let id = r.get::<i64, _>("id");
        let st = s.account_gate(id).await.snapshot(i64::MAX).await;
        accounts.push(json!({
            "id": id, "name": r.get::<String, _>("name"),
            "balance_micro": r.get::<i64, _>("balance_micro"),
            "held_micro": r.get::<i64, _>("held_micro"),
            "active": st.active, "waiting": st.waiting,
        }));
    }
    Ok(Json(json!({ "nodes": nodes, "comfy_nodes": comfy, "accounts": accounts })))
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
    Ok(Json(json!({ "items": items, "dir": s.cfg.backup_dir().display().to_string() })))
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

/// `extra_headers` / `extra_body` / `capabilities` 在 schema 里是
/// `NOT NULL DEFAULT '{}'`，绑 NULL 会直接触发约束失败。
/// 读路径本来就容忍非法 JSON（回退空对象），所以这里给 "{}" 即可。
fn json_str(v: &Option<Value>) -> String {
    v.as_ref().map(|v| v.to_string()).unwrap_or_else(|| "{}".into())
}
