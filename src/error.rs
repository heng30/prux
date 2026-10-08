use std::error::Error as StdError;
use std::io;

/// Convenience alias used by all library code.
pub type Result<T> = std::result::Result<T, Error>;

/// 库内统一的错误类型，覆盖消息、IO、JSON、网络等失败场景。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Plain message error (semantic / validation failures).
    #[error("{0}")]
    Message(String),

    /// Filesystem error with context.
    #[error("{context}: {source}")]
    Io {
        /// 出错时正在执行的操作描述。
        context: String,
        /// 触发失败的底层 IO 错误（错误链来源）。
        #[source]
        source: io::Error,
    },

    /// JSON (de)serialization error with context.
    #[error("{context}: {source}")]
    Json {
        /// 出错时正在解析/序列化的内容描述。
        context: String,
        /// 触发失败的底层 JSON 错误（错误链来源）。
        #[source]
        source: serde_json::Error,
    },

    /// Bare filesystem error (auto-converted via `?`).
    #[error("io error: {0}")]
    PlainIo(#[from] io::Error),

    /// Bare JSON error (auto-converted via `?`).
    #[error("json error: {0}")]
    PlainJson(#[from] serde_json::Error),

    /// HTTP request transport failure.
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),

    /// Provider responded with a non-success HTTP status.
    #[error("provider returned {status}: {body}")]
    ProviderStatus {
        /// provider 返回的 HTTP 状态码。
        status: reqwest::StatusCode,
        /// provider 响应体文本，用于展示错误详情。
        body: String,
    },

    /// Provider reported an error payload.
    #[error("provider error: {0}")]
    ProviderMessage(String),

    /// The model entry uses an API type this library does not support.
    #[error("{api}")]
    UnsupportedApi { api: String },

    /// A session tree entry could not be found.
    #[error("entry not found: {0}")]
    EntryNotFound(String),

    /// 会话 record-log 损坏 / 结构错误（来自 session_v4 的归约/解析校验）。
    /// Display 透传 `RecordLogError` 自身文案，保留类型供上层 `matches!` 判断。
    #[error("{0}")]
    SessionCorrupt(#[from] crate::core::session_v4::RecordLogError),
}

impl Error {
    /// Build a plain [`Error::Message`].
    #[inline]
    pub fn msg(m: impl Into<String>) -> Self {
        Error::Message(m.into())
    }

    /// 完整 Display：展开 source 因果链。
    /// reqwest 传输错误默认 Display 只输出 `error sending request for url (...)`，
    /// 底层原因（connection refused / dns / tls 等）只在 source 链里，
    /// 这里展开出来供重试分类与用户展示使用。
    pub fn display_full(&self) -> String {
        let mut s = self.to_string();
        let mut cur: Option<&(dyn StdError + 'static)> = self.source();
        while let Some(src) = cur {
            s.push_str(": ");
            s.push_str(&src.to_string());
            cur = src.source();
        }
        s
    }
}

impl From<&str> for Error {
    /// 转成 [`Error::Message`]（字符串拷贝一份）。
    #[inline]
    fn from(s: &str) -> Self {
        Error::Message(s.to_string())
    }
}

impl From<String> for Error {
    /// 转成 [`Error::Message`]（直接接管字符串所有权）。
    #[inline]
    fn from(s: String) -> Self {
        Error::Message(s)
    }
}
