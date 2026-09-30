//! API Key 轮换。
//!
//! 一个节点持有**多把** key（用户是不同组织 / 订阅账号，每把限额桶独立）。
//! 某把 key 撞上额度上限时，标记它冷却并切到下一把。
//!
//! 子模块：
//! - [`classify`] 限额错误的有序决策表（**最容易写错的地方**）
//! - [`reset`]    重置时间解析（5 种格式 + 合理性钳制）
//! - [`window`]   软上限分桶窗口计数（主动预测）
//! - `mod.rs`     `KeyRing`：key 池运行态与选 key

pub mod classify;
pub mod reset;
pub mod window;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::domain::node::{LlmNode, NodeKey, NodeKeyState, RateLimitScope};

pub use classify::{QuotaClass, QuotaVerdict};
pub use reset::ResetSource;

/// 默认冷却：没有任何可解析的重置时间时用。
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(5 * 3600 + 5 * 60);
/// 重置时间解析的合理上限，越界视为损坏。
pub const MAX_RESET_HORIZON: Duration = Duration::from_secs(8 * 24 * 3600);
/// `Retry-After` 的上限（上游可能返回荒谬值）。
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// 每把 key 的运行时态。**冷却截止时刻同时落库**——
/// 5 小时冷却若因容器重启丢失，重启后会立刻再撞一次限额。
#[derive(Debug, Clone)]
pub struct KeyRuntime {
    pub cooldown_until: Option<Instant>,
    pub soft_cap_reached: bool,
    pub window_started_at: Option<Instant>,
    pub window_tokens_used: i64,
    pub state: NodeKeyState,
}

impl KeyRuntime {
    pub fn from_key(k: &NodeKey) -> Self {
        Self {
            cooldown_until: k.cooldown_until.map(|t| {
                Instant::now()
                    + Duration::from_secs((t - unix_now()).max(0) as u64)
            }),
            soft_cap_reached: false,
            window_started_at: None,
            window_tokens_used: k.window_tokens_used,
            state: k.state,
        }
    }

    pub fn usable(&self, enabled: bool) -> bool {
        if !enabled || self.state == NodeKeyState::Disabled || self.state == NodeKeyState::Quarantined
        {
            return false;
        }
        if let Some(until) = self.cooldown_until {
            if Instant::now() < until {
                return false;
            }
        }
        !self.soft_cap_reached
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 全进程共享的 key 池运行态。
pub struct KeyRing {
    pool: SqlitePool,
    /// key_id → 运行时态
    runtime: Mutex<HashMap<i64, KeyRuntime>>,
}

impl KeyRing {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            runtime: Mutex::new(HashMap::new()),
        }
    }

    /// 从节点配置刷新运行态（配置变更时调用）。
    pub async fn sync(&self, nodes: &[LlmNode]) {
        let now = unix_now();
        let mut rt = self.runtime.lock().await;
        for n in nodes {
            for k in &n.keys {
                let entry = rt.entry(k.id).or_insert_with(|| KeyRuntime::from_key(k));
                entry.state = k.state;
                entry.window_tokens_used = k.window_tokens_used;
                // 冷却截止时刻以 DB 为准：它是跨重启语义的一部分
                entry.cooldown_until = k.cooldown_until.and_then(|t| {
                    if t <= now {
                        None
                    } else {
                        Some(Instant::now() + Duration::from_secs((t - now) as u64))
                    }
                });
            }
        }
        // 清理已删除节点的残留
        let live: std::collections::HashSet<i64> =
            nodes.iter().flat_map(|n| n.keys.iter().map(|k| k.id)).collect();
        rt.retain(|id, _| live.contains(id));
    }

    /// 这个节点此刻有没有可用 key。没有的话它就不该参与选点。
    pub async fn node_has_usable_key(&self, node: &LlmNode) -> bool {
        let rt = self.runtime.lock().await;
        node.keys.iter().any(|k| {
            rt.get(&k.id)
                .map(|r| r.usable(k.enabled))
                .unwrap_or_else(|| k.is_usable_at(unix_now()))
        })
    }

    /// 挑一把可用 key：最久未用优先（轮询公平），再比 `sort_order`。
    pub async fn pick(&self, node: &LlmNode) -> Option<NodeKey> {
        let rt = self.runtime.lock().await;
        let now = unix_now();
        let mut candidates: Vec<&NodeKey> = node
            .keys
            .iter()
            .filter(|k| {
                rt.get(&k.id)
                    .map(|r| r.usable(k.enabled))
                    .unwrap_or_else(|| k.is_usable_at(now))
            })
            .collect();
        candidates.sort_by(|a, b| {
            a.last_used_at
                .unwrap_or(0)
                .cmp(&b.last_used_at.unwrap_or(0))
                .then(a.sort_order.cmp(&b.sort_order))
        });
        candidates.first().map(|k| (*k).clone())
    }

    pub async fn mark_used(&self, key_id: i64) {
        let mut rt = self.runtime.lock().await;
        if let Some(r) = rt.get_mut(&key_id) {
            r.window_started_at = Some(Instant::now());
        }
    }

    /// 记一次实际消耗（结算时调用），并顺带刷新软上限判定。
    pub async fn add_usage(&self, key: &NodeKey, tokens: i64, now: Instant) {
        let mut rt = self.runtime.lock().await;
        let e = rt
            .entry(key.id)
            .or_insert_with(|| KeyRuntime::from_key(key));
        window::bump(e, key, tokens, now);
    }

    /// 把 key 拉进冷却 / 隔离。**冷却状态落库**，
    /// 这样容器重启后不会立刻再撞一次同样的限额。
    pub async fn apply_verdict(
        &self,
        key: &NodeKey,
        verdict: &QuotaVerdict,
        request_id: Option<&str>,
    ) {
        let now_unix = unix_now();

        let (cooldown_secs, state) = match verdict.class {
            QuotaClass::LongQuota => {
                let secs = verdict
                    .reset_at
                    .map(|t| (t - now_unix).max(0) as u64)
                    .unwrap_or(DEFAULT_COOLDOWN.as_secs());
                (secs, NodeKeyState::Cooling)
            }
            // KeyDead 是永久的：401/403/泄露/欠费，人工介入才恢复
            QuotaClass::KeyDead => (0, NodeKeyState::Quarantined),
            // Transient 不该走到这里；真走到了也只是短暂冷却
            QuotaClass::Transient => (
                verdict
                    .reset_at
                    .map(|t| (t - now_unix).max(0) as u64)
                    .unwrap_or(30),
                NodeKeyState::Cooling,
            ),
        };

        let cooldown_until = if matches!(state, NodeKeyState::Cooling) {
            Some(now_unix + cooldown_secs as i64)
        } else {
            None
        };

        {
            let mut rt = self.runtime.lock().await;
            let e = rt
                .entry(key.id)
                .or_insert_with(|| KeyRuntime::from_key(key));
            e.state = state;
            e.cooldown_until = cooldown_until.map(|t| {
                Instant::now() + Duration::from_secs((t - now_unix).max(0) as u64)
            });
        }

        // 持久化：状态 + 判定依据 + 冷却截止时刻
        let _ = sqlx::query(
            "UPDATE llm_node_key
             SET state = ?, cooldown_until = ?, reset_source = ?, quota_class = ?,
                 matched_rule = ?, matched_signal = ?, count_429 = count_429 + ?,
                 count_rotations = count_rotations + ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(cooldown_until)
        .bind(verdict.reset_source.as_str())
        .bind(verdict.class.as_str())
        .bind(verdict.rule)
        .bind(&verdict.signal)
        .bind(if matches!(verdict.class, QuotaClass::Transient) { 1 } else { 0 })
        .bind(if matches!(verdict.class, QuotaClass::LongQuota | QuotaClass::KeyDead) {
            1
        } else {
            0
        })
        .bind(now_unix)
        .bind(key.id)
        .execute(&self.pool)
        .await;

        // 审计：轮换决策必须事后可查
        if matches!(verdict.class, QuotaClass::LongQuota | QuotaClass::KeyDead) {
            let _ = sqlx::query(
                "INSERT INTO key_rotation_event
                   (key_id, node_id, request_id, quota_class, matched_rule, matched_signal,
                    cooldown_ms, reset_source, created_at)
                 VALUES (?,?,?,?,?,?,?,?,?)",
            )
            .bind(key.id)
            .bind(key.node_id)
            .bind(request_id)
            .bind(verdict.class.as_str())
            .bind(verdict.rule)
            .bind(&verdict.signal)
            .bind((cooldown_secs * 1000) as i64)
            .bind(verdict.reset_source.as_str())
            .bind(now_unix)
            .execute(&self.pool)
            .await;
        }
    }

    /// 手工解除冷却（管理页面用）。
    pub async fn clear_cooldown(&self, key_id: i64) -> ApiResult2<()> {
        let now = unix_now();
        sqlx::query(
            "UPDATE llm_node_key
             SET state='active', cooldown_until=NULL, quota_class=NULL,
                 matched_rule=NULL, matched_signal=NULL, reset_source=NULL, updated_at=?
             WHERE id=?",
        )
        .bind(now)
        .bind(key_id)
        .execute(&self.pool)
        .await?;

        let mut rt = self.runtime.lock().await;
        if let Some(e) = rt.get_mut(&key_id) {
            e.state = NodeKeyState::Active;
            e.cooldown_until = None;
        }
        Ok(())
    }

    /// 全部 key 耗尽时的恢复时刻（取最早）。给 503 响应体用。
    pub async fn earliest_recovery(&self, node: &LlmNode) -> Option<i64> {
        let rt = self.runtime.lock().await;
        let now = unix_now();
        let mut times: Vec<i64> = node
            .keys
            .iter()
            .filter_map(|k| {
                rt.get(&k.id).and_then(|r| r.cooldown_until).map(|i| {
                    now + i.saturating_duration_since(Instant::now()).as_secs() as i64
                })
            })
            .collect();
        times.sort_unstable();
        times.first().copied()
    }
}

type ApiResult2<T> = Result<T, crate::error::ApiError>;

/// 节点级限额作用域：`account` 时**不轮换**，原地等。
pub fn should_rotate(scope: RateLimitScope, class: QuotaClass) -> bool {
    if scope == RateLimitScope::Account {
        return false;
    }
    matches!(class, QuotaClass::LongQuota | QuotaClass::KeyDead)
}
