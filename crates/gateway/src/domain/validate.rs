//! 写入前的归一化与校验。**所有写库入口都必须先过这里**，
//! 保证非法配置进不了库，运行时就不必反复防御。

use crate::error::{ApiError, ApiResult};
use crate::protocol::upstream_chat::MAX_STOP_SEQUENCES;

use super::window::DisabledWindow;

/// 节点名（LLM 与 ComfyUI 共用同一套字符集）。
const NODE_NAME_MAX: usize = 64;

fn text(v: Option<&str>, field: &str, required: bool, max: usize) -> ApiResult<String> {
    let t = v.unwrap_or("").trim().to_string();
    if t.is_empty() {
        if required {
            return Err(ApiError::bad_request(format!("{field} 不能为空")));
        }
        return Ok(String::new());
    }
    if t.chars().count() > max {
        return Err(ApiError::bad_request(format!("{field} 长度不能超过 {max}")));
    }
    Ok(t)
}

fn int_in(v: i64, field: &str, lo: i64, hi: i64) -> ApiResult<i64> {
    if v < lo || v > hi {
        return Err(ApiError::bad_request(format!("{field} 必须在 {lo}–{hi} 之间")));
    }
    Ok(v)
}

pub fn validate_node_name(name: &str) -> ApiResult<String> {
    let n = text(Some(name), "节点名称", true, NODE_NAME_MAX)?;
    let mut chars = n.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-');
    if !first_ok || !rest_ok {
        return Err(ApiError::bad_request(
            "节点名称只能包含字母、数字、下划线、点或短横，且以字母数字开头",
        ));
    }
    Ok(n)
}

pub fn validate_http_url(url: &str, field: &str) -> ApiResult<String> {
    let u = text(Some(url), field, true, 500)?;
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return Err(ApiError::bad_request(format!(
            "{field} 必须以 http:// 或 https:// 开头"
        )));
    }
    Ok(u.trim_end_matches('/').to_string())
}

pub fn validate_optional_url(url: Option<&str>, field: &str) -> ApiResult<Option<String>> {
    match url {
        Some(u) if !u.trim().is_empty() => Ok(Some(validate_http_url(u, field)?)),
        _ => Ok(None),
    }
}

/// 校验「每日启用禁用时间窗」。LLM 节点与 ComfyUI 端点共用。
///
/// 起止相同等价于永久禁用，**写入时直接拒绝**——请改用启用开关，
/// 否则运行时「这个节点到底能不能用」会变得没法解释。
pub fn validate_window(
    start: Option<i64>,
    end: Option<i64>,
    timezone: Option<&str>,
) -> ApiResult<DisabledWindow> {
    match (start, end) {
        (None, None) => Ok(DisabledWindow::default()),
        (Some(s), Some(e)) => {
            int_in(s, "禁用时段开始", 0, 23)?;
            int_in(e, "禁用时段结束", 0, 23)?;
            if s == e {
                return Err(ApiError::bad_request(
                    "禁用时段起止不能相同（等价于永久禁用，请直接停用该节点）",
                ));
            }
            let tz = text(timezone, "禁用时段时区", true, 64)?;
            if tz.parse::<chrono_tz::Tz>().is_err() {
                return Err(ApiError::bad_request(format!("未知时区: {tz}")));
            }
            Ok(DisabledWindow::new(Some(s as u8), Some(e as u8), Some(tz)))
        }
        _ => Err(ApiError::bad_request("禁用时段必须同时填写开始与结束")),
    }
}

pub const MAX_NODE_CONCURRENCY: i64 = 64;
pub const MAX_COMFYUI_CONCURRENCY: i64 = 64;
pub const MAX_ACCOUNT_CONCURRENCY: i64 = 64;
pub const MAX_ACCOUNT_QUEUE: i64 = 256;

/// `stop_sequences` 上限与上游一致；超出截断并告知调用方记日志。
pub fn clamp_stop(stop: Vec<String>) -> (Vec<String>, bool) {
    let truncated = stop.len() > MAX_STOP_SEQUENCES;
    (
        stop.into_iter().take(MAX_STOP_SEQUENCES).collect(),
        truncated,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_name_must_start_alphanumeric() {
        assert!(validate_node_name("n1").is_ok());
        assert!(validate_node_name("_n1").is_err());
        assert!(validate_node_name("n-1.a_b").is_ok());
        assert!(validate_node_name("节点").is_err());
    }

    #[test]
    fn url_must_be_http_and_gets_trimmed() {
        assert_eq!(
            validate_http_url("http://a:1234/", "base_url").unwrap(),
            "http://a:1234"
        );
        assert!(validate_http_url("ftp://a", "base_url").is_err());
    }

    #[test]
    fn empty_optional_url_becomes_none() {
        assert_eq!(validate_optional_url(Some(""), "lan").unwrap(), None);
        assert_eq!(validate_optional_url(None, "lan").unwrap(), None);
    }

    /// 起止相同必须拒绝——它是「这个节点到底能不能用」的死结。
    #[test]
    fn equal_window_bounds_are_rejected() {
        let err = validate_window(Some(3), Some(3), Some("UTC")).unwrap_err();
        assert!(err.message.contains("不能相同"));
    }

    #[test]
    fn window_requires_valid_timezone() {
        assert!(validate_window(Some(1), Some(6), None).is_err());
        assert!(validate_window(Some(1), Some(6), Some("Not/AZone")).is_err());
        assert!(validate_window(Some(1), Some(6), Some("UTC")).is_ok());
    }

    #[test]
    fn half_configured_window_is_rejected() {
        assert!(validate_window(Some(1), None, Some("UTC")).is_err());
        assert!(validate_window(None, Some(1), Some("UTC")).is_err());
    }

    #[test]
    fn no_window_configured_is_fine() {
        assert!(!validate_window(None, None, None).unwrap().is_configured());
    }

    #[test]
    fn stop_sequences_clamp_to_four() {
        let (out, truncated) =
            clamp_stop(vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()]);
        assert_eq!(out.len(), 4);
        assert!(truncated);
    }
}
