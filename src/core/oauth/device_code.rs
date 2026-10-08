//! RFC 8628 device authorization grant 通用轮询器
//!
//! 各 provider 流程负责：发起 device 授权请求（拿 device_code / user_code）、
//! 解析 token 端点的轮询响应。本模块只负责轮询节奏：间隔、慢速增加、超时。
//!
//! 使用方式（TUI tick 驱动）：
//! 1. `DeviceCodeFlow::now()` 判断是否到轮询时机（`poll_due_in() == 0`）
//! 2. 发起一次 token 端点请求
//! 3. 根据结果调用 `on_pending()` / `on_slow_down(interval)` / 或完成

use super::OAuthCredential;
use crate::{
    error::{Error, Result},
    utils::{http, time::now_ms},
};
use serde_json::Value;
use std::{future::Future, pin::Pin, time::Duration};

/// RFC 8628 §3.2：若 authorization server 省略 interval，客户端必须用 5 秒。
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
/// RFC 8628 §3.5：slow_down 意味着间隔必须增加 5 秒。
pub const SLOW_DOWN_INTERVAL_INCREMENT_MS: u64 = 5000;

/// 发起设备授权请求的 future（`request_device` 返回）
pub type RequestDeviceFuture<'a> = Pin<Box<dyn Future<Output = Result<DeviceInfo>> + Send + 'a>>;
/// 轮询 token 端点的 future（`poll_token` 返回）
pub type PollTokenFuture<'a> =
    Pin<Box<dyn Future<Output = Result<DevicePollResult<DeviceSuccess>>> + Send + 'a>>;
/// 中间值换最终凭据的 future（`finalize` 返回）：(凭据, 进度消息)
pub type FinalizeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(OAuthCredential, Vec<String>)>> + Send + 'a>>;

/// 一次轮询请求的结果（provider 解析 token 端点响应后返回）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePollResult<T> {
    /// 用户尚未授权，继续等
    Pending,
    /// 服务器要求放慢（RFC 8628 §3.5；带服务器建议间隔）
    SlowDown { interval_seconds: Option<u64> },
    /// 授权完成，携带凭据/中间值
    Complete(T),
    /// 授权失败（可读错误信息）
    Failed(String),
}

/// device-code 轮询状态机（同步非阻塞；发起 HTTP 请求由调用方负责）
#[derive(Debug, Clone)]
pub struct DeviceCodePoller {
    /// 超时时刻（epoch ms）；0 = 无限
    pub expires_at: u64,
    /// 当前轮询间隔（秒）
    pub interval_seconds: u64,
    /// 下次允许轮询的时刻（epoch ms）
    pub next_poll_at: u64,
    /// 收到 slow_down 的次数（超时报文区分）
    pub slow_down_count: u32,
}

impl DeviceCodePoller {
    /// 新建轮询器。
    /// `interval_seconds`：服务器返回的 interval（缺省 5s）；
    /// `expires_in_seconds`：设备码有效期（该端点超时），None = 不超时；
    /// `wait_before_first_poll`：GitHub 等要求首个 poll 前等待一个间隔。
    pub fn new(
        interval_seconds: Option<u64>,
        expires_in_seconds: Option<u64>,
        wait_before_first_poll: bool,
    ) -> Self {
        let now = now_ms();
        let interval_seconds = interval_seconds
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)
            .max(1);
        let expires_at = expires_in_seconds
            .map(|s| now.saturating_add(s * 1000))
            .unwrap_or(0);
        DeviceCodePoller {
            expires_at,
            interval_seconds,
            next_poll_at: if wait_before_first_poll {
                now.saturating_add(interval_seconds * 1000)
            } else {
                0
            },
            slow_down_count: 0,
        }
    }

    /// 距下次允许轮询的毫秒数（0 = 现在可轮询）
    pub fn poll_due_in(&self) -> u64 {
        self.next_poll_at.saturating_sub(now_ms())
    }

    /// 是否已超时（设备码过期）
    pub fn expired(&self) -> bool {
        self.expires_at != 0 && now_ms() >= self.expires_at
    }

    /// 授权仍在等待：推进到下一个轮询窗口
    pub fn on_pending(&mut self) {
        self.next_poll_at = now_ms().saturating_add(self.interval_seconds * 1000);
    }

    /// slow_down：采纳服务器间隔，否则按 RFC 8628 §3.5 增加 5 秒。
    pub fn on_slow_down(&mut self, interval_seconds: Option<u64>) {
        self.slow_down_count += 1;
        self.interval_seconds = match interval_seconds {
            Some(s) if s > 0 => s,
            _ => self
                .interval_seconds
                .saturating_add(SLOW_DOWN_INTERVAL_INCREMENT_MS / 1000),
        }
        .max(1);
        self.next_poll_at = now_ms().saturating_add(self.interval_seconds * 1000);
    }
}

/// 登录流程状态机（provider 实现 DeviceCodeHandler，UI 驱动推进）
/// 设备授权响应（RFC 8628 公共字段；各 provider 自行填充）
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// 设备码（轮询 token 端点时回传）
    pub device_code: String,
    /// 展示给用户的短码（如 AB-1234）
    pub user_code: String,
    /// 展示给用户的验证 URI（优先 verification_uri_complete）
    pub verification_uri: String,
    /// 服务器建议轮询间隔（省略用 5s）
    pub interval_seconds: Option<u64>,
    /// 设备码有效期（秒；省略用 15 分钟）
    pub expires_in_seconds: Option<u64>,
    /// 首个 poll 前是否等待一个间隔（GitHub/Kimi/xAI 等要求）
    pub wait_before_first_poll: bool,
}

/// 轮询完成的成果：凭据或中间授权值（copilot github token / kimi 等）
#[derive(Debug, Clone)]
pub enum DeviceSuccess {
    /// 直接拿到最终凭据（kimi / xai）
    Credential(OAuthCredential),
    /// 中间值：需再经 [`DeviceCodeHandler::finalize`] 换最终凭据
    Intermediate(String),
}

/// provider 的 device-code 流程实现（async 方法以 `Pin<Box<dyn Future>>` 形式声明以保持 dyn 兼容）
pub trait DeviceCodeHandler: std::fmt::Debug + Send {
    /// provider 标识（写入 LoginKind.provider_id；也用于错误文案）
    fn id(&self) -> &'static str;

    /// 发起设备授权请求（拿 device_code / user_code / verification_uri）
    fn request_device(&self) -> RequestDeviceFuture<'_>;

    /// 轮询 token 端点（授权完成前返回 Pending/SlowDown）
    fn poll_token<'a>(&'a self, device_code: &str) -> PollTokenFuture<'a>;

    /// 中间授权值 → 最终凭据（kimi/xai 直接返回凭据，无需实现）。
    /// 返回 (凭据, 进度消息列表)——progress 避免跨线程回调（dyn FnMut 非 Send），
    /// 由调用方在完成后统一展示（如 copilot “Enabling models…”）。
    fn finalize(&self, _intermediate: String) -> FinalizeFuture<'_> {
        let id = self.id();
        Box::pin(async move {
            Err(Error::msg(format!(
                "{} device flow requires intermediate finalization, which is not implemented",
                id
            )))
        })
    }

    /// 登录前需要询问用户的文本（如 copilot enterprise domain）；None = 无需询问。
    /// 返回 Some 时流程会先进入 AwaitInput，用户提交后调 [`accept_input`]。
    fn initial_prompt(&self) -> Option<String> {
        None
    }

    /// 处理用户对 initial_prompt 的回答（写入 flow；随后流程进入 Init 发起设备请求）。
    fn accept_input(&self, _flow: &mut DeviceFlow, _input: &str) -> Result<()> {
        Ok(())
    }
}

/// 流程步骤
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStep {
    /// 尚未发起设备授权请求
    Init,
    /// 需要先询问用户（如 copilot enterprise domain）；awaiting 输入
    AwaitInput,
    /// 轮询中
    Polling,
    /// 已结束（成功/失败）
    Finished,
}

impl DeviceStep {
    /// 是否为终结状态（成功/失败后不再推进）
    pub fn is_finished(&self) -> bool {
        matches!(self, DeviceStep::Finished)
    }
}

/// device-code 登录进度（UI tick 驱动）
#[derive(Debug)]
pub struct DeviceFlow {
    /// 设备码（轮询用；Init 阶段为空）
    pub device_code: String,
    /// 展示给用户的短码（Init 阶段为空）
    pub user_code: String,
    /// 展示给用户的验证 URI（Init 阶段为空）
    pub verification_uri: String,
    /// 轮询节奏控制；Init 阶段为 None（首次 tick 发起设备请求后填入）
    pub poller: Option<DeviceCodePoller>,
    /// 当前流程步骤
    pub step: DeviceStep,
    /// AwaitInput 步骤的提示文案
    pub prompt: String,
    /// AwaitInput 步骤的输入占位示例（如 copilot 的 "e.g., company.ghe.com"）
    pub placeholder: String,
}

impl Default for DeviceFlow {
    /// 初始状态：各字段清空、`step` 为 [`DeviceStep::Init`]、`poller` 为 None。
    fn default() -> Self {
        DeviceFlow {
            device_code: String::new(),
            user_code: String::new(),
            verification_uri: String::new(),
            poller: None,
            step: DeviceStep::Init,
            prompt: String::new(),
            placeholder: String::new(),
        }
    }
}

/// 设备授权响应解析的公共安全校验：验证 URI 必须是 http(s)
///（防 `javascript:` 等可执行 scheme 被直接展示给用户）
pub fn trust_http_url(value: &str) -> bool {
    if !value.starts_with("https://") && !value.starts_with("http://") {
        return false;
    }
    // 最少形如 scheme://host
    match value.split_once("://") {
        Some((_, rest)) => !rest.is_empty(),
        None => false,
    }
}

/// 公共 HTTP 小工具：form 编码 POST + JSON 响应
///
/// `url`：token/device 端点；`fields`：form 字段（键值均会自动 urlencode）。
/// 返回 (status, json body)；非 JSON 响应返回空 object。
pub async fn post_form_json(url: &str, fields: &[(&str, &str)]) -> Result<(u16, Value)> {
    let body = fields
        .iter()
        .map(|(k, v)| format!("{}={}", http::urlencode(k), http::urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let client = http::build_client()?;
    let response = client
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|source| Error::msg(format!("HTTP request failed for {}: {}", url, source)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, json))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_interval_and_immediate_poll() {
        let p = DeviceCodePoller::new(None, None, false);
        assert_eq!(p.interval_seconds, DEFAULT_POLL_INTERVAL_SECONDS);
        assert_eq!(p.poll_due_in(), 0);
        assert!(!p.expired());
    }

    #[test]
    fn wait_before_first_poll_defers() {
        let p = DeviceCodePoller::new(Some(2), None, true);
        assert_eq!(p.interval_seconds, 2);
        assert!(p.poll_due_in() > 0, "首次轮询前应等待一个间隔");
        assert!(p.poll_due_in() <= 2000);
    }

    #[test]
    fn pending_advances_next_poll() {
        let mut p = DeviceCodePoller::new(Some(3), None, false);
        p.on_pending();
        assert!(p.poll_due_in() > 0 && p.poll_due_in() <= 3000);
    }

    #[test]
    fn slow_down_adopts_server_interval() {
        let mut p = DeviceCodePoller::new(Some(2), None, false);
        p.on_slow_down(Some(10));
        assert_eq!(p.interval_seconds, 10);
        assert_eq!(p.slow_down_count, 1);
        assert!(p.poll_due_in() > 0 && p.poll_due_in() <= 10000);
    }

    #[test]
    fn slow_down_without_interval_increments_five_seconds() {
        let mut p = DeviceCodePoller::new(Some(2), None, false);
        p.on_slow_down(None);
        assert_eq!(p.interval_seconds, 7);
    }

    #[test]
    fn expiry_uses_deadline() {
        let p = DeviceCodePoller::new(Some(1), Some(3600), false);
        assert!(!p.expired());
        // 超长等待的 poller 过期
        let old = DeviceCodePoller {
            expires_at: 1,
            interval_seconds: 1,
            next_poll_at: 0,
            slow_down_count: 0,
        };
        assert!(old.expired());
    }
}
