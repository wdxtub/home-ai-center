//! 节点健康复检。
//!
//! 为什么需要它：网关的冷却退避是**被动**的——节点挂了要等下一次真实请求
//! 撞上才知道。家里的节点经常是「睡一觉起来就好了」，如果没人来试，
//! 它们会一直躺在冷却里白白浪费。
//!
//! 复检用一次**最小真实推理**（`max_completion_tokens: 1`），
//! 而不是 `GET /models`：后者不会触发模型加载，「恢复」可能只是服务在跑
//! 而模型没加载，真实请求仍然会超时。

use std::sync::Arc;
use std::time::Duration;

use crate::gate::node_gate::FailureKind;
use crate::state::AppState;
use crate::upstream::openai::{self, UpstreamError};

pub async fn probe_all(s: &Arc<AppState>) {
    let snap = s.snapshot();
    let Ok(client) = openai::OpenAiClient::new(20) else {
        return;
    };

    for node in &snap.nodes {
        if !node.enabled {
            continue;
        }
        // 冷却中的节点才值得探：正常的节点没必要打扰
        if !s.health.is_cooling(node.id).await {
            continue;
        }

        // 用任意一把「没被隔离」的 key 探。全部隔离说明是人为封的，
        // 探了也只是重复报同一个错。
        let key = match node
            .keys
            .iter()
            .find(|k| k.enabled && k.state != crate::domain::node::NodeKeyState::Quarantined)
        {
            Some(k) => k,
            None => continue,
        };
        // 探针必须走**真实的模型名**：上游报「没有这个模型」说明
        // 模型没加载，恰恰是最值得复检的信号。
        let Some(model) = snap
            .routes
            .iter()
            .find(|r| r.node_id == node.id)
            .map(|r| r.upstream_model.clone())
        else {
            continue;
        };

        // 和真实请求同一套地址优先级（内网优先）。探针走公网而请求走内网，
        // 就会出现「探针说活着、请求连不上」的假象。
        let urls = node.base_urls(s.health.is_lan_down(node.id).await);
        let mut outcome: Result<(), String> = Err("没有可用地址".into());
        for (i, url) in urls.iter().enumerate() {
            outcome = openai::probe(
                &client,
                url,
                &key.secret,
                &model,
                &node.extra_body,
                i + 1 < urls.len(),
            )
            .await
            .map_err(|e| e.to_string());
            if outcome.is_ok() {
                if i > 0 {
                    s.health.mark_lan_up(node.id).await;
                }
                break;
            }
            let transport = matches!(&outcome, Err(m) if is_transport(m));
            if transport && i + 1 < urls.len() {
                s.health.mark_lan_down(node.id, url).await;
                continue;
            }
            break;
        }

        match outcome {
            Ok(()) => {
                s.health.mark_success(node.id).await;
            }
            Err(e) => {
                // 探针只延长冷却，不做新的退避升级：
                // 它证明「还不行」，但没提供新的信息量。
                tracing::debug!(node = node.name, error = %e, "复检未通过");
                s.health
                    .mark_failure(node.id, FailureKind::Soft, &e)
                    .await;
            }
        }
    }
}

/// 探针错误文案里带这些词，就认为是「连不上」而不是「连上了但答错」。
fn is_transport(msg: &str) -> bool {
    const MARKERS: [&str; 5] = [
        "error sending request",
        "connection refused",
        "timed out",
        "timeout",
        "dns error",
    ];
    let lower = msg.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

pub async fn scheduler(s: Arc<AppState>) {
    let interval = Duration::from_secs(s.cfg.probe_interval_secs.max(10) * 60);
    // 启动后先等一会儿：刚起来的进程没必要立刻去打扰还在启动的节点
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        probe_all(&s).await;
        tokio::time::sleep(interval).await;
    }
}

/// ComfyUI 端点复检。只探 `/system_stats`，不排队。
pub async fn probe_comfy(s: &Arc<AppState>) {
    let snap = s.snapshot();
    let Ok(client) = crate::upstream::comfyui::ComfyClient::new(
        Duration::from_secs(5),
        Duration::from_secs(5),
    ) else {
        return;
    };
    for n in &snap.comfy_nodes {
        if n.enabled && client.probe(n).await.is_ok() {
            tracing::debug!(node = n.name, "ComfyUI 端点正常");
        }
    }
}

/// 把一次上游失败归到节点健康表。供请求路径复用。
pub async fn note_failure(s: &Arc<AppState>, node_id: i64, e: &UpstreamError) {
    let (kind, msg) = match e {
        UpstreamError::Node { hard, message } => (
            if *hard { FailureKind::Hard } else { FailureKind::Soft },
            message.as_str(),
        ),
        UpstreamError::Transport { message, timeout } => (
            if *timeout { FailureKind::Soft } else { FailureKind::Hard },
            message.as_str(),
        ),
        // 限额是 key 的问题、模型加载中节点是活的、瞬时错误原地重试就有：
        // 都不该动节点冷却
        UpstreamError::Quota(_)
        | UpstreamError::ModelLoading
        | UpstreamError::Client(_)
        | UpstreamError::Transient { .. } => {
            return;
        }
    };
    s.health.mark_failure(node_id, kind, msg).await;
}
