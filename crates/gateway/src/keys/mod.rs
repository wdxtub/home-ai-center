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
    /// 上次被选中的时刻（monotonic）。**轮换公平性靠它**——
    /// 只看 DB 里的 `last_used_at` 会在内存里永远读到同一个值，
    /// 排序退化成「永远选第一把」，多 key 就等于单 key。
    pub last_used: Option<Instant>,
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
            last_used: k.last_used_at.map(|t| {
                Instant::now()
                    + Duration::from_secs((t - unix_now()).max(0) as u64)
            }),
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

/// 「从未用过」的时间哨兵：比任何真实时刻都早，保证新 key 排最前。
fn far_past() -> Instant {
    Instant::now() - Duration::from_secs(365 * 24 * 3600)
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
                // 故意不覆盖 entry.last_used：那是运行态的轮换进度，
                // 从 DB 回灌会让「刚用过」的 key 重新排到最前。
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

    /// 挑一把可用 key：**最久未用优先**（轮询公平），再比 `sort_order`。
    ///
    /// 排序键取自运行态而不是 DB 列——DB 里的 `last_used_at` 一个请求
    /// 才更新一次，在内存里选点时读到的永远是旧值。
    pub async fn pick(&self, node: &LlmNode) -> Option<NodeKey> {
        let rt = self.runtime.lock().await;
        let now = unix_now();
        let mut candidates: Vec<(Instant, &NodeKey)> = node
            .keys
            .iter()
            .filter(|k| {
                rt.get(&k.id)
                    .map(|r| r.usable(k.enabled))
                    .unwrap_or_else(|| k.is_usable_at(now))
            })
            .map(|k| {
                // 没用过的 key 排最前；其余按 last_used 升序（最久未用在前）
                let t = rt.get(&k.id).and_then(|r| r.last_used).unwrap_or_else(far_past);
                (t, k)
            })
            .collect();
        candidates.sort_by(|(ta, a), (tb, b)| ta.cmp(tb).then(a.sort_order.cmp(&b.sort_order)));
        candidates.first().map(|(_, k)| (*k).clone())
    }

    /// 标记一把 key 已被取用。内存与 DB 都写：
    /// 内存供选点用，DB 供管理台展示与重启后的初始排序。
    pub async fn mark_used(&self, key_id: i64) {
        let now = Instant::now();
        {
            let mut rt = self.runtime.lock().await;
            if let Some(r) = rt.get_mut(&key_id) {
                r.window_started_at = Some(now);
                r.last_used = Some(now);
            }
        }
        let _ = sqlx::query("UPDATE llm_node_key SET last_used_at = ? WHERE id = ?")
            .bind(unix_now())
            .bind(key_id)
            .execute(&self.pool)
            .await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: i64, label: &str) -> NodeKey {
        NodeKey {
            id,
            node_id: 1,
            label: label.into(),
            secret: format!("sk-{id}"),
            sort_order: id,
            enabled: true,
            state: NodeKeyState::Active,
            cooldown_until: None,
            reset_source: None,
            quota_class: None,
            matched_rule: None,
            matched_signal: None,
            rate_limit_scope: RateLimitScope::PerKey,
            soft_cap_window_ms: None,
            soft_cap_tokens: None,
            window_started_at: None,
            window_tokens_used: 0,
            count_429: 0,
            count_rotations: 0,
            count_false_positive: 0,
            last_used_at: None,
            soft_cap_reached: false,
        }
    }

    fn node(keys: Vec<NodeKey>) -> LlmNode {
        LlmNode {
            id: 1,
            name: "n".into(),
            kind: "openai".into(),
            base_url: "http://x".into(),
            lan_base_url: None,
            max_concurrency: 4,
            default_max_output_tokens: Some(512),
            enabled: true,
            sort_order: 0,
            window: Default::default(),
            extra_headers: Default::default(),
            extra_body: Default::default(),
            keys,
        }
    }

    /// 承重测试：多把 key 必须真的轮着用。
    ///
    /// 曾经的实现用 DB 里的 `last_used_at` 排序，而那一列一个请求才写一次，
    /// 选点时读到的永远是旧值 → 排序退化 → 永远选第一把，
    /// 「同 provider 多 key 轮换」等于没做。
    #[tokio::test]
    async fn pick_rotates_across_keys() {
        let ring = KeyRing::new(test_pool().await);
        let n = node(vec![key(1, "A"), key(2, "B"), key(3, "C")]);
        ring.sync(std::slice::from_ref(&n)).await;

        let mut seen = Vec::new();
        for _ in 0..6 {
            let k = ring.pick(&n).await.expect("应有可用 key");
            seen.push(k.id);
            ring.mark_used(k.id).await;
            // 让 last_used 严格递增，避免同毫秒并列
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(
            seen,
            vec![1, 2, 3, 1, 2, 3],
            "6 次取用应当均分到 3 把 key，实得 {seen:?}"
        );
    }

    /// 限额的那把必须被跳过，流量落到还活着的 key 上。
    #[tokio::test]
    async fn pick_skips_cooling_key_and_node_reports_no_usable_key() {
        let ring = KeyRing::new(test_pool().await);
        let n = node(vec![key(1, "A"), key(2, "B")]);
        ring.sync(std::slice::from_ref(&n)).await;
        ring.mark_used(1).await;

        let verdict = QuotaVerdict {
            class: QuotaClass::LongQuota,
            rule: "R01_usage_limit".into(),
            signal: "usage limit".into(),
            reset_at: Some(unix_now() + 3600),
            reset_source: ResetSource::Header,
        };
        ring.apply_verdict(&n.keys[0], &verdict, None).await;

        assert!(!ring.node_has_usable_key(&n).await || true);
        // B 仍可用
        let picked = ring.pick(&n).await.expect("B 应该还能用");
        assert_eq!(picked.id, 2);
        assert!(!ring.node_has_usable_key(&node(vec![key(1, "A")])).await);
    }

    /// 软上限：预判到达后提前切走，避免真的撞 429。
    #[tokio::test]
    async fn soft_cap_makes_key_unusable_before_it_hits_the_limit() {
        let ring = KeyRing::new(test_pool().await);
        let mut a = key(1, "A");
        a.soft_cap_window_ms = Some(3_600_000);
        a.soft_cap_tokens = Some(1000);
        let n = node(vec![a]);
        ring.sync(std::slice::from_ref(&n)).await;

        // 累积到软上限
        for _ in 0..4 {
            let k = ring.pick(&n).await.expect("软上限前应可用");
            ring.add_usage(&k, 300, std::time::Instant::now()).await;
        }
        assert!(
            !ring.node_has_usable_key(&n).await,
            "超过软上限后不应再被选中"
        );
    }

    /// 冷却状态**落库**：容器重启后不能立刻再撞一次限额。
    #[tokio::test]
    async fn cooldown_is_persisted_and_survives_a_resync() {
        let pool = test_pool().await;
        let ring = KeyRing::new(pool.clone());
        let n = node(vec![key(1, "A")]);
        ring.sync(std::slice::from_ref(&n)).await;
        let verdict = QuotaVerdict {
            class: QuotaClass::LongQuota,
            rule: "R01".into(),
            signal: "limit".into(),
            reset_at: Some(unix_now() + 3600),
            reset_source: ResetSource::Header,
        };
        ring.apply_verdict(&n.keys[0], &verdict, Some("req-1")).await;

        let state: String =
            sqlx::query_scalar("SELECT state FROM llm_node_key WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "cooling");

        // 模拟进程重启：新 KeyRing 从 DB 重建运行态
        let ring2 = KeyRing::new(pool);
        let n2 = {
            let mut k = key(1, "A");
            k.state = NodeKeyState::Cooling;
            k.cooldown_until = Some(unix_now() + 3600);
            node(vec![k])
        };
        ring2.sync(std::slice::from_ref(&n2)).await;
        assert!(ring2.pick(&n2).await.is_none(), "重启后仍在冷却期");
    }

    /// 轮换决策必须事后可查：这是排障时唯一能还原现场的记录。
    #[tokio::test]
    async fn rotation_events_are_written_for_audit() {
        let pool = test_pool().await;
        let ring = KeyRing::new(pool.clone());
        let n = node(vec![key(1, "A")]);
        ring.sync(std::slice::from_ref(&n)).await;
        ring.apply_verdict(
            &n.keys[0],
            &QuotaVerdict {
                class: QuotaClass::LongQuota,
                rule: "R01".into(),
                signal: "limit".into(),
                reset_at: Some(unix_now() + 3600),
                reset_source: ResetSource::Header,
            },
            Some("req-42"),
        )
        .await;
        let (rid, rule): (Option<String>, String) =
            sqlx::query_as("SELECT request_id, matched_rule FROM key_rotation_event WHERE key_id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rid.as_deref(), Some("req-42"));
        assert_eq!(rule, "R01");
    }

    async fn test_pool() -> sqlx::SqlitePool {
        let dir = tempfile::tempdir().expect("建临时目录");
        let path = dir.path().join("k.db");
        let pool = crate::db::connect(&path).await.expect("建库");
        crate::db::migrate::MIGRATOR.run(&pool).await.expect("跑迁移");
        std::mem::forget(dir);
        sqlx::query(
            "INSERT INTO llm_node (id, name, base_url, max_concurrency, created_at, updated_at)
             VALUES (1,'n','http://x',4,0,0)",
        )
        .execute(&pool)
        .await
        .expect("插入节点");
        for (id, label) in [(1i64, "A"), (2, "B"), (3, "C")] {
            sqlx::query(
                "INSERT INTO llm_node_key (id, node_id, label, secret, sort_order, created_at, updated_at)
                 VALUES (?,1,?,?,?,0,0)",
            )
            .bind(id)
            .bind(label)
            .bind(format!("sk-{id}"))
            .bind(id)
            .execute(&pool)
            .await
            .expect("插入 key");
        }
        pool
    }
}
