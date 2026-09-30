//! 从 DB 装载运行时快照（`Snapshot` 的唯一构造点）。

use std::collections::HashMap;

use sqlx::{Row, SqlitePool};

use crate::domain::node::{LlmNode, LlmRoute, NodeKey, NodeKeyState, RateLimitScope};
use crate::domain::pricing::{ModelPrice, PriceTable};
use crate::domain::window::DisabledWindow;
use crate::domain::workflow::{Workflow, WorkflowMode};
use crate::state::Snapshot;

/// 载入配置快照。读路径只调这里一次，之后走内存。
pub async fn load_snapshot(pool: &SqlitePool) -> anyhow::Result<Snapshot> {
    let mut snap = Snapshot::default();

    // ── 设置 ──
    for row in sqlx::query("SELECT key, value FROM setting")
        .fetch_all(pool)
        .await?
    {
        let key: String = row.get("key");
        let raw: String = row.get("value");
        let v = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
        snap.settings.insert(key, v);
    }

    // ── 节点 ──
    let node_rows = sqlx::query(
        "SELECT id, name, kind, base_url, lan_base_url, max_concurrency,
                default_max_output_tokens, enabled, sort_order,
                disabled_start, disabled_end, disabled_timezone,
                extra_headers, extra_body
         FROM llm_node ORDER BY sort_order, id",
    )
    .fetch_all(pool)
    .await?;

    for r in node_rows {
        snap.nodes.push(LlmNode {
            id: r.get("id"),
            name: r.get("name"),
            kind: r.get("kind"),
            base_url: r.get("base_url"),
            lan_base_url: r.get("lan_base_url"),
            max_concurrency: r.get("max_concurrency"),
            default_max_output_tokens: r.get("default_max_output_tokens"),
            enabled: r.get::<i64, _>("enabled") != 0,
            sort_order: r.get("sort_order"),
            extra_headers: serde_json::from_str(&r.get::<String, _>("extra_headers"))
                .unwrap_or(serde_json::Value::Object(Default::default())),
            extra_body: serde_json::from_str(&r.get::<String, _>("extra_body"))
                .unwrap_or(serde_json::Value::Object(Default::default())),
            window: DisabledWindow::new(
                r.get::<Option<i64>, _>("disabled_start").map(|v| v as u8),
                r.get::<Option<i64>, _>("disabled_end").map(|v| v as u8),
                r.get("disabled_timezone"),
            ),
            keys: Vec::new(),
        });
    }

    // ── key 池 ──
    let key_rows = sqlx::query(
        "SELECT id, node_id, label, secret, sort_order, enabled, state, cooldown_until,
                reset_source, quota_class, matched_rule, matched_signal, rate_limit_scope,
                soft_cap_window_ms, soft_cap_tokens, window_started_at, window_tokens_used,
                count_429, count_rotations, count_false_positive, last_used_at
         FROM llm_node_key ORDER BY node_id, sort_order, id",
    )
    .fetch_all(pool)
    .await?;

    for r in key_rows {
        let node_id: i64 = r.get("node_id");
        let Some(node) = snap.nodes.iter_mut().find(|n| n.id == node_id) else {
            continue;
        };
        let state = NodeKeyState::parse(&r.get::<String, _>("state"));
        let window_started_at: Option<i64> = r.get("window_started_at");
        let window_tokens_used: i64 = r.get("window_tokens_used");
        let soft_cap_reached = match (r.get::<Option<i64>, _>("soft_cap_window_ms"), r.get::<Option<i64>, _>("soft_cap_tokens")) {
            (Some(_), Some(cap)) => {
                let started_ok = window_started_at
                    .map(|s| crate::keys::unix_now() - s < r.get::<Option<i64>, _>("soft_cap_window_ms").unwrap())
                    .unwrap_or(false);
                started_ok && window_tokens_used >= cap
            }
            _ => false,
        };
        node.keys.push(NodeKey {
            id: r.get("id"),
            node_id,
            label: r.get("label"),
            secret: r.get("secret"),
            sort_order: r.get("sort_order"),
            enabled: r.get::<i64, _>("enabled") != 0,
            state,
            cooldown_until: r.get("cooldown_until"),
            reset_source: r.get("reset_source"),
            quota_class: r.get("quota_class"),
            matched_rule: r.get("matched_rule"),
            matched_signal: r.get("matched_signal"),
            rate_limit_scope: RateLimitScope::parse(&r.get::<String, _>("rate_limit_scope")),
            soft_cap_window_ms: r.get("soft_cap_window_ms"),
            soft_cap_tokens: r.get("soft_cap_tokens"),
            window_started_at,
            window_tokens_used,
            count_429: r.get("count_429"),
            count_rotations: r.get("count_rotations"),
            count_false_positive: r.get("count_false_positive"),
            last_used_at: r.get("last_used_at"),
            soft_cap_reached,
        });
    }

    // ── 路由 ──
    let route_rows = sqlx::query(
        "SELECT id, model_name, node_id, upstream_model, enabled, priority
         FROM llm_route WHERE enabled = 1 ORDER BY model_name, priority, id",
    )
    .fetch_all(pool)
    .await?;

    for r in route_rows {
        let route = LlmRoute {
            id: r.get("id"),
            model_name: r.get("model_name"),
            node_id: r.get("node_id"),
            upstream_model: r.get("upstream_model"),
            enabled: true,
            priority: r.get("priority"),
        };
        snap.model_routes
            .entry(route.model_name.clone())
            .or_default()
            .push(route.node_id);
        snap.routes.push(route);
    }

    // ── 工作流 ──
    for r in sqlx::query(
        "SELECT id, name, mode, node_ids, comfy_workflow, param_slots, price_micro,
                enabled, archive, archive_retention_days
         FROM workflow ORDER BY id",
    )
    .fetch_all(pool)
    .await?
    {
        snap.workflows.push(Workflow {
            id: r.get("id"),
            name: r.get("name"),
            mode: if r.get::<String, _>("mode") == "raw" {
                WorkflowMode::Raw
            } else {
                WorkflowMode::Template
            },
            node_ids: serde_json::from_str(&r.get::<String, _>("node_ids")).unwrap_or_default(),
            comfy_workflow: serde_json::from_str(&r.get::<String, _>("comfy_workflow"))
                .unwrap_or(serde_json::Value::Object(Default::default())),
            param_slots: serde_json::from_str(&r.get::<String, _>("param_slots"))
                .unwrap_or_default(),
            price_micro: r.get("price_micro"),
            enabled: r.get::<i64, _>("enabled") != 0,
            archive: r.get::<i64, _>("archive") != 0,
            archive_retention_days: r.get("archive_retention_days"),
        });
    }

    // ── 定价 ──
    let mut by_model = HashMap::new();
    for r in sqlx::query("SELECT model_name, input_micro_per_1k, output_micro_per_1k, cached_input_micro_per_1k FROM model_price")
        .fetch_all(pool)
        .await?
    {
        by_model.insert(
            r.get::<String, _>("model_name"),
            ModelPrice {
                input_micro_per_1k: r.get("input_micro_per_1k"),
                output_micro_per_1k: r.get("output_micro_per_1k"),
                cached_input_micro_per_1k: r.get("cached_input_micro_per_1k"),
            },
        );
    }
    snap.prices = PriceTable {
        default_model: None,
        by_model,
        default_image_micro: 0,
        by_workflow: HashMap::new(),
    };
    // 全局默认价与工作流单价放在 setting 里
    if let Some(v) = snap.settings.get("default_model_price") {
        snap.prices.default_model = serde_json::from_value(v.clone()).ok();
    }
    if let Some(v) = snap.settings.get("default_image_micro").and_then(|v| v.as_i64()) {
        snap.prices.default_image_micro = v;
    }
    for w in &snap.workflows {
        snap.prices.by_workflow.insert(w.name.clone(), w.price_micro);
    }

    Ok(snap)
}
