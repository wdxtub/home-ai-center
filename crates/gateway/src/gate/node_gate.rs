//! 节点层调度：最小占用率选点 + 冷却退避。
//!
//! 选点规则（与 `erotic_sci` 一致）：
//! **占用率** `active/limit` 最低者优先 → 并列比 `active` 少 → 再并列按配置顺序。
//! 用占用率而不是绝对并发，是为了让「1 台 1 并发 + 1 台 4 并发」时
//! 不会把慢机器一直塞满。
//!
//! **key 可用性是节点可选条件的一部分**：全部 key 冷却 / 隔离时该节点
//! 自动退出选点，无需在「选完点发现没 key」的地方打补丁。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Mutex;

use crate::domain::node::{LlmNode, NodeKey};
use crate::keys::KeyRing;
use crate::state::AppState;

/// 连接级失败的退避档位。
const OFFLINE_BASE: Duration = Duration::from_secs(60);
const OFFLINE_MAX: Duration = Duration::from_secs(1800);
/// 超时（可能只是模型在冷启动）按更轻的档位退避。
const TIMEOUT_BASE: Duration = Duration::from_secs(30);
const TIMEOUT_MAX: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// 连接拒绝 / 5xx：节点不可达。
    Hard,
    /// 超时：节点还活着，只是暂时不可用。
    Soft,
}

#[derive(Debug, Clone)]
pub struct NodeState {
    pub healthy: bool,
    pub consecutive_failures: u32,
    /// 冷却截止时刻（monotonic）。
    pub retry_after: Option<std::time::Instant>,
    pub last_error: Option<String>,
    pub last_success_at: Option<std::time::Instant>,
    pub cooldown_secs_remaining: i64,
}

impl Default for NodeState {
    fn default() -> Self {
        Self {
            healthy: true,
            consecutive_failures: 0,
            retry_after: None,
            last_error: None,
            last_success_at: None,
            cooldown_secs_remaining: 0,
        }
    }
}

#[derive(Default)]
pub struct NodeHealth {
    states: Mutex<HashMap<i64, NodeState>>,
    /// 内网地址「连不上」的截止时刻。
    ///
    /// 和 `NodeState.retry_after` 分开：内网不通**不代表节点坏了**
    /// （同一台机器的公网地址可能好好的），所以不能进节点冷却，
    /// 否则一次换网络就能把所有节点拖进冷却里。
    lan_until: Mutex<HashMap<i64, std::time::Instant>>,
}

/// 内网不可达后多久再试一次内网。取中间值：短了反复付连接失败的钱，
/// 长了家里内网恢复后要干等。
const LAN_RETRY_AFTER: Duration = Duration::from_secs(120);

impl NodeHealth {
    /// 这个节点的内网地址现在是不是不可用。
    pub async fn is_lan_down(&self, node_id: i64) -> bool {
        let mut m = self.lan_until.lock().await;
        match m.get(&node_id) {
            Some(t) if std::time::Instant::now() < *t => true,
            Some(_) => {
                m.remove(&node_id);
                false
            }
            None => false,
        }
    }

    pub async fn mark_lan_down(&self, node_id: i64, url: &str) {
        self.lan_until
            .lock()
            .await
            .insert(node_id, std::time::Instant::now() + LAN_RETRY_AFTER);
        tracing::warn!(
            node = node_id,
            url = %url,
            retry_after_secs = LAN_RETRY_AFTER.as_secs(),
            "内网地址连不上，先走公网"
        );
    }

    /// 内网真的通了才清标记。用公网成功**不能**清，否则每个请求
    /// 都会重新去撞一次不通的内网。
    pub async fn mark_lan_up(&self, node_id: i64) {
        self.lan_until.lock().await.remove(&node_id);
    }

    pub async fn lan_down_remaining(&self, node_id: i64) -> i64 {
        let m = self.lan_until.lock().await;
        m.get(&node_id)
            .map(|t| (t.saturating_duration_since(std::time::Instant::now())).as_secs() as i64)
            .unwrap_or(0)
    }

    pub async fn mark_failure(&self, node_id: i64, kind: FailureKind, error: &str) {
        let mut m = self.states.lock().await;
        let st = m.entry(node_id).or_default();
        st.healthy = false;
        st.consecutive_failures = st.consecutive_failures.saturating_add(1);
        let (base, ceiling) = match kind {
            FailureKind::Hard => (OFFLINE_BASE, OFFLINE_MAX),
            FailureKind::Soft => (TIMEOUT_BASE, TIMEOUT_MAX),
        };
        let delay = (base * 2u32.saturating_pow(st.consecutive_failures - 1).min(16))
            .min(ceiling);
        st.retry_after = Some(std::time::Instant::now() + delay);
        st.cooldown_secs_remaining = delay.as_secs() as i64;
        st.last_error = Some(error.chars().take(200).collect());
        tracing::warn!(
            node = node_id,
            cooldown_secs = delay.as_secs(),
            error = %st.last_error.as_deref().unwrap_or(""),
            "节点标记离线并进入冷却"
        );
    }

    pub async fn mark_success(&self, node_id: i64) {
        let mut m = self.states.lock().await;
        let st = m.entry(node_id).or_default();
        if !st.healthy {
            tracing::info!(node = node_id, "节点恢复在线");
        }
        st.healthy = true;
        st.consecutive_failures = 0;
        st.retry_after = None;
        st.cooldown_secs_remaining = 0;
        st.last_error = None;
        st.last_success_at = Some(std::time::Instant::now());
    }

    /// 冷却到期自动回到候选池。
    pub async fn is_cooling(&self, node_id: i64) -> bool {
        let mut m = self.states.lock().await;
        let Some(st) = m.get_mut(&node_id) else {
            return false;
        };
        match st.retry_after {
            Some(t) if std::time::Instant::now() < t => true,
            Some(_) => {
                // 到期：恢复，但失败计数保留，连续失败会继续拉长下一次退避
                st.retry_after = None;
                st.cooldown_secs_remaining = 0;
                false
            }
            None => false,
        }
    }

    pub async fn snapshot(&self) -> HashMap<i64, NodeState> {
        self.states.lock().await.clone()
    }
}

/// 一次节点占用的结果：拿到哪个节点、用哪把 key。
pub struct NodePick {
    pub node: LlmNode,
    pub key: NodeKey,
    pub lease: crate::gate::SlotLease,
    pub queue_wait_ms: i64,
    /// 实际发给上游的模型名。**可能与客户端请求的模型名不同**——
    /// 调度器已经知道选中了哪条路由，就该由它把映射结果一起带出来。
    pub upstream_model: String,
}

/// 选点时的比较键。**按占用率排，不是按绝对并发**——
/// 否则「1 台 1 并发 + 1 台 4 并发」时那台快的会被一直塞满。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cand {
    ratio: f64,
    active: i64,
    sort_order: i64,
    node_id: i64,
}

pub struct NodeScheduler {
    /// 进程级共享的节点健康表。**绝不能每个请求新建一份**——
    /// 否则「标记离线 + 冷却」在下一个请求眼里根本不存在，
    /// 退避机制等于没写。
    pub health: Arc<NodeHealth>,
    pub keys: Arc<KeyRing>,
}

impl NodeScheduler {
    pub fn new(health: Arc<NodeHealth>, keys: Arc<KeyRing>) -> Self {
        Self { health, keys }
    }

    /// 选一个可用节点并占住它的槽位。
    ///
    /// 候选 = 有路由 ∧ 启用 ∧ 不在禁用时段 ∧ 未冷却 ∧ **至少一把可用 key**。
    pub async fn acquire(
        &self,
        state: &AppState,
        model: &str,
        priority: crate::gate::Priority,
        queue_timeout: Duration,
    ) -> Result<NodePick, crate::error::ApiError> {
        let snap = state.snapshot();
        let candidates: Vec<i64> = snap
            .nodes_for_model(model)
            .into_iter()
            .filter(|id| {
                snap.node(*id).is_some_and(|n| n.enabled && !n.window.in_disabled_hours())
            })
            .collect();

        if candidates.is_empty() {
            return Err(crate::error::ApiError::not_found(format!(
                "模型 {model} 没有可用的节点（未配置路由，或节点已停用 / 落在禁用时段）"
            )));
        }

        let started = std::time::Instant::now();
        loop {
            // 1) 先按占用率挑一个当前真有槽位的节点
            let mut best: Option<Cand> = None; // (占用率, active, sort_order, node_id)
            for id in &candidates {
                let Some(n) = snap.node(*id) else { continue };
                if !self.keys.node_has_usable_key(n).await {
                    continue; // key 全耗尽 → 该节点不参与
                }
                if self.health.is_cooling(*id).await {
                    continue;
                }
                let gate = state.node_gate(*id).await;
                let st = gate.snapshot(n.max_concurrency).await;
                if st.active >= n.max_concurrency {
                    continue;
                }
                let cand = Cand {
                    ratio: st.active as f64 / n.max_concurrency as f64,
                    active: st.active,
                    sort_order: n.sort_order,
                    node_id: *id,
                };
                best = match best {
                    None => Some(cand),
                    Some(cur) => {
                        if (cand.ratio, cand.active, cand.sort_order, cand.node_id)
                            < (cur.ratio, cur.active, cur.sort_order, cur.node_id)
                        {
                            Some(cand)
                        } else {
                            Some(cur)
                        }
                    }
                };
            }

            if let Some(Cand { node_id, .. }) = best {
                if let Some(n) = snap.node(node_id) {
                    let gate = state.node_gate(node_id).await;
                    let lease = gate
                        .acquire(n.max_concurrency, queue_timeout.as_secs() as i64 * 4 + 4, priority, Some(queue_timeout))
                        .await
                        .map_err(|e| match e {
                            crate::gate::GateError::QueueFull => crate::error::ApiError::queue_full(
                                format!("节点 {} 的并发已排满，请稍后再试", n.name),
                            ),
                            crate::gate::GateError::Timeout => crate::error::ApiError::queue_full(
                                format!("等待节点 {} 空闲超时", n.name),
                            ),
                        })?;
                    if let Some(key) = self.keys.pick(n).await {
                        let key_id = key.id;
                        self.keys.mark_used(key_id).await;
                        let upstream_model = snap.upstream_model(model, node_id);
                        return Ok(NodePick {
                            node: n.clone(),
                            key,
                            lease,
                            queue_wait_ms: started.elapsed().as_millis() as i64,
                            upstream_model,
                        });
                    }
                }
            }

            // 2) 全都满了 → 找最空闲的节点排队等
            let mut fallback: Option<i64> = None;
            for id in &candidates {
                let Some(n) = snap.node(*id) else { continue };
                if !self.keys.node_has_usable_key(n).await {
                    continue;
                }
                match fallback.and_then(|f| snap.node(f).map(|x| x.max_concurrency)) {
                    Some(cur) if cur <= n.max_concurrency => {}
                    _ => fallback = Some(*id),
                }
            }

            if started.elapsed() >= queue_timeout {
                return Err(crate::error::ApiError::queue_full(format!(
                    "模型 {model} 的所有节点都满载，排队超时"
                ))
                .with_retry_after(5));
            }
            if let Some(fid) = fallback {
                if let Some(n) = snap.node(fid) {
                    let gate = state.node_gate(fid).await;
                    match gate
                        .acquire(
                            n.max_concurrency,
                            queue_timeout.as_secs() as i64 * 4 + 4,
                            priority,
                            Some(Duration::from_millis(200)),
                        )
                        .await
                    {
                        Ok(lease) => {
                            if let Some(key) = self.keys.pick(n).await {
                                let key_id = key.id;
                                self.keys.mark_used(key_id).await;
                                let upstream_model = snap.upstream_model(model, fid);
                                return Ok(NodePick {
                                    node: n.clone(),
                                    key,
                                    lease,
                                    queue_wait_ms: started.elapsed().as_millis() as i64,
                                    upstream_model,
                                });
                            }
                            continue;
                        }
                        Err(_) => {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    }
                }
            }

            // 连一把可用 key 都没有
            let any_node = candidates
                .iter()
                .filter_map(|id| snap.node(*id))
                .next();
            if let Some(n) = any_node {
                if let Some(at) = self.keys.earliest_recovery(n).await {
                    let now = crate::keys::unix_now();
                    let wait = (at - now).max(1) as u64;
                    return Err(crate::error::ApiError::no_capacity(format!(
                        "节点 {} 的全部 API Key 都已限额，最早恢复：{}（约 {} 分钟后）",
                        n.name,
                        chrono::DateTime::from_timestamp(at, 0)
                            .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                            .unwrap_or_else(|| at.to_string()),
                        wait / 60
                    ))
                    .with_retry_after(wait.min(3600))
                    .with_reset_at(at));
                }
            }
            return Err(crate::error::ApiError::no_capacity(format!(
                "模型 {model} 当前没有可用节点（全部 key 已限额或节点离线）"
            )));
        }
    }
}

/// 节点健康快照（管理页面用）。
#[derive(Debug, Clone, Serialize)]
pub struct HealthRow {
    pub node_id: i64,
    pub name: String,
    pub healthy: bool,
    pub cooldown_secs_remaining: i64,
    pub last_error: Option<String>,
    pub last_success_at: Option<i64>,
}

#[cfg(test)]
mod lan_tests {
    use super::*;

    /// 内网不通**不能**进节点冷却：同一台机器的公网地址可能好好的，
    /// 进冷却等于一次换网络就把所有节点拖下线。
    #[tokio::test]
    async fn lan_down_does_not_cool_the_node() {
        let h = NodeHealth::default();
        assert!(!h.is_lan_down(1).await);
        h.mark_lan_down(1, "http://192.168.50.197:1234/v1").await;
        assert!(h.is_lan_down(1).await);
        // 节点本身仍然是健康的、没进冷却
        assert!(!h.is_cooling(1).await);
        let st = h.snapshot().await;
        assert!(!st.contains_key(&1), "内网标记不该新建节点健康记录");
    }

    #[tokio::test]
    async fn lan_up_clears_the_mark() {
        let h = NodeHealth::default();
        h.mark_lan_down(1, "http://lan/v1").await;
        h.mark_lan_up(1).await;
        assert!(!h.is_lan_down(1).await);
    }

    /// 标记是按节点分的，不能互相污染。
    #[tokio::test]
    async fn lan_state_is_per_node() {
        let h = NodeHealth::default();
        h.mark_lan_down(1, "http://lan1/v1").await;
        assert!(h.is_lan_down(1).await);
        assert!(!h.is_lan_down(2).await);
    }
}
