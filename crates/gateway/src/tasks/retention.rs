//! 调用明细保留策略。
//!
//! **明细会无限增长**：一次出图的元数据不多，但一天的生图调用量
//! 轻松上万行。不清理的话 SQLite 文件会一直涨，备份也跟着变大。
//!
//! 清理口径：按 `created_at` 删 `request_log`，**`usage_daily` 永久保留**
//! ——日汇总才是做趋势分析的东西，粒度粗、占用小、丢不得。

use std::sync::Arc;

use crate::state::AppState;

/// 默认保留天数。家用场景 90 天足够回溯账单又不至于撑爆磁盘。
pub const DEFAULT_RETENTION_DAYS: i64 = 90;

pub async fn purge_once(s: &Arc<AppState>) -> anyhow::Result<u64> {
    let days = s
        .snapshot()
        .setting_i64("log_retention_days", DEFAULT_RETENTION_DAYS);
    let removed = crate::billing::usage::purge_older_than(&s.pool, days).await?;
    if removed > 0 {
        tracing::info!(removed, days, "清理过期调用明细");
    }
    Ok(removed)
}

/// 每天凌晨与备份错开半小时跑一次。
pub async fn scheduler(s: Arc<AppState>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)).await;
        if let Err(e) = purge_once(&s).await {
            tracing::error!(error = %e, "清理明细异常");
        }
    }
}
