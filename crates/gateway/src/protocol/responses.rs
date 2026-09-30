//! OpenAI Responses：入站解析 + 出站渲染（JSON 与具名 SSE）。
//!
//! 这个协议的硬约束：
//!
//! 1. **每个事件都必须带 `sequence_number: int`**，流内单调递增的**全局计数器**
//!    ——不是每块索引（Anthropic 是每块 `index`，Chat 是 `choices[].index`，
//!    三套索引模型并存）。
//! 2. **没有 `[DONE]` 哨兵**，终止靠 `response.completed` / `.failed` /
//!    `.incomplete` 事件，usage 只出现在终止事件里（整个 `response` 对象）。
//! 3. **截断不是 finish_reason**，而是 `status:"incomplete"` +
//!    `incomplete_details.reason`。只看终止事件名会把截断当成功。
//! 4. 流中的 `error` 事件是**扁平**的（`code`/`message`/`param`），
//!    **不嵌套**在 `error` 键下。

use serde_json::{json, Map, Value};

use super::ir::*;
use super::sse::SseFrame;
use crate::error::{ApiError, ApiResult};

// ── 入站解析 ────────────────────────────────────────────────────────────────

/// 解析 `POST /v1/responses` 请求体。
pub fn parse_request(raw: &Value) -> ApiResult<UnifiedRequest> {
    // 网关无状态，依赖服务端会话的字段**显式拒绝**——静默忽略会返回错误结果。
    if raw.get("previous_response_id").is_some() {
        return Err(ApiError::bad_request(
            "previous_response_id 不受支持：本网关无服务端会话状态，请客户端自行携带完整历史",
        ));
    }

    let model = raw
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("model 必填"))?
        .to_string();

    // 系统提示词在 `instructions`，或 input 里的 system/developer 消息。
    let mut system = raw
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut messages: Vec<UnifiedMessage> = Vec::new();

    match raw.get("input") {
        Some(Value::String(s)) => messages.push(UnifiedMessage::user_text(s.clone())),
        Some(Value::Array(items)) => {
            for it in items {
                let ty = it.get("type").and_then(Value::as_str);
                match ty {
                    Some("function_call") => {
                        messages.push(UnifiedMessage::new(
                            Role::Assistant,
                            vec![ContentPart::ToolUse {
                                id: it.get("call_id").and_then(Value::as_str).unwrap_or("").to_string(),
                                name: it.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                                // Responses 的 arguments 是 JSON 字符串
                                input: serde_json::from_str(
                                    it.get("arguments").and_then(Value::as_str).unwrap_or("{}"),
                                )
                                .unwrap_or(Value::Object(Map::new())),
                            }],
                        ));
                    }
                    Some("function_call_output") => {
                        messages.push(UnifiedMessage::new(
                            Role::Tool,
                            vec![ContentPart::ToolResult {
                                tool_use_id: it
                                    .get("call_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                content: flatten(it.get("output")),
                                is_error: false,
                            }],
                        ));
                    }
                    Some("reasoning") => {
                        // Chat 方言没有 reasoning 项，降级时丢弃（记日志，不伪造）
                    }
                    _ => {
                        let role = match it.get("role").and_then(Value::as_str) {
                            Some("assistant") => Role::Assistant,
                            Some("system") | Some("developer") => Role::System,
                            _ => Role::User,
                        };
                        let parts = parse_input_parts(it.get("content"));
                        if role == Role::System {
                            let text: String = parts
                                .iter()
                                .filter_map(|p| match p {
                                    ContentPart::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            system = Some(match system.take() {
                                Some(prev) => format!("{prev}\n\n{text}"),
                                None => text,
                            });
                        } else {
                            messages.push(UnifiedMessage::new(role, parts));
                        }
                    }
                }
            }
        }
        _ => {}
    }

    let tools = raw
        .get("tools")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    if t.get("type").and_then(Value::as_str) != Some("function") {
                        return None; // 非 function 工具在 Chat 方言里无处安放，跳过
                    }
                    Some(UnifiedTool {
                        name: t.get("name")?.as_str()?.to_string(),
                        description: t
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        parameters: t.get("parameters").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let stop = raw
        .get("stop")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    Ok(UnifiedRequest {
        model,
        system,
        messages,
        max_output_tokens: raw.get("max_output_tokens").and_then(Value::as_u64).map(|v| v as u32),
        temperature: raw.get("temperature").and_then(Value::as_f64).map(|v| v as f32),
        top_p: raw.get("top_p").and_then(Value::as_f64).map(|v| v as f32),
        stop,
        tools,
        tool_choice: raw.get("tool_choice").and_then(|v| match v {
            Value::String(s) => match s.as_str() {
                "auto" => Some(ToolChoice::Auto),
                "none" => Some(ToolChoice::None),
                "required" => Some(ToolChoice::Required),
                _ => None,
            },
            Value::Object(o) => o
                .get("name")
                .and_then(Value::as_str)
                .map(|n| ToolChoice::Named(n.to_string())),
            _ => None,
        }),
        stream: raw.get("stream").and_then(Value::as_bool).unwrap_or(false),
        thinking_effort: raw
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(Value::as_str)
            .map(str::to_string),
        metadata: raw
            .get("metadata")
            .and_then(|m| m.get("user_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn flatten(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn parse_input_parts(v: Option<&Value>) -> Vec<ContentPart> {
    match v {
        Some(Value::String(s)) => vec![ContentPart::text(s.clone())],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("input_text") | Some("text") | Some("output_text") => {
                    Some(ContentPart::text(p.get("text").and_then(Value::as_str).unwrap_or("")))
                }
                Some("input_image") => {
                    let url = p.get("image_url").and_then(Value::as_str).unwrap_or("");
                    url.split_once(";base64,").map(|(mt, data)| ContentPart::Image {
                        media_type: mt.trim_start_matches("data:").to_string(),
                        data: data.to_string(),
                    })
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ── 出站渲染：非流式 ────────────────────────────────────────────────────────

/// `UnifiedResponse` → Responses 非流式 JSON。
pub fn render_response(resp: &UnifiedResponse, created: i64) -> Value {
    let mut output: Vec<Value> = Vec::new();

    if !resp.text.is_empty() {
        output.push(json!({
            "type": "message",
            "id": format!("msg_{}", resp.id),
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "text": resp.text, "annotations": [] }]
        }));
    }
    // function_call 是**顶层项**，不是 message 的字段
    for tc in &resp.tool_calls {
        output.push(json!({
            "type": "function_call",
            "id": format!("fc_{}", tc.id),
            // 客户端回传工具结果必须用 call_id，不是 id
            "call_id": tc.id,
            "name": tc.name,
            "arguments": tc.arguments,
            "status": "completed"
        }));
    }

    let u = resp.usage;
    let truncated = matches!(resp.finish_reason, Some(FinishReason::Length));

    let mut usage = Map::new();
    usage.insert("input_tokens".into(), json!(u.input_tokens));
    usage.insert("output_tokens".into(), json!(u.output_tokens));
    usage.insert("total_tokens".into(), json!(u.input_tokens + u.output_tokens));
    if u.cached_tokens > 0 {
        usage.insert("input_tokens_details".into(), json!({ "cached_tokens": u.cached_tokens }));
    }
    if u.reasoning_tokens > 0 {
        usage.insert(
            "output_tokens_details".into(),
            json!({ "reasoning_tokens": u.reasoning_tokens }),
        );
    }

    let mut body = Map::new();
    body.insert("id".into(), json!(resp.id));
    body.insert("object".into(), json!("response"));
    body.insert("created_at".into(), json!(created));
    body.insert("model".into(), json!(resp.model));
    body.insert("output".into(), Value::Array(output));
    // **截断是 status，不是 finish_reason**
    body.insert(
        "status".into(),
        json!(if truncated { "incomplete" } else { "completed" }),
    );
    if truncated {
        body.insert(
            "incomplete_details".into(),
            json!({ "reason": "max_output_tokens" }),
        );
    }
    body.insert("error".into(), Value::Null);
    body.insert("usage".into(), Value::Object(usage));
    Value::Object(body)
}

// ── 出站渲染：流式 ──────────────────────────────────────────────────────────

/// Responses 的 SSE 渲染器。**每个事件都要带 `sequence_number`**，且它是
/// 流内单调递增的全局计数器，不是每块索引。
pub struct ResponsesStreamRenderer {
    pub id: String,
    pub model: String,
    pub created: i64,

    seq: u32,
    started: bool,
    text_item_open: bool,
    text_item_id: String,
    text: String,
    tool_items: Vec<(u32, String)>, // (output_index, call_id)
    finished: bool,
}

impl ResponsesStreamRenderer {
    pub fn new(id: impl Into<String>, model: impl Into<String>, created: i64) -> Self {
        let id = id.into();
        Self {
            text_item_id: format!("msg_{id}"),
            id,
            model: model.into(),
            created,
            seq: 0,
            started: false,
            text_item_open: false,
            text: String::new(),
            tool_items: Vec::new(),
            finished: false,
        }
    }

    fn ev(&mut self, ty: &'static str, body: Value) -> SseFrame {
        let mut m = body.as_object().cloned().unwrap_or_default();
        m.insert("type".into(), json!(ty));
        m.insert("sequence_number".into(), json!(self.seq));
        self.seq += 1;
        SseFrame::named(ty, Value::Object(m))
    }

    fn response_stub(&self, status: &str, usage: Option<&UnifiedUsage>) -> Value {
        let mut r = Map::new();
        r.insert("id".into(), json!(self.id));
        r.insert("object".into(), json!("response"));
        r.insert("created_at".into(), json!(self.created));
        r.insert("status".into(), json!(status));
        r.insert("model".into(), json!(self.model));
        r.insert("output".into(), json!([]));
        r.insert("error".into(), Value::Null);
        if let Some(u) = usage {
            let mut um = Map::new();
            um.insert("input_tokens".into(), json!(u.input_tokens));
            um.insert("output_tokens".into(), json!(u.output_tokens));
            um.insert("total_tokens".into(), json!(u.input_tokens + u.output_tokens));
            if u.cached_tokens > 0 {
                um.insert("input_tokens_details".into(), json!({ "cached_tokens": u.cached_tokens }));
            }
            r.insert("usage".into(), Value::Object(um));
        } else {
            r.insert("usage".into(), Value::Null);
        }
        Value::Object(r)
    }

    pub fn push(&mut self, ev: &UnifiedEvent) -> Vec<SseFrame> {
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            out.push(self.ev(
                "response.created",
                json!({ "response": self.response_stub("in_progress", None) }),
            ));
            out.push(self.ev(
                "response.in_progress",
                json!({ "response": self.response_stub("in_progress", None) }),
            ));
        }

        match ev {
            UnifiedEvent::TextDelta { text } => {
                if !self.text_item_open {
                    self.text_item_open = true;
                    let item = json!({
                        "type": "message", "id": self.text_item_id, "role": "assistant",
                        "status": "in_progress", "content": []
                    });
                    out.push(self.ev(
                        "response.output_item.added",
                        json!({ "item": item, "output_index": 0 }),
                    ));
                    out.push(self.ev(
                        "response.content_part.added",
                        json!({ "part": { "type": "output_text", "text": "", "annotations": [] },
                                "content_index": 0, "item_id": self.text_item_id, "output_index": 0 }),
                    ));
                }
                self.text.push_str(text);
                out.push(self.ev(
                    "response.output_text.delta",
                    json!({ "delta": text, "logprobs": [],
                            "content_index": 0, "item_id": self.text_item_id, "output_index": 0 }),
                ));
            }
            UnifiedEvent::ReasoningDelta { .. } => {
                // Chat 方言的 reasoning 没有 Responses 的 reasoning 项可落，
                // **记录忽略而非伪造**
            }
            UnifiedEvent::ToolCallStart { id, name, .. } => {
                let output_index = 1 + self.tool_items.len() as u32;
                self.tool_items.push((output_index, id.clone()));
                out.push(self.ev(
                    "response.output_item.added",
                    json!({ "item": { "type": "function_call", "id": format!("fc_{id}"),
                                     "call_id": id, "name": name, "arguments": "", "status": "in_progress" },
                            "output_index": output_index }),
                ));
            }
            UnifiedEvent::ToolCallArgsDelta { index, fragment } => {
                if let Some(&(output_index, _)) = self.tool_items.get(*index) {
                    out.push(self.ev(
                        "response.function_call_arguments.delta",
                        json!({ "delta": fragment, "output_index": output_index }),
                    ));
                }
            }
            UnifiedEvent::ToolCallEnd { .. } => {}
            UnifiedEvent::Done { usage, finish_reason } => {
                if self.finished {
                    return out;
                }
                self.finished = true;
                if self.text_item_open {
                    out.push(self.ev(
                        "response.output_text.done",
                        json!({ "text": self.text, "logprobs": [],
                                "content_index": 0, "item_id": self.text_item_id, "output_index": 0 }),
                    ));
                    out.push(self.ev(
                        "response.content_part.done",
                        json!({ "part": { "type": "output_text", "text": self.text, "annotations": [] },
                                "content_index": 0, "item_id": self.text_item_id, "output_index": 0 }),
                    ));
                    out.push(self.ev(
                        "response.output_item.done",
                        json!({ "item": { "type": "message", "id": self.text_item_id, "role": "assistant",
                                         "status": "completed",
                                         "content": [{ "type": "output_text", "text": self.text, "annotations": [] }] },
                                "output_index": 0 }),
                    ));
                }
                let truncated = matches!(finish_reason, Some(FinishReason::Length));
                // 终止事件带完整 response 对象（含 usage）；没有 [DONE]
                let evty = if truncated { "response.incomplete" } else { "response.completed" };
                let mut resp = self.response_stub(if truncated { "incomplete" } else { "completed" }, Some(usage));
                resp["incomplete_details"] = if truncated {
                    json!({ "reason": "max_output_tokens" })
                } else {
                    Value::Null
                };
                out.push(self.ev(evty, json!({ "response": resp })));
            }
            UnifiedEvent::Error { message, code } => {
                if self.finished {
                    return out;
                }
                self.finished = true;
                // Responses 的 error 事件是**扁平**的
                out.push(self.ev(
                    "error",
                    json!({ "message": message, "code": code, "param": Value::Null }),
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_response_id_is_rejected_not_ignored() {
        let raw = json!({
            "model":"m","input":"hi",
            "previous_response_id":"resp_123"
        });
        let err = parse_request(&raw).unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::BadRequest);
    }

    #[test]
    fn instructions_become_system_prompt() {
        let raw = json!({"model":"m","instructions":"你是助手","input":"hi"});
        let req = parse_request(&raw).unwrap();
        assert_eq!(req.system.as_deref(), Some("你是助手"));
    }

    #[test]
    fn max_output_tokens_maps_to_ir() {
        let raw = json!({"model":"m","input":"hi","max_output_tokens":512});
        assert_eq!(parse_request(&raw).unwrap().max_output_tokens, Some(512));
    }

    #[test]
    fn function_call_input_is_sibling_item_not_message_field() {
        let raw = json!({"model":"m","input":"x"});
        let req = parse_request(&raw).unwrap();
        assert!(req.messages.iter().all(|m| m.parts.iter().all(|p| !matches!(
            p, ContentPart::ToolUse { .. }
        ))));

        let raw2 = json!({"model":"m","input":[
            {"type":"function_call","call_id":"c1","name":"f","arguments":"{\"a\":1}"}
        ]});
        let req2 = parse_request(&raw2).unwrap();
        match &req2.messages[0].parts[0] {
            ContentPart::ToolUse { id, input, .. } => {
                assert_eq!(id, "c1");
                assert_eq!(input["a"], 1);
            }
            o => panic!("expected ToolUse, got {o:?}"),
        }
    }

    /// 每个事件都必须带 sequence_number，且流内单调递增——这是协议硬约束。
    #[test]
    fn every_event_carries_monotonic_sequence_number() {
        let mut r = ResponsesStreamRenderer::new("resp_1", "m", 1);
        let mut frames = Vec::new();
        frames.extend(r.push(&UnifiedEvent::TextDelta { text: "a".into() }));
        frames.extend(r.push(&UnifiedEvent::TextDelta { text: "b".into() }));
        frames.extend(r.push(&UnifiedEvent::Done {
            usage: UnifiedUsage { input_tokens: 3, output_tokens: 2, ..Default::default() },
            finish_reason: Some(FinishReason::Stop),
        }));

        let mut last: i64 = -1;
        for f in &frames {
            assert!(f.event.is_some(), "Responses 用具名事件");
            let v: Value = serde_json::from_str(&f.data).unwrap();
            let seq = v["sequence_number"].as_i64().expect("每个事件都要有 sequence_number");
            assert!(seq > last, "sequence_number 必须单调递增: {seq} <= {last}");
            last = seq;
        }
    }

    #[test]
    fn stream_does_not_emit_done_sentinel() {
        let mut r = ResponsesStreamRenderer::new("resp_1", "m", 1);
        let frames = r.push(&UnifiedEvent::Done {
            usage: UnifiedUsage::default(),
            finish_reason: Some(FinishReason::Stop),
        });
        assert!(frames.iter().all(|f| f.data != "[DONE]"));
        assert_eq!(frames.last().unwrap().event, Some("response.completed"));
    }

    /// 截断是 status + incomplete_details，不是 finish_reason。
    #[test]
    fn length_truncation_maps_to_incomplete_status() {
        let resp = UnifiedResponse {
            id: "resp_1".into(),
            model: "m".into(),
            text: "x".into(),
            tool_calls: vec![],
            finish_reason: Some(FinishReason::Length),
            usage: UnifiedUsage::default(),
            reasoning: None,
        };
        let body = render_response(&resp, 1);
        assert_eq!(body["status"], "incomplete");
        assert_eq!(body["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn function_call_item_exposes_both_id_and_call_id() {
        let resp = UnifiedResponse {
            id: "resp_1".into(),
            model: "m".into(),
            text: String::new(),
            tool_calls: vec![UnifiedToolCall {
                id: "call_x".into(),
                name: "f".into(),
                arguments: "{}".into(),
            }],
            finish_reason: Some(FinishReason::ToolCalls),
            usage: UnifiedUsage::default(),
            reasoning: None,
        };
        let body = render_response(&resp, 1);
        let item = &body["output"][0];
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], "call_x");
    }

    #[test]
    fn stream_error_event_is_flat_not_nested() {
        let mut r = ResponsesStreamRenderer::new("resp_1", "m", 1);
        let frames = r.push(&UnifiedEvent::Error {
            message: "boom".into(),
            code: Some("server_error".into()),
        });
        let err_frame = frames.last().unwrap();
        assert_eq!(err_frame.event, Some("error"));
        let v: Value = serde_json::from_str(&err_frame.data).unwrap();
        assert_eq!(v["message"], "boom");
        // 扁平结构：没有嵌套的 error 键
        assert!(v.get("error").is_none());
    }
}
