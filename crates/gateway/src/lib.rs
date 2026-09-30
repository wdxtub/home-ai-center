//! 家庭内部 AI 网关。
//!
//! 一次请求要穿过这条链路：
//! **鉴权 → 定价 → 预扣 → 账号闸门 → 节点闸门 → 上游 → 结算**。
//! 三种客户端协议共用它，差别只在入站解析与出站渲染。

pub mod api;
pub mod billing;
pub mod config;
pub mod db;
pub mod domain;
pub mod error;
pub mod gate;
pub mod keys;
pub mod protocol;
pub mod state;
pub mod upstream;
