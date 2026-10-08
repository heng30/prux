//! `mcp` 扩展的错误类型。

use crate::utils::file_lock::LockError;
use http::header::{InvalidHeaderName, InvalidHeaderValue};
use rmcp::{service::ClientInitializeError, transport::auth::AuthError};
use std::error::Error as StdError;

/// `mcp` 扩展统一 `Result`。
pub type Result<T> = std::result::Result<T, McpError>;

/// 构造 [`McpError::Message`] 并提前返回
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::extensions::mcp::error::McpError::Message(format!($($arg)*)))
    };
}

pub(crate) use crate::bail;

/// `mcp` 扩展错误。
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// 配置校验 / 语义错误
    #[error("{0}")]
    Message(String),

    /// 服务器未在配置中定义。
    #[error("MCP server `{0}` is not configured")]
    ServerNotConfigured(String),

    /// 服务器已被禁用。
    #[error("MCP server `{0}` is disabled")]
    ServerDisabled(String),

    /// 缓存与服务器里都找不到指定工具。
    #[error("MCP tool `{server}/{tool}` was not found")]
    ToolNotFound {
        /// 目标服务器名。
        server: String,
        /// 目标工具名。
        tool: String,
    },

    /// 单次 RPC 超时。
    #[error("{label} timed out after {millis} ms")]
    Timeout {
        /// 操作标签（如 `MCP tool call`）。
        label: String,
        /// 超时阈值（毫秒）。
        millis: u128,
    },

    /// RPC 因父级取消而中止。
    #[error("{label} cancelled")]
    Cancelled {
        /// 操作标签（如 `MCP tool call`）。
        label: String,
    },

    /// 带上下文的底层错误（读盘 / 解析 / 建连等）。
    #[error("{context}: {source}")]
    Context {
        /// 上层补充的上下文描述。
        context: String,
        /// 被包装的原始错误。
        #[source]
        source: Box<dyn StdError + Send + Sync + 'static>,
    },

    /// 无上下文透传的文件系统错误。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// 无上下文透传的 JSON 错误。
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// URL 解析失败。
    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),

    /// HTTP 头名称非法。
    #[error("invalid HTTP header name: {0}")]
    HeaderName(#[from] InvalidHeaderName),

    /// HTTP 头值非法。
    #[error("invalid HTTP header value: {0}")]
    HeaderValue(#[from] InvalidHeaderValue),

    /// 跨进程文件锁获取失败。
    #[error(transparent)]
    Lock(#[from] LockError),

    /// rmcp OAuth 错误。
    #[error("MCP OAuth: {0}")]
    Auth(Box<AuthError>),

    /// MCP 连接初始化失败（`serve` 失败）。
    #[error("MCP connection failed: {0}")]
    Connect(Box<ClientInitializeError>),
}

impl From<AuthError> for McpError {
    /// 认证失败转成 [`McpError::Auth`]（保留底层错误）。
    fn from(error: AuthError) -> Self {
        Self::Auth(Box::new(error))
    }
}

impl From<ClientInitializeError> for McpError {
    /// 客户端初始化失败转成 [`McpError::Connect`]（保留底层错误）。
    fn from(error: ClientInitializeError) -> Self {
        Self::Connect(Box::new(error))
    }
}

/// 为 `Result` / `Option` 附加错误上下文
///
/// - `Result<T, E>`：包装为 [`McpError::Context`]，保留底层错误为 `source`；
/// - `Option<T>`：转成 [`McpError::Message`]（无底层错误可挂）。
pub trait Context<T> {
    /// 惰性无关的固定上下文。
    fn context(self, context: impl Into<String>) -> Result<T>;

    /// 仅在出错时求值的上下文（可复用格式化参数）。
    fn with_context<C, F>(self, context: F) -> Result<T>
    where
        F: FnOnce() -> C,
        C: Into<String>;
}

impl<T, E> Context<T> for std::result::Result<T, E>
where
    E: StdError + Send + Sync + 'static,
{
    /// 出错时包装为 [`McpError::Context`]，底层错误保留为 `source`。
    fn context(self, context: impl Into<String>) -> Result<T> {
        self.map_err(|source| McpError::Context {
            context: context.into(),
            source: Box::new(source),
        })
    }

    /// 同 [`Context::context`]，但上下文文本仅在出错时求值。
    fn with_context<C, F>(self, context: F) -> Result<T>
    where
        F: FnOnce() -> C,
        C: Into<String>,
    {
        self.map_err(|source| McpError::Context {
            context: context().into(),
            source: Box::new(source),
        })
    }
}

impl<T> Context<T> for Option<T> {
    /// `None` 时转成 [`McpError::Message`]（无底层错误可挂）。
    fn context(self, context: impl Into<String>) -> Result<T> {
        self.ok_or_else(|| McpError::Message(context.into()))
    }

    /// 同 [`Context::context`]，但上下文文本仅在 `None` 时求值。
    fn with_context<C, F>(self, context: F) -> Result<T>
    where
        F: FnOnce() -> C,
        C: Into<String>,
    {
        self.ok_or_else(|| McpError::Message(context().into()))
    }
}
