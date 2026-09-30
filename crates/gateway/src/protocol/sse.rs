//! SSE 帧写出工具。
//!
//! 三种协议的 SSE 装载方式不同：Chat 是**无名**事件（只有 `data:`），
//! Responses 与 Anthropic 是**具名**事件（`event:` + `data:`，且 `type`
//! 在 JSON 里重复一次）。两种都支持。

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// `None` 表示匿名事件（OpenAI Chat 的写法）。
    pub event: Option<&'static str>,
    pub data: String,
}

impl SseFrame {
    pub fn data_only(data: impl Serialize) -> Self {
        Self {
            event: None,
            data: serde_json::to_string(&data).unwrap_or_else(|_| "{}".into()),
        }
    }

    pub fn named(event: &'static str, data: impl Serialize) -> Self {
        Self {
            event: Some(event),
            data: serde_json::to_string(&data).unwrap_or_else(|_| "{}".into()),
        }
    }

    /// 原始字符串帧（用于 `[DONE]` 这类非 JSON 负载）。
    pub fn raw(event: Option<&'static str>, data: impl Into<String>) -> Self {
        Self {
            event,
            data: data.into(),
        }
    }

    pub fn done() -> Self {
        Self {
            event: None,
            data: "[DONE]".into(),
        }
    }

    /// 序列化成线上字节。
    ///
    /// `data` 中的换行必须按 SSE 规范拆成多行 `data:`，否则一个多行 JSON
    /// 会被解析成两个事件。
    pub fn encode(&self) -> String {
        let mut out = String::new();
        if let Some(ev) = self.event {
            out.push_str("event: ");
            out.push_str(ev);
            out.push('\n');
        }
        for line in self.data.split('\n') {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn named_event_encodes_both_lines() {
        let f = SseFrame::named("message_stop", json!({"type":"message_stop"}));
        let s = f.encode();
        assert!(s.starts_with("event: message_stop\ndata: {"));
        assert!(s.ends_with("\n\n"));
    }

    #[test]
    fn anonymous_event_has_no_event_line() {
        let f = SseFrame::data_only(json!({"a":1}));
        assert!(!f.encode().starts_with("event:"));
        assert!(f.encode().contains("data: {\"a\":1}"));
    }

    #[test]
    fn multiline_payload_becomes_multiple_data_lines() {
        // SSE 规范：一段负载里的换行必须每行都加 data: 前缀
        let f = SseFrame::raw(None, "line1\nline2");
        let s = f.encode();
        assert!(s.contains("data: line1\ndata: line2\n"));
    }

    #[test]
    fn done_sentinel_is_exact() {
        assert_eq!(SseFrame::done().data, "[DONE]");
        assert!(SseFrame::done().encode() == "data: [DONE]\n\n");
    }
}
