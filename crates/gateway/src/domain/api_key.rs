//! 调用方的 API Key。
//!
//! Key 曾经是 `account` 表的一列，于是「同账号多把 key 各自限流」在结构上做不到。
//! 现在它是一张独立的表：**并发额度在 key 上，余额在 account 上**。
//!
//! 判据是「钱和频率是两件事」：余额是账号的（共用一份），速率是这把 key 的
//! （各算各的）。建 3 把 key 就是 3 份并发 —— 账号层面**不再**单独设限，
//! 两层闸门会配出「key 限 100、账号总 20」这种没人看得懂的拒绝。

use serde::{Deserialize, Serialize};

use super::account::ResourceKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: i64,
    pub account_id: i64,
    /// 给人看的名字，同账号内唯一。
    pub name: String,
    /// SHA-256。明文只在创建与轮换的响应里出现一次。
    pub key_hash: String,
    /// 前 8 位，供界面辨认，不足以用来鉴权。
    pub key_prefix: String,
    pub enabled: bool,
    /// 每把 key 独立的并发额度。`0` 或 `NULL` 意味着不限。
    pub llm_max_concurrency: i64,
    pub llm_max_queue: i64,
    pub image_max_concurrency: i64,
    pub image_max_queue: i64,
}

impl ApiKey {
    /// 该 key 在这一类资源上的并发上限。`<= 0` 视为不限。
    pub fn max_concurrency(&self, kind: ResourceKind) -> i64 {
        let v = match kind {
            ResourceKind::Llm => self.llm_max_concurrency,
            ResourceKind::Image => self.image_max_concurrency,
        };
        v.max(0)
    }

    pub fn max_queue(&self, kind: ResourceKind) -> i64 {
        let v = match kind {
            ResourceKind::Llm => self.llm_max_queue,
            ResourceKind::Image => self.image_max_queue,
        };
        v.max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(llm: i64, image: i64) -> ApiKey {
        ApiKey {
            id: 1,
            account_id: 1,
            name: "k".into(),
            key_hash: "h".into(),
            key_prefix: "h".into(),
            enabled: true,
            llm_max_concurrency: llm,
            llm_max_queue: 8,
            image_max_concurrency: image,
            image_max_queue: 4,
        }
    }

    #[test]
    fn each_kind_reads_its_own_knob() {
        let k = key(8, 2);
        assert_eq!(k.max_concurrency(ResourceKind::Llm), 8);
        assert_eq!(k.max_concurrency(ResourceKind::Image), 2);
        assert_eq!(k.max_queue(ResourceKind::Llm), 8);
        assert_eq!(k.max_queue(ResourceKind::Image), 4);
    }

    /// 负数额度会传进闸门当成「永远拿不到名额」，队列会一直排到超时。
    /// 归一成 0（不限）比让它表现得像限流更诚实。
    #[test]
    fn negative_quotas_clamp_to_unlimited() {
        let k = key(-1, -5);
        assert_eq!(k.max_concurrency(ResourceKind::Llm), 0);
        assert_eq!(k.max_concurrency(ResourceKind::Image), 0);
    }
}
