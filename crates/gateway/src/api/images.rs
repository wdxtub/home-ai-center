//! 网关面：出图端点。
//!
//! 两条通道：
//! - **模板**（默认）：`POST /v1/images/generations` 传 `workflow` + `params`，
//!   服务端用命名工作流填充占位符。
//! - **原始**：`mode: "raw"` 时 `params.workflow` 直接当 ComfyUI workflow 用。
//!
//! ## 换端点的边界
//!
//! 只有**提交之前**能换。ComfyUI 一旦 `/prompt` 成功，任务就绑死在那个
//! 端点上，重投会双倍扣费并产生重复图。所以代码上刻意分成
//! 「选点 → 渲染 → 提交」与「轮询 → 取图」两段，
//! 只有前一段允许 failover。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use serde_json::{json, Value};

use crate::api::auth::Authed;
use crate::billing::{ledger, usage};
use crate::domain::account::ResourceKind;
use crate::domain::pricing::PriceTable;
use crate::domain::workflow::WorkflowMode;
use crate::error::{ApiError, ApiResult};
use crate::gate::comfy_gate::{self, ComfyHealth};
use crate::gate::Priority;
use crate::protocol::ir::UnifiedUsage;
use crate::state::AppState;
use crate::upstream::comfyui::{ComfyClient, ComfyError};
use crate::upstream::workflow;

/// 出图轮询上限。跟 LLM 的整体 timeout 分开，因为任务在节点上**真的在跑**。
const POLL_TIMEOUT: Duration = Duration::from_secs(600);

pub async fn generate(
    State(s): State<Arc<AppState>>,
    a: Authed,
    body: axum::Json<Value>,
) -> ApiResult<Response> {
    let started = Instant::now();
    let request_id = uuid::Uuid::new_v4().to_string();
    let acct = a.0;
    let req = body.0;
    let snap = s.snapshot();

    // ── 定位工作流 ──
    let name = req
        .get("workflow")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("缺少 workflow 字段（命名工作流名）"))?;
    let wf = snap
        .workflows
        .iter()
        .find(|w| w.name == name && w.enabled)
        .ok_or_else(|| ApiError::not_found(format!("工作流 {name} 不存在或已停用")))?;

    let raw_mode = req
        .get("mode")
        .and_then(Value::as_str)
        .map(|m| m == "raw")
        .unwrap_or(false);
    if raw_mode && wf.mode != WorkflowMode::Raw {
        return Err(ApiError::bad_request(format!(
            "工作流 {name} 是模板模式，不能用 raw 通道"
        )));
    }

    // ── 定价：没配价就拒绝 ──
    let price = PriceTable::image_price(&snap.prices, &wf.name);
    if price <= 0 {
        return Err(ApiError::bad_request(format!(
            "工作流 {name} 未配置单价（workflow.price_micro 或默认出图单价）"
        )));
    }
    // 批量大小藏在 workflow 的 batch_size 节点里，网关看不见。
    // 让调用方给个提示（缺省 1）来预扣，避免多图批次结算时余额被扣成负数。
    let batch_hint = req
        .get("n")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 64) as i64;
    let reserved = price.saturating_mul(batch_hint);
    ledger::reserve(&s.pool, acct.id, reserved, &request_id).await?;

    let result = run(s.clone(), &acct, wf, &req, &request_id, started, reserved, raw_mode).await;
    match result {
        Ok(v) => Ok(axum::Json(v).into_response()),
        Err(e) => {
            ledger::release_all(&s.pool, acct.id, reserved, &request_id).await;
            usage::write(
                &s.pool,
                &error_entry(&acct, name, &request_id, started, &e, false),
            )
            .await
            .ok();
            Ok(e.into_response_for(
                crate::protocol::Protocol::Chat,
                Some(&request_id),
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    s: Arc<AppState>,
    acct: &crate::domain::account::Account,
    wf: &crate::domain::workflow::Workflow,
    req: &Value,
    request_id: &str,
    started: Instant,
    reserved: i64,
    raw_mode: bool,
) -> ApiResult<Value> {
    // ── 渲染最终 workflow JSON ──
    let comfy_json: Value = if raw_mode {
        req.get("params")
            .and_then(|p| p.get("workflow"))
            .cloned()
            .ok_or_else(|| ApiError::bad_request("raw 通道需要在 params.workflow 里给完整 workflow"))?
    } else {
        let params = req
            .get("params")
            .cloned()
            .unwrap_or_else(|| json!({}))
            .as_object()
            .cloned()
            .ok_or_else(|| ApiError::bad_request("params 必须是对象"))?;
        // 只允许注入工作流声明过的槽位，避免调用方往里塞任意键
        let allowed: std::collections::HashSet<&str> =
            wf.param_slots.iter().map(String::as_str).collect();
        let unknown: Vec<&str> = params
            .keys()
            .map(String::as_str)
            .filter(|k| !allowed.contains(k))
            .collect();
        if !unknown.is_empty() {
            return Err(ApiError::bad_request(format!(
                "工作流 {} 没有这些参数槽：{}（已声明：{}）",
                wf.name,
                unknown.join(", "),
                if wf.param_slots.is_empty() {
                    "无".to_string()
                } else {
                    wf.param_slots.join(", ")
                }
            )));
        }
        workflow::render(&wf.comfy_workflow, &params)
            .map_err(|e| ApiError::bad_request(e.0))?
            .0
    };

    // ── 账号闸门 ──
    let queue_timeout = Duration::from_secs(s.cfg.queue_timeout_secs.max(600));
    let acct_gate = s.account_gate(acct.id).await;
    let acct_lease = acct_gate
        .acquire(
            acct.max_concurrency(ResourceKind::Image),
            acct.max_queue(ResourceKind::Image),
            Priority::Batch,
            Some(queue_timeout),
        )
        .await
        .map_err(|e| ApiError::queue_full(format!("账号出图并发已满：{e}")))?;

    let client = ComfyClient::new(Duration::from_secs(10), POLL_TIMEOUT)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let health = ComfyHealth::default();

    // ── 提交前：允许换端点 ──
    let mut last: Option<ApiError> = None;
    let mut rendered = None;
    for _ in 0..3 {
        let pick = match comfy_gate::acquire(&s, &health, &wf.node_ids, queue_timeout).await {
            Ok(p) => p,
            Err(e) => return Err(e),
        };
        match client
            .render(&pick.node, &comfy_json, request_id)
            .await
        {
            Ok(r) => {
                health.mark_success(pick.node.id).await;
                let imgs = r.images;
                let queue_wait = pick.queue_wait_ms;
                drop(pick.lease);
                rendered = Some((pick.node.name.clone(), pick.node.id, imgs, queue_wait));
                break;
            }
            Err(e) => {
                let (retryable, err) = match e {
                    // 工作流本身错了：换端点只是把同一个坏工作流再发一遍
                    ComfyError::Workflow(m) => (false, ApiError::bad_request(m)),
                    ComfyError::Timeout => (true, ApiError::upstream("出图超时")),
                    ComfyError::Job { message } => (false, ApiError::upstream(message)),
                    ComfyError::Endpoint { message } => (true, ApiError::upstream(message)),
                };
                drop(pick.lease);
                if retryable {
                    // 只有提交前失败才能换端点重试
                    health.mark_failure(pick.node.id).await;
                    last = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }

    let Some((node_name, node_id, images, queue_wait_ms)) = rendered else {
        return Err(last.unwrap_or_else(|| ApiError::upstream("没有可用的 ComfyUI 端点")));
    };
    let _ = acct_lease;

    if images.is_empty() {
        return Err(ApiError::upstream("ComfyUI 没有返回任何图片"));
    }

    // ── 取图：默认 base64 内联，按工作流开关落盘 ──
    let mut out: Vec<Value> = Vec::with_capacity(images.len());
    let snap2 = s.snapshot();
    let node_cfg = snap2.comfy_node(node_id);
    for img in &images {
        let bytes = match node_cfg {
            Some(n) => client.fetch_image(n, img).await,
            None => Err(ComfyError::Job {
                message: "端点已下线".into(),
            }),
        };
        let bytes = bytes.map_err(|e| ApiError::upstream(e.to_string()))?;

        let url = if wf.archive {
            match archive(&s, request_id, img, &bytes).await {
                Ok(u) => u,
                Err(e) => {
                    // 归档失败不该让整次出图失败，图已经生成出来了
                    tracing::error!(error = %e, "图片归档失败");
                    Value::Null
                }
            }
        } else {
            Value::Null
        };

        out.push(json!({
            "b64_json": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "url": url,
            "filename": img.filename,
        }));
    }

    // ── 结算：按张数计价 ──
    let cost = PriceTable::image_cost(&snap2.prices, &wf.name, images.len() as u32);
    if let Err(e) = ledger::settle(&s.pool, acct.id, reserved, cost, request_id).await {
        tracing::error!(error = %e, "出图结算失败");
        ledger::release_all(&s.pool, acct.id, reserved, request_id).await;
        return Err(ApiError::internal("出图结算失败"));
    }

    usage::write(
        &s.pool,
        &usage::LogEntry {
            request_id: request_id.to_string(),
            account_id: acct.id,
            kind: "image",
            protocol: None,
            target: wf.name.clone(),
            node_id: Some(node_id),
            node_name: Some(node_name),
            key_id: None,
            rotated_count: 0,
            stream: false,
            status: "ok",
            error_kind: None,
            error_message: None,
            queue_wait_ms,
            latency_ms: started.elapsed().as_millis() as i64,
            usage: UnifiedUsage::default(),
            tokens_source: "none",
            image_count: images.len() as i64,
            cost_micro: cost,
            notes: Vec::new(),
        },
    )
    .await
    .ok();

    Ok(json!({
        "created": chrono::Utc::now().timestamp(),
        "data": out,
        "usage": { "images": images.len(), "cost_micro": cost },
    }))
}

/// 落盘归档（工作流级开关）。失败只记日志，不影响出图结果。
async fn archive(
    s: &Arc<AppState>,
    request_id: &str,
    img: &crate::upstream::comfyui::ImageRef,
    bytes: &[u8],
) -> anyhow::Result<Value> {
    use std::io::Write;
    let dir = s.cfg.image_dir().join(request_id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(&img.filename);
    let mut f = std::fs::File::create(&path)?;
    f.write_all(bytes)?;
    Ok(Value::String(format!("/api/admin/images/{}", path.file_name().unwrap_or_default().to_string_lossy())))
}

fn error_entry(
    acct: &crate::domain::account::Account,
    target: &str,
    request_id: &str,
    started: Instant,
    e: &ApiError,
    image: bool,
) -> usage::LogEntry {
    usage::LogEntry {
        request_id: request_id.to_string(),
        account_id: acct.id,
        kind: "image",
        protocol: None,
        target: target.to_string(),
        node_id: None,
        node_name: None,
        key_id: None,
        rotated_count: 0,
        stream: false,
        status: "error",
        error_kind: Some(e.kind.openai_type().to_string()),
        error_message: Some(e.message.clone()),
        queue_wait_ms: 0,
        latency_ms: started.elapsed().as_millis() as i64,
        usage: UnifiedUsage::default(),
        tokens_source: "none",
        image_count: 0,
        cost_micro: 0,
        notes: if image { vec!["image".into()] } else { Vec::new() },
    }
}

/// `GET /v1/workflows` —— 出图侧的「模型列表」。
pub async fn list_workflows(State(s): State<Arc<AppState>>, _a: Authed) -> Json<Value> {
    let snap = s.snapshot();
    let data: Vec<Value> = snap
        .workflows
        .iter()
        .filter(|w| w.enabled)
        .map(|w| {
            json!({
                "id": w.name,
                "object": "workflow",
                "mode": w.mode,
                "params": w.param_slots,
                "price_micro": w.price_micro,
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": data }))
}
