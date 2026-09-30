//! OpenAI Chat Completions：入站解析 + 出站渲染（JSON 与 SSE 各一）。
//!
//! 解析与渲染都与其他两个协议同构（IR ↔ Chat），只是字段名与终止信号
//! 是 Chat 自己的一套。

use serde_json::{json, Map, Value};

use super::ir::*;
use super::sse::SseFrame;
use crate::error::{ApiError, ApiResult};

// ── 入站解析 ────────────────────────────────────────────────────────────────

/// 解析 `POST /v1/chat/completions` 请求体。
pub fn parse_request(raw: &Value) -> ApiResult<UnifiedRequest> {
    let model = raw
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("model 必填"))?
        .to_string();

    let mut system = None;
    let mut messages = Vec::new();

    let arr = raw
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("messages 必填"))?;

    for m in arr {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            "system" | "developer" => {
                // Chat 把系统提示词放在 messages 里；IR 用独立的 system 字段。
                if let Some(text) = content_to_text(m.get("content")) {
                    system = Some(match system.take() {
                        Some(prev) => format!("{prev}\n\n{text}"),
                        None => text,
                    });
                }
            }
            "tool" => {
                messages.push(UnifiedMessage::new(
                    Role::Tool,
                    vec![ContentPart::ToolResult {
                        tool_use_id: m
                            .get("tool_call_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        content: content_to_text(m.get("content")).unwrap_or_default(),
                        is_error: false,
                    }],
                ));
            }
            "assistant" => {
                let mut parts = Vec::new();
                if let Some(t) = content_to_text(m.get("content")) {
                    if !t.is_empty() {
                        parts.push(ContentPart::text(t));
                    }
                }
                if let Some(rc) = m.get("reasoning_content").and_then(Value::as_str) {
                    if !rc.is_empty() {
                        parts.push(ContentPart::Thinking { text: rc.to_string() });
                    }
                }
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for tc in calls {
                        let f = tc.get("function");
                        let args = f
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        // Chat 的 arguments 是字符串，IR 存已解析对象。
                        let input = serde_json::from_str(args).unwrap_or(Value::Object(Map::new()));
                        parts.push(ContentPart::ToolUse {
                            id: tc.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                            name: f
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            input,
                        });
                    }
                }
                messages.push(UnifiedMessage::new(Role::Assistant, parts));
            }
            _ => {
                messages.push(UnifiedMessage::new(
                    Role::User,
                    parse_user_content(m.get("content"))?,
                ));
            }
        }
    }

    let tools = raw
        .get("tools")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let f = t.get("function")?;
                    Some(UnifiedTool {
                        name: f.get("name")?.as_str()?.to_string(),
                        description: f
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        parameters: f.get("parameters").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(UnifiedRequest {
        model,
        system,
        messages,
        max_output_tokens: raw
            .get("max_completion_tokens")
            .or_else(|| raw.get("max_tokens"))
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        temperature: raw.get("temperature").and_then(Value::as_f64).map(|v| v as f32),
        top_p: raw.get("top_p").and_then(Value::as_f64).map(|v| v as f32),
        stop: parse_stop(raw.get("stop")),
        tools,
        tool_choice: raw.get("tool_choice").and_then(parse_tool_choice),
        stream: raw.get("stream").and_then(Value::as_bool).unwrap_or(false),
        thinking_effort: raw
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .map(str::to_string),
        metadata: raw.get("user").and_then(Value::as_str).map(str::to_string),
    })
}

fn content_to_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            let mut out = String::new();
            for p in arr {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn parse_user_content(v: Option<&Value>) -> ApiResult<Vec<ContentPart>> {
    match v {
        Some(Value::String(s)) => Ok(vec![ContentPart::text(s.clone())]),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for p in arr {
                match p.get("type").and_then(Value::as_str) {
                    Some("text") | Some("input_text") => {
                        if let Some(t) = p.get("text").and_then(Value::as_str) {
                            parts.push(ContentPart::text(t));
                        }
                    }
                    Some("image_url") => {
                        let url = p
                            .get("image_url")
                            .and_then(|i| i.get("url"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if let Some((mt, data)) = url.split_once(";base64,") {
                            let media_type = mt.trim_start_matches("data:").to_string();
                            parts.push(ContentPart::Image {
                                media_type,
                                data: data.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(parts)
        }
        _ => Ok(Vec::new()),
    }
}

fn parse_stop(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_tool_choice(v: &Value) -> Option<ToolChoice> {
    match v {
        Value::String(s) => match s.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => None,
        },
        Value::Object(o) => o
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .map(|n| ToolChoice::Named(n.to_string())),
        _ => None,
    }
}

// ── 出站渲染：非流式 ────────────────────────────────────────────────────────

/// `UnifiedResponse` → Chat 非流式 JSON。
pub fn render_response(resp: &UnifiedResponse, created: i64) -> Value {
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert("content".into(), json!(resp.text));
    if let Some(r) = &resp.reasoning {
        message.insert("reasoning_content".into(), json!(r));
    }
    if !resp.tool_calls.is_empty() {
        message.insert(
            "tool_calls".into(),
            Value::Array(
                resp.tool_calls
                    .iter()
                    .map(|tc| {
                        json!({
                            "id": tc.id,
                            "type": "function",
                            "function": { "name": tc.name, "arguments": tc.arguments }
                        })
                    })
                    .collect(),
            ),
        );
    }

    let u = resp.usage;
    let mut usage = Map::new();
    usage.insert("prompt_tokens".into(), json!(u.input_tokens));
    usage.insert("completion_tokens".into(), json!(u.output_tokens));
    usage.insert(
        "total_tokens".into(),
        json!(u.input_tokens + u.output_tokens),
    );
    if u.cached_tokens > 0 {
        usage.insert(
            "prompt_tokens_details".into(),
            json!({ "cached_tokens": u.cached_tokens }),
        );
    }
    if u.reasoning_tokens > 0 {
        usage.insert(
            "completion_tokens_details".into(),
            json!({ "reasoning_tokens": u.reasoning_tokens }),
        );
    }

    json!({
        "id": resp.id,
        "object": "chat.completion",
        "created": created,
        "model": resp.model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason_str(resp.finish_reason),
            "logprobs": Value::Null,
        }],
        "usage": Value::Object(usage),
    })
}

pub fn finish_reason_str(fr: Option<FinishReason>) -> &'static str {
    match fr {
        Some(FinishReason::Stop) | None => "stop",
        Some(FinishReason::Length) => "length",
        Some(FinishReason::ToolCalls) => "tool_calls",
        Some(FinishReason::ContentFilter) => "content_filter",
        Some(FinishReason::Other) => "stop",
    }
}

// ── 出站渲染：流式 ──────────────────────────────────────────────────────────

/// Chat 的 SSE 是**匿名事件**（只有 `data:`），终止靠 `[DONE]` 哨兵。
pub struct ChatStreamRenderer {
    pub id: String,
    pub model: String,
    pub created: i64,
    started: bool,
    finished: bool,
}

impl ChatStreamRenderer {
    pub fn new(id: impl Into<String>, model: impl Into<String>, created: i64) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            created,
            started: false,
            finished: false,
        }
    }

    fn envelope(&self, delta: Value, finish: Value) -> SseFrame {
        SseFrame::data_only(json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish, "logprobs": Value::Null }],
        }))
    }

    pub fn push(&mut self, ev: &UnifiedEvent) -> Vec<SseFrame> {
        let mut out = Vec::new();
        match ev {
            UnifiedEvent::TextDelta { text } => {
                if !self.started {
                    self.started = true;
                    out.push(self.envelope(json!({ "role": "assistant", "content": "" }), Value::Null));
                }
                out.push(self.envelope(json!({ "content": text }), Value::Null));
            }
            UnifiedEvent::ReasoningDelta { text } => {
                if !self.started {
                    self.started = true;
                    out.push(self.envelope(json!({ "role": "assistant", "content": "" }), Value::Null));
                }
                out.push(self.envelope(json!({ "reasoning_content": text }), Value::Null));
            }
            UnifiedEvent::ToolCallStart { index, id, name } => {
                if !self.started {
                    self.started = true;
                    out.push(self.envelope(json!({ "role": "assistant", "content": Value::Null }), Value::Null));
                }
                out.push(self.envelope(
                    json!({ "tool_calls": [{ "index": index, "id": id, "type": "function",
                        "function": { "name": name, "arguments": "" } }] }),
                    Value::Null,
                ));
            }
            UnifiedEvent::ToolCallArgsDelta { index, fragment } => {
                out.push(self.envelope(
                    json!({ "tool_calls": [{ "index": index,
                        "function": { "arguments": fragment } }] }),
                    Value::Null,
                ));
            }
            UnifiedEvent::ToolCallEnd { .. } => {}
            UnifiedEvent::Done { usage, finish_reason } => {
                if self.finished {
                    return out;
                }
                self.finished = true;
                if !self.started {
                    self.started = true;
                    out.push(self.envelope(
                        json!({ "role": "assistant", "content": "" }),
                        json!(finish_reason_str(*finish_reason)),
                    ));
                } else {
                    out.push(self.envelope(
                        json!({}),
                        json!(finish_reason_str(*finish_reason)),
                    ));
                }
                // 末块：choices 为空数组，只带 usage。这是 OpenAI 的既定形状，
                // 客户端会照此解析，**必须原样生成**。
                if !usage.is_empty() {
                    out.push(SseFrame::data_only(json!({
                        "id": self.id,
                        "object": "chat.completion.chunk",
                        "created": self.created,
                        "model": self.model,
                        "choices": [],
                        "usage": {
                            "prompt_tokens": usage.input_tokens,
                            "completion_tokens": usage.output_tokens,
                            "total_tokens": usage.input_tokens + usage.output_tokens,
                        }
                    })));
                }
                out.push(SseFrame::done());
            }
            UnifiedEvent::Error { message, .. } => {
                if self.finished {
                    return out;
                }
                self.finished = true;
                out.push(SseFrame::data_only(json!({
                    "error": { "message": message, "type": "api_error" }
                })));
                out.push(SseFrame::done());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_messages_are_hoisted_to_ir_system_field() {
        let raw = json!({
            "model":"m",
            "messages":[
                {"role":"system","content":"A"},
                {"role":"developer","content":"B"},
                {"role":"user","content":"hi"}
            ]
        });
        let req = parse_request(&raw).unwrap();
        assert_eq!(req.system.as_deref(), Some("A\n\nB"));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, Role::User);
    }

    #[test]
    fn tool_result_round_trips_through_parse() {
        let raw = json!({
            "model":"m",
            "messages":[{"role":"tool","tool_call_id":"c1","content":"42"}]
        });
        let req = parse_request(&raw).unwrap();
        match &req.messages[0].parts[0] {
            ContentPart::ToolResult { tool_use_id, content, .. } => {
                assert_eq!(tool_use_id, "c1");
                assert_eq!(content, "42");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn assistant_tool_call_arguments_parse_into_object() {
        let raw = json!({
            "model":"m",
            "messages":[{"role":"assistant","content":null,"tool_calls":[
                {"id":"c1","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}}
            ]}]
        });
        let req = parse_request(&raw).unwrap();
        match &req.messages[0].parts[0] {
            ContentPart::ToolUse { input, .. } => assert_eq!(input["a"], 1),
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn stream_ends_with_done_sentinel() {
        let mut r = ChatStreamRenderer::new("c1", "m", 1);
        r.push(&UnifiedEvent::TextDelta { text: "hi".into() });
        let frames = r.push(&UnifiedEvent::Done {
            usage: UnifiedUsage { input_tokens: 5, output_tokens: 2, ..Default::default() },
            finish_reason: Some(FinishReason::Stop),
        });
        assert_eq!(frames.last().unwrap(), &SseFrame::done());
    }

    #[test]
    fn final_usage_chunk_has_empty_choices() {
        let mut r = ChatStreamRenderer::new("c1", "m", 1);
        r.push(&UnifiedEvent::TextDelta { text: "hi".into() });
        let frames = r.push(&UnifiedEvent::Done {
            usage: UnifiedUsage { input_tokens: 5, output_tokens: 2, ..Default::default() },
            finish_reason: Some(FinishReason::Stop),
        });
        // 倒数第二帧是 usage 帧，choices 必须是空数组
        let usage_frame = &frames[frames.len() - 2];
        let v: Value = serde_json::from_str(&usage_frame.data).unwrap();
        assert_eq!(v["choices"].as_array().unwrap().len(), 0);
        assert_eq!(v["usage"]["prompt_tokens"], 5);
    }

    #[test]
    fn anonymous_events_have_no_event_name() {
        let mut r = ChatStreamRenderer::new("c1", "m", 1);
        let frames = r.push(&UnifiedEvent::TextDelta { text: "x".into() });
        assert!(frames.iter().all(|f| f.event.is_none()));
    }
}
