//! 上游 OpenAI Chat Completions 客户端。
//!
//! 节点只讲这一种方言。这里的职责：
//! - 组装请求（含节点级 `extra_body` / `extra_headers`）；
//! - **强制注入 `stream_options.include_usage`**，否则拿不到 token 数；
//! - 把错误分类成「限额类（换 key）」「瞬时类（原地等）」「节点故障（换点）」；
//! - 把 SSE chunk 流式转成 `UnifiedEvent`。
//!
//! 出错时不重建 client：节点 URL 固定，client 可以一直复用。

use futures::StreamExt;
use serde_json::Value;
use std::collections::VecDeque;

use crate::keys::classify::{self, Failure, QuotaClass, QuotaVerdict};
use crate::protocol::ir::UnifiedEvent;
use crate::protocol::upstream_chat;

/// 上游失败。三类处置完全不同，不能混为一谈。
#[derive(Debug)]
pub enum UpstreamError {
    /// 限额类：换一把 key 重试，**不判节点离线**。
    Quota(QuotaVerdict),
    /// 瞬时类：原地退避，**不换 key**。
    Transient { retry_after_secs: Option<u64>, message: String },
    /// 节点故障：换点 + 冷却节点。
    Node { hard: bool, message: String },
    /// LM Studio「模型加载中」，原地等，不算故障。
    ModelLoading,
    /// 请求体问题（内容错误），原样抛给客户端。
    Client(String),
}

/// 命中「模型加载中」的判据。节点是活的，只是模型还没加载完。
const MODEL_LOADING_MARKER: &str = "no models loaded";

pub struct OpenAiClient {
    http: reqwest::Client,
}

impl OpenAiClient {
    pub fn new(timeout_secs: u64) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            // 出图 / 内网直连不读系统代理：macOS 的 Clash 之类会把内网请求
            // 带回 502，让「内网优先」永远失效。
            .no_proxy()
            .build()?;
        Ok(Self { http })
    }

    fn endpoint(base_url: &str) -> String {
        format!("{}/chat/completions", base_url.trim_end_matches('/'))
    }

    /// 发一次非流式请求。
    pub async fn post_chat(
        &self,
        base_url: &str,
        api_key: &str,
        body: &Value,
        extra_headers: &Value,
    ) -> Result<Value, UpstreamError> {
        let mut req = self
            .http
            .post(Self::endpoint(base_url))
            .bearer_auth(api_key)
            .json(body);
        if let Some(map) = extra_headers.as_object() {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    req = req.header(k, s);
                }
            }
        }
        let resp = req.send().await.map_err(|e| UpstreamError::Node {
            hard: !matches!(e, reqwest::Error { .. } if e.is_timeout()),
            message: e.to_string(),
        })?;

        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = resp.text().await.unwrap_or_default();
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

        if (200..300).contains(&status) {
            if let Some(msg) = error_envelope(&json) {
                return Err(envelope_error(msg));
            }
            return Ok(json);
        }
        Err(classify_failure(status, retry_after, &json, &text))
    }

    /// 发一次流式请求，返回 SSE 字节流。
    pub async fn post_chat_stream(
        &self,
        base_url: &str,
        api_key: &str,
        body: &Value,
        extra_headers: &Value,
    ) -> Result<reqwest::Response, UpstreamError> {
        let mut req = self
            .http
            .post(Self::endpoint(base_url))
            .bearer_auth(api_key)
            .json(body);
        if let Some(map) = extra_headers.as_object() {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    req = req.header(k, s);
                }
            }
        }
        let resp = req.send().await.map_err(|e| UpstreamError::Node {
            hard: !e.is_timeout(),
            message: e.to_string(),
        })?;

        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            // 2xx 不保证是 SSE。路径写错时 LM Studio 同样回 200，
            // body 却是 JSON 错误信封——直接当流读会一路读到 EOF，
            // 客户端只看到「正常结束的空回答」。
            //
            // 非 SSE 时把 body 缓冲下来再重建 Response：既能把错误信封
            // 挑出来报错，也不会把 body 吃掉。
            let looks_sse = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.contains("text/event-stream"));
            if !looks_sse {
                // 缓冲下来重建一个 Response：既能把错误信封挑出来报错，
                // 也不会把 body 吃掉。流式路径只用 `bytes_stream()`，
                // 因此丢 URL/扩展没有影响。
                let (status, version, headers) = (resp.status(), resp.version(), resp.headers().clone());
                let bytes = match resp.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        return Err(UpstreamError::Node {
                            hard: false,
                            message: format!("读取上游响应体失败：{e}"),
                        })
                    }
                };
                if let Ok(json) = serde_json::from_slice::<Value>(&bytes) {
                    if let Some(msg) = error_envelope(&json) {
                        return Err(envelope_error(msg));
                    }
                }
                let mut rebuilt = http::Response::builder().status(status).version(version);
                if let Some(h) = rebuilt.headers_mut() {
                    *h = headers;
                }
                return match rebuilt.body(bytes) {
                    Ok(r) => Ok(reqwest::Response::from(r)),
                    Err(e) => Err(UpstreamError::Node {
                        hard: false,
                        message: format!("重建上游响应失败：{e}"),
                    }),
                };
            }
            return Ok(resp);
        }
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = resp.text().await.unwrap_or_default();
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Err(classify_failure(status, retry_after, &json, &text))
    }
}

/// 挑出「2xx 里裹着的错误信封」。
///
/// OpenAI 兼容服务端有个很坑的习惯：**路径写错也回 200**，body 却是
/// `{"error": "Unexpected endpoint or method. (POST /chat/completions)"}`
/// （LM Studio 原样如此）。不挑出来的话，`parse_response` 会把它当成
/// 「一个没有 choices 的正常补全」，客户端收到空回答，网关还照常按
/// 估算 token 计费——配置错了却什么都看不出来。
fn error_envelope(json: &Value) -> Option<String> {
    let e = json.get("error")?;
    let msg = match e {
        Value::String(s) if !s.trim().is_empty() => s.trim().to_string(),
        Value::Object(_) => classify::error_message(json),
        _ => return None,
    };
    Some(msg)
}

fn envelope_error(msg: String) -> UpstreamError {
    // 软失败：不冷却节点、不轮换 key——上游是通的，只是这次没给结果。
    // 但必须**响亮地**报出去，而不是伪装成一次空补全。
    UpstreamError::Node {
        hard: false,
        message: format!(
            "上游返回 2xx 但响应体是错误：{msg}。\
             若 base_url 只填到了端口，请补上 /v1 —— 网关会在其后拼 /chat/completions。"
        ),
    }
}

/// 把一次 HTTP 失败分派到三条处置路径之一。
pub fn classify_failure(
    status: u16,
    retry_after: Option<String>,
    json: &Value,
    raw_text: &str,
) -> UpstreamError {
    let lower = raw_text.to_ascii_lowercase();

    // LM Studio 的 JIT 加载：节点是活的，不该判离线
    if status == 400 && lower.contains(MODEL_LOADING_MARKER) {
        return UpstreamError::ModelLoading;
    }

    let header_fn = |k: &str| {
        if k.eq_ignore_ascii_case("retry-after") {
            retry_after.clone()
        } else {
            None
        }
    };
    let verdict = classify::classify(&Failure {
        status,
        headers: &header_fn,
        body: json,
    });

    match verdict.class {
        QuotaClass::LongQuota | QuotaClass::KeyDead => UpstreamError::Quota(verdict),
        QuotaClass::Transient => {
            // 5xx 属于节点故障（换点 + 冷却），不是「原地等」
            if status >= 500 || status == 429 && verdict.rule == "R12_upstream_5xx" {
                return UpstreamError::Node {
                    hard: status >= 500,
                    message: classify::error_message(json),
                };
            }
            if status == 401 || status == 403 || status == 402 {
                return UpstreamError::Node {
                    hard: false,
                    message: classify::error_message(json),
                };
            }
            UpstreamError::Transient {
                retry_after_secs: verdict
                    .reset_at
                    .map(|t| (t - crate::keys::unix_now()).max(0) as u64),
                message: classify::error_message(json),
            }
        }
    }
}

/// 把上游 SSE 字节流解析成 `UnifiedEvent`。
///
/// 末块陷阱：开 `include_usage` 后只有最后一个 chunk 带真实 usage，
/// 且它的 `choices` 是**空数组**——朴素实现写 `chunk.choices[0]` 会崩。
pub fn parse_sse_stream(body: reqwest::Response) -> impl futures::Stream<Item = UnifiedEvent> {
    let stream = body.bytes_stream().map(|r| r.ok()).map(Option::unwrap_or_default);
    futures::stream::unfold(
        SseReader {
            body: Box::pin(stream),
            buf: String::new(),
            eof: false,
            // 跨事件保留 finish_reason / usage：它们分处两块
            chunks: upstream_chat::ChunkState::new(),
            pending: VecDeque::new(),
        },
        |mut st| async move {
            loop {
                // SSE 以空行分隔事件；攒够一个就吐出去
                while let Some(pos) = find_event_end(&st.buf) {
                    let raw: String = st.buf.drain(..pos).collect();
                    if let Some(mut evs) = st.chunks.feed_raw(&raw) {
                        if !evs.is_empty() {
                            let ev = evs.remove(0);
                            st.pending.extend(evs);
                            return Some((ev, st));
                        }
                    }
                }
                if st.eof {
                    // 最后一帧常常没有收尾空行，必须把缓冲区里剩下的也解析掉，
                    // 否则整条回复会丢掉最后一个 token。
                    let rest = std::mem::take(&mut st.buf);
                    if let Some(mut evs) = st.chunks.feed_raw(&rest) {
                        if !evs.is_empty() {
                            let ev = evs.remove(0);
                            st.pending.extend(evs);
                            return Some((ev, st));
                        }
                    }
                    // 流结束：补终止事件。三个协议都靠它收尾，
                    // 漏掉 message_stop 会让 Anthropic SDK 永久挂起。
                    return st.chunks.finish().into_iter().next().map(|ev| (ev, st));
                }
                if let Some(ev) = st.pending.pop_front() {
                    return Some((ev, st));
                }
                match st.body.next().await {
                    Some(bytes) => st.buf.push_str(&String::from_utf8_lossy(&bytes)),
                    None => st.eof = true,
                }
            }
        },
    )
}

struct SseReader {
    body: std::pin::Pin<Box<dyn futures::Stream<Item = bytes::Bytes> + Send>>,
    buf: String,
    eof: bool,
    chunks: upstream_chat::ChunkState,
    pending: VecDeque<UnifiedEvent>,
}


/// 事件级入口。流式路径由 `parse_sse_stream` 复用同一个状态机。
#[cfg(test)]
fn parse_one_event(raw: &str) -> Option<UnifiedEvent> {
    upstream_chat::ChunkState::new()
        .feed_raw(raw)
        .and_then(|v| v.into_iter().next())
}

/// SSE 以空行分隔事件。空行可能是 `\n\n` 或 `\r\n\r\n`。
fn find_event_end(buf: &str) -> Option<usize> {
    if let Some(i) = buf.find("\n\n") {
        return Some(i + 2);
    }
    if let Some(i) = buf.find("\r\n\r\n") {
        return Some(i + 4);
    }
    None
}

/// 健康探测：发一个最小 chat 请求。成功即代表推理链路真正可用。
///
/// 不用 `GET /models`：它无法触发模型加载，「恢复」可能只是服务在跑而
/// 模型未加载，真实请求仍会超时。
pub async fn probe(
    client: &OpenAiClient,
    base_url: &str,
    api_key: &str,
    model: &str,
    extra_body: &Value,
) -> Result<(), String> {
    let mut body = serde_json::json!({
        "model": model,
        "messages": [{"role":"user","content":"ping"}],
        "max_completion_tokens": 1,
    });
    if let (Some(b), Some(obj)) = (extra_body.as_object(), body.as_object_mut()) {
        for (k, v) in b {
            obj.insert(k.clone(), v.clone());
        }
    }
    match client.post_chat(base_url, api_key, &body, &Value::Null).await {
        Ok(_) => Ok(()),
        Err(UpstreamError::ModelLoading) => Ok(()), // 加载中也算活着
        Err(UpstreamError::Quota(_)) => Err("quota".into()),
        Err(e) => Err(format!("{e:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_loading_is_not_a_node_fault() {
        let e = classify_failure(
            400,
            None,
            &json!({"error":{"message":"No models loaded. Try loading them first."}}),
            "No models loaded",
        );
        assert!(matches!(e, UpstreamError::ModelLoading));
    }

    #[test]
    fn long_quota_error_becomes_quota_path() {
        let e = classify_failure(
            429,
            None,
            &json!({"error":{"message":"You have reached your specified API usage limits."}}),
            "",
        );
        assert!(matches!(e, UpstreamError::Quota(_)));
    }

    #[test]
    fn transient_429_with_retry_after_waits() {
        let e = classify_failure(
            429,
            Some("20".into()),
            &json!({"error":{"type":"rate_limit_error","message":"x"}}),
            "",
        );
        match e {
            UpstreamError::Transient { retry_after_secs, .. } => {
                assert!(retry_after_secs.unwrap() <= 61);
            }
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    /// 防 litellm 反模式：5xx 是节点故障（换点），不是换 key。
    #[test]
    fn upstream_5xx_is_node_fault_not_quota() {
        let e = classify_failure(503, None, &json!({"error":{"message":"overloaded"}}), "");
        assert!(matches!(e, UpstreamError::Node { .. }));
    }

    #[test]
    fn sse_event_split_handles_chunked_arrival() {
        let raw = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let ev = parse_one_event(raw).unwrap();
        assert_eq!(
            ev,
            UnifiedEvent::TextDelta { text: "hi".into() }
        );
    }

    #[test]
    fn sse_done_sentinel_becomes_done_event() {
        let ev = parse_one_event("data: [DONE]\n\n").unwrap();
        assert!(matches!(ev, UnifiedEvent::Done { .. }));
    }

    /// 回归：上游最后一帧常常不带收尾空行，朴素实现会把它整段丢掉，
    /// 表现为「回复少最后几个字」或「Anthropic 一直等 message_stop」。
    #[test]
    fn trailing_event_without_blank_line_is_not_lost() {
        let mut st = SseReader {
            body: Box::pin(futures::stream::empty()),
            buf: "data: {\"choices\":[{\"delta\":{\"content\":\"tail\"}}]}\n\ndata: [DONE]".into(),
            eof: true,
            chunks: upstream_chat::ChunkState::new(),
            pending: VecDeque::new(),
        };
        // 先吃掉完整事件，缓冲区只剩没有收尾空行的最后一段
        let pos = find_event_end(&st.buf).unwrap();
        let raw: String = st.buf.drain(..pos).collect();
        let evs = st.chunks.feed_raw(&raw).expect("完整事件应产出事件");
        assert!(matches!(evs[0], UnifiedEvent::TextDelta { .. }));
        // 缓冲区里剩下的最后一段没有收尾空行，仍要被解析出来
        let rest = std::mem::take(&mut st.buf);
        let evs = st.chunks.feed_raw(&rest).expect("无收尾空行的尾帧也不能丢");
        assert!(matches!(evs[0], UnifiedEvent::Done { .. }));
    }

    #[test]
    fn sse_multiline_data_is_joined() {
        let ev = parse_one_event("data: {\"choices\":\ndata: [{\"delta\":{\"content\":\"x\"}}]}\n\n").unwrap();
        assert!(matches!(ev, UnifiedEvent::TextDelta { .. }));
    }

    /// LM Studio 对错误路径回的是 200 + 错误信封，不是 404。
    /// 这条判据一旦失效，空补全就会被当成正常结果照常计费。
    #[test]
    fn two_hundred_with_error_envelope_is_not_a_completion() {
        let json: Value = serde_json::from_str(
            r#"{"error":"Unexpected endpoint or method. (POST /chat/completions)"}"#,
        )
        .unwrap();
        let msg = error_envelope(&json).expect("必须认出错误信封");
        assert!(msg.contains("Unexpected endpoint"));

        match envelope_error(msg) {
            UpstreamError::Node { hard, message } => {
                // 软失败：上游是通的，只是这次没给结果，不该冷却节点
                assert!(!hard, "错误信封不该把节点判死");
                assert!(message.contains("/v1"), "错误信息要提示 base_url 该怎么填：{message}");
            }
            other => panic!("实得 {other:?}"),
        }
    }

    #[test]
    fn normal_completion_has_no_error_envelope() {
        let json: Value = serde_json::from_str(
            r#"{"id":"x","model":"m","choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        )
        .unwrap();
        assert!(error_envelope(&json).is_none(), "正常补全不能被误判");
    }

    /// 嵌套的 `error` 字段（工具调用里的）不能被当成信封。
    #[test]
    fn nested_error_field_is_not_an_envelope() {
        let json: Value = serde_json::from_str(
            r#"{"choices":[{"message":{"content":"ok"}}],"error":null}"#,
        )
        .unwrap();
        assert!(error_envelope(&json).is_none());
    }
}
