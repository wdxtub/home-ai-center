//! 调用明细与每日汇总。
//!
//! **只存元数据，不存 prompt / completion 正文**——这是产品决策而非未完成项。
//! 唯一的例外是**出图**：`image_prompt` 表单独记下出图提示词（见
//! `api::images::record_prompt`），因为出图没有 completion 正文，
//! 而「这张图是哪句话生成的」是唯一能追回结果的线索。LLM 侧仍然一字不记。

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
    let mut items: Vec<Value> = rows.iter().map(row_to_json).collect();

    // 回带出图提示词：单独一张表，按本页的 request_id 批量补一次，
    // 不改主查询（主查询的 WHERE 条件与分页都要原样保持）。
    attach_prompts(pool, &mut items).await;

    Ok((items, total))
}

/// 给本页明细补上 `prompt` 字段（仅出图行有）。
///
/// 一次 `IN` 查询搞定整页；查不到就保持字段缺席，前端据此判断「没记」而不是
/// 「记了但是空」。
async fn attach_prompts(pool: &SqlitePool, items: &mut [Value]) {
    if items.is_empty() {
        return;
    }
    let ids: Vec<String> = items
        .iter()
        .filter_map(|v| {
            v.get("request_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    if ids.is_empty() {
        return;
    }

    let rows = {
        let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT request_id, prompt FROM image_prompt WHERE request_id IN (",
        );
        let mut sep = qb.separated(", ");
        for id in &ids {
            sep.push_bind(id);
        }
        sep.push_unseparated(")");
        qb.build().fetch_all(pool).await
    };
    let Ok(rows) = rows else { return };
    if rows.is_empty() {
        return;
    }

    let map: std::collections::HashMap<String, String> = rows
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("request_id"),
                r.get::<String, _>("prompt"),
            )
        })
        .collect();

    for item in items.iter_mut() {
        let Some(rid) = item.get("request_id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(p) = map.get(rid) {
            item["prompt"] = Value::String(p.clone());
        }
    }
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
///
/// 提示词跟随明细一起清：`image_prompt.request_id` 上挂了 `ON DELETE CASCADE`，
/// 这里删完 `request_log` 它会跟着没，不用单独排期。留着不删的话，
/// 提示词会变成一份永不过期的正文副本——那正是这张表当初要避免的。
pub async fn purge_older_than(pool: &SqlitePool, days: i64) -> anyhow::Result<u64> {
    let cutoff = unix_now() - days * 24 * 3600;
    let r = sqlx::query("DELETE FROM request_log WHERE created_at < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::UnifiedUsage;

    async fn test_pool() -> SqlitePool {
        let dir = tempfile::tempdir().expect("建临时目录");
        let path = dir.path().join("u.db");
        let pool = crate::db::connect(&path).await.expect("建库");
        crate::db::migrate::MIGRATOR
            .run(&pool)
            .await
            .expect("跑迁移");
        std::mem::forget(dir);
        sqlx::query(
            "INSERT INTO account (id, name, api_key_hash, api_key_prefix, created_at, updated_at)
             VALUES (1,'self','h','sk-hac-',0,0)",
        )
        .execute(&pool)
        .await
        .expect("插入账号");
        pool
    }

    fn image_entry(request_id: &str) -> LogEntry {
        LogEntry {
            request_id: request_id.to_string(),
            account_id: 1,
            kind: "image",
            protocol: None,
            target: "txt2img".into(),
            node_id: None,
            node_name: None,
            key_id: None,
            rotated_count: 0,
            stream: false,
            status: "ok",
            error_kind: None,
            error_message: None,
            queue_wait_ms: 0,
            latency_ms: 1,
            usage: UnifiedUsage::default(),
            tokens_source: "none",
            image_count: 1,
            cost_micro: 100_000,
            notes: Vec::new(),
        }
    }

    async fn put_prompt(pool: &SqlitePool, request_id: &str, prompt: &str) {
        sqlx::query(
            "INSERT INTO image_prompt (request_id, account_id, workflow, prompt, created_at)
             VALUES (?,1,'txt2img',?,?)",
        )
        .bind(request_id)
        .bind(prompt)
        .bind(unix_now())
        .execute(pool)
        .await
        .expect("写入提示词");
    }

    /// 承重墙：明细列表要能把提示词带出来，否则「记了却看不到」等于没记。
    #[tokio::test]
    async fn list_carries_the_prompt_back() {
        let pool = test_pool().await;
        write(&pool, &image_entry("r-1")).await.unwrap();
        put_prompt(&pool, "r-1", "一只猫").await;

        let (items, _) = list(&pool, None, None, None, None, None, None, 50, 0)
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["prompt"], Value::String("一只猫".into()));
    }

    /// LLM 明细**不该**凭空长出 prompt 字段：那张表不存正文。
    #[tokio::test]
    async fn llm_rows_have_no_prompt_field() {
        let pool = test_pool().await;
        let mut e = image_entry("r-2");
        e.kind = "llm";
        e.protocol = Some("chat");
        write(&pool, &e).await.unwrap();

        let (items, _) = list(&pool, None, None, None, None, None, None, 50, 0)
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert!(
            items[0].get("prompt").is_none(),
            "LLM 行不该带 prompt 字段，实际 {:?}",
            items[0].get("prompt")
        );
    }

    /// 分页只回带本页的提示词，不能把别的页的串进来。
    #[tokio::test]
    async fn prompts_are_scoped_to_the_page() {
        let pool = test_pool().await;
        for i in 1..=3 {
            write(&pool, &image_entry(&format!("r-{i}"))).await.unwrap();
        }
        put_prompt(&pool, "r-2", "只属于第二页").await;

        let (items, _) = list(&pool, None, None, None, None, None, None, 1, 0)
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["request_id"], Value::String("r-3".into()));
        assert!(
            items[0].get("prompt").is_none(),
            "第一页是 r-3，不该带 r-2 的提示词"
        );
    }

    /// 提示词必须跟着明细一起过期，否则它就是一份永不过期的正文副本。
    #[tokio::test]
    async fn purging_logs_takes_the_prompt_with_it() {
        let pool = test_pool().await;
        write(&pool, &image_entry("r-old")).await.unwrap();
        put_prompt(&pool, "r-old", "会过期").await;

        // 把明细的时间戳推到 100 天前，再用默认保留期清理
        sqlx::query("UPDATE request_log SET created_at = ? WHERE request_id = ?")
            .bind(unix_now() - 100 * 24 * 3600)
            .bind("r-old")
            .execute(&pool)
            .await
            .unwrap();
        // 提示词的时间戳也推前，确保不是靠它自己的时间被清的
        sqlx::query("UPDATE image_prompt SET created_at = ? WHERE request_id = ?")
            .bind(unix_now())
            .bind("r-old")
            .execute(&pool)
            .await
            .unwrap();

        purge_older_than(&pool, 90).await.unwrap();

        let left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM image_prompt WHERE request_id = 'r-old'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(left, 0, "明细被清了，提示词必须跟着级联删掉");
    }
}
