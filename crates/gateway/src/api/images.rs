//! 网关面：出图端点。
//!
//! 两条通道：
//! - **模板**（默认）：`POST /v1/images/generations` 只传 `params`
//!   （`prompt` / `size` / `seed`），服务端用那条出图工作流填充占位符。
//!   请求体里**没有** `workflow`：出图工作流只有一条，选它没有意义。
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
    // 请求体里**没有** workflow 字段：工作流是 ComfyUI 图，是实现细节，
    // 调用方选的是 model。解析规则见 domain::workflow::resolve_by_model。
    let raw_mode = req.get("mode").and_then(Value::as_str) == Some("raw");
    let wf = if raw_mode {
        crate::domain::workflow::resolve_raw(&snap.workflows)
    } else {
        // model 放顶层（跟 /v1/chat/completions 和 OpenAI 的出图端点一致），
        // 不在 params 里——params 是模板占位符的白名单，混进去会被当成
        // 「不支持的参数」报一句让人摸不着头脑的话。
        if let Some(m) = req.get("params").and_then(|p| p.get("model")) {
            return Err(ApiError::bad_request(format!(
                "model 放请求体顶层，不在 params 里：\"model\": {}{}",
                m.as_str()
                    .map(|s| format!("\"{s}\""))
                    .unwrap_or_else(|| m.to_string()),
                req.get("model")
                    .map(|_| "。你已经在顶层传过了，删掉 params 里这个")
                    .unwrap_or("")
            )));
        }
        crate::domain::workflow::resolve_by_model(
            &snap.workflows,
            req.get("model").and_then(Value::as_str),
        )
    }
    .map_err(ApiError::bad_request)?;

    // 字段删了不等于收到就装没看见：body 是裸 Value，多余字段会被 serde 静默吞掉。
    // 调用方传 `workflow: "zit-1024"` 会拿到默认那条的图还以为自己在选工作流——
    // 「配了不生效」是本仓库最该根除的一类问题，宁可 400 也要说出来。
    reject_legacy_workflow_field(&req, &wf.name)?;
    let name = wf.name.clone();

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
                &error_entry(&acct, &name, &request_id, started, &e, false),
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
        // 只放行固定的那几个，并把 size 展开成 width/height。
        // 校验与展开都在 prepare_slots 里，真实提交与「试跑」共用同一条路径。
        let slots = workflow::prepare_slots(&wf.comfy_workflow, &params)
            .map_err(|e| ApiError::bad_request(e.0))?;
        let (rendered, _used, unresolved) =
            workflow::render(&wf.comfy_workflow, &slots).map_err(|e| ApiError::bad_request(e.0))?;
        // 属于允许集的占位符没填（比如调用方没传 seed）→ 当场拦。
        // 不拦的话 `{{seed}}` 字面量会送到 ComfyUI，调用方拿到的是
        // 「Prompt outputs failed validation」，完全看不出是自己少传了参数。
        crate::domain::workflow::check_unresolved(&wf.name, &unresolved)
            .map_err(ApiError::bad_request)?;
        // 不属于允许集的（negative_prompt 等）只提醒不停：那是模板该写
        // `{{negative_prompt|}}` 的事，不该让调用方为它背锅。
        if !unresolved.is_empty() {
            tracing::warn!(
                workflow = %wf.name,
                ?unresolved,
                "模板里有调用方传不进来的占位符，已按字面量提交；\
                 建议在模板里给它们加默认值"
            );
        }
        rendered
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

    // ── 取图：内联 base64 返回。图片不落盘，由调用方自行保存 ──
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

        out.push(json!({
            "b64_json": base64::engine::general_purpose::STANDARD.encode(&bytes),
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

    // 明细落库之后再记提示词：外键指向 request_log，顺序反了会插不进去
    record_prompt(
        &s,
        acct.id,
        &wf.name,
        request_id,
        prompt_of(req, raw_mode),
    )
    .await;

    Ok(json!({
        "created": chrono::Utc::now().timestamp(),
        "data": out,
        "usage": { "images": images.len(), "cost_micro": cost },
    }))
}

/// 请求体顶层不该再有 `workflow`——出图工作流只有一条。
///
/// 单独抽出来而不是就地 `if`：body 是裸 `Value`，serde 不会拒绝多余字段，
/// 静默吞掉等于让调用方以为自己在选工作流。报错里要带上**实际会用哪条**，
/// 否则对方只能去翻管理台猜。
fn reject_legacy_workflow_field(req: &Value, wf_name: &str) -> ApiResult<()> {
    let Some(given) = req.get("workflow") else {
        return Ok(());
    };
    Err(ApiError::bad_request(format!(
        "出图不再接受 workflow 字段：出图只有一条工作流（{wf_name}），尺寸由 params.size 决定。\
         你传的是 {given}，请删掉 workflow 字段"
    )))
}

/// 从请求里取提示词。
///
/// 只认 `params.prompt`（模板模式）——raw 通道的 workflow 由调用方整份提交，
/// 网关不替它猜哪段文本是提示词，所以记空串。记空比猜错更有用：
/// 猜错会让人以为「提示词不可见」，实际是「这次没记」。
fn prompt_of(req: &Value, raw_mode: bool) -> &str {
    if raw_mode {
        return "";
    }
    req.get("params")
        .and_then(|p| p.get("prompt"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// 记提示词。失败只记日志：图已经生成并结算了，不能因为一条辅助记录回滚。
///
/// 必须在 `usage::write` **之后**调用——`image_prompt.request_id` 外键指向
/// `request_log.request_id`，明细还没落库时插入会撞外键约束。
async fn record_prompt(
    s: &Arc<AppState>,
    acct_id: i64,
    workflow: &str,
    request_id: &str,
    prompt: &str,
) {
    if prompt.is_empty() {
        return;
    }
    if let Err(e) = sqlx::query(
        "INSERT INTO image_prompt (request_id, account_id, workflow, prompt, created_at)
         VALUES (?,?,?,?,?)",
    )
    .bind(request_id)
    .bind(acct_id)
    .bind(workflow)
    .bind(prompt)
    .bind(crate::billing::usage::unix_now())
    .execute(&s.pool)
    .await
    {
        tracing::error!(error = %e, request_id, "记录出图提示词失败");
    }
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
///
/// 这里列的是**模型**，不是工作流：调用方要传的是 `model`，工作流是网关
/// 内部的实现细节。`default` 的含义也变了——只有一个模型时不传 `model`
/// 也能用，多于一个必须显式指定，这个 `default` 就是告诉调用方「不传行不行」。
pub async fn list_workflows(State(s): State<Arc<AppState>>, _a: Authed) -> Json<Value> {
    let snap = s.snapshot();
    // 与 resolve_by_model 同一条路径：推不出模型名的不进列表，
    // 否则这里报出来的东西会跟实际能选的对不上。
    let models: Vec<&crate::domain::workflow::Workflow> = snap
        .workflows
        .iter()
        .filter(|w| w.enabled && w.mode == WorkflowMode::Template)
        .filter(|w| !w.model.is_empty())
        .collect();

    let data: Vec<Value> = models
        .iter()
        .map(|w| {
            json!({
                // 调用方传回来的就是这个值
                "id": w.model,
                "object": "model",
                // 哪条工作流实现的，排障时对着看；调用方不用管
                "workflow": w.name,
                // 全局固定的参数面，不是每个工作流各报各的
                "params": crate::domain::workflow::allowed_slots_json(),
                // 尺寸不自由传，只有这两组；连像素一起给，省得调用方猜
                "sizes": crate::domain::workflow::size_presets_json(),
                "default_size": crate::domain::workflow::DEFAULT_SIZE,
                // 不传 model 会不会被接受：只有唯一一个模型时才可以
                "default": models.len() == 1,
                "price_micro": w.price_micro,
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": data }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req_with(params: Value) -> Value {
        json!({ "params": params })
    }

    /// 顶层 `workflow` 是删掉的字段。裸 Value 不会自动拒它，
    /// 静默忽略 = 调用方以为在选工作流，实际拿到的是默认那条。
    #[test]
    fn legacy_workflow_field_is_rejected_not_ignored() {
        assert!(reject_legacy_workflow_field(&req_with(json!({})), "zit-scene").is_ok());
        let e = reject_legacy_workflow_field(
            &json!({"workflow": "zit-1024", "params": {"prompt": "x"}}),
            "zit-scene",
        )
        .expect_err("顶层 workflow 必须报错");
        let msg = e.to_string();
        // 三样都要在：字段名、实际会用哪条、对方传了什么
        assert!(msg.contains("workflow"), "{msg}");
        assert!(msg.contains("zit-scene"), "{msg}");
        assert!(msg.contains("zit-1024"), "{msg}");
    }

    /// 报错不能因为对方传了非字符串就 panic 或漏信息。
    #[test]
    fn legacy_workflow_field_error_survives_non_string() {
        let e = reject_legacy_workflow_field(&json!({"workflow": 7}), "zit-scene")
            .expect_err("非字符串的 workflow 同样要拒");
        assert!(e.to_string().contains("7"), "{e}");
    }

    #[test]
    fn template_mode_takes_prompt_from_params() {
        let req = req_with(json!({ "prompt": "一只猫", "seed": 42 }));
        assert_eq!(prompt_of(&req, false), "一只猫");
    }

    /// 缺 `params.prompt` 的工作流（有槽位但调用方没传）不能当成空提示词记一条，
    /// 也不能 panic——槽位是工作流自己声明的，不一定包含 prompt。
    #[test]
    fn missing_prompt_yields_empty_not_a_panic() {
        assert_eq!(prompt_of(&req_with(json!({ "seed": 1 })), false), "");
        assert_eq!(prompt_of(&req_with(json!({})), false), "");
        assert_eq!(prompt_of(&json!({}), false), "");
    }

    /// prompt 传成非字符串（数字 / 对象）时按「没给」处理，不记脏数据。
    #[test]
    fn non_string_prompt_is_not_recorded() {
        assert_eq!(prompt_of(&req_with(json!({ "prompt": 42 })), false), "");
        assert_eq!(
            prompt_of(&req_with(json!({ "prompt": {"a":1} })), false),
            ""
        );
    }

    /// raw 通道由调用方整份提交 workflow，网关不猜哪段是提示词 → 记空。
    #[test]
    fn raw_mode_records_no_prompt() {
        let req = req_with(json!({ "prompt": "一只猫", "workflow": {"1": {}} }));
        assert_eq!(prompt_of(&req, true), "");
    }
}
