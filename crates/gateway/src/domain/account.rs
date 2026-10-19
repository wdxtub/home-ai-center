//! 账号：余额。
//!
//! **并发额度不在这里** —— 它在 [`super::api_key::ApiKey`] 上。
//! 余额是账号的（同一个账号的 key 共用一份），速率是每把 key 的（各算各的）。
//! key 曾经也是这里的一列，那时一个账号只能有一把 key，无法按 key 限流。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    /// 可用余额（微元）。已扣除 `held_micro`。
    pub balance_micro: i64,
    /// 在途预扣（微元）。准入时加、结算时减。
    pub held_micro: i64,
}

impl Account {
    /// 真正可再预扣的额度。
    pub fn available_micro(&self) -> i64 {
        self.balance_micro - self.held_micro
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Llm,
    Image,
}

impl ResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Llm => "llm",
            ResourceKind::Image => "image",
        }
    }
}
