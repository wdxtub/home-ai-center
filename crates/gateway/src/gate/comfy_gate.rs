//! ComfyUI 端点调度。
//!
//! 与 LLM 选点的关键差异：**换端点只允许发生在提交之前**。
//! ComfyUI 一次出图几十秒，`POST /prompt` 成功之后任务就绑死在那个端点上了；
//! 此时再换端点重投会产生**双倍计费 + 重复图**。所以这里把
//! 「提交前」与「提交后」分成两段，拿到的结构也必须能表达这个边界。

use std::time::Duration;

use crate::domain::comfy::ComfyNode;
use crate::error::{ApiError, ApiResult};
use crate::gate::{Priority, SlotLease};
use crate::state::AppState;

/// 端点故障后的冷却。ComfyUI 掉线通常是重启（加载模型几分钟），
/// 短冷却会让网关在它还没起来时反复打过去。
const COOLDOWN: Duration = Duration::from_secs(300);

/// 一次出图占住的东西。
pub struct ComfyPick {
    pub node: ComfyNode,
    pub lease: SlotLease,
    pub queue_wait_ms: i64,
}

#[derive(Default)]
pub struct ComfyHealth {
    cooling: tokio::sync::Mutex<std::collections::HashMap<i64, std::time::Instant>>,
}

impl ComfyHealth {
    pub async fn is_cooling(&self, node_id: i64) -> bool {
        let mut m = self.cooling.lock().await;
        match m.get(&node_id) {
            Some(t) if std::time::Instant::now() < *t => true,
            Some(_) => {
                m.remove(&node_id);
                false
            }
            None => false,
        }
    }

    pub async fn mark_failure(&self, node_id: i64) {
        self.cooling
            .lock()
            .await
            .insert(node_id, std::time::Instant::now() + COOLDOWN);
        tracing::warn!(node = node_id, "ComfyUI 端点进入冷却");
    }

    pub async fn mark_success(&self, node_id: i64) {
        self.cooling.lock().await.remove(&node_id);
    }
}

/// 选一个 ComfyUI 端点并占住槽位。
///
/// 排序与 LLM 选点同源：**占用率最低者优先**。
/// 出图动辄几十秒，按绝对并发排会让慢端点永远排不上。
pub async fn acquire(
    s: &AppState,
    health: &ComfyHealth,
    node_ids: &[i64],
    queue_timeout: Duration,
) -> ApiResult<ComfyPick> {
    let snap = s.snapshot();
    let candidates = snap.comfy_nodes_for(node_ids);
    if candidates.is_empty() {
        return Err(ApiError::not_found(
            "没有可用的 ComfyUI 端点（未配置 / 已停用 / 落在禁用时段）",
        ));
    }

    let started = std::time::Instant::now();
    loop {
        // 先一次性取冷却状态：既避免在选择循环里反复抢锁，
        // 也能在「全部冷却」时立刻给出可预期的答复而不是干等。
        let mut cooling = Vec::with_capacity(candidates.len());
        for id in &candidates {
            cooling.push(health.is_cooling(*id).await);
        }
        if candidates
            .iter()
            .zip(&cooling)
            .all(|(_, c)| *c)
        {
            return Err(ApiError::no_capacity("全部 ComfyUI 端点都在冷却中").with_retry_after(60));
        }

        let mut best: Option<(f64, i64, i64, i64)> = None; // (占用率, active, sort_order, id)
        for (id, is_cooling) in candidates.iter().zip(&cooling) {
            let Some(n) = snap.comfy_node(*id) else { continue };
            if *is_cooling {
                continue;
            }
            let st = s.comfy_gate(*id).await.snapshot(n.max_concurrency).await;
            if st.active >= n.max_concurrency {
                continue;
            }
            let cand = (
                st.active as f64 / n.max_concurrency as f64,
                st.active,
                n.sort_order,
                *id,
            );
            best = match best {
                None => Some(cand),
                Some(cur) => Some(if cand < cur { cand } else { cur }),
            };
        }

        if let Some((_, _, _, node_id)) = best {
            let n = snap.comfy_node(node_id).expect("候选来自同一快照");
            let gate = s.comfy_gate(node_id).await;
            if let Ok(lease) = gate
                .acquire(
                    n.max_concurrency,
                    // 出图请求天然是长任务，队列给得比 LLM 宽
                    n.max_concurrency * 8 + 8,
                    Priority::Batch,
                    Some(queue_timeout),
                )
                .await
            {
                return Ok(ComfyPick {
                    node: n.clone(),
                    lease,
                    queue_wait_ms: started.elapsed().as_millis() as i64,
                });
            }
        }

        if started.elapsed() >= queue_timeout {
            return Err(ApiError::queue_full("所有 ComfyUI 端点都满载，排队超时").with_retry_after(10));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}
