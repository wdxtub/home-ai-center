//! 账务：预扣 → 实扣 → 退差。
//!
//! 余额硬扣减遇到长耗时请求（生图几分钟）时，若只在结束时扣款，
//! 并发请求会同时看到「余额充足」然后一起透支。因此：
//!
//! 1. **预扣**：准入时按最大可能费用 `reserved` 冻结，走条件更新
//!    `WHERE balance_micro - held_micro >= reserved`，受影响 0 行即 402。
//! 2. **实扣**：调用结束后按真实 usage 算出 `actual`，落库并释放冻结。
//! 3. **退差**：`held` 减 `reserved`、`balance` 减 `actual`，差额自动还回。
//!
//! 整个结算在**单事务**内完成，保证并发下不超支、不重复扣费。

use sqlx::{Row, SqlitePool};

use crate::error::{ApiError, ApiResult};
use crate::protocol::tokenize;
use crate::protocol::ir::UnifiedRequest;

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 预扣一笔。返回实际冻结的金额。
pub async fn reserve(
    pool: &SqlitePool,
    account_id: i64,
    amount_micro: i64,
    request_id: &str,
) -> ApiResult<i64> {
    if amount_micro <= 0 {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let now = unix_now();

    // 条件更新：可用额度不足时受影响行数为 0
    let res = sqlx::query(
        "UPDATE account SET held_micro = held_micro + ?, updated_at = ?
         WHERE id = ? AND enabled = 1 AND balance_micro - held_micro >= ?",
    )
    .bind(amount_micro)
    .bind(now)
    .bind(account_id)
    .bind(amount_micro)
    .execute(&mut *tx)
    .await?;

    if res.rows_affected() == 0 {
        let (bal, held) = sqlx::query("SELECT balance_micro, held_micro FROM account WHERE id = ?")
            .bind(account_id)
            .fetch_optional(&mut *tx)
            .await?
            .map(|r| (r.get::<i64, _>(0), r.get::<i64, _>(1)))
            .unwrap_or((0, 0));
        tx.rollback().await?;
        return Err(ApiError::payment_required(format!(
            "余额不足：可用 {} 微元，本次需要 {} 微元（余额 {} / 冻结 {}）",
            bal - held,
            amount_micro,
            bal,
            held
        )));
    }

    let after: i64 = sqlx::query_scalar("SELECT balance_micro FROM account WHERE id = ?")
        .bind(account_id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO balance_txn (account_id, kind, amount_micro, balance_after, request_id, note, created_at)
         VALUES (?, 'settle', ?, ?, ?, '预扣', ?)",
    )
    .bind(account_id)
    .bind(amount_micro)
    .bind(after)
    .bind(request_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(amount_micro)
}

/// 释放全部冻结（调用失败，不产生费用）。
pub async fn release_all(pool: &SqlitePool, account_id: i64, reserved: i64, request_id: &str) {
    if reserved <= 0 {
        return;
    }
    let mut tx = match pool.begin().await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "释放冻结失败");
            return;
        }
    };
    let now = unix_now();
    let _ = sqlx::query(
        "UPDATE account SET held_micro = MAX(0, held_micro - ?), updated_at = ? WHERE id = ?",
    )
    .bind(reserved)
    .bind(now)
    .bind(account_id)
    .execute(&mut *tx)
    .await;
    let _ = sqlx::query(
        "INSERT INTO balance_txn (account_id, kind, amount_micro, balance_after, request_id, note, created_at)
         VALUES (?, 'refund', ?, (SELECT balance_micro FROM account WHERE id = ?), ?, '调用失败释放冻结', ?)",
    )
    .bind(account_id)
    .bind(reserved)
    .bind(account_id)
    .bind(request_id)
    .bind(now)
    .execute(&mut *tx)
    .await;
    let _ = tx.commit().await;
}

/// 结算：按真实 usage 扣费并释放冻结，差额自动还回。
pub async fn settle(
    pool: &SqlitePool,
    account_id: i64,
    reserved: i64,
    actual_micro: i64,
    request_id: &str,
) -> ApiResult<i64> {
    let mut tx = pool.begin().await?;
    let now = unix_now();
    let actual = actual_micro.max(0);

    let bal: i64 = sqlx::query_scalar("SELECT balance_micro FROM account WHERE id = ?")
        .bind(account_id)
        .fetch_one(&mut *tx)
        .await?;
    let new_bal = bal - actual;

    // 冻结全部释放；余额按实际扣减
    sqlx::query(
        "UPDATE account SET held_micro = MAX(0, held_micro - ?), balance_micro = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(reserved)
    .bind(new_bal)
    .bind(now)
    .bind(account_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO balance_txn (account_id, kind, amount_micro, balance_after, request_id, note, created_at)
         VALUES (?, 'settle', ?, ?, ?, '调用结算', ?)",
    )
    .bind(account_id)
    .bind(-actual)
    .bind(new_bal)
    .bind(request_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(actual)
}

/// 充值 / 人工调整。
pub async fn adjust(
    pool: &SqlitePool,
    account_id: i64,
    delta_micro: i64,
    note: &str,
) -> ApiResult<i64> {
    let mut tx = pool.begin().await?;
    let now = unix_now();
    let bal: i64 = sqlx::query_scalar("SELECT balance_micro FROM account WHERE id = ?")
        .bind(account_id)
        .fetch_one(&mut *tx)
        .await?;
    let new_bal = bal + delta_micro;
    sqlx::query("UPDATE account SET balance_micro = ?, updated_at = ? WHERE id = ?")
        .bind(new_bal)
        .bind(now)
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO balance_txn (account_id, kind, amount_micro, balance_after, request_id, note, created_at)
         VALUES (?, ?, ?, ?, NULL, ?, ?)",
    )
    .bind(account_id)
    .bind(if delta_micro >= 0 { "topup" } else { "adjust" })
    .bind(delta_micro)
    .bind(new_bal)
    .bind(note)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(new_bal)
}

/// 崩溃恢复对账：残留的 `held_micro` 一律清零。
///
/// 已知取舍：崩溃窗口内最多有一笔预留被释放（宁可少收，不可超收）。
pub async fn reconcile_holds(pool: &SqlitePool) -> anyhow::Result<usize> {
    let res = sqlx::query(
        "UPDATE account SET held_micro = 0, updated_at = ? WHERE held_micro != 0",
    )
    .bind(unix_now())
    .execute(pool)
    .await?;
    Ok(res.rows_affected() as usize)
}

/// 预估一次 LLM 请求的最大费用（微元），用于预扣。
pub fn estimate_llm_reserve(
    price: &crate::domain::pricing::ModelPrice,
    req: &UnifiedRequest,
    fallback_max_output: u32,
) -> i64 {
    let est_in = tokenize::estimate_request(req);
    let est_out = req.max_output_tokens.unwrap_or(fallback_max_output);
    crate::domain::pricing::PriceTable::llm_cost(price, est_in, 0, est_out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::pricing::ModelPrice;
    use crate::protocol::ir::{UnifiedMessage, UnifiedRequest, UnifiedUsage};

    fn price() -> ModelPrice {
        ModelPrice {
            input_micro_per_1k: 1000,
            output_micro_per_1k: 2000,
            cached_input_micro_per_1k: None,
        }
    }

    fn req(max: Option<u32>) -> UnifiedRequest {
        UnifiedRequest {
            model: "m".into(),
            messages: vec![UnifiedMessage::user_text("hello world")],
            max_output_tokens: max,
            ..Default::default()
        }
    }

    #[test]
    fn reserve_estimates_using_max_output_when_absent() {
        let with_max = estimate_llm_reserve(&price(), &req(Some(1000)), 4096);
        let without = estimate_llm_reserve(&price(), &req(None), 4096);
        // 没给 max_tokens 时按节点默认值估，应更大
        assert!(without > with_max);
    }

    #[test]
    fn reserve_is_proportional_to_output_tokens() {
        let small = estimate_llm_reserve(&price(), &req(Some(100)), 4096);
        let big = estimate_llm_reserve(&price(), &req(Some(1000)), 4096);
        assert!(big > small);
    }

    #[test]
    fn zero_priced_model_reserves_nothing() {
        let free = ModelPrice {
            input_micro_per_1k: 0,
            output_micro_per_1k: 0,
            cached_input_micro_per_1k: None,
        };
        assert_eq!(estimate_llm_reserve(&free, &req(Some(1000)), 4096), 0);
    }

    // ── 真库测试：三段式账务 ──

    /// 用**临时文件库**，不要用 `sqlite::memory:`：
    /// 后者每开一条连接就是一个全新空库，连接池一旦回收连接，
    /// 刚插进去的账号就凭空消失（表现为 `fetch_one` 返回 no rows）。
    async fn test_pool() -> sqlx::SqlitePool {
        let dir = tempfile::tempdir().expect("建临时目录");
        let path = dir.path().join("t.db");
        let pool = crate::db::connect(&path).await.expect("建库");
        crate::db::migrate::MIGRATOR
            .run(&pool)
            .await
            .expect("跑迁移");
        // 目录句柄交回给调用方之前不能删：池还在用这个文件
        std::mem::forget(dir);
        pool
    }

    async fn add_account(pool: &sqlx::SqlitePool, balance: i64) -> i64 {
        let now = unix_now();
        sqlx::query(
            "INSERT INTO account (name, api_key_hash, api_key_prefix, balance_micro, created_at, updated_at)
             VALUES ('t','h','p',?,?,?)",
        )
        .bind(balance)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query_scalar("SELECT id FROM account WHERE name='t'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn bal(pool: &sqlx::SqlitePool, id: i64) -> (i64, i64) {
        sqlx::query_as("SELECT balance_micro, held_micro FROM account WHERE id=?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// 预扣 → 实扣 → 退差：余额只减真实费用，冻结必须清零。
    #[tokio::test]
    async fn settle_refunds_the_difference() {
        let pool = test_pool().await;
        let id = add_account(&pool, 1_000_000).await;

        reserve(&pool, id, 500_000, "r1").await.unwrap();
        assert_eq!(bal(&pool, id).await, (1_000_000, 500_000));

        settle(&pool, id, 500_000, 123_456, "r1").await.unwrap();
        // 余额扣 123456，冻结全部释放
        assert_eq!(bal(&pool, id).await, (876_544, 0));
    }

    /// 余额不足必须硬拒绝，且不留下任何冻结。
    #[tokio::test]
    async fn reserve_rejects_when_balance_insufficient() {
        let pool = test_pool().await;
        let id = add_account(&pool, 1000).await;
        let e = reserve(&pool, id, 5000, "r1").await.unwrap_err();
        assert_eq!(e.kind, crate::error::ErrorKind::PaymentRequired);
        assert_eq!(bal(&pool, id).await, (1000, 0));
    }

    /// 已有在途预扣时，可用余额要按「余额 − 冻结」算，
    /// 否则并发请求会一起看到「余额充足」然后一起透支。
    #[tokio::test]
    async fn reserve_accounts_for_in_flight_holds() {
        let pool = test_pool().await;
        let id = add_account(&pool, 1000).await;
        reserve(&pool, id, 800, "r1").await.unwrap();
        // 只剩 200 可用
        assert!(reserve(&pool, id, 300, "r2").await.is_err());
        assert!(reserve(&pool, id, 200, "r2").await.is_ok());
    }

    #[tokio::test]
    async fn release_all_clears_the_hold() {
        let pool = test_pool().await;
        let id = add_account(&pool, 1000).await;
        reserve(&pool, id, 800, "r1").await.unwrap();
        release_all(&pool, id, 800, "r1").await;
        assert_eq!(bal(&pool, id).await, (1000, 0));
    }

    /// 崩溃恢复：残留冻结一律清零（宁可少收，不可超收）。
    #[tokio::test]
    async fn reconcile_zeroes_orphan_holds() {
        let pool = test_pool().await;
        let id = add_account(&pool, 1000).await;
        reserve(&pool, id, 800, "r1").await.unwrap();
        assert_eq!(reconcile_holds(&pool).await.unwrap(), 1);
        assert_eq!(bal(&pool, id).await, (1000, 0));
        assert_eq!(reconcile_holds(&pool).await.unwrap(), 0);
    }

    /// 充值要能落回 balance 并留一笔可查的流水。
    #[tokio::test]
    async fn adjust_topup_writes_balance_and_txn() {
        let pool = test_pool().await;
        let id = add_account(&pool, 0).await;
        assert_eq!(adjust(&pool, id, 250_000, "充值").await.unwrap(), 250_000);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM balance_txn WHERE account_id=?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn settle_cost_matches_usage_directly() {
        let u = UnifiedUsage {
            input_tokens: 1000,
            cached_tokens: 0,
            output_tokens: 1000,
            reasoning_tokens: 0,
        };
        let c = crate::domain::pricing::PriceTable::llm_cost(&price(), u.input_tokens, u.cached_tokens, u.output_tokens);
        assert_eq!(c, 3000);
    }
}
