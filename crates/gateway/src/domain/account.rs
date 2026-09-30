//! 账号：API Key、余额、并发额度。

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
    pub llm_max_concurrency: i64,
    pub llm_max_queue: i64,
    pub image_max_concurrency: i64,
    pub image_max_queue: i64,
    pub rpm_limit: Option<i64>,
}

impl Account {
    /// 真正可再预扣的额度。
    pub fn available_micro(&self) -> i64 {
        self.balance_micro - self.held_micro
    }

    pub fn max_concurrency(&self, kind: ResourceKind) -> i64 {
        match kind {
            ResourceKind::Llm => self.llm_max_concurrency,
            ResourceKind::Image => self.image_max_concurrency,
        }
    }

    pub fn max_queue(&self, kind: ResourceKind) -> i64 {
        match kind {
            ResourceKind::Llm => self.llm_max_queue,
            ResourceKind::Image => self.image_max_queue,
        }
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
