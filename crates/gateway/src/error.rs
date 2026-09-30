//! 统一错误类型与**按客户端协议渲染**的错误体。
//!
//! 三种协议的 HTTP 错误体形状互不相同，响应体必须跟着客户端协议走：
//! - OpenAI Chat   → `{"error":{"message","type","param","code"}}`
//! - OpenAI Responses → HTTP 层同样是 `{"error":{...}}`；但**流中的** `error` 事件是
//!   扁平的 `{type,code,message,param,sequence_number}`，见 `protocol::responses`。
//! - Anthropic Messages → `{"type":"error","error":{"type","message"},"request_id":...}`

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::protocol::Protocol;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 400 参数非法，或客户端用了网关明确不支持的字段（如 `previous_response_id`）。
    BadRequest,
    /// 401 Key 无效或账号停用。
    Unauthorized,
    /// 402 余额不足。
    PaymentRequired,
    /// 404 模型无路由 / 工作流不存在。
    NotFound,
    /// 429 队列已满或排队超时。
    TooManyRequests,
    /// 500 内部错误。
    Internal,
    /// 502 上游在重试后仍失败。
    Upstream,
    /// 503 当前无可用资源（无节点 / 全部 key 耗尽）。
    NoCapacity,
}

impl ErrorKind {
    pub fn status(self) -> StatusCode {
        match self {
            ErrorKind::BadRequest => StatusCode::BAD_REQUEST,
            ErrorKind::Unauthorized => StatusCode::UNAUTHORIZED,
            ErrorKind::PaymentRequired => StatusCode::PAYMENT_REQUIRED,
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
            ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::Upstream => StatusCode::BAD_GATEWAY,
            ErrorKind::NoCapacity => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// Anthropic 的 `error.type` 词表与其余两套并不重合，需单独映射。
    pub fn anthropic_type(self) -> &'static str {
        match self {
            ErrorKind::BadRequest => "invalid_request_error",
            ErrorKind::Unauthorized => "authentication_error",
            ErrorKind::PaymentRequired => "billing_error",
            ErrorKind::NotFound => "not_found_error",
            ErrorKind::TooManyRequests => "rate_limit_error",
            ErrorKind::Internal | ErrorKind::Upstream => "api_error",
            ErrorKind::NoCapacity => "overloaded_error",
        }
    }

    pub fn openai_type(self) -> &'static str {
        match self {
            ErrorKind::BadRequest => "invalid_request_error",
            ErrorKind::Unauthorized => "authentication_error",
            ErrorKind::PaymentRequired => "insufficient_quota",
            ErrorKind::NotFound => "invalid_request_error",
            ErrorKind::TooManyRequests => "rate_limit_error",
            ErrorKind::Internal | ErrorKind::Upstream => "api_error",
            ErrorKind::NoCapacity => "server_error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub kind: ErrorKind,
    pub message: String,
    /// 全部 key 耗尽时给出最早恢复时刻（unix 秒），进 503 响应体。
    pub reset_at: Option<i64>,
    /// 429 / 503 时给 `Retry-After`。
    pub retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            reset_at: None,
            retry_after: None,
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::BadRequest, msg)
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unauthorized, msg)
    }
    pub fn payment_required(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::PaymentRequired, msg)
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, msg)
    }
    pub fn upstream(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Upstream, msg)
    }
    pub fn no_capacity(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::NoCapacity, msg)
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, msg)
    }

    pub fn queue_full(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::TooManyRequests, msg).with_retry_after(5)
    }

    pub fn too_many_requests(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::TooManyRequests, msg)
    }

    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs);
        self
    }

    pub fn with_reset_at(mut self, at: i64) -> Self {
        self.reset_at = Some(at);
        self
    }

    /// 按客户端协议渲染 HTTP 错误体。
    pub fn to_body(&self, protocol: Protocol, request_id: Option<&str>) -> serde_json::Value {
        match protocol {
            Protocol::Chat => json!({
                "error": {
                    "message": self.message,
                    "type": self.kind.openai_type(),
                    "param": serde_json::Value::Null,
                    "code": serde_json::Value::Null,
                }
            }),
            Protocol::Responses => json!({
                "error": {
                    "message": self.message,
                    "type": self.kind.openai_type(),
                    "code": serde_json::Value::Null,
                }
            }),
            Protocol::Messages => json!({
                "type": "error",
                "error": { "type": self.kind.anthropic_type(), "message": self.message },
                "request_id": request_id,
            }),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %e, "内部错误");
        ApiError::internal("内部错误")
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!(error = %e, "database error");
        ApiError::internal("数据库错误")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.into_response_for(Protocol::Chat, None)
    }
}

impl ApiError {
    pub fn into_response_for(self, protocol: Protocol, request_id: Option<&str>) -> Response {
        let mut builder =
            axum::response::Response::builder().status(self.kind.status());
        if let Some(secs) = self.retry_after {
            builder = builder.header("retry-after", secs.to_string());
        }
        if let Some(rid) = request_id {
            builder = builder.header("x-request-id", rid);
        }
        builder
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                self.to_body(protocol, request_id).to_string(),
            ))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_error_shape_is_nested() {
        let e = ApiError::payment_required("余额不足");
        let body = e.to_body(Protocol::Messages, Some("req_1"));
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "billing_error");
        assert_eq!(body["error"]["message"], "余额不足");
        assert_eq!(body["request_id"], "req_1");
    }

    #[test]
    fn openai_error_shape_is_flat_envelope() {
        let e = ApiError::too_many_requests("排队已满");
        let body = e.to_body(Protocol::Chat, None);
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["message"], "排队已满");
    }

    #[test]
    fn no_capacity_maps_to_overloaded_for_anthropic() {
        let e = ApiError::no_capacity("没有可用节点");
        assert_eq!(e.to_body(Protocol::Messages, None)["error"]["type"], "overloaded_error");
        assert_eq!(e.kind.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
