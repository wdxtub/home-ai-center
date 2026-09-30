//! 调用明细与每日汇总。
//!
//! **只存元数据，不存 prompt / completion 正文**——这是产品决策而非未完成项。

use serde_json::Value;
use sqlx::{Row, SqlitePool};

use crate::protocol::ir::UnifiedUsage;

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 记账写入。结算与日志落在**同一个事务**里，保证两者一致。
pub struct LogEntry {
    pub request_id: String,
    pub account_id: i64,
    pub kind: &'static str,
    pub protocol: Option<&'static str>,
    pub target: String,
    pub node_id: Option<i64>,
    pub node_name: Option<String>,
    pub key_id: Option<i64>,
    pub rotated_count: i64,
    pub stream: bool,
    pub status: &'static str,
    pub error_kind: Option<String>,
    pub error_message: Option<String>,
    pub queue_wait_ms: i64,
    pub latency_ms: i64,
    pub usage: UnifiedUsage,
    pub tokens_source: &'static str,
    pub image_count: i64,
    pub cost_micro: i64,
    /// 附加的降级/丢弃标记（`stop_truncated`、`thinking_dropped`…）。
    pub notes: Vec<String>,
}

pub async fn write(pool: &SqlitePool, e: &LogEntry) -> anyhow::Result<()> {
    let now = unix_now();
    let err = e
        .error_message
        .as_deref()
        .map(|m| m.chars().take(200).collect::<String>());
    let mut note = e.notes.join(",");

    // 节点/模型维度用于日汇总
    let node_id = e.node_id.unwrap_or(0);
    let day = chrono::Local::now().format("%Y-%m-%d").to_string();

    sqlx::query(
        "INSERT OR REPLACE INTO request_log
           (request_id, account_id, kind, protocol, target, node_id, node_name, key_id,
            rotated_count, stream, status, error_kind, error_message, queue_wait_ms,
            latency_ms, prompt_tokens, completion_tokens, cached_tokens, reasoning_tokens,
            tokens_source, image_count, cost_micro, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&e.request_id)
    .bind(e.account_id)
    .bind(e.kind)
    .bind(e.protocol)
    .bind(&e.target)
    .bind(node_id)
    .bind(&e.node_name)
    .bind(e.key_id)
    .bind(e.rotated_count)
    .bind(e.stream as i64)
    .bind(e.status)
    .bind(&e.error_kind)
    .bind(&err)
    .bind(e.queue_wait_ms)
    .bind(e.latency_ms)
    .bind(e.usage.input_tokens as i64)
    .bind(e.usage.output_tokens as i64)
    .bind(e.usage.cached_tokens as i64)
    .bind(e.usage.reasoning_tokens as i64)
    .bind(e.tokens_source)
    .bind(e.image_count)
    .bind(e.cost_micro)
    .bind(now)
    .execute(pool)
    .await?;

    // 日汇总：与明细同一次写入，避免对不上
    let err_delta = if e.status == "error" { 1 } else { 0 };
    sqlx::query(
        "INSERT INTO usage_daily
           (day, account_id, target, node_id, requests, errors, prompt_tokens,
            completion_tokens, image_count, cost_micro)
         VALUES (?,?,?,?,1,?,?,?,?,?)
         ON CONFLICT(day, account_id, target, node_id) DO UPDATE SET
           requests = requests + 1,
           errors = errors + ?,
           prompt_tokens = prompt_tokens + ?,
           completion_tokens = completion_tokens + ?,
           image_count = image_count + ?,
           cost_micro = cost_micro + ?",
    )
    .bind(&day)
    .bind(e.account_id)
    .bind(&e.target)
    .bind(node_id)
    .bind(err_delta)
    .bind(e.usage.input_tokens as i64)
    .bind(e.usage.output_tokens as i64)
    .bind(e.image_count)
    .bind(e.cost_micro)
    .bind(err_delta)
    .bind(e.usage.input_tokens as i64)
    .bind(e.usage.output_tokens as i64)
    .bind(e.image_count)
    .bind(e.cost_micro)
    .execute(pool)
    .await?;

    if let Some(ek) = &e.error_kind {
        if note.is_empty() {
            note = ek.clone();
        }
    }
    let _ = note;
    Ok(())
}

/// 明细列表（管理页面用，支持时间 / 账号 / 模型 / 状态 / 协议筛选）。
pub async fn list(
    pool: &SqlitePool,
    account_id: Option<i64>,
    target: Option<&str>,
    status: Option<&str>,
    protocol: Option<&str>,
    from: Option<i64>,
    to: Option<i64>,
    limit: i64,
    offset: i64,
) -> anyhow::Result<(Vec<Value>, i64)> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT * FROM request_log WHERE 1=1");
    if let Some(a) = account_id {
        qb.push(" AND account_id = ").push_bind(a);
    }
    if let Some(t) = target {
        qb.push(" AND target = ").push_bind(t.to_string());
    }
    if let Some(st) = status {
        qb.push(" AND status = ").push_bind(st.to_string());
    }
    if let Some(p) = protocol {
        qb.push(" AND protocol = ").push_bind(p.to_string());
    }
    if let Some(f) = from {
        qb.push(" AND created_at >= ").push_bind(f);
    }
    if let Some(t) = to {
        qb.push(" AND created_at <= ").push_bind(t);
    }
    let count_sql = format!("{} ", qb.sql());
    let total: i64 = sqlx::query_scalar(&count_sql)
        .fetch_optional(pool)
        .await?
        .unwrap_or(0);

    qb.push(" ORDER BY id DESC LIMIT ").push_bind(limit);
    qb.push(" OFFSET ").push_bind(offset);
    let rows = qb.build().fetch_all(pool).await?;
    let items = rows.iter().map(row_to_json).collect();
    Ok((items, total))
}

fn row_to_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    serde_json::json!({
        "id": r.get::<i64, _>("id"),
        "request_id": r.get::<String, _>("request_id"),
        "account_id": r.get::<i64, _>("account_id"),
        "kind": r.get::<String, _>("kind"),
        "protocol": r.get::<Option<String>, _>("protocol"),
        "target": r.get::<String, _>("target"),
        "node_name": r.get::<Option<String>, _>("node_name"),
        "key_id": r.get::<Option<i64>, _>("key_id"),
        "rotated_count": r.get::<i64, _>("rotated_count"),
        "stream": r.get::<i64, _>("stream") != 0,
        "status": r.get::<String, _>("status"),
        "error_kind": r.get::<Option<String>, _>("error_kind"),
        "error_message": r.get::<Option<String>, _>("error_message"),
        "queue_wait_ms": r.get::<i64, _>("queue_wait_ms"),
        "latency_ms": r.get::<i64, _>("latency_ms"),
        "prompt_tokens": r.get::<i64, _>("prompt_tokens"),
        "completion_tokens": r.get::<i64, _>("completion_tokens"),
        "cached_tokens": r.get::<i64, _>("cached_tokens"),
        "reasoning_tokens": r.get::<i64, _>("reasoning_tokens"),
        "tokens_source": r.get::<Option<String>, _>("tokens_source"),
        "image_count": r.get::<i64, _>("image_count"),
        "cost_micro": r.get::<i64, _>("cost_micro"),
        "created_at": r.get::<i64, _>("created_at"),
    })
}

/// 统计汇总：按天 / 账号 / 模型 / 节点。
pub async fn summary(
    pool: &SqlitePool,
    from_day: Option<&str>,
    to_day: Option<&str>,
    account_id: Option<i64>,
) -> anyhow::Result<Vec<Value>> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT day, account_id, target, node_id,
                SUM(requests) AS requests, SUM(errors) AS errors,
                SUM(prompt_tokens) AS prompt_tokens, SUM(completion_tokens) AS completion_tokens,
                SUM(image_count) AS image_count, SUM(cost_micro) AS cost_micro
         FROM usage_daily WHERE 1=1",
    );
    if let Some(f) = from_day {
        qb.push(" AND day >= ").push_bind(f.to_string());
    }
    if let Some(t) = to_day {
        qb.push(" AND day <= ").push_bind(t.to_string());
    }
    if let Some(a) = account_id {
        qb.push(" AND account_id = ").push_bind(a);
    }
    qb.push(" GROUP BY day, account_id, target, node_id ORDER BY day DESC");

    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "day": r.get::<String, _>("day"),
                "account_id": r.get::<i64, _>("account_id"),
                "target": r.get::<String, _>("target"),
                "node_id": r.get::<i64, _>("node_id"),
                "requests": r.get::<i64, _>("requests"),
                "errors": r.get::<i64, _>("errors"),
                "prompt_tokens": r.get::<i64, _>("prompt_tokens"),
                "completion_tokens": r.get::<i64, _>("completion_tokens"),
                "image_count": r.get::<i64, _>("image_count"),
                "cost_micro": r.get::<i64, _>("cost_micro"),
            })
        })
        .collect())
}

/// 清理超期的调用明细。`usage_daily` 永久保留。
pub async fn purge_older_than(pool: &SqlitePool, days: i64) -> anyhow::Result<u64> {
    let cutoff = unix_now() - days * 24 * 3600;
    let r = sqlx::query("DELETE FROM request_log WHERE created_at < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}
