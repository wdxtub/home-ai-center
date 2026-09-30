//! 软上限（主动预测）：在**发请求之前**就跳过已达上限的 key，
//! 用户因此完全看不到限额错误。
//!
//! 采用**分桶窗口**而非滑动窗口（`claude-code-router` 的做法）：
//! `window_started_at` 一变就重置，计数保留 `2 × windowMs`。
//! 分桶在 5 小时这个量级上够用，且实现与内存占用都简单得多。
//!
//! 计数在**结算时**累加真实用量，而不是发请求时——发出前无法知道真实消耗。
//! 代价是软上限略微「迟到」一个请求，这正是期望行为。

use std::time::{Duration, Instant};

use crate::domain::node::NodeKey;

use super::KeyRuntime;

/// 分桶窗口保留时长。
const WINDOW_RETENTION_MULTIPLIER: u64 = 2;

/// 推进窗口并累加消耗，随后刷新软上限判定。
pub fn bump(rt: &mut KeyRuntime, key: &NodeKey, tokens: i64, now: Instant) {
    let (Some(window_ms), Some(cap)) = (key.soft_cap_window_ms, key.soft_cap_tokens) else {
        return; // 未配软上限
    };
    let window = Duration::from_millis(window_ms.max(1) as u64);

    // 窗口过期 → 重置
    match rt.window_started_at {
        Some(started) if now.duration_since(started) < window => {}
        _ => {
            rt.window_started_at = Some(now);
            rt.window_tokens_used = 0;
        }
    }
    rt.window_tokens_used += tokens;
    rt.soft_cap_reached = rt.window_tokens_used >= cap;
}

/// 当前是否已因软上限而不可用。
pub fn reached(rt: &KeyRuntime, key: &NodeKey) -> bool {
    if key.soft_cap_tokens.is_none() || key.soft_cap_window_ms.is_none() {
        return false;
    }
    // 窗口已过期 → 重新可用
    if let (Some(started), Some(window_ms)) = (rt.window_started_at, key.soft_cap_window_ms) {
        if Instant::now().duration_since(started) >= Duration::from_millis(window_ms.max(1) as u64) {
            return false;
        }
    }
    rt.soft_cap_reached
}

/// 窗口还剩多少配额，供管理页面画进度条。
pub fn remaining(key: &NodeKey, used: i64) -> Option<i64> {
    key.soft_cap_tokens.map(|cap| (cap - used).max(0))
}

/// 窗口的复用时长（保留 2 个窗口，方便解释跨窗口的在途消耗）。
pub fn retention(key: &NodeKey) -> Option<Duration> {
    key.soft_cap_window_ms
        .map(|ms| Duration::from_millis(ms.max(1) as u64 * WINDOW_RETENTION_MULTIPLIER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::node::{NodeKeyState, RateLimitScope};

    fn key(window_ms: i64, cap: i64) -> NodeKey {
        NodeKey {
            id: 1,
            node_id: 1,
            label: "k".into(),
            secret: "s".into(),
            sort_order: 0,
            enabled: true,
            state: NodeKeyState::Active,
            cooldown_until: None,
            reset_source: None,
            quota_class: None,
            matched_rule: None,
            matched_signal: None,
            rate_limit_scope: RateLimitScope::PerKey,
            soft_cap_window_ms: Some(window_ms),
            soft_cap_tokens: Some(cap),
            window_started_at: None,
            window_tokens_used: 0,
            count_429: 0,
            count_rotations: 0,
            count_false_positive: 0,
            last_used_at: None,
            soft_cap_reached: false,
        }
    }

    fn rt() -> KeyRuntime {
        KeyRuntime {
            cooldown_until: None,
            soft_cap_reached: false,
            window_started_at: None,
            window_tokens_used: 0,
            state: NodeKeyState::Active,
        }
    }

    #[test]
    fn no_soft_cap_configured_never_reaches() {
        // 两个字段都为 None 才叫「没配」
        let mut k = key(1000, 1000);
        k.soft_cap_tokens = None;
        k.soft_cap_window_ms = None;
        let mut r = rt();
        bump(&mut r, &k, 1_000_000, Instant::now());
        assert!(!reached(&r, &k));
    }

    #[test]
    fn usage_accumulates_until_cap() {
        let k = key(3_600_000, 1000);
        let mut r = rt();
        let now = Instant::now();
        bump(&mut r, &k, 400, now);
        assert!(!reached(&r, &k));
        bump(&mut r, &k, 400, now);
        assert!(!reached(&r, &k));
        bump(&mut r, &k, 400, now);
        assert!(reached(&r, &k), "达上限后应提前换 key");
        assert_eq!(r.window_tokens_used, 1200);
    }

    #[test]
    fn new_window_resets_the_counter() {
        let k = key(100, 1000);
        let mut r = rt();
        bump(&mut r, &k, 900, Instant::now());
        assert!(!reached(&r, &k));
        // 窗口过期后再累加 → 从零开始
        std::thread::sleep(Duration::from_millis(120));
        bump(&mut r, &k, 100, Instant::now());
        assert!(!reached(&r, &k), "新窗口内 100 < 1000");
        assert_eq!(r.window_tokens_used, 100);
    }

    #[test]
    fn expired_window_makes_key_usable_again() {
        let k = key(50, 100);
        let mut r = rt();
        bump(&mut r, &k, 500, Instant::now());
        assert!(reached(&r, &k));
        std::thread::sleep(Duration::from_millis(70));
        assert!(!reached(&r, &k), "窗口过期后软上限不再生效");
    }

    #[test]
    fn remaining_and_retention_are_derived() {
        let k = key(1000, 5000);
        assert_eq!(remaining(&k, 1200), Some(3800));
        assert_eq!(remaining(&k, 9000), Some(0));
        assert_eq!(retention(&k), Some(Duration::from_millis(2000)));
    }
}
