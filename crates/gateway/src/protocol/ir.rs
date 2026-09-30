//! 内部规范表示（IR）。三种客户端协议都归一到这一组结构，
//! 上游永远渲染成 OpenAI Chat Completions。

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── 请求方向 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// `ToolResult` 在 IR 里是内容块，但渲染到 Chat 时必须**提升为独立的
/// `role:"tool"` 消息**——三种协议的载体形态不同，IR 只记语义。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        /// base64 载荷（不含 data URL 前缀）。
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        /// 已解析的对象。渲染到 OpenAI 时需 `stringify`，渲染到 Anthropic 时原样。
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    Thinking {
        text: String,
    },
}

impl ContentPart {
    pub fn text(t: impl Into<String>) -> Self {
        ContentPart::Text { text: t.into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedMessage {
    pub role: Role,
    pub parts: Vec<ContentPart>,
}

impl UnifiedMessage {
    pub fn new(role: Role, parts: Vec<ContentPart>) -> Self {
        Self { role, parts }
    }
    pub fn user_text(t: impl Into<String>) -> Self {
        Self::new(Role::User, vec![ContentPart::text(t)])
    }
    /// 纯文本内容拼接，用于 token 估算与降级渲染。
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            if let ContentPart::Text { text } = p {
                out.push_str(text);
            }
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Named(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnifiedRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<UnifiedMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<UnifiedTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
}

impl UnifiedRequest {
    /// 入站内容的字符总数，供 token 估算使用（计费预扣与 Anthropic
    /// `message_start` 的输入 token 共用同一套估算）。
    pub fn inbound_char_count(&self) -> usize {
        let mut n = self.model.len();
        if let Some(s) = &self.system {
            n += s.len();
        }
        for m in &self.messages {
            n += m.plain_text().len();
            for p in &m.parts {
                if let ContentPart::Image { data, .. } = p {
                    n += data.len() / 4; // base64 约 4 字符 1 字节
                }
            }
        }
        n += self
            .tools
            .iter()
            .map(|t| t.parameters.to_string().len())
            .sum::<usize>();
        n
    }
}

// ── 响应方向 ────────────────────────────────────────────────────────────────

/// **计费的唯一口径**。
///
/// 语义固定为：`input_tokens` **含** `cached_tokens`；`output_tokens`
/// **含** `reasoning_tokens`。两个子集是包含关系，不可相加。
///
/// 之所以单独定一份：三种协议的 usage 字段名与呈现时机都不同
/// （Anthropic 无 `total_tokens`、Chat 末块 `choices: []`、
/// Anthropic `message_start` 必须提前给输入 token），
/// 账目一旦跟着客户端协议走就会算错。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedUsage {
    pub input_tokens: u32,
    pub cached_tokens: u32,
    pub output_tokens: u32,
    pub reasoning_tokens: u32,
}

impl UnifiedUsage {
    pub fn input_uncached(&self) -> u32 {
        self.input_tokens.saturating_sub(self.cached_tokens)
    }
    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Other,
}

impl FinishReason {
    /// 解析上游 Chat 的 `finish_reason`。未知值归入 `Other`——
    /// 客户端据此分支判断，映射错会致重试循环。
    pub fn from_upstream(s: &str) -> Self {
        match s {
            "stop" => FinishReason::Stop,
            "length" => FinishReason::Length,
            "tool_calls" | "function_call" => FinishReason::ToolCalls,
            "content_filter" => FinishReason::ContentFilter,
            _ => FinishReason::Other,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedToolCall {
    pub id: String,
    pub name: String,
    /// JSON 字符串。IR 统一存字符串，渲染到 Anthropic 时再 parse 成对象。
    pub arguments: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnifiedResponse {
    pub id: String,
    pub model: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<UnifiedToolCall>,
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
    #[serde(default)]
    pub usage: UnifiedUsage,
}

// ── 流式事件 ────────────────────────────────────────────────────────────────

/// 上游 Chat 的 SSE chunk 归一后的流式事件。
///
/// 三种协议的事件序列与终止信号都不同（Chat 用 `[DONE]`、Responses 用
/// `response.completed`、Anthropic 用 `message_stop`），所以渲染器必须是
/// **有状态**的：见 `chat::ChatStreamRenderer` 等三个结构体。
#[derive(Debug, Clone, PartialEq)]
pub enum UnifiedEvent {
    TextDelta { text: String },
    ReasoningDelta { text: String },
    ToolCallStart { index: usize, id: String, name: String },
    /// `arguments` 的**分片**，需要跨事件拼接后再 parse。
    ToolCallArgsDelta { index: usize, fragment: String },
    ToolCallEnd { index: usize },
    Done {
        usage: UnifiedUsage,
        finish_reason: Option<FinishReason>,
    },
    Error { message: String, code: Option<String> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_uncached_subtracts_cached_subset() {
        let u = UnifiedUsage {
            input_tokens: 100,
            cached_tokens: 40,
            output_tokens: 10,
            reasoning_tokens: 3,
        };
        assert_eq!(u.input_uncached(), 60);
        // 两个子集是包含关系，绝不可相加
        assert!(u.cached_tokens <= u.input_tokens);
        assert!(u.reasoning_tokens <= u.output_tokens);
    }

    #[test]
    fn inbound_chars_count_tools_and_images() {
        let req = UnifiedRequest {
            model: "m".into(),
            system: Some("sys".into()),
            messages: vec![UnifiedMessage::new(
                Role::User,
                vec![ContentPart::text("hello"), ContentPart::Image { media_type: "image/png".into(), data: "a".repeat(400) }],
            )],
            tools: vec![UnifiedTool {
                name: "t".into(),
                description: String::new(),
                parameters: serde_json::json!({"a":1}),
            }],
            ..Default::default()
        };
        // 400 base64 字符 ≈ 100 "字节"，按 len/4 计
        assert!(req.inbound_char_count() >= 100 + 3 + 5 + 3);
    }

    #[test]
    fn finish_reason_parses_legacy_function_call() {
        assert_eq!(FinishReason::from_upstream("function_call"), FinishReason::ToolCalls);
        assert_eq!(FinishReason::from_upstream("wat"), FinishReason::Other);
    }
}
