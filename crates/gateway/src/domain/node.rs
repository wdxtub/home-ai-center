//! LLM 节点、上游 key 池与模型路由。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::window::DisabledWindow;

/// 节点承载**硬件属性**：并发度、可用时段、地址。模型映射是另一层。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmNode {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub lan_base_url: Option<String>,
    pub max_concurrency: i64,
    pub default_max_output_tokens: Option<i64>,
    pub enabled: bool,
    pub sort_order: i64,
    pub extra_headers: Value,
    pub extra_body: Value,
    pub window: DisabledWindow,
    /// 该节点的全量 key 池。key 可用性是「节点能否被调度」的一部分。
    pub keys: Vec<NodeKey>,
}

impl LlmNode {
    /// 至少有一把 key 可用（未冷却、未隔离、启用、未超软上限）。
    /// 由 `keys::KeyRing` 在运行时判定，这里只做静态筛选。
    pub fn enabled_keys(&self, now: i64) -> Vec<&NodeKey> {
        self.keys
            .iter()
            .filter(|k| k.is_usable_at(now))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeKeyState {
    Active,
    Cooling,
    Quarantined,
    Disabled,
}

impl NodeKeyState {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKeyState::Active => "active",
            NodeKeyState::Cooling => "cooling",
            NodeKeyState::Quarantined => "quarantined",
            NodeKeyState::Disabled => "disabled",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "cooling" => NodeKeyState::Cooling,
            "quarantined" => NodeKeyState::Quarantined,
            "disabled" => NodeKeyState::Disabled,
            _ => NodeKeyState::Active,
        }
    }
}

/// 限额作用域。
///
/// `perKey`（默认）：每把 key 有独立限额桶，轮换有效——用户是不同组织 / 订阅账号。
/// `account`：同一账号下的多把 key 共用一个桶，**429 原地等、不轮换**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RateLimitScope {
    PerKey,
    Account,
}

impl RateLimitScope {
    /// 词表必须与 migration 里 `CHECK (rate_limit_scope IN (...))` 一致。
    /// 写库路径一律走这里，不要在 handler 里手写字面量。
    pub fn as_str(self) -> &'static str {
        match self {
            RateLimitScope::PerKey => "perKey",
            RateLimitScope::Account => "account",
        }
    }

    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("account") {
            RateLimitScope::Account
        } else {
            RateLimitScope::PerKey
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeKey {
    pub id: i64,
    pub node_id: i64,
    pub label: String,
    /// 明文（API 层只回 label + 末 4 位）。
    pub secret: String,
    pub sort_order: i64,
    pub enabled: bool,
    pub state: NodeKeyState,
    pub cooldown_until: Option<i64>,
    pub reset_source: Option<String>,
    pub quota_class: Option<String>,
    pub matched_rule: Option<String>,
    pub matched_signal: Option<String>,
    pub rate_limit_scope: RateLimitScope,
    pub soft_cap_window_ms: Option<i64>,
    pub soft_cap_tokens: Option<i64>,
    pub window_started_at: Option<i64>,
    pub window_tokens_used: i64,
    pub count_429: i64,
    pub count_rotations: i64,
    pub count_false_positive: i64,
    pub last_used_at: Option<i64>,

    /// 运行时的软上限判断结果，由 `keys::window` 计算后回填。
    pub soft_cap_reached: bool,
}

impl NodeKey {
    pub fn tail4(&self) -> &str {
        let n = self.secret.len();
        if n <= 4 {
            &self.secret
        } else {
            &self.secret[n - 4..]
        }
    }

    /// 这把 key 现在能不能用。
    ///
    /// 冷却到期**自动恢复**——存的是绝对时间戳，进程重启后语义依然正确。
    pub fn is_usable_at(&self, now: i64) -> bool {
        if !self.enabled || self.state == NodeKeyState::Disabled || self.state == NodeKeyState::Quarantined
        {
            return false;
        }
        if let Some(until) = self.cooldown_until {
            if until > now {
                return false;
            }
        }
        !self.soft_cap_reached
    }
}

/// 模型名 → 节点。多对多：一个节点可服务多个模型，一个模型可落到多个节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRoute {
    pub id: i64,
    pub model_name: String,
    pub node_id: i64,
    /// 实际发给上游的模型名（可能与对外暴露名不同）。
    pub upstream_model: String,
    pub enabled: bool,
    pub priority: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(state: NodeKeyState, cooldown_until: Option<i64>) -> NodeKey {
        NodeKey {
            id: 1,
            node_id: 1,
            label: "k".into(),
            secret: "sk-secret-abcd".into(),
            sort_order: 0,
            enabled: true,
            state,
            cooldown_until,
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

    #[test]
    fn tail4_hides_secret_but_keeps_last_four() {
        assert_eq!(key(NodeKeyState::Active, None).tail4(), "abcd");
    }

    #[test]
    fn cooling_key_is_unusable_until_cooldown_expires() {
        assert!(!key(NodeKeyState::Cooling, Some(1000)).is_usable_at(999));
        // 冷却到期自动恢复——这是「冷却状态持久化」能跨重启的原因
        assert!(key(NodeKeyState::Cooling, Some(1000)).is_usable_at(1000));
    }

    #[test]
    fn quarantined_key_never_recovers_on_its_own() {
        assert!(!key(NodeKeyState::Quarantined, Some(1000)).is_usable_at(99999));
    }

    #[test]
    fn soft_cap_reached_key_is_skipped() {
        let mut k = key(NodeKeyState::Active, None);
        k.soft_cap_reached = true;
        assert!(!k.is_usable_at(0));
    }

    #[test]
    fn node_with_no_usable_key_is_not_schedulable() {
        let node = LlmNode {
            id: 1,
            name: "n1".into(),
            kind: "lmstudio".into(),
            base_url: "http://a".into(),
            lan_base_url: None,
            max_concurrency: 2,
            default_max_output_tokens: None,
            enabled: true,
            sort_order: 0,
            extra_headers: Value::Null,
            extra_body: Value::Null,
            window: DisabledWindow::default(),
            keys: vec![
                key(NodeKeyState::Cooling, Some(999_999)),
                key(NodeKeyState::Quarantined, None),
            ],
        };
        assert!(node.enabled_keys(0).is_empty());
    }
}
