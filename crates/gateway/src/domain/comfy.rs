//! ComfyUI 端点与出图工作流的领域模型。
//!
//! 与 LLM 节点分开建模：ComfyUI 没有 key、没有模型路由，
//! 调度维度是**端点并发 + 排队**，故障语义也不同
//! （400 往往是工作流写错，不是节点坏了）。

use serde::{Deserialize, Serialize};

use crate::domain::window::DisabledWindow;

/// 一个 ComfyUI 计算端点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComfyNode {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    /// 内网地址。提交前若内网不通可以改走公网。
    pub lan_base_url: Option<String>,
    pub username: String,
    pub password: String,
    pub max_concurrency: i64,
    pub enabled: bool,
    pub sort_order: i64,
    /// 启用/禁用时间窗。跨午夜按 `start > end` 口径。
    pub window: DisabledWindow,
}

impl ComfyNode {
    /// 出图时优先走内网：外网回环到内网机器会白白多一跳，
    /// 而且家里的宽带上行通常远小于局域网。
    pub fn effective_base_url(&self) -> &str {
        self.lan_base_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.base_url)
    }
}
