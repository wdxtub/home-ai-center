//! 网关面：三种入站协议端点 + 模型列表 + 用量查询。
//!
//! 三个协议端点共用**同一条准入链路**
//! （鉴权 → 定价 → 预扣 → 账号闸门 → 节点闸门 → 上游 → 结算），
//! 差别只在入站解析与出站渲染。
//!
//! ## 结算时机是这里最要紧的一件事
//!
//! 非流式在返回响应前就能算出费用，直接结算。流式的 token 数**只有
//! `Done` 事件里才有**，所以结算必须挂到流上：流被读完或被客户端掐断，
//! 尾部的 `StreamTail` 随流一起析构，`Drop` 里 spawn 一次结算。
//! 提前结算会把「还没生成的 token」记成 0，账直接对不上。

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::auth::Authed;
use crate::billing::{ledger, usage};
use crate::domain::account::{Account, ResourceKind};
use crate::domain::node::NodeKey;
use crate::domain::pricing::{ModelPrice, PriceTable};
use crate::error::{ApiError, ApiResult};
use crate::gate::node_gate::{FailureKind, NodePick, NodeScheduler};
use crate::gate::{Priority, SlotLease};
use crate::protocol::ir::*;
use crate::protocol::sse::SseFrame;
use crate::protocol::{chat, messages, responses, tokenize, upstream_chat, Protocol};
use crate::state::AppState;
use crate::upstream::openai::{self, UpstreamError};

fn new_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn elapsed_ms(since: Instant) -> i64 {
    since.elapsed().as_millis() as i64
}

/// 上游没给 usage 时的兜底估算。**记 0 等于白嫖**，
/// 而且客户端看到的 usage 会和账上扣的对不上。
fn estimate_usage(est_input: u32, out_chars: usize) -> UnifiedUsage {
    UnifiedUsage {
        input_tokens: est_input,
        cached_tokens: 0,
        output_tokens: tokenize::estimate_chars_as_tokens(out_chars),
        reasoning_tokens: 0,
    }
}

// ── 模型列表 ────────────────────────────────────────────────────────────────

pub async fn models(State(s): State<Arc<AppState>>, _a: Authed) -> Json<Value> {
    let snap = s.snapshot();
    let items: Vec<Value> = snap
        .models()
        .into_iter()
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "home-ai-center",
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": items }))
}

// ── 三个协议端点 ────────────────────────────────────────────────────────────

pub async fn chat_completions(
    State(s): State<Arc<AppState>>,
    a: Authed,
    body: axum::Json<Value>,
) -> ApiResult<Response> {
    let req = chat::parse_request(&body)?;
    serve(s, a.0, req, Protocol::Chat).await
}

pub async fn responses(
    State(s): State<Arc<AppState>>,
    a: Authed,
    body: axum::Json<Value>,
) -> ApiResult<Response> {
    let req = responses::parse_request(&body)?;
    serve(s, a.0, req, Protocol::Responses).await
}

pub async fn messages(
    State(s): State<Arc<AppState>>,
    a: Authed,
    body: axum::Json<Value>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if !headers.contains_key("anthropic-version") {
        tracing::debug!("请求未带 anthropic-version，按缺失处理（不阻断）");
    }
    let mut req = messages::parse_request(&body)?;
    // Anthropic 的 max_tokens 是必填。缺失时用节点默认值补上，
    // 否则会在上游那里换一个更没有依据的默认值。
    if req.max_output_tokens.is_none() {
        req.max_output_tokens = s
            .snapshot()
            .nodes
            .iter()
            .find_map(|n| n.default_max_output_tokens)
            .map(|v| v as u32)
            .or(Some(4096));
    }
    serve(s, a.0, req, Protocol::Messages).await
}

// ── 预扣的持有权 ────────────────────────────────────────────────────────────

/// 预扣的持有者。`Arc` 共享 + `Drop` 兜底：
/// 最后一个持有者析构时若仍未结算，就把冻结全额还回去。
///
/// 宁可少收一次钱，也不能让一笔冻结永久挂死在某个账号上。
struct Reservation {
    pool: SqlitePool,
    account_id: i64,
    amount: i64,
    request_id: String,
    done: AtomicBool,
}

impl Reservation {
    fn new(
        pool: SqlitePool,
        account_id: i64,
        amount: i64,
        request_id: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            account_id,
            amount,
            request_id,
            done: AtomicBool::new(false),
        })
    }

    async fn reserve(self: &Arc<Self>) -> ApiResult<()> {
        ledger::reserve(&self.pool, self.account_id, self.amount, &self.request_id)
            .await
            .map(|_| ())
    }

    /// 按真实用量扣费并释放冻结；失败时兜底全额释放。
    async fn settle(&self, cost_micro: i64) {
        match ledger::settle(
            &self.pool,
            self.account_id,
            self.amount,
            cost_micro,
            &self.request_id,
        )
        .await
        {
            Ok(_) => {}
            Err(e) => {
                tracing::error!(error = %e, "结算失败，退回预扣");
                ledger::release_all(&self.pool, self.account_id, self.amount, &self.request_id).await;
            }
        }
        self.done.store(true, Ordering::Release);
    }

    async fn release(&self) {
        ledger::release_all(&self.pool, self.account_id, self.amount, &self.request_id).await;
        self.done.store(true, Ordering::Release);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.done.load(Ordering::Acquire) {
            return;
        }
        let pool = self.pool.clone();
        let account_id = self.account_id;
        let amount = self.amount;
        let request_id = self.request_id.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(async move {
                    tracing::warn!(
                        account_id,
                        amount,
                        "预扣未结算即析构，兜底释放"
                    );
                    ledger::release_all(&pool, account_id, amount, &request_id).await;
                });
            }
            Err(_) => {
                // 不在运行时里（测试或关闭过程中）：残留冻结由启动对账回收
                tracing::error!(account_id, amount, "无 Tokio 运行时，预扣残留等待启动对账");
            }
        }
    }
}

// ── 准入链路 ────────────────────────────────────────────────────────────────

async fn serve(
    s: Arc<AppState>,
    acct: Account,
    req: UnifiedRequest,
    protocol: Protocol,
) -> ApiResult<Response> {
    let started = Instant::now();
    let request_id = new_request_id();
    let snap = s.snapshot();

    // ① 定价：没配价就拒绝，不允许免费白嫖
    let price = snap
        .prices
        .model_price(&req.model)
        .ok_or_else(|| ApiError::bad_request(format!("模型 {} 未配置单价", req.model)))?;

    // ② 预扣：按最大可能费用冻结，杜绝并发透支
    let fallback_max = snap
        .nodes
        .iter()
        .find_map(|n| n.default_max_output_tokens)
        .map(|v| v as u32)
        .unwrap_or(4096);
    let reserved = ledger::estimate_llm_reserve(&price, &req, fallback_max);
    let reservation = Reservation::new(s.pool.clone(), acct.id, reserved, request_id.clone());
    reservation.reserve().await?;

    // req 之后会被移进准入链路，失败日志需要的两件事先留下
    let fail_model = req.model.clone();
    let fail_stream = req.stream;

    match run_admission(
        &s,
        &acct,
        req,
        protocol,
        &request_id,
        started,
        &price,
        reservation.clone(),
    )
    .await
    {
        Ok(resp) => Ok(resp),
        Err(e) => {
            reservation.release().await;
            log_failure(
                &s,
                &acct,
                &fail_model,
                fail_stream,
                protocol,
                &request_id,
                started,
                &e,
            )
            .await;
            // 错误体按**客户端协议**渲染，Anthropic SDK 认自己那套形状
            Ok(e.into_response_for(protocol, Some(&request_id)))
        }
    }
}

/// 一次调用的落库上下文。流式与非流式共用，保证两边记账口径一致。
#[derive(Clone)]
struct LogCtx {
    request_id: String,
    account_id: i64,
    protocol: Protocol,
    model: String,
    node: NodeKey,
    node_name: String,
    rotated: i64,
    queue_wait_ms: i64,
    started: Instant,
    notes: Vec<String>,
    estimated: bool,
}

async fn run_admission(
    s: &Arc<AppState>,
    acct: &Account,
    req: UnifiedRequest,
    protocol: Protocol,
    request_id: &str,
    started: Instant,
    price: &ModelPrice,
    reservation: Arc<Reservation>,
) -> ApiResult<Response> {
    let queue_timeout = Duration::from_secs(s.cfg.queue_timeout_secs);
    let est_input = tokenize::estimate_request(&req);

    // ③ 账号闸门：先拿到公平额度，再去抢节点。
    //    这一份名额要**占满整个请求**——包括中途换 key、换点，
    //    以及整个流式过程。中途丢掉就等于绕过了账号并发上限。
    let acct_gate = s.account_gate(acct.id).await;
    let acct_lease = acct_gate
        .acquire(
            acct.max_concurrency(ResourceKind::Llm),
            acct.max_queue(ResourceKind::Llm),
            Priority::Batch,
            Some(queue_timeout),
        )
        .await
        .map_err(|e| ApiError::queue_full(format!("当前账号并发已满：{e}")))?;

    let http = openai::OpenAiClient::new(300).map_err(|e| ApiError::internal(e.to_string()))?;
    let sched = NodeScheduler::new(s.health.clone(), s.keys.clone());
    let max_rot = s.cfg.max_key_rotation;

    let mut tried_nodes: Vec<i64> = Vec::new();
    let mut rotated = 0i64;
    let mut last_err: Option<ApiError> = None;
    // 额度耗尽的恢复时刻，用于 503 响应体
    let mut exhausted_at: Option<i64> = None;

    for _attempt in 0..=max_rot {
        if tried_nodes.len() > max_rot {
            break;
        }
        let pick = sched
            .acquire(s, &req.model, Priority::Batch, queue_timeout)
            .await?;

        // ⑤ 渲染上游请求（单一方言：OpenAI Chat Completions）
        let (mut body, stop_truncated) = upstream_chat::render_request(&req);
        if stop_truncated {
            tracing::warn!(model = %req.model, "stop_sequences 超过上限已截断");
        }
        // 节点级参数：各后端不统一（LM Studio 用 reasoning_effort、DeepSeek 用
        // extra_body.thinking），因此逐节点配，不做自动探测。
        if let (Some(extra), Some(obj)) = (pick.node.extra_body.as_object(), body.as_object_mut()) {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }

        // ⑥ 执行。流式在这里只拿到响应头，token 数要等流读完。
        let upstream_resp = if req.stream {
            exec_stream(&http, &pick, &body).await.map(ExecResult::Stream)
        } else {
            exec_once(&http, &pick, &body).await
        };

        let ctx = LogCtx {
            request_id: request_id.to_string(),
            account_id: acct.id,
            protocol,
            model: req.model.clone(),
            node_name: pick.node.name.clone(),
            node: pick.key.clone(),
            rotated,
            queue_wait_ms: pick.queue_wait_ms,
            started,
            notes: if stop_truncated {
                vec!["stop_truncated".into()]
            } else {
                Vec::new()
            },
            estimated: false,
        };

        match upstream_resp {
            Ok(ExecResult::Once(resp)) => {
                // 老后端可能不给 usage；按已收到的正文估算，绝不记 0
                let mut resp = resp;
                let mut estimated = false;
                if resp.usage.is_empty() {
                    resp.usage = estimate_usage(est_input, resp.text.chars().count());
                    estimated = true;
                }
                let usage = resp.usage;
                let cost = PriceTable::llm_cost(price, usage.input_tokens, usage.cached_tokens, usage.output_tokens);
                reservation.settle(cost).await;
                s.keys
                    .add_usage(
                        &pick.key,
                        (usage.input_tokens + usage.output_tokens) as i64,
                        Instant::now(),
                    )
                    .await;

                let mut ctx = ctx;
                ctx.estimated = estimated;
                usage::write(
                    &s.pool,
                    &log_entry(&ctx, usage, estimated, elapsed_ms(started), cost),
                )
                .await
                .ok();

                drop(pick.lease);
                sched.health.mark_success(pick.node.id).await;
                let body = render_once(protocol, &resp);
                drop(acct_lease);
                return Ok(json_response(body));
            }
            Ok(ExecResult::Stream(resp)) => {
                // 结算与名额释放都交给流尾部：流没结束就还名额，
                // 等于并发闸门对流式请求完全不生效。
                let tail = StreamTail {
                    state: s.clone(),
                    reservation,
                    ctx,
                    usage: UnifiedUsage::default(),
                    saw_done: false,
                    est_input,
                    out_chars: 0,
                    price: *price,
                    _slots: HeldSlots {
                        _account: acct_lease,
                        _node: pick.lease,
                    },
                };
                let (renderer, lead) = renderer_for(protocol, &req, est_input);
                let frames = build_stream(openai::parse_sse_stream(resp), renderer, lead, tail);
                return Ok(sse_response(frames));
            }
            Err(UpstreamError::Quota(verdict)) => {
                // 限额类：标记 key → 换一把重试，**不判节点离线**
                s.keys.apply_verdict(&pick.key, &verdict, Some(request_id)).await;
                rotated += 1;
                if let Some(at) = verdict.reset_at {
                    exhausted_at = Some(exhausted_at.map_or(at, |v: i64| v.min(at)));
                }
                last_err = Some(ApiError::no_capacity(format!(
                    "模型 {} 的 key「{}」已限额（{}）",
                    req.model, pick.key.label, verdict.signal
                )));
                tried_nodes.push(pick.node.id);
                drop(pick.lease);
                continue;
            }
            Err(UpstreamError::ModelLoading) => {
                // 节点是活的，只是模型在冷启动：原地等，不动冷却
                tokio::time::sleep(Duration::from_secs(5)).await;
                last_err = Some(ApiError::upstream("模型正在加载，请重试"));
                continue;
            }
            Err(UpstreamError::Transient { retry_after_secs, message }) => {
                // 瞬时类：原地等，**不换 key**
                let wait = retry_after_secs.unwrap_or(5).clamp(1, 30);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                last_err = Some(ApiError::upstream(message));
                continue;
            }
            Err(UpstreamError::Node { hard, message }) => {
                // 节点故障：冷却 + 换点
                sched
                    .health
                    .mark_failure(
                        pick.node.id,
                        if hard { FailureKind::Hard } else { FailureKind::Soft },
                        &message,
                    )
                    .await;
                last_err = Some(ApiError::upstream(message));
                tried_nodes.push(pick.node.id);
                drop(pick.lease);
                continue;
            }
            Err(UpstreamError::Client(m)) => {
                drop(pick.lease);
                return Err(ApiError::bad_request(m));
            }
        }
    }

    // 所有 key 试完：把最早恢复时间带上，客户端能自己决定等多久
    let mut e = last_err
        .unwrap_or_else(|| ApiError::upstream("上游调用失败"));
    if let Some(at) = exhausted_at {
        let now = crate::keys::unix_now();
        e = e
            .with_reset_at(at)
            .with_retry_after((at - now).max(1) as u64);
    }
    Err(e)
}

// ── 上游执行 ────────────────────────────────────────────────────────────────

enum ExecResult {
    Once(UnifiedResponse),
    Stream(reqwest::Response),
}

async fn exec_once(
    http: &openai::OpenAiClient,
    pick: &NodePick,
    body: &Value,
) -> Result<ExecResult, UpstreamError> {
    let raw = http
        .post_chat(
            &pick.node.base_url,
            &pick.key.secret,
            body,
            &pick.node.extra_headers,
        )
        .await?;
    Ok(ExecResult::Once(upstream_chat::parse_response(&raw)))
}

async fn exec_stream(
    http: &openai::OpenAiClient,
    pick: &NodePick,
    body: &Value,
) -> Result<reqwest::Response, UpstreamError> {
    http.post_chat_stream(
        &pick.node.base_url,
        &pick.key.secret,
        body,
        &pick.node.extra_headers,
    )
    .await
}

// ── 流式渲染 ────────────────────────────────────────────────────────────────

/// 三个出站渲染器的统一外壳。只有这里知道「客户端用哪种协议」，
/// 上游那边始终是同一种方言。
enum OutRenderer {
    Chat(chat::ChatStreamRenderer),
    Responses(responses::ResponsesStreamRenderer),
    Messages(messages::MessagesStreamRenderer),
}

impl OutRenderer {
    fn push(&mut self, ev: &UnifiedEvent) -> Vec<SseFrame> {
        match self {
            OutRenderer::Chat(r) => r.push(ev),
            OutRenderer::Responses(r) => r.push(ev),
            OutRenderer::Messages(r) => r.push(ev),
        }
    }
}

/// 建渲染器，并返回**必须先发**的头帧。
///
/// Anthropic 的 `message_start` 必须第一帧就带上输入 token 数，
/// 而上游要到流结束才给 `prompt_tokens`——只能用本地估算。
/// Chat 与 Responses 都不需要前置帧。
fn renderer_for(
    protocol: Protocol,
    req: &UnifiedRequest,
    est_input: u32,
) -> (OutRenderer, Vec<SseFrame>) {
    let created = chrono::Utc::now().timestamp();
    let id = format!("chatcmpl_{}", uuid::Uuid::new_v4().simple());
    match protocol {
        Protocol::Chat => (
            OutRenderer::Chat(chat::ChatStreamRenderer::new(&id, &req.model, created)),
            Vec::new(),
        ),
        Protocol::Responses => (
            OutRenderer::Responses(responses::ResponsesStreamRenderer::new(
                &id,
                &req.model,
                created,
            )),
            Vec::new(),
        ),
        Protocol::Messages => {
            let msg_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
            (
                OutRenderer::Messages(messages::MessagesStreamRenderer::new(
                    &msg_id,
                    &req.model,
                    est_input,
                )),
                vec![messages::message_start_frame(
                    &msg_id,
                    &req.model,
                    est_input,
                )],
            )
        }
    }
}

/// 流式请求占住的名额。活到流结束为止。
struct HeldSlots {
    _account: SlotLease,
    _node: SlotLease,
}

/// 流的尾部账本。**流读完、客户端断连、错误返回，三条路径都会走到这里。**
///
/// 用 `Drop` 而不是「流正常结束时的最后一步」：客户端半路掐断时
/// 上游的 token 已经花了，账必须照记。
struct StreamTail {
    state: Arc<AppState>,
    reservation: Arc<Reservation>,
    ctx: LogCtx,
    usage: UnifiedUsage,
    saw_done: bool,
    est_input: u32,
    out_chars: usize,
    price: ModelPrice,
    _slots: HeldSlots,
}

/// `Drop` 里能拿走的东西（`Drop` 不能移动字段，只能克隆）。
struct TailSnapshot {
    state: Arc<AppState>,
    reservation: Arc<Reservation>,
    ctx: LogCtx,
    usage: UnifiedUsage,
    est_input: u32,
    out_chars: usize,
    price: ModelPrice,
}

impl StreamTail {
    fn snapshot(&self) -> TailSnapshot {
        TailSnapshot {
            state: self.state.clone(),
            reservation: self.reservation.clone(),
            ctx: self.ctx.clone(),
            usage: self.usage,
            est_input: self.est_input,
            out_chars: self.out_chars,
            price: self.price,
        }
    }
}

impl Drop for StreamTail {
    fn drop(&mut self) {
        let t = self.snapshot();
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(t.finish());
            }
            Err(_) => {
                tracing::error!(request_id = %self.ctx.model, "流结束但无运行时，预扣等待启动对账");
            }
        }
    }
}

impl TailSnapshot {
    async fn finish(self) {
        // 上游没给 usage（老后端不认 stream_options，或中途断流）：
        // 按已收到的字符估算。**记 0 等于白嫖**。
        let estimated = self.usage.is_empty();
        let usage = if estimated {
            estimate_usage(self.est_input, self.out_chars)
        } else {
            self.usage
        };

        let cost = PriceTable::llm_cost(
            &self.price,
            usage.input_tokens,
            usage.cached_tokens,
            usage.output_tokens,
        );
        self.reservation.settle(cost).await;

        self.state
            .keys
            .add_usage(
                &self.ctx.node,
                (usage.input_tokens + usage.output_tokens) as i64,
                Instant::now(),
            )
            .await;

        let mut ctx = self.ctx;
        ctx.estimated = estimated;
        usage::write(
            &self.state.pool,
            &log_entry(&ctx, usage, estimated, elapsed_ms(ctx.started), cost),
        )
        .await
        .ok();
    }
}

/// 成功路径的落库条目。失败路径另走 [`log_failure`]。
fn log_entry(
    ctx: &LogCtx,
    u: UnifiedUsage,
    estimated: bool,
    latency_ms: i64,
    cost: i64,
) -> usage::LogEntry {
    usage::LogEntry {
        request_id: ctx.request_id.clone(),
        account_id: ctx.account_id,
        kind: "llm",
        protocol: Some(ctx.protocol.as_str()),
        target: ctx.model.clone(),
        node_id: Some(ctx.node.node_id),
        node_name: Some(ctx.node_name.clone()),
        key_id: Some(ctx.node.id),
        rotated_count: ctx.rotated,
        stream: true,
        status: "ok",
        error_kind: None,
        error_message: None,
        queue_wait_ms: ctx.queue_wait_ms,
        latency_ms,
        usage: u,
        tokens_source: if estimated { "estimated" } else { "upstream" },
        image_count: 0,
        cost_micro: cost,
        notes: ctx.notes.clone(),
    }
}

// ── 流的状态机 ──────────────────────────────────────────────────────────────

/// 上游事件 → 客户端帧 的纯状态机。**不碰数据库、不碰网络**，
/// 所以三个协议的终止信号可以逐字节断言。
struct FramePump {
    renderer: OutRenderer,
    pending: VecDeque<SseFrame>,
    upstream_done: bool,
    saw_done: bool,
}

impl FramePump {
    fn new(renderer: OutRenderer, lead: Vec<SseFrame>) -> Self {
        Self {
            renderer,
            pending: lead.into(),
            upstream_done: false,
            saw_done: false,
        }
    }

    /// 喂一个上游事件，产出本轮要写出的帧。
    fn feed(&mut self, ev: &UnifiedEvent) -> Vec<SseFrame> {
        if matches!(ev, UnifiedEvent::Done { .. }) {
            self.saw_done = true;
        }
        let frames = self.renderer.push(ev);
        self.pending.extend(frames.iter().cloned());
        frames
    }

    /// 上游 EOF。**必须补终止事件**：三个协议都靠它收尾，
    /// 漏掉 `message_stop` 会让 Anthropic SDK 永久挂起。
    ///
    /// 兜底 usage 必须一并带上：客户端看到的数字要和账上扣的一致。
    fn finish(&mut self, fallback: UnifiedUsage) -> Vec<SseFrame> {
        self.upstream_done = true;
        if self.saw_done {
            return Vec::new();
        }
        self.saw_done = true;
        let frames = self.renderer.push(&UnifiedEvent::Done {
            usage: fallback,
            finish_reason: Some(FinishReason::Stop),
        });
        self.pending.extend(frames.clone());
        frames
    }

    fn next_frame(&mut self) -> Option<SseFrame> {
        self.pending.pop_front()
    }

    fn exhausted(&self) -> bool {
        self.pending.is_empty() && self.upstream_done
    }
}

struct StreamState {
    src: std::pin::Pin<Box<dyn futures::Stream<Item = UnifiedEvent> + Send>>,
    pump: FramePump,
    tail: StreamTail,
}

type ByteStream = futures::stream::BoxStream<'static, Result<Bytes, std::io::Error>>;

fn build_stream(
    src: impl futures::Stream<Item = UnifiedEvent> + Send + 'static,
    renderer: OutRenderer,
    lead: Vec<SseFrame>,
    tail: StreamTail,
) -> ByteStream {
    futures::stream::unfold(
        StreamState {
            src: Box::pin(src),
            pump: FramePump::new(renderer, lead),
            tail,
        },
        |mut st| async move {
            loop {
                if let Some(f) = st.pump.next_frame() {
                    return Some((Ok(Bytes::from(f.encode())), st));
                }
                if st.pump.upstream_done {
                    return None; // st 析构 → StreamTail::drop → 结算
                }
                match st.src.next().await {
                    Some(ev) => {
                        match &ev {
                            UnifiedEvent::TextDelta { text }
                            | UnifiedEvent::ReasoningDelta { text } => {
                                st.tail.out_chars += text.chars().count()
                            }
                            UnifiedEvent::Done { usage, .. } => st.tail.usage = *usage,
                            _ => {}
                        }
                        st.pump.feed(&ev);
                    }
                    None => {
                        let fb = estimate_usage(st.tail.est_input, st.tail.out_chars);
                        st.pump.finish(fb);
                    }
                }
            }
        },
    )
    .boxed()
}

// ── 响应组装 ────────────────────────────────────────────────────────────────

fn json_response(body: Value) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-request-id", uuid::Uuid::new_v4().to_string())
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn sse_response(frames: ByteStream) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no") // 关闭 nginx 缓冲，否则流式退化成阻塞
        .body(Body::from_stream(frames))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn render_once(protocol: Protocol, r: &UnifiedResponse) -> Value {
    let created = chrono::Utc::now().timestamp();
    match protocol {
        Protocol::Chat => chat::render_response(r, created),
        Protocol::Responses => responses::render_response(r, created),
        Protocol::Messages => messages::render_response(r),
    }
}

async fn log_failure(
    s: &Arc<AppState>,
    acct: &Account,
    model: &str,
    stream: bool,
    protocol: Protocol,
    request_id: &str,
    started: Instant,
    e: &ApiError,
) {
    usage::write(
        &s.pool,
        &usage::LogEntry {
            request_id: request_id.to_string(),
            account_id: acct.id,
            kind: "llm",
            protocol: Some(protocol.as_str()),
            target: model.to_string(),
            node_id: None,
            node_name: None,
            key_id: None,
            rotated_count: 0,
            stream,
            status: "error",
            error_kind: Some(e.kind.openai_type().to_string()),
            error_message: Some(e.message.clone()),
            queue_wait_ms: 0,
            latency_ms: elapsed_ms(started),
            usage: UnifiedUsage::default(),
            tokens_source: "none",
            image_count: 0,
            cost_micro: 0,
            notes: Vec::new(),
        },
    )
    .await
    .ok();
}

// ── 自身用量查询 ────────────────────────────────────────────────────────────

pub async fn usage_info(
    State(s): State<Arc<AppState>>,
    a: Authed,
) -> ApiResult<Json<Value>> {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let row: (i64, i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(cost_micro),0), COALESCE(SUM(prompt_tokens+completion_tokens),0), COUNT(*)
         FROM usage_daily WHERE account_id = ? AND day = ?",
    )
    .bind(a.0.id)
    .bind(&today)
    .fetch_one(&s.pool)
    .await?;

    let by_protocol: Vec<(String, i64)> = sqlx::query_as(
        "SELECT COALESCE(protocol,'llm'), COUNT(*) FROM request_log
         WHERE account_id = ? AND created_at >= strftime('%s','now','-30 days')
         GROUP BY 1",
    )
    .bind(a.0.id)
    .fetch_all(&s.pool)
    .await
    .unwrap_or_default();

    Ok(Json(json!({
        "account": a.0.name,
        "balance_micro": a.0.balance_micro,
        "held_micro": a.0.held_micro,
        "available_micro": a.0.available_micro(),
        "today": { "cost_micro": row.0, "tokens": row.1, "requests": row.2, "day": today },
        "requests_by_protocol_30d": by_protocol
            .into_iter()
            .map(|(k, v)| (k, json!(v)))
            .collect::<serde_json::Map<_, _>>(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> UnifiedRequest {
        UnifiedRequest {
            model: "qwen3".into(),
            messages: vec![UnifiedMessage::user_text("你好")],
            ..Default::default()
        }
    }

    /// 把一个泵跑到结束，返回全部帧。
    fn drain(protocol: Protocol, events: &[UnifiedEvent], est: u32) -> Vec<SseFrame> {
        let (r, lead) = renderer_for(protocol, &req(), est);
        let mut pump = FramePump::new(r, lead);
        let mut out_chars = 0usize;
        for e in events {
            match e {
                UnifiedEvent::TextDelta { text } | UnifiedEvent::ReasoningDelta { text } => {
                    out_chars += text.chars().count()
                }
                _ => {}
            }
            pump.feed(e);
        }
        pump.finish(estimate_usage(est, out_chars));
        let mut out = Vec::new();
        while let Some(f) = pump.next_frame() {
            out.push(f);
        }
        assert!(pump.exhausted());
        out
    }

    fn done() -> UnifiedEvent {
        UnifiedEvent::Done {
            usage: UnifiedUsage {
                input_tokens: 11,
                output_tokens: 7,
                cached_tokens: 0,
                reasoning_tokens: 0,
            },
            finish_reason: Some(FinishReason::Stop),
        }
    }

    #[test]
    fn chat_stream_ends_with_done_sentinel() {
        let frames = drain(
            Protocol::Chat,
            &[UnifiedEvent::TextDelta { text: "hi".into() }, done()],
            20,
        );
        assert_eq!(frames.last().unwrap().data, "[DONE]");
        // 末块 usage 帧的 choices 必须是空数组
        let usage_frame = &frames[frames.len() - 2];
        let v: Value = serde_json::from_str(&usage_frame.data).unwrap();
        assert_eq!(v["choices"].as_array().unwrap().len(), 0);
        assert_eq!(v["usage"]["prompt_tokens"], 11);
    }

    /// Anthropic 没有 [DONE]，message_stop 就是终止信号。漏发会让 SDK 永久挂起。
    #[test]
    fn messages_stream_ends_with_message_stop() {
        let frames = drain(
            Protocol::Messages,
            &[UnifiedEvent::TextDelta { text: "hi".into() }, done()],
            20,
        );
        assert_eq!(frames.last().unwrap().event, Some("message_stop"));
        assert!(frames.iter().all(|f| f.data != "[DONE]"));
    }

    #[test]
    fn messages_stream_starts_with_message_start_carrying_estimated_input() {
        let frames = drain(Protocol::Messages, &[done()], 4242);
        let first: Value = serde_json::from_str(&frames[0].data).unwrap();
        assert_eq!(frames[0].event, Some("message_start"));
        assert_eq!(first["message"]["usage"]["input_tokens"], 4242);
    }

    #[test]
    fn responses_stream_every_frame_carries_a_monotonic_sequence_number() {
        let frames = drain(
            Protocol::Responses,
            &[
                UnifiedEvent::TextDelta { text: "a".into() },
                UnifiedEvent::TextDelta { text: "b".into() },
                done(),
            ],
            20,
        );
        let seqs: Vec<i64> = frames
            .iter()
            .map(|f| {
                serde_json::from_str::<Value>(&f.data).unwrap()["sequence_number"]
                    .as_i64()
                    .unwrap()
            })
            .collect();
        assert!(seqs.len() >= 4);
        assert!(
            seqs.windows(2).all(|w| w[0] + 1 == w[1]),
            "sequence_number 必须流内全局单调递增，实得 {seqs:?}"
        );
        assert_eq!(
            frames.last().unwrap().event,
            Some("response.completed")
        );
    }

    /// 回归：上游中途断流（没发 Done）时也必须收尾。
    /// 没有这一步，Anthropic 客户端会一直挂着，槽位永远不释放。
    #[test]
    fn truncated_upstream_stream_still_terminates() {
        for protocol in [Protocol::Chat, Protocol::Responses, Protocol::Messages] {
            let frames = drain(
                protocol,
                &[UnifiedEvent::TextDelta { text: "半句".into() }],
                20,
            );
            let last = frames.last().unwrap();
            match protocol {
                Protocol::Chat => assert_eq!(last.data, "[DONE]"),
                Protocol::Messages => assert_eq!(last.event, Some("message_stop")),
                Protocol::Responses => assert_eq!(last.event, Some("response.completed")),
            }
        }
    }

    /// 上游已经正常收过尾时，不能再补一次终止事件。
    #[test]
    fn finish_is_idempotent_after_done() {
        let (r, lead) = renderer_for(Protocol::Chat, &req(), 20);
        let mut pump = FramePump::new(r, lead);
        pump.feed(&done());
        let mut count = 0;
        while let Some(_) = pump.next_frame() {
            count += 1;
        }
        let extra = pump.finish(UnifiedUsage::default());
        assert!(extra.is_empty(), "重复收尾会多发一帧终止信号");
        let mut after = 0;
        while let Some(_) = pump.next_frame() {
            after += 1;
        }
        assert_eq!(after, 0);
        assert!(count > 0);
    }

    /// 上游的 usage 是空的（老后端不认 stream_options），
    /// 此时必须走估算而不是记 0 —— 记 0 等于白嫖。
    #[test]
    fn missing_upstream_usage_falls_back_to_estimate() {
        let frames = drain(
            Protocol::Chat,
            &[UnifiedEvent::TextDelta { text: "abcd".into() }],
            100,
        );
        let usage_frame = &frames[frames.len() - 2];
        let v: Value = serde_json::from_str(&usage_frame.data).unwrap();
        // 4 个字符至少也该估出 1 个 token，而不是 0
        assert!(v["usage"]["completion_tokens"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn tool_call_args_are_streamed_as_delta_fragments() {
        let frames = drain(
            Protocol::Messages,
            &[
                UnifiedEvent::ToolCallStart {
                    index: 0,
                    id: "toolu_1".into(),
                    name: "f".into(),
                },
                UnifiedEvent::ToolCallArgsDelta {
                    index: 0,
                    fragment: "{\"a\":".into(),
                },
                done(),
            ],
            20,
        );
        let arg_frames: Vec<&SseFrame> = frames
            .iter()
            .filter(|f| f.data.contains("input_json_delta"))
            .collect();
        assert_eq!(arg_frames.len(), 1);
    }
}
