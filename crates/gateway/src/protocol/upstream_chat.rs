//! **唯一一份上游渲染**：`UnifiedRequest` → OpenAI Chat Completions 请求体。
//!
//! 所有节点都用这一个方言（LM Studio / Ollama / OMLX / vLLM / DeepSeek）。
//! 本模块同时负责**反向**：把上游 Chat 的 JSON 响应与 SSE chunk 归一成 IR，
//! 供三个出站渲染器使用。

use serde_json::{json, Map, Value};

use super::ir::*;

/// `stop` 序列上限。Chat 只接受最多 4 条，超出静默截断会改变模型行为，
/// 因此这里截断并返回标记，由调用方记入 `error_kind=stop_truncated`。
pub const MAX_STOP_SEQUENCES: usize = 4;

/// 渲染成上游 Chat 请求体。返回 `(body, stop_truncated)`。
pub fn render_request(req: &UnifiedRequest) -> (Value, bool) {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(system) = &req.system {
        if !system.is_empty() {
            messages.push(json!({ "role": "system", "content": system }));
        }
    }

    // IR 里 ToolResult 是内容块（Anthropic 把它放在 user 消息里），
    // Chat 要求它是**独立消息**。逐条消息处理以保持对话顺序——
    // 一次性把所有工具结果提到最前面会打乱轮次。
    for m in &req.messages {
        let tool_results: Vec<&ContentPart> = m
            .parts
            .iter()
            .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
            .collect();
        let rest: Vec<ContentPart> = m
            .parts
            .iter()
            .filter(|p| !matches!(p, ContentPart::ToolResult { .. }))
            .cloned()
            .collect();

        for p in tool_results {
            if let ContentPart::ToolResult { tool_use_id, content, .. } = p {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                }));
            }
        }
        if rest.is_empty() {
            continue;
        }

        let m = &UnifiedMessage { role: m.role, parts: rest };
        match m.role {
            Role::Tool => {}
            Role::Assistant => {
                let mut msg = Map::new();
                msg.insert("role".into(), json!("assistant"));
                let text = m.plain_text();
                msg.insert("content".into(), json!(if text.is_empty() { Value::Null } else { Value::String(text) }));

                let tool_calls: Vec<Value> = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::ToolUse { id, name, input } => Some(json!({
                            "id": id,
                            "type": "function",
                            // Chat 的 arguments 是 JSON 字符串；IR 里是已解析对象。
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                            }
                        })),
                        _ => None,
                    })
                    .collect();
                if !tool_calls.is_empty() {
                    msg.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                messages.push(Value::Object(msg));
            }
            Role::User | Role::System => {
                // 用户消息可能混有图片，必须走多模态数组形态。
                let has_image = m.parts.iter().any(|p| matches!(p, ContentPart::Image { .. }));
                if !has_image {
                    messages.push(json!({ "role": m.role_as_chat(), "content": m.plain_text() }));
                } else {
                    let mut arr: Vec<Value> = Vec::new();
                    for p in &m.parts {
                        match p {
                            ContentPart::Text { text } => {
                                arr.push(json!({ "type": "text", "text": text }))
                            }
                            ContentPart::Image { media_type, data } => arr.push(json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{media_type};base64,{data}") }
                            })),
                            _ => {}
                        }
                    }
                    messages.push(json!({ "role": m.role_as_chat(), "content": arr }));
                }
            }
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages));
    if let Some(n) = req.max_output_tokens {
        // max_completion_tokens 是现代字段名；max_tokens 已被新模型拒绝。
        body.insert("max_completion_tokens".into(), json!(n));
    }
    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        body.insert("top_p".into(), json!(p));
    }

    let mut stop_truncated = false;
    if !req.stop.is_empty() {
        let take = req.stop.len().min(MAX_STOP_SEQUENCES);
        stop_truncated = take < req.stop.len();
        body.insert(
            "stop".into(),
            Value::Array(req.stop[..take].iter().map(|s| json!(s)).collect()),
        );
    }

    if !req.tools.is_empty() {
        // Chat 的工具声明比另外两种协议**多嵌一层 `function`**。
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        if let Some(tc) = &req.tool_choice {
            body.insert(
                "tool_choice".into(),
                match tc {
                    ToolChoice::Auto => json!("auto"),
                    ToolChoice::None => json!("none"),
                    ToolChoice::Required => json!("required"),
                    ToolChoice::Named(n) => json!({"type":"function","function":{"name":n}}),
                },
            );
        }
    }

    if req.stream {
        body.insert("stream".into(), json!(true));
        // 不显式 opt-in 就完全拿不到 token 数，也就无法给 Anthropic /
        // Responses 填 usage，更无法计费。
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if let Some(u) = &req.metadata {
        body.insert("user".into(), json!(u));
    }

    (Value::Object(body), stop_truncated)
}

impl UnifiedMessage {
    fn role_as_chat(&self) -> &'static str {
        match self.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

// ── 上游响应 → IR ───────────────────────────────────────────────────────────

/// 把上游 Chat 的非流式响应归一成 `UnifiedResponse`。
pub fn parse_response(raw: &Value) -> UnifiedResponse {
    let id = raw.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let model = raw.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let choice = raw.get("choices").and_then(Value::as_array).and_then(|c| c.first());

    let msg = choice.and_then(|c| c.get("message"));
    let text = msg
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let reasoning = msg
        .and_then(|m| m.get("reasoning_content"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let tool_calls: Vec<UnifiedToolCall> = msg
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    let f = tc.get("function")?;
                    Some(UnifiedToolCall {
                        id: tc.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                        name: f.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                        arguments: f
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .map(FinishReason::from_upstream);

    UnifiedResponse {
        id,
        model,
        text,
        reasoning,
        tool_calls,
        finish_reason,
        usage: parse_usage(raw.get("usage")),
    }
}

/// 上游 `usage` → `UnifiedUsage`。
///
/// 注意口径：`prompt_tokens` **已经包含** `cached_tokens`，
/// `completion_tokens` **已经包含** `reasoning_tokens`。两个子集不可相加。
pub fn parse_usage(usage: Option<&Value>) -> UnifiedUsage {
    let Some(u) = usage else {
        return UnifiedUsage::default();
    };
    UnifiedUsage {
        input_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        cached_tokens: u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        output_tokens: u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        reasoning_tokens: u
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    }
}

/// 把一个上游 `chat.completion.chunk` 归一成若干 `UnifiedEvent`。
///
/// **末块陷阱**：开 `stream_options.include_usage` 后，只有最后一个 chunk 带
/// 真实 `usage`，且它的 `choices` 是**空数组**——朴素实现若写
/// `chunk.choices[0]` 会在这里崩掉。
pub fn parse_chunk(raw: &Value) -> Vec<UnifiedEvent> {
    let mut out = Vec::new();

    if let Some(err) = raw.get("error") {
        out.push(UnifiedEvent::Error {
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("上游流式错误")
                .to_string(),
            code: err.get("code").and_then(Value::as_str).map(str::to_string),
        });
        return out;
    }

    if let Some(choice) = raw
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
    {
        let delta = choice.get("delta");
        if let Some(d) = delta {
            if let Some(t) = d.get("reasoning_content").and_then(Value::as_str) {
                if !t.is_empty() {
                    out.push(UnifiedEvent::ReasoningDelta { text: t.to_string() });
                }
            }
            if let Some(t) = d.get("content").and_then(Value::as_str) {
                if !t.is_empty() {
                    out.push(UnifiedEvent::TextDelta { text: t.to_string() });
                }
            }
            if let Some(calls) = d.get("tool_calls").and_then(Value::as_array) {
                for tc in calls {
                    let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let f = tc.get("function");
                    let name = f.and_then(|f| f.get("name")).and_then(Value::as_str);
                    let id = tc.get("id").and_then(Value::as_str);
                    if name.is_some() || id.is_some() {
                        out.push(UnifiedEvent::ToolCallStart {
                            index,
                            id: id.unwrap_or("").to_string(),
                            name: name.unwrap_or("").to_string(),
                        });
                    }
                    if let Some(args) = f.and_then(|f| f.get("arguments")).and_then(Value::as_str) {
                        if !args.is_empty() {
                            out.push(UnifiedEvent::ToolCallArgsDelta {
                                index,
                                fragment: args.to_string(),
                            });
                        }
                    }
                }
            }
        }
        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            out.push(UnifiedEvent::Done {
                usage: parse_usage(raw.get("usage")),
                finish_reason: Some(FinishReason::from_upstream(fr)),
            });
        }
    } else if raw.get("usage").is_some_and(|u| !u.is_null()) {
        // 末块：choices 为空，只带 usage。补一个 Done 让渲染器收尾。
        out.push(UnifiedEvent::Done {
            usage: parse_usage(raw.get("usage")),
            finish_reason: None,
        });
    }

    out
}

/// 上游流是否已结束（收到 `[DONE]` 哨兵）。
pub fn is_done_sentinel(data: &str) -> bool {
    data.trim() == "[DONE]"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> UnifiedRequest {
        UnifiedRequest {
            model: "qwen".into(),
            system: Some("你是助手".into()),
            messages: vec![UnifiedMessage::user_text("你好")],
            ..Default::default()
        }
    }

    #[test]
    fn system_prompt_becomes_leading_system_message() {
        let (body, _) = render_request(&req());
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "你是助手");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn tool_result_is_promoted_to_standalone_tool_message() {
        let r = UnifiedRequest {
            model: "m".into(),
            messages: vec![UnifiedMessage::new(
                Role::User,
                vec![ContentPart::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "结果".into(),
                    is_error: false,
                }],
            )],
            ..Default::default()
        };
        let (body, _) = render_request(&r);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "call_1");
    }

    #[test]
    fn tool_use_arguments_are_stringified() {
        let r = UnifiedRequest {
            model: "m".into(),
            messages: vec![UnifiedMessage::new(
                Role::Assistant,
                vec![ContentPart::ToolUse {
                    id: "call_1".into(),
                    name: "f".into(),
                    input: serde_json::json!({"a":1}),
                }],
            )],
            ..Default::default()
        };
        let (body, _) = render_request(&r);
        // Chat 的 arguments 必须是 JSON 字符串，不是对象
        let args = body["messages"][0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(args, r#"{"a":1}"#);
    }

    #[test]
    fn stop_sequences_truncate_beyond_four() {
        let r = UnifiedRequest {
            model: "m".into(),
            messages: vec![UnifiedMessage::user_text("x")],
            stop: vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
            ..Default::default()
        };
        let (body, truncated) = render_request(&r);
        assert!(truncated);
        assert_eq!(body["stop"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn stream_always_opts_into_usage() {
        let r = UnifiedRequest { stream: true, ..req() };
        let (body, _) = render_request(&r);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn final_usage_chunk_with_empty_choices_is_handled() {
        // 末块陷阱：choices 为空数组，只带 usage
        let chunk = json!({"id":"c","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4}});
        let evs = parse_chunk(&chunk);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            UnifiedEvent::Done { usage, .. } => {
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.output_tokens, 4);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn usage_subfields_are_subsets_not_additions() {
        let u = parse_usage(Some(&json!({
            "prompt_tokens": 100,
            "prompt_tokens_details": {"cached_tokens": 40},
            "completion_tokens": 20,
            "completion_tokens_details": {"reasoning_tokens": 6}
        })));
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.cached_tokens, 40);
        assert_eq!(u.output_tokens, 20);
        assert_eq!(u.reasoning_tokens, 6);
        assert!(u.cached_tokens <= u.input_tokens);
    }

    #[test]
    fn tool_call_chunks_map_to_start_and_args() {
        let chunk = json!({
            "choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"call_9","function":{"name":"get_weather","arguments":"{\"loc"}}
            ]}}]
        });
        let evs = parse_chunk(&chunk);
        assert!(matches!(evs[0], UnifiedEvent::ToolCallStart { .. }));
        assert!(matches!(evs[1], UnifiedEvent::ToolCallArgsDelta { .. }));
    }
}
