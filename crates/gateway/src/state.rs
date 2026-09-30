//! 进程内配置快照与热更新总线。
//!
//! **配置（节点 / key / 路由 / 定价 / 工作流）以 DB 为唯一事实源**，
//! 运行时读 `ArcSwap<Snapshot>`，读路径零查库。管理端写入在事务提交后
//! 重建快照并广播——保存即生效，无需重启。

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use sqlx::SqlitePool;

use crate::config::Config;
use crate::domain::account::Account;
use crate::domain::node::{LlmNode, LlmRoute};
use crate::domain::pricing::PriceTable;
use crate::domain::window::DisabledWindow;
use crate::domain::workflow::Workflow;
use crate::gate::SlotGate;
use crate::keys::KeyRing;

/// 一份完整的运行时配置。
#[derive(Default)]
pub struct Snapshot {
    pub nodes: Vec<LlmNode>,
    pub routes: Vec<LlmRoute>,
    /// ComfyUI 端点。LLM 与出图的调度维度不同，**不共用一张表**。
    pub comfy_nodes: Vec<crate::domain::comfy::ComfyNode>,
    pub workflows: Vec<Workflow>,
    pub prices: PriceTable,
    pub settings: HashMap<String, serde_json::Value>,
    /// 模型名 → 该模型可用的节点 id（按 priority、id 升序）。
    pub model_routes: HashMap<String, Vec<i64>>,
}

impl Snapshot {
    pub fn node(&self, id: i64) -> Option<&LlmNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn comfy_node(&self, id: i64) -> Option<&crate::domain::comfy::ComfyNode> {
        self.comfy_nodes.iter().find(|n| n.id == id)
    }

    /// 某工作流可用的端点：显式指定则取交集，否则用全部启用端点。
    pub fn comfy_nodes_for(&self, node_ids: &[i64]) -> Vec<i64> {
        let all: Vec<i64> = self
            .comfy_nodes
            .iter()
            .filter(|n| n.enabled && !n.window.in_disabled_hours())
            .map(|n| n.id)
            .collect();
        if node_ids.is_empty() {
            return all;
        }
        all.into_iter().filter(|id| node_ids.contains(id)).collect()
    }

    /// 某模型的上游节点 id 列表。
    pub fn nodes_for_model(&self, model: &str) -> Vec<i64> {
        self.model_routes.get(model).cloned().unwrap_or_default()
    }

    /// 某个 (对外模型名, 节点) 实际要发给上游的模型名。
    ///
    /// 路由表里 `model_name` 与 `upstream_model` 分开存就是为了让对外名
    /// 和上游名解耦（同一个模型在不同节点上别名不同、灰度换模型名）。
    /// 找不到就退回对外名——那正是绝大多数情况下的正确答案。
    ///
    /// 返回 `String` 而不是 `&str`：兜底分支要返回**传进来**的那个名字，
    /// 它的生命周期比 `self` 短，硬借用会把两个生命周期绑在一起。
    pub fn upstream_model(&self, model: &str, node_id: i64) -> String {
        self.routes
            .iter()
            .find(|r| r.model_name == model && r.node_id == node_id)
            .map(|r| r.upstream_model.clone())
            .unwrap_or_else(|| model.to_string())
    }

    /// 全部对外暴露的工作流名（出图侧相当于「模型列表」）。
    pub fn workflow_names(&self) -> Vec<String> {
        self.workflows
            .iter()
            .filter(|w| w.enabled)
            .map(|w| w.name.clone())
            .collect()
    }

    /// 全部对外暴露的模型名。
    pub fn models(&self) -> Vec<String> {
        let mut v: Vec<String> = self.model_routes.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn setting_i64(&self, key: &str, default: i64) -> i64 {
        self.settings
            .get(key)
            .and_then(|v| v.as_i64())
            .unwrap_or(default)
    }

    pub fn setting_str(&self, key: &str, default: &str) -> String {
        self.settings
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or(default)
            .to_string()
    }
}

/// 全进程共享状态。
pub struct AppState {
    pub cfg: Config,
    pub pool: SqlitePool,
    pub snapshot: arc_swap::ArcSwap<Snapshot>,
    /// 每个节点一个并发闸门，key = node id 的字符串形式。
    pub node_gates: tokio::sync::RwLock<HashMap<i64, Arc<SlotGate>>>,
    /// 账号层闸门，key = account id。
    pub account_gates: tokio::sync::RwLock<HashMap<i64, Arc<SlotGate>>>,
    /// 每个 ComfyUI 端点一个并发闸门。**一端点一泳道**：
    /// ComfyUI 自己也有队列，网关这层再叠一次才不会把端点压垮。
    pub comfy_gates: tokio::sync::RwLock<HashMap<i64, Arc<SlotGate>>>,
    pub keys: Arc<KeyRing>,
    /// 节点健康与冷却退避表，**进程级唯一**。
    pub health: Arc<crate::gate::node_gate::NodeHealth>,
}

impl AppState {
    pub async fn node_gate(&self, node_id: i64) -> Arc<SlotGate> {
        if let Some(g) = self.node_gates.read().await.get(&node_id) {
            return g.clone();
        }
        let mut w = self.node_gates.write().await;
        w.entry(node_id)
            .or_insert_with(|| Arc::new(SlotGate::new(format!("node:{node_id}"))))
            .clone()
    }

    pub async fn comfy_gate(&self, node_id: i64) -> Arc<SlotGate> {
        if let Some(g) = self.comfy_gates.read().await.get(&node_id) {
            return g.clone();
        }
        let mut w = self.comfy_gates.write().await;
        w.entry(node_id)
            .or_insert_with(|| Arc::new(SlotGate::new(format!("comfy:{node_id}"))))
            .clone()
    }

    pub async fn account_gate(&self, account_id: i64) -> Arc<SlotGate> {
        if let Some(g) = self.account_gates.read().await.get(&account_id) {
            return g.clone();
        }
        let mut w = self.account_gates.write().await;
        w.entry(account_id)
            .or_insert_with(|| Arc::new(SlotGate::new(format!("account:{account_id}"))))
            .clone()
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    /// 从 DB 重建快照并广播。**读路径零查库**的关键。
    pub async fn reload(&self) -> anyhow::Result<()> {
        let snap = crate::db::load::load_snapshot(&self.pool).await?;
        self.snapshot.store(Arc::new(snap));
        self.keys.sync(&self.snapshot.load().nodes).await;

        // 配置改了之后，闸门里的等待者要按新限额重新评估一次
        let snap = self.snapshot.load_full();
        for n in &snap.nodes {
            self.node_gate(n.id).await.nudge(n.max_concurrency).await;
        }
        for n in &snap.comfy_nodes {
            self.comfy_gate(n.id).await.nudge(n.max_concurrency).await;
        }
        Ok(())
    }
}

/// 运行态概览（管理页面「运行时」用）。
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeView {
    pub nodes: Vec<NodeRuntimeView>,
    pub accounts: Vec<AccountRuntimeView>,
    pub totals: Totals,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeRuntimeView {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub max_concurrency: i64,
    pub active: i64,
    pub waiting: usize,
    pub in_disabled_window: bool,
    pub usable_keys: usize,
    pub total_keys: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountRuntimeView {
    pub id: i64,
    pub name: String,
    pub active: i64,
    pub waiting: usize,
    pub balance_micro: i64,
    pub held_micro: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub active: i64,
    pub waiting: usize,
    pub rejected: u64,
}

pub fn window_of(n: &LlmNode) -> DisabledWindow {
    n.window.clone()
}

pub type SharedAccount = Account;

#[cfg(test)]
mod tests {
    use super::*;

    fn route(model: &str, node: i64, upstream: &str) -> LlmRoute {
        LlmRoute {
            id: node,
            model_name: model.into(),
            node_id: node,
            upstream_model: upstream.into(),
            enabled: true,
            priority: 0,
        }
    }

    /// 对外名与上游名解耦是这个方法存在的全部理由：
    /// 同一模型在 A 节点叫 `qwen3.5-4b-heretic`、在 B 节点叫 `mlx-4b` 时，
    /// 必须各发各的。发错的话上游要么报「没这个模型」，要么回空补全。
    #[test]
    fn upstream_model_follows_the_picked_route() {
        let mut s = Snapshot::default();
        s.routes.push(route("qwen3.5-4b-heretic", 1, "heretic-gguf"));
        s.routes.push(route("qwen3.5-4b-heretic", 2, "heretic-mlx"));
        assert_eq!(s.upstream_model("qwen3.5-4b-heretic", 1), "heretic-gguf");
        assert_eq!(s.upstream_model("qwen3.5-4b-heretic", 2), "heretic-mlx");
    }

    /// 绝大多数情况两边同名，取不到路由时退回对外名才是对的。
    #[test]
    fn unknown_route_falls_back_to_the_requested_name() {
        let mut s = Snapshot::default();
        s.routes.push(route("a", 1, "a-upstream"));
        assert_eq!(s.upstream_model("a", 99), "a");
        assert_eq!(s.upstream_model("never-configured", 1), "never-configured");
    }
}
