//! ComfyUI 出图工作流：服务端模板（默认）与原始 JSON 两条通道。

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMode {
    /// 服务端维护命名工作流，客户端只传参数；占位符 `{{prompt}}` / `{{width|1024}}`。
    Template,
    /// 客户端直接提交完整 workflow JSON。
    Raw,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: i64,
    pub name: String,
    pub mode: WorkflowMode,
    /// 该工作流可用的 ComfyUI 端点（空 = 全部启用端点）。
    pub node_ids: Vec<i64>,
    /// ComfyUI API 格式的 workflow JSON（模板模式下含占位符）。
    pub comfy_workflow: Value,
    /// 允许注入的参数字段名。
    pub param_slots: Vec<String>,
    pub price_micro: i64,
    pub enabled: bool,
    pub archive: bool,
    pub archive_retention_days: Option<i64>,
}
