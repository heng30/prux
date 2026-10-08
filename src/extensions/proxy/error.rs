//! 网关错误与「上游错误文本 → HTTP 状态」的嗅探。
//!
//! provider 层把失败编码进流（`stop_reason == "error"` + `error_message` 字符串），
//! 状态码只以文本形式留在消息里（`provider returned 429 Too Many Requests: ...`），
//! 因此这里集中做一次嗅探：嗅不到就退化为 `502`（不让畸形文本变成 200 或 500）。
//!
//! 分类原则：
//! - 上游 4xx **透传同款状态码**（`429` 尤其重要：客户端靠它触发退避重试）；
//! - 上游 5xx / 网络错误 → `502`；超时 → `504`；非 HTTP 的 provider 错误 → `502`；
//! - 网关自身的校验失败 → `400` / `404`。

use hyper::StatusCode;
use serde_json::{Value, json};

/// 一条可直接变成 HTTP 响应的网关错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayError {
    /// 回给客户端的 HTTP 状态码
    pub status: StatusCode,
    /// OpenAI `error.type`
    pub kind: &'static str,
    /// OpenAI `error.code`
    pub code: Option<String>,
    /// 面向客户端的错误描述（上游 4xx 时保留原文）
    pub message: String,
    /// OpenAI `error.param`：出错的请求字段名
    pub param: Option<String>,
}

impl GatewayError {
    /// 组装一条网关错误；`code` / `param` 为 None 表示 JSON 里省略对应字段。
    fn new(
        status: StatusCode,
        kind: &'static str,
        code: Option<&str>,
        message: impl Into<String>,
        param: Option<&str>,
    ) -> Self {
        Self {
            status,
            kind,
            code: code.map(|c| c.to_string()),
            message: message.into(),
            param: param.map(|p| p.to_string()),
        }
    }

    /// `400 invalid_request_error`
    pub fn invalid(message: impl Into<String>, param: Option<&str>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            None,
            message,
            param,
        )
    }

    /// `400 unsupported_parameter`：字段语义会改变输出但本网关无法如实表达 → 显式拒绝，不静默降级。
    pub fn unsupported(param: &str, note: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some("unsupported_parameter"),
            format!("`{param}` is not supported by this gateway: {note}"),
            Some(param),
        )
    }

    /// `404 model_not_found`
    pub fn model_not_found(model: &str, provider: &str, examples: &[String]) -> Self {
        let hint = if examples.is_empty() {
            String::new()
        } else {
            format!(" Known ids include: {}.", examples.join(", "))
        };
        Self::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            Some("model_not_found"),
            format!("model `{model}` is not available on provider `{provider}`.{hint}"),
            Some("model"),
        )
    }

    /// `401`（入站 token 校验失败）
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "invalid_request_error",
            Some("invalid_api_key"),
            "missing or invalid Authorization header (expected `Bearer <token>`)",
            None,
        )
    }

    /// `413`（请求体超过上限）
    pub fn too_large(limit: usize) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            Some("request_too_large"),
            format!("request body exceeds the {limit} byte limit"),
            None,
        )
    }

    /// `429 rate_limit_exceeded`（并发上限）
    pub fn too_many_requests(limit: usize) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            Some("rate_limit_exceeded"),
            format!("too many concurrent requests (limit {limit}), retry later"),
            None,
        )
    }

    /// `500`
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            None,
            message,
            None,
        )
    }

    /// `404`（未知路径）
    pub fn not_found(path: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            Some("not_found"),
            format!(
                "unknown endpoint `{path}` (supported: /v1/chat/completions, /v1/models, /health)"
            ),
            None,
        )
    }

    /// `405`（路径存在但方法不对）
    pub fn method_not_allowed(method: &str, path: &str) -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "invalid_request_error",
            Some("method_not_allowed"),
            format!("method {method} is not allowed on `{path}`"),
            None,
        )
    }

    /// 由上游错误文本构造：嗅探其中的 HTTP 状态码并分类。
    pub fn from_upstream(message: impl Into<String>) -> Self {
        let message = message.into();
        match sniff_upstream_status(&message) {
            Some(status) if status.is_client_error() => {
                let (kind, code) = if status == StatusCode::TOO_MANY_REQUESTS {
                    ("rate_limit_error", Some("rate_limit_exceeded"))
                } else {
                    ("invalid_request_error", Some("upstream_error"))
                };
                Self::new(status, kind, code, message, None)
            }
            Some(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "api_error",
                Some("upstream_error"),
                message,
                None,
            ),
            None if looks_like_timeout(&message) => Self::new(
                StatusCode::GATEWAY_TIMEOUT,
                "api_error",
                Some("upstream_timeout"),
                message,
                None,
            ),
            None => Self::new(
                StatusCode::BAD_GATEWAY,
                "api_error",
                Some("upstream_error"),
                message,
                None,
            ),
        }
    }

    /// OpenAI 错误响应体
    pub fn body(&self) -> Value {
        json!({
            "error": {
                "message": self.message,
                "type": self.kind,
                "param": self.param,
                "code": self.code,
            }
        })
    }
}

/// 从错误文本里嗅探上游状态码（`provider returned 429 ...`）。
///
/// 唯一的脆弱点集中在这里：provider 层的 `Error::ProviderStatus` 的 Display 是
/// `provider returned {status}: {body}`，故按该前缀取紧邻的 3 位数字。
fn sniff_upstream_status(message: &str) -> Option<StatusCode> {
    /// 上游状态错误文本的固定前缀，紧随其后的是三位 HTTP 状态码。
    const MARKER: &str = "provider returned ";
    let rest = message.split(MARKER).nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();

    if digits.len() != 3 {
        return None;
    }

    let code: u16 = digits.parse().ok()?;
    StatusCode::from_u16(code).ok()
}

/// 错误文本（大小写不敏感）里是否含 timeout / timed out，用于把无状态码的网络错误归为 504。
fn looks_like_timeout(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("timed out") || m.contains("timeout")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_status_code_from_provider_status_text() {
        let e = GatewayError::from_upstream(
            "provider returned 429 Too Many Requests: {\"error\":{\"message\":\"slow down\"}}",
        );
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.kind, "rate_limit_error");
        assert_eq!(e.code.as_deref(), Some("rate_limit_exceeded"));
        assert!(e.message.contains("slow down"), "上游原文应保留");

        let e = GatewayError::from_upstream("provider returned 401 Unauthorized: bad key");
        assert_eq!(e.status, StatusCode::UNAUTHORIZED, "4xx 应透传同款状态码");

        let e = GatewayError::from_upstream("provider returned 404 Not Found: no such model");
        assert_eq!(e.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn upstream_5xx_and_unknown_degrade_to_502() {
        let e = GatewayError::from_upstream("provider returned 500 Internal Server Error: boom");
        assert_eq!(e.status, StatusCode::BAD_GATEWAY);

        let e = GatewayError::from_upstream("provider error: model exploded");
        assert_eq!(e.status, StatusCode::BAD_GATEWAY);
        assert_eq!(e.code.as_deref(), Some("upstream_error"));

        let e = GatewayError::from_upstream("request failed: error sending request for url");
        assert_eq!(e.status, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn timeouts_map_to_504() {
        let e = GatewayError::from_upstream("request failed: operation timed out");
        assert_eq!(e.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(e.code.as_deref(), Some("upstream_timeout"));
    }

    #[test]
    fn malformed_status_text_does_not_fake_a_status() {
        // 3 位数字之外（如 4 位/缺失）不应被当作状态码
        assert_eq!(sniff_upstream_status("provider returned 4290: x"), None);
        assert_eq!(sniff_upstream_status("provider returned xyz: x"), None);
        assert_eq!(sniff_upstream_status("no marker here"), None);
        assert!(sniff_upstream_status("provider returned 429: x").is_some());
    }

    #[test]
    fn body_shape_matches_openai_error() {
        let body = GatewayError::invalid("bad thing", Some("model")).body();
        assert_eq!(body["error"]["message"], "bad thing");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["param"], "model");
    }
}
