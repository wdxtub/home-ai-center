//! 协议适配层：**单上游方言 + 客户端薄适配器**。
//!
//! 家里的后端（LM Studio / Ollama / OMLX / vLLM）只讲 OpenAI Chat Completions，
//! 部分后端根本不支持 `/v1/responses`。因此上游永远只有一种格式；客户端协议
//! 只影响两件事：**入站解析**与**出站渲染**。
//!
//! 三个协议 × 两个方向 = 6 个薄适配器，替代 3×3 的九组双向转换。
//! IR 保留是为了将来某个节点原生支持 Responses 时加一条分支即可。

pub mod chat;
pub mod ir;
pub mod messages;
pub mod responses;
pub mod sse;
pub mod tokenize;
pub mod upstream_chat;

use serde::{Deserialize, Serialize};

pub use ir::*;
pub use sse::SseFrame;

/// 客户端使用的入站协议。**由请求路径决定**，不靠嗅探请求体。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Chat,
    Responses,
    Messages,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Chat => "chat",
            Protocol::Responses => "responses",
            Protocol::Messages => "messages",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_serializes_for_request_log() {
        assert_eq!(serde_json::to_string(&Protocol::Messages).unwrap(), "\"messages\"");
    }
}
