//! 并发闸门。
//!
//! 自研 [`SlotGate`] 而非 `tokio::sync::Semaphore`：需要**双优先级队列**
//! （交互请求优先于批处理）、可热改的限额、以及「等不到就快速失败」
//! 而不是无限排队。

pub mod node_gate;
pub mod slot_gate;

pub use node_gate::{NodeHealth, NodePick, NodeScheduler};
pub use slot_gate::{GateError, GateStats, Priority, SlotGate, SlotLease};
