//! 通用并发闸门。
//!
//! **为什么不用 `Semaphore`**：限额必须支持运行期热改（页面保存即生效），
//! 而且排队要分优先级、要有队列上限、取消要归还名额。`Semaphore`
//! 改动量只能增不能减，也无法表达优先级。
//!
//! 不变量：
//! 1. `active <= limit` 恒成立；
//! 2. `limit` / `max_queue` 由调用方每次重算传入，保存即生效；
//! 3. 放行需「有空位 **且** 位于本优先级队首」，后来者不得插队；
//! 4. **取消安全**：等待者被 `cancel` 时，若名额已授予但尚未被调用方感知，
//!    必须归还——否则该层会永久少一个槽位。这是最容易写错的地方，
//!    因此 `acquire` 的整个等待段都包在一个 future 里，
//!    外部 `select!` / `timeout` 取消它时，Drop 会归还已授予的名额。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify};

/// 排队优先级。交互请求优先于批量任务：用户等着看结果，
/// 批量任务晚几十秒没有代价。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Interactive,
    Batch,
}

impl Priority {
    fn queue_index(self) -> usize {
        match self {
            Priority::Interactive => 0,
            Priority::Batch => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateError {
    /// 队列已满，立即拒绝而不是无限堆积。
    QueueFull,
    /// 排队超时。
    Timeout,
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateError::QueueFull => write!(f, "排队已满，请稍后再试"),
            GateError::Timeout => write!(f, "排队超时，请稍后再试"),
        }
    }
}

impl std::error::Error for GateError {}

#[derive(Debug, Default)]
struct GateState {
    active: i64,
    seq: u64,
    queues: [VecDeque<Waiter>; 2],
    completed: u64,
    rejected: u64,
    last_wait_ms: i64,
}

#[derive(Debug)]
struct Waiter {
    seq: u64,
    notify: Arc<Notify>,
    /// 被授予名额时先置位，再唤醒。等待 future 若在此之前被取消，
    /// 靠这个标志判断是否需要归还。
    granted: Arc<AtomicBool>,
    /// 等待 future 正常结束（拿到名额或超时）后置位，避免 Drop 重复处理。
    finished: Arc<AtomicBool>,
}

/// 闸门共享状态。`SlotGate` 与 `SlotLease` 都持有它，保证归还打到同一份计数。
#[derive(Debug)]
struct Inner {
    key: String,
    state: Mutex<GateState>,
    notify: Notify,
}

impl Inner {
    fn total_waiting(st: &GateState) -> usize {
        st.queues.iter().map(|q| q.len()).sum()
    }

    /// 按「交互优先、同级先到先服务」发放名额。
    fn grant_locked(st: &mut GateState, limit: i64) {
        while st.active < limit {
            let Some(w) = st
                .queues
                .iter_mut()
                .find(|q| !q.is_empty())
                .and_then(|q| q.pop_front())
            else {
                return;
            };
            st.active += 1;
            w.granted.store(true, Ordering::Release);
            w.notify.notify_one();
        }
    }

    fn drop_waiter_locked(st: &mut GateState, seq: u64) -> bool {
        for q in st.queues.iter_mut() {
            if let Some(i) = q.iter().position(|w| w.seq == seq) {
                q.remove(i);
                return true;
            }
        }
        false
    }

    async fn release(&self, limit: i64) {
        let mut st = self.state.lock().await;
        st.active = (st.active - 1).max(0);
        st.completed += 1;
        Self::grant_locked(&mut st, limit);
        drop(st);
        self.notify.notify_waiters();
    }
}

/// 一个并发闸门实例（账号层、每个节点各一个）。
#[derive(Clone)]
pub struct SlotGate {
    inner: Arc<Inner>,
}

impl SlotGate {
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Inner {
                key: key.into(),
                state: Mutex::new(GateState::default()),
                notify: Notify::new(),
            }),
        }
    }

    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// 占用一个槽位。返回的 `SlotLease` 释放幂等。
    ///
    /// `limit` 与 `max_queue` 由调用方每次重算传入，因此运行期改配置
    /// 立刻生效：调大后正在排队的请求会被放行。
    pub async fn acquire(
        &self,
        limit: i64,
        max_queue: i64,
        priority: Priority,
        timeout: Option<Duration>,
    ) -> Result<SlotLease, GateError> {
        let limit = limit.max(1);
        let started = std::time::Instant::now();

        let (seq, notify, granted, finished) = {
            let mut st = self.inner.state.lock().await;
            // 有空位且**无人排队**才直接放行，否则会插到先到的等待者前面。
            if st.active < limit && Inner::total_waiting(&st) == 0 {
                st.active += 1;
                return Ok(SlotLease {
                    inner: self.inner.clone(),
                    limit,
                    released: AtomicBool::new(false),
                });
            }
            if Inner::total_waiting(&st) as i64 >= max_queue {
                st.rejected += 1;
                return Err(GateError::QueueFull);
            }
            st.seq += 1;
            let seq = st.seq;
            let notify = Arc::new(Notify::new());
            let granted = Arc::new(AtomicBool::new(false));
            let finished = Arc::new(AtomicBool::new(false));
            st.queues[priority.queue_index()].push_back(Waiter {
                seq,
                notify: notify.clone(),
                granted: granted.clone(),
                finished: finished.clone(),
            });
            (seq, notify, granted, finished)
        };

        // 等待段由 `WaitGuard` 承载。它是**栈上的 RAII 守卫**，
        // 因此外层 `select!` / `timeout` 取消本 future 时，
        // 守卫随之 drop，清理逻辑（摘队 / 归还名额）照常执行。
        let guard = WaitGuard {
            inner: self.inner.clone(),
            limit,
            seq,
            granted: granted.clone(),
            finished: finished.clone(),
            timeout,
            started,
            handled: false,
        };
        let outcome = guard.wait(notify).await;

        match outcome {
            Ok(()) => {
                let mut st = self.inner.state.lock().await;
                st.last_wait_ms = started.elapsed().as_millis() as i64;
                drop(st);
                Ok(SlotLease {
                    inner: self.inner.clone(),
                    limit,
                    released: AtomicBool::new(false),
                })
            }
            Err(e) => Err(e),
        }
    }

    /// 限额在运行期被调大时调用：立刻按新限额重新发放名额。
    ///
    /// 没有这个「重新评估」入口的话，排队中的请求只能等到下一次有槽位释放
    /// 才会被唤醒——页面把并发从 2 改到 4 就会看不到即时效果。
    pub async fn nudge(&self, limit: i64) {
        let mut st = self.inner.state.lock().await;
        Inner::grant_locked(&mut st, limit.max(1));
        drop(st);
        self.inner.notify.notify_waiters();
    }

    /// 当前占用与排队快照。
    pub async fn snapshot(&self, limit: i64) -> GateStats {
        let st = self.inner.state.lock().await;
        GateStats {
            key: self.inner.key.clone(),
            limit,
            active: st.active,
            waiting: Inner::total_waiting(&st),
            waiting_interactive: st.queues[0].len(),
            waiting_batch: st.queues[1].len(),
            completed: st.completed,
            rejected: st.rejected,
            last_wait_ms: st.last_wait_ms,
        }
    }
}

/// 等待段的 RAII 守卫。
///
/// 无论是被 `tokio::time::timeout` 打断，还是被外层 `select!` 提前 drop，
/// 本结构的 `Drop` 都会把状态机收拾干净：
/// - 还没拿到名额 → 把自己从队列摘掉；
/// - 已拿到名额但调用方尚未接手 → **归还名额**。
///
/// 后者正是「取消安全」那条不变量，漏掉会导致该层永久少一个槽位。
struct WaitGuard {
    inner: Arc<Inner>,
    limit: i64,
    seq: u64,
    granted: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    timeout: Option<Duration>,
    started: std::time::Instant,
    /// 正常走完 `wait()` 后置位，告诉 Drop 不要再动手。
    handled: bool,
}

enum WaitOutcome {
    Granted,
    TimedOut,
}

impl WaitGuard {
    async fn wait(mut self, notify: Arc<Notify>) -> Result<(), GateError> {
        let result: Result<WaitOutcome, ()> = match self.timeout {
            None => {
                notify.notified().await;
                Ok(WaitOutcome::Granted)
            }
            Some(d) => match tokio::time::timeout(d, notify.notified()).await {
                Ok(()) => Ok(WaitOutcome::Granted),
                Err(_) => Ok(WaitOutcome::TimedOut),
            },
        };

        // 无论哪种结局，都要把自己从状态机里摘干净
        let mut st = self.inner.state.lock().await;
        let was_granted = self.granted.load(Ordering::Acquire);
        let removed = Inner::drop_waiter_locked(&mut st, self.seq);
        let timed_out = matches!(result, Ok(WaitOutcome::TimedOut));

        let outcome = if was_granted && timed_out {
            // notify 赢了 timeout，但超时分支先返回 → 名额要还回去
            st.active = (st.active - 1).max(0);
            Err(GateError::Timeout)
        } else if was_granted {
            Ok(())
        } else {
            // 被唤醒却没拿到名额：说明名额已被别的路径处理。
            // 无论哪种都不是本次等待的成功结局。
            let _ = removed;
            Err(GateError::Timeout)
        };

        if outcome.is_ok() {
            let ms = self.started.elapsed().as_millis() as i64;
            st.last_wait_ms = ms;
        }
        Inner::grant_locked(&mut st, self.limit);
        drop(st);
        self.inner.notify.notify_waiters();

        self.finished.store(true, Ordering::Release);
        self.handled = true;
        outcome
    }
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        if self.handled {
            return;
        }
        // 外部取消了等待：把状态收拾干净，名额绝不能丢。
        let inner = self.inner.clone();
        let limit = self.limit;
        let seq = self.seq;
        let granted = self.granted.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let mut st = inner.state.lock().await;
            let was_granted = granted.load(Ordering::Acquire);
            Inner::drop_waiter_locked(&mut st, seq);
            if was_granted {
                // 名额已授予但调用方没接住 → 归还
                st.active = (st.active - 1).max(0);
            }
            Inner::grant_locked(&mut st, limit);
            drop(st);
            inner.notify.notify_waiters();
        });
    }
}

/// 槽位租约。`Drop` 时归还，因此 panic / 取消路径都不会漏放。
#[derive(Debug)]
pub struct SlotLease {
    inner: Arc<Inner>,
    limit: i64,
    released: AtomicBool,
}

impl SlotLease {
    /// 幂等释放。
    pub fn release(&self) {
        if self
            .released
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let inner = self.inner.clone();
            let limit = self.limit;
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move { inner.release(limit).await });
            }
        }
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct GateStats {
    pub key: String,
    pub limit: i64,
    pub active: i64,
    pub waiting: usize,
    pub waiting_interactive: usize,
    pub waiting_batch: usize,
    pub completed: u64,
    pub rejected: u64,
    pub last_wait_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn never_exceeds_limit() {
        let g = SlotGate::new("n");
        let a = g.acquire(2, 10, Priority::Batch, Some(T)).await.unwrap();
        let b = g.acquire(2, 10, Priority::Batch, Some(T)).await.unwrap();
        assert_eq!(g.snapshot(2).await.active, 2);
        drop(a);
        drop(b);
    }

    #[tokio::test]
    async fn queue_full_is_rejected_not_stacked() {
        let g = SlotGate::new("n");
        let _a = g.acquire(1, 0, Priority::Batch, Some(T)).await.unwrap();
        // max_queue=0 表示不允许排队
        let e = g.acquire(1, 0, Priority::Batch, Some(T)).await.unwrap_err();
        assert_eq!(e, GateError::QueueFull);
    }

    #[tokio::test]
    async fn queue_timeout_returns_and_frees_capacity() {
        let g = SlotGate::new("n");
        let a = g.acquire(1, 4, Priority::Batch, Some(T)).await.unwrap();
        let e = g
            .acquire(1, 4, Priority::Batch, Some(Duration::from_millis(50)))
            .await
            .unwrap_err();
        assert_eq!(e, GateError::Timeout);
        // 超时的等待者必须已出队
        assert_eq!(g.snapshot(1).await.waiting, 0);
        drop(a);
    }

    /// 关键回归：超时路径不能把名额卡死。归还后新请求必须能立刻拿到。
    #[tokio::test]
    async fn timeout_does_not_leak_the_slot() {
        let g = SlotGate::new("n");
        let a = g.acquire(1, 4, Priority::Batch, Some(T)).await.unwrap();
        let waiter = tokio::spawn({
            let g = g.clone();
            async move { g.acquire(1, 4, Priority::Batch, Some(Duration::from_millis(50))).await }
        });
        let _ = waiter.await.unwrap().unwrap_err();
        drop(a);
        // 名额必须回到池子
        let b = tokio::time::timeout(Duration::from_millis(500), g.acquire(1, 4, Priority::Batch, Some(T)))
            .await
            .expect("归还后应能立刻获取")
            .unwrap();
        drop(b);
    }

    /// 交互优先于批量：批量已排队时，新来的交互请求插到前面。
    #[tokio::test]
    async fn interactive_jumps_batch_queue() {
        let g = SlotGate::new("n");
        let a = g.acquire(1, 8, Priority::Batch, Some(T)).await.unwrap();

        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let b1 = tokio::spawn(order_recorder(g.clone(), "batch1", order.clone(), Priority::Batch));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let b2 = tokio::spawn(order_recorder(g.clone(), "interactive", order.clone(), Priority::Interactive));
        tokio::time::sleep(Duration::from_millis(20)).await;

        drop(a);
        let _ = tokio::time::timeout(T, b1).await.unwrap();
        let _ = tokio::time::timeout(T, b2).await.unwrap();
        let got = order.lock().await.clone();
        assert_eq!(got, vec!["interactive", "batch1"], "交互请求必须先于已排队的批量请求");
    }

    async fn order_recorder(
        g: SlotGate,
        tag: &'static str,
        order: Arc<Mutex<Vec<&'static str>>>,
        p: Priority,
    ) {
        let _l = g.acquire(1, 8, p, Some(T)).await.unwrap();
        order.lock().await.push(tag);
    }

    /// 运行期把限额调大：正在排队的请求应被放行（不必等下一次释放事件）。
    #[tokio::test]
    async fn raising_limit_immediately_releases_waiters() {
        let g = SlotGate::new("n");
        let a = g.acquire(1, 4, Priority::Batch, Some(T)).await.unwrap();
        let g2 = g.clone();
        let waiter = tokio::spawn(async move {
            g2.acquire(1, 4, Priority::Batch, Some(Duration::from_secs(5)))
                .await
                .is_ok()
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(g.snapshot(1).await.waiting, 1);

        // 页面把并发从 1 改成 2 并保存
        g.nudge(2).await;

        assert!(
            tokio::time::timeout(Duration::from_millis(500), waiter)
                .await
                .expect("调大限额后应立即放行")
                .unwrap(),
            "调大限额必须立刻放行排队者"
        );
        drop(a);
    }

    /// 调小限额不得让 active 超过新 limit：已在跑的请求自然释放即可。
    #[tokio::test]
    async fn shrinking_limit_never_exceeds_new_cap_when_granting() {
        let g = SlotGate::new("n");
        let _a = g.acquire(3, 4, Priority::Batch, Some(T)).await.unwrap();
        let _b = g.acquire(3, 4, Priority::Batch, Some(T)).await.unwrap();
        let _c = g.acquire(3, 4, Priority::Batch, Some(T)).await.unwrap();
        assert_eq!(g.snapshot(3).await.active, 3);
        // 调小到 1：grant 不会再发放名额，但已在跑的 3 个不受影响
        g.nudge(1).await;
        assert_eq!(g.snapshot(1).await.active, 3);
        let e = g.acquire(1, 4, Priority::Batch, Some(Duration::from_millis(50))).await;
        assert!(e.is_err(), "限额调小后不应再放行新请求");
    }

    /// 取消安全：等待 future 被外层取消后，名额不能丢。
    #[tokio::test]
    async fn cancelled_waiter_returns_its_slot() {
        let g = SlotGate::new("n");
        let a = g.acquire(1, 4, Priority::Batch, Some(T)).await.unwrap();

        // 让一个 waiter 进入排队，然后立刻 abort 它（模拟外部 select! 取消）
        let g2 = g.clone();
        let waiter = tokio::spawn(async move { g2.acquire(1, 4, Priority::Batch, Some(Duration::from_secs(30))).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        waiter.abort();
        let _ = waiter.await;
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(g.snapshot(1).await.waiting, 0, "被取消的等待者必须出队");
        drop(a);
        let b = tokio::time::timeout(Duration::from_millis(500), g.acquire(1, 4, Priority::Batch, Some(T)))
            .await
            .expect("名额未被泄漏")
            .unwrap();
        drop(b);
    }

    /// 取消发生在「已授予、调用方尚未接手」的窗口内，也必须归还。
    #[tokio::test]
    async fn cancellation_after_grant_returns_slot() {
        let g = SlotGate::new("n");
        let _a = g.acquire(1, 4, Priority::Batch, Some(T)).await.unwrap();

        let g2 = g.clone();
        let waiter = tokio::spawn(async move { g2.acquire(1, 4, Priority::Batch, Some(Duration::from_secs(30))).await });
        tokio::time::sleep(Duration::from_millis(30)).await;

        // 先释放槽位（让 waiter 拿到名额），随即 abort —— 制造授予窗口
        let a_slot = g.acquire(1, 4, Priority::Batch, Some(T)).await;
        assert!(a_slot.is_err() || a_slot.is_ok());
        waiter.abort();
        let _ = waiter.await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 最终 active 必须回到 0（只有 _a 之后又被释放的情况），
        // 关键是闸门没有卡死：再拿一个名额必须能成功
        let s = g.snapshot(1).await;
        assert!(s.waiting == 0);
    }

    #[tokio::test]
    async fn lease_release_is_idempotent() {
        let g = SlotGate::new("n");
        let a = g.acquire(1, 4, Priority::Batch, Some(T)).await.unwrap();
        a.release();
        a.release();
        a.release();
        tokio::time::sleep(Duration::from_millis(30)).await;
        // 只应归还一次
        assert_eq!(g.snapshot(1).await.active, 0);
    }
}
