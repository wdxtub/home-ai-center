//! Anthropic Messages：入站解析 + 出站渲染（JSON 与具名 SSE）。
//!
//! 这个协议是三者里约束最紧的，动手前务必记住这四条：
//!
//! 1. **流式是具名事件**（`event:` + `data:`，且 `type` 在 JSON 里重复一次）；
//!    终止靠 `message_stop`，**没有 `[DONE]` 哨兵**——漏发它会让 Anthropic
//!    官方 SDK 的 `finalMessage()` 永久挂起。
//! 2. **`content_block_start` / `content_block_stop` 必须按 index 配对**；
//!    `text:""` 与 `input:{}` 这些空占位是契约的一部分，不是装饰。
//! 3. **usage 出现两次**：`message_start` 给输入 token（必须立即可得，不能推迟
//!    到结尾），`message_delta` 给**累积**输出 token（可出现多次，取最后一次，
//!    累加会虚增）。
//! 4. **缓存口径不同**：Anthropic 定义总输入 = `input_tokens` +
//!    `cache_creation` + `cache_read`（三项**相加**），而上游 Chat 的
//!    `prompt_tokens` **已经包含** `cached_tokens`。直接照抄会重复计算。

use serde_json::{json, Map, Value};

use super::ir::*;
use super::sse::SseFrame;
use crate::error::{ApiError, ApiResult};

// ── 入站解析 ────────────────────────────────────────────────────────────────

/// 解析 `POST /v1/messages` 请求体。
pub fn parse_request(raw: &Value) -> ApiResult<UnifiedRequest> {
    let model = raw
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("model 必填"))?
        .to_string();

    // Anthropic 的系统提示词在**顶层** `system` 字段，messages 里没有 system role。
    let system = match raw.get("system") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(blocks)) => {
            let mut out = String::new();
            for b in blocks {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                }
            }
            (!out.is_empty()).then_some(out)
        }
        _ => None,
    };

    let mut messages = Vec::new();
    if let Some(arr) = raw.get("messages").and_then(Value::as_array) {
        for m in arr {
            let role = match m.get("role").and_then(Value::as_str) {
                Some("assistant") => Role::Assistant,
                Some("system") | Some("developer") => Role::System,
                _ => Role::User,
            };
            messages.push(UnifiedMessage::new(role, parse_blocks(m.get("content"))?));
        }
    }

    let tools = raw
        .get("tools")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    // Anthropic 的 schema 字段叫 `input_schema`，不是 `parameters`。
                    let name = t.get("name")?.as_str()?.to_string();
                    Some(UnifiedTool {
                        name,
                        description: t
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        parameters: t.get("input_schema").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let stop: Vec<String> = raw
        .get("stop_sequences")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    let thinking_effort = raw
        .get("thinking")
        .and_then(|t| t.get("effort"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            raw.get("output_config")
                .and_then(|o| o.get("effort"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });

    Ok(UnifiedRequest {
        model,
        system,
        messages,
        // max_tokens 在 Anthropic 是必填；缺失时留 None，由调用方注入节点默认值。
        max_output_tokens: raw.get("max_tokens").and_then(Value::as_u64).map(|v| v as u32),
        temperature: raw.get("temperature").and_then(Value::as_f64).map(|v| v as f32),
        top_p: raw.get("top_p").and_then(Value::as_f64).map(|v| v as f32),
        stop,
        tools,
        tool_choice: raw.get("tool_choice").and_then(parse_tool_choice),
        stream: raw.get("stream").and_then(Value::as_bool).unwrap_or(false),
        thinking_effort,
        metadata: raw
            .get("metadata")
            .and_then(|m| m.get("user_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn parse_tool_choice(v: &Value) -> Option<ToolChoice> {
    match v.get("type").and_then(Value::as_str) {
        Some("auto") => Some(ToolChoice::Auto),
        Some("any") => Some(ToolChoice::Required), // Anthropic 的 any == Chat 的 required
        Some("none") => Some(ToolChoice::None),
        Some("tool") => v
            .get("name")
            .and_then(Value::as_str)
            .map(|n| ToolChoice::Named(n.to_string())),
        _ => None,
    }
}

fn parse_blocks(v: Option<&Value>) -> ApiResult<Vec<ContentPart>> {
    match v {
        Some(Value::String(s)) => Ok(vec![ContentPart::text(s.clone())]),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for b in arr {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            parts.push(ContentPart::text(t));
                        }
                    }
                    Some("image") => {
                        let src = b.get("source");
                        if let Some(d) = src.and_then(|s| s.get("data")).and_then(Value::as_str) {
                            parts.push(ContentPart::Image {
                                media_type: src
                                    .and_then(|s| s.get("media_type"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("image/png")
                                    .to_string(),
                                data: d.to_string(),
                            });
                        }
                    }
                    Some("tool_use") => {
                        parts.push(ContentPart::ToolUse {
                            id: b.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                            name: b.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                            // Anthropic 的 `input` 已经是对象，直接进 IR。
                            input: b.get("input").cloned().unwrap_or(Value::Object(Map::new())),
                        });
                    }
                    Some("tool_result") => {
                        parts.push(ContentPart::ToolResult {
                            tool_use_id: b
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            content: flatten_tool_result(b.get("content")),
                            is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                        });
                    }
                    Some("thinking") => {
                        if let Some(t) = b.get("thinking").and_then(Value::as_str) {
                            parts.push(ContentPart::Thinking { text: t.to_string() });
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

/// `tool_result.content` 可以是字符串或内容块数组，统一压成字符串。
fn flatten_tool_result(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

// ── 出站渲染：非流式 ────────────────────────────────────────────────────────

/// `UnifiedResponse` → Anthropic 非流式 JSON。
///
/// 缓存口径按上文的减法处理：三项相加恰好等于真实 `prompt_tokens`。
pub fn render_response(resp: &UnifiedResponse) -> Value {
    let mut content: Vec<Value> = Vec::new();
    if let Some(r) = &resp.reasoning {
        if !r.is_empty() {
            // 上游 Chat 没有 thinking 块的概念，`signature` 只能是空串。
            content.push(json!({ "type": "thinking", "thinking": r, "signature": "" }));
        }
    }
    if !resp.text.is_empty() {
        content.push(json!({ "type": "text", "text": resp.text }));
    }
    for tc in &resp.tool_calls {
        content.push(json!({
            "type": "tool_use",
            "id": tc.id,
            "name": tc.name,
            // Anthropic 的 `input` 是已解析对象；IR 存的是 JSON 字符串。
            "input": serde_json::from_str::<Value>(&tc.arguments)
                .unwrap_or_else(|_| Value::Object(Map::new())),
        }));
    }

    let u = resp.usage;
    json!({
        "id": resp.id,
        "type": "message",
        "role": "assistant",
        "model": resp.model,
        "content": content,
        "stop_reason": stop_reason(resp.finish_reason),
        "stop_sequence": Value::Null,
        "usage": {
            // 忠实策略：input_tokens 减去缓存部分，另两项补齐，三项相加 = 真实输入
            "input_tokens": u.input_uncached(),
            "output_tokens": u.output_tokens,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": u.cached_tokens,
        }
    })
}

pub fn stop_reason(fr: Option<FinishReason>) -> &'static str {
    match fr {
        Some(FinishReason::Length) => "max_tokens",
        Some(FinishReason::ToolCalls) => "tool_use",
        Some(FinishReason::ContentFilter) => "refusal",
        // `pause_turn` / `model_context_window_exceeded` 没有上游信号，
        // 降级成 end_turn 而不是编造。
        _ => "end_turn",
    }
}

// ── 出站渲染：流式 ──────────────────────────────────────────────────────────

/// Anthropic 的 SSE 渲染器是**有状态**的：需要维护当前打开的 block index
/// 与工具调用块，才能正确配对 `content_block_start` / `content_block_stop`。
pub struct MessagesStreamRenderer {
    pub id: String,
    pub model: String,
    /// 估算的输入 token 数——`message_start` 必须立即带出来，
    /// 而上游 Chat 只在流结束时才给 `prompt_tokens`。
    pub estimated_input_tokens: u32,

    /// 已发出的 block index。文本/思考/工具三类块**共享同一个分配器**——
    /// Anthropic 的 index 是流内全局的，重复使用会让 SDK 解出乱序内容。
    next_block_index: u32,
    /// 当前打开的文本块 index（`None` 表示没打开）
    open_text: Option<u32>,
    open_thinking: Option<u32>,
    /// index -> 是否是工具调用块（工具块需要 `input_json_delta`）
    open_tools: Vec<(u32, bool)>,
    finished: bool,
}

impl MessagesStreamRenderer {
    pub fn new(id: impl Into<String>, model: impl Into<String>, estimated_input_tokens: u32) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            estimated_input_tokens,
            next_block_index: 0,
            open_text: None,
            open_thinking: None,
            open_tools: Vec::new(),
            finished: false,
        }
    }

    fn alloc_block(&mut self) -> u32 {
        let i = self.next_block_index;
        self.next_block_index += 1;
        i
    }

    fn close_text(&mut self, out: &mut Vec<SseFrame>) {
        if let Some(idx) = self.open_text.take() {
            out.push(SseFrame::named(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": idx }),
            ));
        }
    }

    fn close_thinking(&mut self, out: &mut Vec<SseFrame>) {
        if let Some(idx) = self.open_thinking.take() {
            out.push(SseFrame::named(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": idx }),
            ));
        }
    }

    pub fn push(&mut self, ev: &UnifiedEvent) -> Vec<SseFrame> {
        let mut out = Vec::new();
        match ev {
            UnifiedEvent::TextDelta { text } => {
                let idx = match self.open_text {
                    Some(i) => i,
                    None => {
                        // 思考块要先收尾，否则 index 序列对不上
                        self.close_thinking(&mut out);
                        let i = self.alloc_block();
                        self.open_text = Some(i);
                        // text:"" 是契约的一部分，不能省
                        out.push(SseFrame::named(
                            "content_block_start",
                            json!({ "type": "content_block_start", "index": i,
                                    "content_block": { "type": "text", "text": "" } }),
                        ));
                        i
                    }
                };
                out.push(SseFrame::named(
                    "content_block_delta",
                    json!({ "type": "content_block_delta", "index": idx,
                            "delta": { "type": "text_delta", "text": text } }),
                ));
            }
            UnifiedEvent::ReasoningDelta { text } => {
                let idx = match self.open_thinking {
                    Some(i) => i,
                    None => {
                        self.close_text(&mut out);
                        let i = self.alloc_block();
                        self.open_thinking = Some(i);
                        out.push(SseFrame::named(
                            "content_block_start",
                            json!({ "type": "content_block_start", "index": i,
                                    "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
                        ));
                        i
                    }
                };
                out.push(SseFrame::named(
                    "content_block_delta",
                    json!({ "type": "content_block_delta", "index": idx,
                            "delta": { "type": "thinking_delta", "thinking": text } }),
                ));
            }
            UnifiedEvent::ToolCallStart { index, id, name } => {
                self.close_text(&mut out);
                let idx = self.alloc_block();
                // 工具块先记着，参数分片要往这里发
                self.open_tools.push((idx, false));
                out.push(SseFrame::named(
                    "content_block_start",
                    json!({ "type": "content_block_start", "index": idx,
                            // input:{} 是契约的一部分
                            "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }),
                ));
                let _ = index;
            }
            UnifiedEvent::ToolCallArgsDelta { fragment, .. } => {
                if let Some(&(idx, _)) = self.open_tools.first() {
                    out.push(SseFrame::named(
                        "content_block_delta",
                        json!({ "type": "content_block_delta", "index": idx,
                                "delta": { "type": "input_json_delta", "partial_json": fragment } }),
                    ));
                }
            }
            UnifiedEvent::ToolCallEnd { .. } => {}
            UnifiedEvent::Done { usage, finish_reason } => {
                if self.finished {
                    return out;
                }
                self.finished = true;
                self.close_text(&mut out);
                self.close_thinking(&mut out);
                while let Some((idx, _)) = self.open_tools.first().copied() {
                    out.push(SseFrame::named(
                        "content_block_stop",
                        json!({ "type": "content_block_stop", "index": idx }),
                    ));
                    self.open_tools.remove(0);
                }
                // message_delta 带的 usage 是**累积**的，只能出现一次
                out.push(SseFrame::named(
                    "message_delta",
                    json!({ "type": "message_delta",
                            "delta": { "stop_reason": stop_reason(*finish_reason), "stop_sequence": Value::Null },
                            "usage": { "output_tokens": usage.output_tokens } }),
                ));
                // 没有 [DONE]，message_stop 就是终止信号——漏发会让 SDK 挂起
                out.push(SseFrame::named("message_stop", json!({ "type": "message_stop" })));
            }
            UnifiedEvent::Error { message, .. } => {
                if self.finished {
                    return out;
                }
                self.finished = true;
                self.close_text(&mut out);
                self.close_thinking(&mut out);
                out.push(SseFrame::named(
                    "error",
                    json!({ "type": "error",
                            "error": { "type": "api_error", "message": message } }),
                ));
            }
        }
        out
    }
}

/// 流开始时先发 `message_start`——它必须**立即**带上输入 token 数。
pub fn message_start_frame(
    id: &str,
    model: &str,
    estimated_input_tokens: u32,
) -> SseFrame {
    SseFrame::named(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": { "input_tokens": estimated_input_tokens, "output_tokens": 1 }
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_comes_from_top_level_field() {
        let raw = json!({
            "model": "m",
            "max_tokens": 100,
            "system": "你是助手",
            "messages": [{"role":"user","content":"hi"}]
        });
        let req = parse_request(&raw).unwrap();
        assert_eq!(req.system.as_deref(), Some("你是助手"));
        assert_eq!(req.max_output_tokens, Some(100));
    }

    #[test]
    fn input_schema_is_the_tool_schema_field() {
        let raw = json!({
            "model":"m","max_tokens":10,
            "messages":[{"role":"user","content":"x"}],
            "tools":[{"name":"f","input_schema":{"type":"object"}}]
        });
        let req = parse_request(&raw).unwrap();
        assert_eq!(req.tools[0].parameters["type"], "object");
    }

    #[test]
    fn tool_use_input_is_already_an_object() {
        let raw = json!({
            "model":"m","max_tokens":10,
            "messages":[{"role":"assistant","content":[
                {"type":"tool_use","id":"t1","name":"f","input":{"a":1}}
            ]}]
        });
        let req = parse_request(&raw).unwrap();
        match &req.messages[0].parts[0] {
            ContentPart::ToolUse { input, .. } => assert_eq!(input["a"], 1),
            o => panic!("expected ToolUse, got {o:?}"),
        }
    }

    #[test]
    fn tool_result_blocks_flatten_to_text() {
        let raw = json!({
            "model":"m","max_tokens":10,
            "messages":[{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"42"}]}
            ]}]
        });
        let req = parse_request(&raw).unwrap();
        match &req.messages[0].parts[0] {
            ContentPart::ToolResult { content, tool_use_id, .. } => {
                assert_eq!(content, "42");
                assert_eq!(tool_use_id, "t1");
            }
            o => panic!("expected ToolResult, got {o:?}"),
        }
    }

    /// 缓存口径是本协议最容易算错的地方：三项相加必须等于真实 prompt_tokens。
    #[test]
    fn cache_accounting_sums_back_to_real_prompt_tokens() {
        let resp = UnifiedResponse {
            id: "msg_1".into(),
            model: "m".into(),
            text: "hi".into(),
            tool_calls: vec![],
            finish_reason: Some(FinishReason::Stop),
            usage: UnifiedUsage {
                input_tokens: 100,
                cached_tokens: 40,
                output_tokens: 10,
                reasoning_tokens: 0,
            },
            reasoning: None,
        };
        let body = render_response(&resp);
        let u = &body["usage"];
        let sum = u["input_tokens"].as_u64().unwrap()
            + u["cache_creation_input_tokens"].as_u64().unwrap()
            + u["cache_read_input_tokens"].as_u64().unwrap();
        assert_eq!(sum, 100, "三项相加必须等于真实输入 token");
        assert_eq!(u["cache_read_input_tokens"], 40);
        assert_eq!(u["input_tokens"], 60);
    }

    #[test]
    fn stream_terminates_with_message_stop_not_done() {
        let mut r = MessagesStreamRenderer::new("msg_1", "m", 12);
        r.push(&UnifiedEvent::TextDelta { text: "hi".into() });
        let frames = r.push(&UnifiedEvent::Done {
            usage: UnifiedUsage { output_tokens: 5, ..Default::default() },
            finish_reason: Some(FinishReason::Stop),
        });
        let last = frames.last().unwrap();
        assert_eq!(last.event, Some("message_stop"));
        assert!(frames.iter().all(|f| f.data != "[DONE]"));
    }

    #[test]
    fn text_block_start_carries_empty_placeholder() {
        let mut r = MessagesStreamRenderer::new("msg_1", "m", 12);
        let frames = r.push(&UnifiedEvent::TextDelta { text: "hello".into() });
        let first: Value = serde_json::from_str(&frames[0].data).unwrap();
        assert_eq!(frames[0].event, Some("content_block_start"));
        assert_eq!(first["content_block"]["text"], "");
        assert_eq!(first["index"], 0);
    }

    #[test]
    fn tool_call_arguments_become_input_json_deltas() {
        let mut r = MessagesStreamRenderer::new("msg_1", "m", 12);
        r.push(&UnifiedEvent::ToolCallStart {
            index: 0,
            id: "toolu_1".into(),
            name: "f".into(),
        });
        let frames = r.push(&UnifiedEvent::ToolCallArgsDelta {
            index: 0,
            fragment: "{\"a\":".into(),
        });
        let v: Value = serde_json::from_str(&frames[0].data).unwrap();
        assert_eq!(frames[0].event, Some("content_block_delta"));
        assert_eq!(v["delta"]["type"], "input_json_delta");
        assert_eq!(v["delta"]["partial_json"], "{\"a\":");
    }

    #[test]
    fn message_start_carries_input_tokens_immediately() {
        let f = message_start_frame("msg_1", "m", 77);
        let v: Value = serde_json::from_str(&f.data).unwrap();
        assert_eq!(f.event, Some("message_start"));
        assert_eq!(v["message"]["usage"]["input_tokens"], 77);
    }

    #[test]
    fn stop_reason_maps_length_to_max_tokens() {
        assert_eq!(stop_reason(Some(FinishReason::Length)), "max_tokens");
        assert_eq!(stop_reason(Some(FinishReason::ToolCalls)), "tool_use");
        assert_eq!(stop_reason(None), "end_turn");
    }
}
