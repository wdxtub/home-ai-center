//! 上游客户端。
//!
//! 家里后端只讲 OpenAI Chat Completions，ComfyUI 则是另一套协议，
//! 两者**互不共享**错误分类：LLM 的 5xx 要换 key/换点，ComfyUI 的
//! 400 往往是工作流写错了，换端点只会把同一个坏工作流再发一遍。

pub mod comfyui;
pub mod openai;
pub mod workflow;
