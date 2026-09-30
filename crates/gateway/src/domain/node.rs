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

    /// 请求候选地址，**内网优先**。
    ///
    /// 家里的机器之间走局域网：外网要绕运营商再绕回来，家里宽带的上行
    /// 通常远小于局域网，那一跳能把流式首字延迟拖到没法用。
    ///
    /// `lan_down` 为真时跳过内网——否则每次请求都要先付一次连接失败的钱。
    /// 返回值按优先级排，调用方**只在连接级失败**时才试下一个；
    /// 连上了但答错（限额、模型没加载、内容错误）说明地址是通的，
    /// 换地址没有意义。
    pub fn base_urls(&self, lan_down: bool) -> Vec<String> {
        let lan = self
            .lan_base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut out = Vec::with_capacity(2);
        if let Some(l) = lan.filter(|_| !lan_down) {
            if l != self.base_url {
                out.push(l.to_string());
            }
        }
        out.push(self.base_url.clone());
        out
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

#[cfg(test)]
mod base_url_tests {
    use super::*;

    fn node(base: &str, lan: Option<&str>) -> LlmNode {
        LlmNode {
            id: 1,
            name: "n1".into(),
            kind: "lmstudio".into(),
            base_url: base.into(),
            lan_base_url: lan.map(str::to_string),
            max_concurrency: 2,
            default_max_output_tokens: None,
            enabled: true,
            sort_order: 0,
            extra_headers: Value::Null,
            extra_body: Value::Null,
            window: Default::default(),
            keys: vec![],
        }
    }

    /// 内网优先是这一整段存在的理由：顺序反了就等于没做。
    #[test]
    fn lan_comes_first() {
        let n = node("http://pub:7001/v1", Some("http://192.168.50.197:1234/v1"));
        assert_eq!(
            n.base_urls(false),
            vec!["http://192.168.50.197:1234/v1", "http://pub:7001/v1"]
        );
    }

    /// 内网刚判不可达，这一段时间内不该每个请求都先撞一次墙。
    #[test]
    fn lan_down_skips_straight_to_public() {
        let n = node("http://pub:7001/v1", Some("http://192.168.50.197:1234/v1"));
        assert_eq!(n.base_urls(true), vec!["http://pub:7001/v1"]);
    }

    #[test]
    fn no_lan_configured_is_just_the_public_url() {
        let n = node("http://pub:7001/v1", None);
        assert_eq!(n.base_urls(false), vec!["http://pub:7001/v1"]);
        // 空串等同于没配
        let n2 = node("http://pub:7001/v1", Some("   "));
        assert_eq!(n2.base_urls(false), vec!["http://pub:7001/v1"]);
    }

    /// 两个地址填成一样时不能试两遍同一个地址。
    #[test]
    fn identical_lan_and_public_are_deduped() {
        let n = node("http://same:1234/v1", Some("http://same:1234/v1"));
        assert_eq!(n.base_urls(false), vec!["http://same:1234/v1"]);
    }
}
