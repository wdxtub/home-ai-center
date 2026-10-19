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

/// 从一条明细推出**纯生成速度**（token/秒）。
///
/// 口径：`completion_tokens / 生成耗时`，生成耗时 =
/// `latency_ms - queue_wait_ms - ttft_ms`。
///
/// **不要用 `completion_tokens / latency_ms`**：那个分母把 prefill
/// （上游读 prompt 的时间）算进去了。长 prompt 的请求会被算得明显更慢，
/// 拿它比较两个上游，实际比的是谁家的 prefill 更长，结论可能是反的。
///
/// 返回 `None` 的三种情况，都必须是「没有数据」而不是 0：
/// - 没有 completion token（空补全、被截断的流）；
/// - 不是流式请求，`ttft_ms` 是 `None`；
/// - 扣完 TTFT 后分母 ≤ 0（极短回答，或两个耗时字段对不上）。
///   报 0 会让平均速度被拉到 0，报成除零会得到天文数字——两个都是编的数。
pub fn decode_tokens_per_sec(
    ttft_ms: Option<i64>,
    queue_wait_ms: i64,
    latency_ms: i64,
    completion_tokens: i64,
) -> Option<f64> {
    let ttft = ttft_ms?;
    if completion_tokens <= 0 {
        return None;
    }
    let decode_ms = latency_ms - queue_wait_ms - ttft;
    if decode_ms <= 0 {
        return None;
    }
    Some(completion_tokens as f64 * 1000.0 / decode_ms as f64)
}

/// 记账写入。结算与日志落在**同一个事务**里，保证两者一致。
pub struct LogEntry {
    pub request_id: String,
    pub account_id: i64,
    /// 调用方那把 key。「每把 key 的 rpm/tpm/张数」就靠它切出来。
    pub api_key_id: i64,
    pub kind: &'static str,
    pub protocol: Option<&'static str>,
    pub target: String,
    pub node_id: Option<i64>,
    pub node_name: Option<String>,
    /// **上游**节点 key（llm_node_key），别和上面的 api_key_id 搞混。
    pub node_key_id: Option<i64>,
    pub rotated_count: i64,
    pub stream: bool,
    pub status: &'static str,
    pub error_kind: Option<String>,
    pub error_message: Option<String>,
    pub queue_wait_ms: i64,
    pub latency_ms: i64,
    /// 上游首 token 等待毫秒，**已扣掉 `queue_wait_ms`**。
    /// 只有流式请求才有；其余情况留 `None`（见 `0007_stream_timing.sql`）。
    pub ttft_ms: Option<i64>,
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
           (request_id, account_id, api_key_id, kind, protocol, target, node_id, node_name,
            node_key_id, rotated_count, stream, status, error_kind, error_message,
            queue_wait_ms, latency_ms, ttft_ms, prompt_tokens, completion_tokens, cached_tokens,
            reasoning_tokens, tokens_source, image_count, cost_micro, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&e.request_id)
    .bind(e.account_id)
    .bind(e.api_key_id)
    .bind(e.kind)
    .bind(e.protocol)
    .bind(&e.target)
    .bind(node_id)
    .bind(&e.node_name)
    .bind(e.node_key_id)
    .bind(e.rotated_count)
    .bind(e.stream as i64)
    .bind(e.status)
    .bind(&e.error_kind)
    .bind(&err)
    .bind(e.queue_wait_ms)
    .bind(e.latency_ms)
    .bind(e.ttft_ms)
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
           (day, account_id, api_key_id, target, node_id, requests, errors, prompt_tokens,
            completion_tokens, image_count, cost_micro)
         VALUES (?,?,?,?,?,1,?,?,?,?,?)
         ON CONFLICT(day, account_id, api_key_id, target, node_id) DO UPDATE SET
           requests = requests + 1,
           errors = errors + ?,
           prompt_tokens = prompt_tokens + ?,
           completion_tokens = completion_tokens + ?,
           image_count = image_count + ?,
           cost_micro = cost_micro + ?",
    )
    .bind(&day)
    .bind(e.account_id)
    .bind(e.api_key_id)
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
/// 明细筛选条件。
///
/// 收成结构体而不是继续加位置参数：筛选条件已经有 8 个，再加一个就把
/// 调用点变成「一串 `None` 里夹着两个数」，**加错位编译器不报错**。
#[derive(Debug, Default, Clone, Copy)]
pub struct LogFilter<'a> {
    pub account_id: Option<i64>,
    pub api_key_id: Option<i64>,
    pub target: Option<&'a str>,
    pub status: Option<&'a str>,
    pub protocol: Option<&'a str>,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub limit: i64,
    pub offset: i64,
}

pub async fn list(pool: &SqlitePool, f: LogFilter<'_>) -> anyhow::Result<(Vec<Value>, i64)> {
    // 筛选条件要**打两次**：一次数总数，一次取当页。抽成闭包，
    // 否则两份条件迟早会对不上——那会变成「翻页翻到一半记录凭空消失」，
    // 而且不报错。
    let filters = |qb: &mut sqlx::QueryBuilder<'_, sqlx::Sqlite>| {
        if let Some(a) = f.account_id {
            qb.push(" AND account_id = ").push_bind(a);
        }
        if let Some(k) = f.api_key_id {
            qb.push(" AND api_key_id = ").push_bind(k);
        }
        if let Some(t) = f.target {
            qb.push(" AND target = ").push_bind(t.to_string());
        }
        if let Some(st) = f.status {
            qb.push(" AND status = ").push_bind(st.to_string());
        }
        if let Some(p) = f.protocol {
            qb.push(" AND protocol = ").push_bind(p.to_string());
        }
        if let Some(x) = f.from {
            qb.push(" AND created_at >= ").push_bind(x);
        }
        if let Some(x) = f.to {
            qb.push(" AND created_at <= ").push_bind(x);
        }
    };

    // 计数必须真的查 COUNT(*)。
    //
    // **别再拿 `qb.sql()` 拼字符串另起一条查询**：`sql()` 只给 SQL 片段、
    // 不给绑定值。条件里的 `?` 没人绑，SQLite 当 NULL 比对 → 恒 0 条，
    // 而且不报错；无筛选时更怪，`query_scalar("SELECT * …")` 会把**第一行的
    // id** 当成总数。分页因此一直显示「共 0 条」，列表里却明明有记录。
    let mut count_qb =
        sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT COUNT(*) FROM request_log WHERE 1=1");
    filters(&mut count_qb);
    let total: i64 = count_qb.build_query_scalar().fetch_one(pool).await?;

    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT * FROM request_log WHERE 1=1");
    filters(&mut qb);
    qb.push(" ORDER BY id DESC LIMIT ").push_bind(f.limit);
    qb.push(" OFFSET ").push_bind(f.offset);
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
        // 调用方那把 key。删 key 后这里是 null：账还在，出处无从查起。
        "api_key_id": r.get::<Option<i64>, _>("api_key_id"),
        "kind": r.get::<String, _>("kind"),
        "protocol": r.get::<Option<String>, _>("protocol"),
        "target": r.get::<String, _>("target"),
        "node_name": r.get::<Option<String>, _>("node_name"),
        // 上游节点 key，别和上面的 api_key_id 混了
        "node_key_id": r.get::<Option<i64>, _>("node_key_id"),
        "rotated_count": r.get::<i64, _>("rotated_count"),
        "stream": r.get::<i64, _>("stream") != 0,
        "status": r.get::<String, _>("status"),
        "error_kind": r.get::<Option<String>, _>("error_kind"),
        "error_message": r.get::<Option<String>, _>("error_message"),
        "queue_wait_ms": r.get::<i64, _>("queue_wait_ms"),
        "latency_ms": r.get::<i64, _>("latency_ms"),
        // 只有流式请求有；非流式与断流是 null，别当 0 读。
        "ttft_ms": r.get::<Option<i64>, _>("ttft_ms"),
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
        sqlx::query("INSERT INTO account (id, name, created_at, updated_at) VALUES (1,'self',0,0)")
            .execute(&pool)
            .await
            .expect("插入账号");
        sqlx::query(
            "INSERT INTO api_key (id, account_id, name, key_hash, key_prefix,
                                  llm_max_concurrency, llm_max_queue,
                                  image_max_concurrency, image_max_queue, created_at, updated_at)
             VALUES (1,1,'默认','h','sk-hac-',2,8,1,4,0,0)",
        )
        .execute(&pool)
        .await
        .expect("插入 key");
        pool
    }

    fn image_entry(request_id: &str) -> LogEntry {
        LogEntry {
            request_id: request_id.to_string(),
            account_id: 1,
            api_key_id: 1,
            kind: "image",
            protocol: None,
            target: "txt2img".into(),
            node_id: None,
            node_name: None,
            node_key_id: None,
            rotated_count: 0,
            stream: false,
            status: "ok",
            error_kind: None,
            error_message: None,
            queue_wait_ms: 0,
            latency_ms: 1,
            ttft_ms: None,
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

    /// 回归守卫：这个 count 一旦再坏，表现是**静默**的 ——
    /// 列表里有记录，页脚却写「共 0 条」，不报错、不掉日志。
    /// 曾经用 `qb.sql()` 拼字符串另起查询（`sql()` 不带绑定值），
    /// 筛选时 `?` 无人绑定恒不匹配，无筛选时又把第一行的 id 当总数。
    #[tokio::test]
    async fn total_counts_everything_not_just_the_first_page() {
        let pool = test_pool().await;
        for i in 0..7 {
            write(&pool, &image_entry(&format!("cnt-{i}")))
                .await
                .unwrap();
        }
        // 翻页：第 2 页，每页 3 条
        let (page2, total) = list(
            &pool,
            LogFilter {
                limit: 3,
                offset: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(total, 7, "总数必须覆盖全部记录，而不是当页或第一行");
        assert_eq!(page2.len(), 3);
        // 带筛选时也必须真的数
        let (_, filtered) = list(
            &pool,
            LogFilter {
                account_id: Some(1),
                target: Some("txt2img"),
                limit: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(filtered, 7, "筛选后的总数不能塌成 0");
        let (_, miss) = list(
            &pool,
            LogFilter {
                api_key_id: Some(999),
                limit: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(miss, 0, "不匹配时才是真的 0");
    }

    /// 同账号两把 key 的账必须**分得开**。这是拆 key 的全部意义：
    /// 分不开的话「每把 key 分别统计」就是一句空话。
    #[tokio::test]
    async fn two_keys_of_one_account_are_billed_apart() {
        let pool = test_pool().await;
        // 第二把 key
        sqlx::query(
            "INSERT INTO api_key (id, account_id, name, key_hash, key_prefix,
                                  llm_max_concurrency, llm_max_queue,
                                  image_max_concurrency, image_max_queue, created_at, updated_at)
             VALUES (2,1,'手机','h2','sk-hac-2',1,4,1,4,0,0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut a = image_entry("k1-a");
        a.api_key_id = 1;
        // 同一把 key 两条记录：request_id 必须不同，否则会被 UNIQUE 顶掉
        let mut b1 = image_entry("k2-a");
        b1.api_key_id = 2;
        let mut b2 = image_entry("k2-b");
        b2.api_key_id = 2;
        write(&pool, &a).await.unwrap();
        write(&pool, &b1).await.unwrap();
        write(&pool, &b2).await.unwrap();

        // 日汇总也得分开，不能合成一个桶
        let per_key: Vec<(i64, i64, i64)> = sqlx::query_as(
            "SELECT api_key_id, SUM(requests), SUM(image_count)
             FROM usage_daily GROUP BY api_key_id ORDER BY api_key_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(per_key, vec![(1, 1, 1), (2, 2, 2)], "每把 key 各记各的");

        // 明细筛选也要能只挑出某把 key
        let (items, total) = list(
            &pool,
            LogFilter {
                api_key_id: Some(2),
                limit: 50,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(total, 2, "只该看到 key 2 的两条");
        assert!(items.iter().all(|r| r["api_key_id"] == 2), "{items:?}");
    }

    /// 删掉 key 之后账还在，只是查不出是哪把 key 发的。
    /// ON DELETE SET NULL 就是这个意图 —— 用 CASCADE 会把别人的历史抹掉。
    #[tokio::test]
    async fn deleting_a_key_keeps_the_history() {
        let pool = test_pool().await;
        write(&pool, &image_entry("k1-a")).await.unwrap();
        sqlx::query("DELETE FROM api_key WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_log")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "账不该跟着 key 一起消失");
        let k: Option<i64> = sqlx::query_scalar("SELECT api_key_id FROM request_log LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(k, None, "出处置空即可");
    }

    /// 承重墙：明细列表要能把提示词带出来，否则「记了却看不到」等于没记。
    #[tokio::test]
    async fn list_carries_the_prompt_back() {
        let pool = test_pool().await;
        write(&pool, &image_entry("r-1")).await.unwrap();
        put_prompt(&pool, "r-1", "一只猫").await;

        let (items, _) = list(
            &pool,
            LogFilter {
                limit: 50,
                ..Default::default()
            },
        )
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

        let (items, _) = list(
            &pool,
            LogFilter {
                limit: 50,
                ..Default::default()
            },
        )
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

        let (items, _) = list(
            &pool,
            LogFilter {
                limit: 1,
                ..Default::default()
            },
        )
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

    // ── TTFT 与生成速度 ────────────────────────────────────────────────────

    /// 列必须真的落库并能读回来。绑定少一个或列名写错，
    /// 这里就会变成 NULL，而 NULL 看着「挺正常」。
    #[tokio::test]
    async fn ttft_survives_the_round_trip() {
        let pool = test_pool().await;
        let mut e = image_entry("ttft-1");
        e.ttft_ms = Some(432);
        write(&pool, &e).await.expect("写明细");

        let got: Option<i64> =
            sqlx::query_scalar("SELECT ttft_ms FROM request_log WHERE request_id = 'ttft-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(got, Some(432));
    }

    /// 出图没有首 token，必须是 NULL 而不是 0。
    /// 0 会被读成「快得离谱」，混进平均值里把统计拉歪。
    #[tokio::test]
    async fn no_ttft_stays_null() {
        let pool = test_pool().await;
        write(&pool, &image_entry("ttft-none"))
            .await
            .expect("写明细");
        let got: Option<i64> =
            sqlx::query_scalar("SELECT ttft_ms FROM request_log WHERE request_id = 'ttft-none'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(got, None, "没有首 token 必须是 NULL");
    }

    /// 口径：纯生成速度 = token / (总耗时 - 排队 - TTFT)。
    /// 100 token、总共 2s、排队 0.2s、TTFT 0.3s → 生成 1.5s → 66.7/s。
    #[test]
    fn throughput_excludes_queue_and_prefill() {
        let tps = decode_tokens_per_sec(Some(300), 200, 2_000, 100).expect("应当算得出");
        assert!((tps - 100.0 * 1000.0 / 1500.0).abs() < 0.01, "{tps}");
    }

    /// 非流式没有 TTFT，不能拿端到端吞吐冒充生成速度。
    #[test]
    fn non_streaming_has_no_throughput() {
        assert_eq!(decode_tokens_per_sec(None, 0, 2_000, 100), None);
    }

    /// 空补全 / 被掐断的流：没有 token 就是没有速度，不能报 0。
    #[test]
    fn zero_tokens_has_no_throughput() {
        assert_eq!(decode_tokens_per_sec(Some(300), 0, 2_000, 0), None);
    }

    /// 分母 ≤ 0 时必须返回 None。
    /// 报 0 会把平均速度拉到 0，除零会得到天文数字——两个都是编的数。
    #[test]
    fn non_positive_generation_time_has_no_throughput() {
        // TTFT 比总耗时还长：两个计时字段互相矛盾
        assert_eq!(decode_tokens_per_sec(Some(5_000), 0, 2_000, 100), None);
        // 扣完正好为 0
        assert_eq!(decode_tokens_per_sec(Some(2_000), 0, 2_000, 100), None);
    }
}
