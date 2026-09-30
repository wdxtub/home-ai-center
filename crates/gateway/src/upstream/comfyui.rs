//! ComfyUI 客户端：提交 → 轮询历史 → 取图。
//!
//! 与 LLM 上游的差别决定了这里**只能换一个地方换端点**：
//! ComfyUI 一次出图要跑几十秒，提交之后任务就绑死在那个端点上了。
//! 因此 failover 只允许发生在**提交之前**（连不上 / 5xx / OOM），
//! 提交之后失败就是失败，绝不重投——重投会双倍扣费并产生重复图。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::domain::comfy::ComfyNode;

/// 一张成品图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub filename: String,
    pub subfolder: String,
    pub kind: String,
}

/// 一次出图的结果。
#[derive(Debug, Clone)]
pub struct Rendered {
    pub images: Vec<ImageRef>,
    pub elapsed: Duration,
    pub queue_wait: Duration,
}

/// ComfyUI 侧失败。分类决定**要不要换端点**。
#[derive(Debug)]
pub enum ComfyError {
    /// 提交前失败：换端点重试。
    Endpoint { message: String },
    /// 提交后失败：只能报错，重投会双倍扣费。
    Job { message: String },
    /// 工作流 / 参数本身有问题：换端点也没用。
    Workflow(String),
    /// 排队或执行超时。
    Timeout,
}

impl std::fmt::Display for ComfyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComfyError::Endpoint { message } => write!(f, "端点不可用：{message}"),
            ComfyError::Job { message } => write!(f, "任务失败：{message}"),
            ComfyError::Workflow(m) => write!(f, "工作流错误：{m}"),
            ComfyError::Timeout => write!(f, "出图超时"),
        }
    }
}

/// 轮询间隔。ComfyUI 的 `/history` 没有推送，只能自己问。
const POLL_INTERVAL: Duration = Duration::from_millis(700);

pub struct ComfyClient {
    http: reqwest::Client,
    /// 轮询上限。生图几分钟很常见，但也不能无限等。
    poll_timeout: Duration,
}

impl ComfyClient {
    pub fn new(connect_timeout: Duration, poll_timeout: Duration) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            // **不要设整体 timeout**：出图请求挂着是在等生成，
            // 用整体超时会把它当失败，任务其实还在节点上跑。
            .no_proxy()
            .build()?;
        Ok(Self { http, poll_timeout })
    }

    fn req(&self, node: &ComfyNode, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if !node.username.is_empty() {
            r.basic_auth(&node.username, Some(&node.password))
        } else {
            r
        }
    }

    /// 跑一次出图。`workflow` 必须是 ComfyUI API 格式（`{"<node_id>": {...}}`）。
    pub async fn render(
        &self,
        node: &ComfyNode,
        workflow: &Value,
        client_id: &str,
    ) -> Result<Rendered, ComfyError> {
        let started = Instant::now();

        let body = json!({ "prompt": workflow, "client_id": client_id });
        let resp = self
            .req(node, self.http.post(format!("{}/prompt", base(node.effective_base_url()))))
            .json(&body)
            .send()
            .await
            .map_err(|e| ComfyError::Endpoint { message: e.to_string() })?;

        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(classify_submit(status, &text));
        }
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let Some(prompt_id) = v.get("prompt_id").and_then(Value::as_str) else {
            return Err(ComfyError::Job {
                message: "ComfyUI 没有返回 prompt_id".into(),
            });
        };

        let images = self.wait_for_images(node, prompt_id, started).await?;
        Ok(Rendered {
            images,
            elapsed: started.elapsed(),
            queue_wait: Duration::ZERO,
        })
    }

    /// 轮询 `/history/{id}`，直到出现输出或超时。
    async fn wait_for_images(
        &self,
        node: &ComfyNode,
        prompt_id: &str,
        started: Instant,
    ) -> Result<Vec<ImageRef>, ComfyError> {
        let url = format!("{}/history/{}", base(node.effective_base_url()), prompt_id);
        loop {
            if started.elapsed() > self.poll_timeout {
                return Err(ComfyError::Timeout);
            }
            let resp = self
                .req(node, self.http.get(&url))
                .send()
                .await
                .map_err(|e| ComfyError::Endpoint { message: e.to_string() })?;
            if resp.status().as_u16() == 404 {
                // 任务还没进历史，继续等
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            let text = resp.text().await.unwrap_or_default();
            let v: Value = serde_json::from_str(&text).map_err(|_| ComfyError::Job {
                message: "history 返回的不是 JSON".into(),
            })?;

            // history 是 { "<prompt_id>": { status, outputs } }
            let Some(entry) = v.get(prompt_id) else {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            };

            if let Some(err) = extract_error(entry) {
                return Err(ComfyError::Job { message: err });
            }
            let images = extract_images(entry);
            if !images.is_empty() {
                return Ok(images);
            }
            // 还在跑：status.completed 为 false
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// 拉一张图的字节。**按需调用**——base64 内联返回时可能根本用不到。
    pub async fn fetch_image(
        &self,
        node: &ComfyNode,
        img: &ImageRef,
    ) -> Result<Vec<u8>, ComfyError> {
        let url = format!(
            "{}/view?filename={}&subfolder={}&type={}",
            base(node.effective_base_url()),
            urlencode(&img.filename),
            urlencode(&img.subfolder),
            urlencode(&img.kind),
        );
        let resp = self
            .req(node, self.http.get(&url))
            .send()
            .await
            .map_err(|e| ComfyError::Endpoint { message: e.to_string() })?;
        if !resp.status().is_success() {
            return Err(ComfyError::Job {
                message: format!("取图失败 HTTP {}", resp.status()),
            });
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| ComfyError::Job { message: e.to_string() })
    }

    /// 健康探测：`/system_stats` 轻量且不排队。
    pub async fn probe(&self, node: &ComfyNode) -> Result<(), String> {
        let resp = self
            .req(node, self.http.get(format!("{}/system_stats", base(node.effective_base_url()))))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("HTTP {}", resp.status()))
        }
    }
}

fn base(url: &str) -> &str {
    url.trim_end_matches('/')
}

/// query 安全的编码集：只保留 RFC 3986 的 unreserved 字符。
/// 直接用 `NON_ALPHANUMERIC` 会把文件名里的 `.` 也编掉，
/// 合法但难看，而且某些 ComfyUI 版本的路由对 `%2E` 处理不一致。
const QUERY_SAFE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn urlencode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, QUERY_SAFE).to_string()
}

/// 提交阶段的错误分类。
///
/// 关键分界：**400 且不是 OOM → 工作流本身错了**，换端点只是把同一个
/// 坏工作流再发一遍。ComfyUI 显存不足也会报 400，所以必须看正文。
pub fn classify_submit(status: u16, text: &str) -> ComfyError {
    let lower = text.to_ascii_lowercase();
    if lower.contains("out of memory") || lower.contains("oom") {
        return ComfyError::Endpoint {
            message: "ComfyUI 显存不足".into(),
        };
    }
    match status {
        400 | 422 => ComfyError::Workflow(truncate(text, 300)),
        401 | 403 => ComfyError::Endpoint {
            message: "ComfyUI 认证失败".into(),
        },
        404 => ComfyError::Endpoint {
            message: "ComfyUI 端点不存在（可能没启动或路径不对）".into(),
        },
        s if s >= 500 => ComfyError::Endpoint {
            message: format!("HTTP {s}: {}", truncate(text, 200)),
        },
        s => ComfyError::Endpoint {
            message: format!("HTTP {s}: {}", truncate(text, 200)),
        },
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.trim().chars().take(n).collect()
}

/// 任务级错误（history 里的 status 段）。
pub fn extract_error(entry: &Value) -> Option<String> {
    let status = entry.get("status")?;
    if status.get("completed").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let msgs = status.get("messages")?.as_array()?;
    for m in msgs {
        // ComfyUI 的形状是 ["execution_error", {exception_message, ...}]
        if let Some(kind) = m.get(0).and_then(Value::as_str) {
            if kind.contains("error") {
                let detail = m
                    .get(1)
                    .and_then(|d| {
                        d.get("exception_message")
                            .and_then(Value::as_str)
                            .or_else(|| d.get("node_type").and_then(Value::as_str))
                    })
                    .unwrap_or(kind);
                return Some(truncate(detail, 300));
            }
        }
    }
    None
}

/// 从 `outputs` 里收集所有图片引用（多批次会跨多个节点）。
pub fn extract_images(entry: &Value) -> Vec<ImageRef> {
    let mut out = Vec::new();
    let Some(outputs) = entry.get("outputs").and_then(Value::as_object) else {
        return out;
    };
    // 用 BTreeMap 保证同一张图跨节点时的顺序稳定，方便测试
    for (_node, v) in outputs.iter().collect::<BTreeMap<_, _>>() {
        let Some(imgs) = v.get("images").and_then(Value::as_array) else {
            continue;
        };
        for im in imgs {
            let filename = im.get("filename").and_then(Value::as_str);
            let Some(filename) = filename else { continue };
            out.push(ImageRef {
                filename: filename.to_string(),
                subfolder: im
                    .get("subfolder")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                kind: im
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("output")
                    .to_string(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oom_in_a_400_is_still_an_endpoint_fault() {
        // ComfyUI 显存不足也报 400。误判成工作流错误会导致
        // 换端点重试，而正确处置是把这个端点降权。
        let e = classify_submit(400, r#"{"error":{"message":"Prompt outputs failed validation: out of memory"}}"#);
        assert!(matches!(e, ComfyError::Endpoint { .. }), "实得 {e:?}");
    }

    #[test]
    fn plain_400_is_a_workflow_error_not_failover() {
        let e = classify_submit(400, r#"{"error":{"message":"node 6: invalid value"}}"#);
        assert!(matches!(e, ComfyError::Workflow(_)), "实得 {e:?}");
    }

    #[test]
    fn images_are_collected_from_every_output_node() {
        let entry = json!({
            "outputs": {
                "9": {"images": [{"filename": "b.png", "subfolder": "", "type": "output"}]},
                "12": {"images": [
                    {"filename": "a_1.png", "subfolder": "", "type": "output"},
                    {"filename": "a_2.png", "subfolder": "", "type": "output"}
                ]}
            }
        });
        let imgs = extract_images(&entry);
        assert_eq!(imgs.len(), 3);
        // 顺序必须稳定，否则测试与归档路径都不可复现
        assert_eq!(imgs[0].filename, "a_1.png");
        assert_eq!(imgs[2].filename, "b.png");
    }

    #[test]
    fn execution_error_message_is_surfaced() {
        let entry = json!({
            "status": {"completed": false, "messages": [
                ["execution_error", {"exception_message": "OOM: not enough memory", "node_type": "KSampler"}]
            ]}
        });
        assert_eq!(
            extract_error(&entry).as_deref(),
            Some("OOM: not enough memory")
        );
    }

    #[test]
    fn completed_status_has_no_error() {
        let entry = json!({
            "status": {"completed": true, "messages": []},
            "outputs": {"9": {"images": [{"filename": "x.png"}]}}
        });
        assert!(extract_error(&entry).is_none());
        assert_eq!(extract_images(&entry).len(), 1);
    }

    /// 正在排队时 status 里没有 error 消息，不能被误判成失败。
    #[test]
    fn running_job_is_not_an_error() {
        let entry = json!({
            "status": {"completed": false, "messages": [["execution_start", {"node": "3"}]]},
            "outputs": {}
        });
        assert!(extract_error(&entry).is_none());
        assert!(extract_images(&entry).is_empty());
    }

    #[test]
    fn filename_with_spaces_is_encoded() {
        assert_eq!(urlencode("a b/c.png"), "a%20b%2Fc.png");
    }
}
