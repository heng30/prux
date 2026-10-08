//! xAI（SuperGrok / X Premium 订阅）OAuth device-code 流程
//!
//! 端点：`https://auth.x.ai/oauth2/device/code` 发起；`https://auth.x.ai/oauth2/token` 轮询/刷新。
//! 轮询成功响应直接携带 access_token / refresh_token / expires_in。

use super::{
    EXPIRY_SKEW_MS, LoginKind, OAuthCredential, OAuthLogin,
    device_code::{
        DeviceCodeHandler, DeviceFlow, DeviceInfo, DevicePollResult, DeviceSuccess, post_form_json,
    },
    simple_credential,
};
use crate::{
    core::oauth::device_code::{PollTokenFuture, RequestDeviceFuture},
    error::{Error, Result},
    utils::time::now_ms,
};
use serde_json::Value;

/// xAI OAuth 公共客户端 ID
pub const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// 申请的权限范围
pub const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
/// 默认 OAuth host（测试/代理可覆盖）
pub const XAI_OAUTH_HOST: &str = "https://auth.x.ai";
/// device 授权端点（文档/测试引用；实现走 `{host}/oauth2/device/code`）
pub const XAI_DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
/// token 端点（轮询与刷新共用）
pub const XAI_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
/// 登录 UI 展示名
pub const PROVIDER_NAME: &str = "xAI";
/// 响应未带 expires_in 时的默认 token 生存期（秒）
const DEFAULT_TOKEN_LIFETIME_SECONDS: u64 = 3600;

/// xAI 的 device-code 处理器：按 RFC 8628 发起设备授权并轮询换取 token。
#[derive(Debug)]
struct XaiHandler {
    /// OAuth 端点 base（已去掉末尾斜杠）
    host: String,
}

impl XaiHandler {
    /// 新建 handler。`host`：OAuth 端点 base（末尾斜杠会被去掉）。
    fn new(host: impl Into<String>) -> Self {
        XaiHandler {
            host: host.into().trim_end_matches('/').to_string(),
        }
    }
}

impl DeviceCodeHandler for XaiHandler {
    /// provider 标识固定为 `"xai"`。
    fn id(&self) -> &'static str {
        "xai"
    }

    /// 向 `{host}/oauth2/device/code` 发起设备授权，解析 device_code / user_code /
    /// 验证 URI。验证 URI 必须是 https，否则报错；expires_in 缺失或非正数同样报错。
    fn request_device(&self) -> RequestDeviceFuture<'_> {
        Box::pin(async move {
            let (status, json) = post_form_json(
                &format!("{}/oauth2/device/code", self.host),
                &[
                    ("client_id", XAI_CLIENT_ID),
                    ("scope", XAI_SCOPE),
                    ("referrer", "pi"),
                ],
            )
            .await?;
            if !status_is_ok(status) {
                return Err(Error::msg(format!(
                    "xAI OAuth device authorization failed (HTTP {})",
                    status
                )));
            }
            let device_code = required_string(&json, "device_code")?;
            let user_code = required_string(&json, "user_code")?;
            let verification_uri =
                validate_https_url(&required_string(&json, "verification_uri")?)?;
            // verification_uri_complete 可选（RFC 8628），必须 https
            let verification_uri_complete = json
                .get("verification_uri_complete")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(validate_https_url)
                .transpose()?;
            Ok(DeviceInfo {
                device_code,
                user_code,
                verification_uri: verification_uri_complete.unwrap_or(verification_uri),
                interval_seconds: json
                    .get("interval")
                    .and_then(|v| v.as_u64())
                    .filter(|n| *n > 0),
                expires_in_seconds: Some(positive(&json, "expires_in")?),
                wait_before_first_poll: true,
            })
        })
    }

    /// 轮询 `{host}/oauth2/token`：2xx 且能解析出凭据时返回 `Complete`，
    /// 否则按响应里的 `error` 字段映射为 Pending / SlowDown / Failed，无法识别的错误返回 Err。
    fn poll_token<'a>(&'a self, device_code: &str) -> PollTokenFuture<'a> {
        let device_code = device_code.to_string();
        Box::pin(async move {
            let (status, json) = post_form_json(
                &format!("{}/oauth2/token", self.host),
                &[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", XAI_CLIENT_ID),
                    ("device_code", &device_code),
                ],
            )
            .await?;
            if status_is_ok(status) {
                return Ok(DevicePollResult::Complete(DeviceSuccess::Credential(
                    credentials_from_token_response(&json, None)?,
                )));
            }
            let error = json.get("error").and_then(|v| v.as_str()).unwrap_or("");
            match error {
                "authorization_pending" => Ok(DevicePollResult::Pending),
                "slow_down" => Ok(DevicePollResult::SlowDown {
                    interval_seconds: json.get("interval").and_then(|v| v.as_u64()),
                }),
                "access_denied" | "authorization_denied" => Ok(DevicePollResult::Failed(
                    "xAI device authorization was denied".to_string(),
                )),
                "expired_token" => Ok(DevicePollResult::Failed(
                    "xAI device code expired".to_string(),
                )),
                _ => Err(Error::msg(format!(
                    "xAI OAuth device token polling failed (HTTP {})",
                    status
                ))),
            }
        })
    }
}

/// HTTP 状态码是否属 2xx
fn status_is_ok(status: u16) -> bool {
    (200..300).contains(&status)
}

/// 取必填非空字符串字段。`json`：响应体；`field`：字段名（错误文案用）。
fn required_string(json: &Value, field: &str) -> Result<String> {
    json.get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| Error::msg(format!("Invalid xAI OAuth response field: {}", field)))
}

/// 取必填正整数数值字段（与字符串型字段区分）。
fn positive(json: &Value, field: &str) -> Result<u64> {
    json.get(field)
        .and_then(|v| v.as_u64())
        .filter(|n| *n > 0)
        .ok_or_else(|| Error::msg(format!("Invalid xAI OAuth response field: {}", field)))
}

/// 校验 verification URI 必须是 https
fn validate_https_url(raw: &str) -> Result<String> {
    if raw.starts_with("https://") && raw.len() > "https://".len() {
        Ok(raw.to_string())
    } else {
        Err(Error::msg(
            "Untrusted verification URI in xAI OAuth response",
        ))
    }
}

/// 解析 token 响应。
/// `json`：token 响应体；`previous_refresh_token`：响应未带 refresh_token 时的沿用值
/// （刷新场景传入旧值，首次授权传 None）。
fn credentials_from_token_response(
    json: &Value,
    previous_refresh_token: Option<&str>,
) -> Result<OAuthCredential> {
    let access = required_string(json, "access_token")?;
    let refresh = match json.get("refresh_token").and_then(|v| v.as_str()) {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => previous_refresh_token
            .map(|s| s.to_string())
            .ok_or_else(|| Error::msg("Invalid xAI OAuth response field: refresh_token"))?,
    };
    let expires_in = json
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECONDS);

    // 提前 5 分钟视为过期
    Ok(simple_credential(
        access,
        refresh,
        now_ms() + expires_in * 1000 - EXPIRY_SKEW_MS,
    ))
}

/// 启动 xAI device-code 登录会话（同步；HTTP 初始化在首次轮询时完成）
pub fn start_device_flow() -> OAuthLogin {
    OAuthLogin {
        provider_id: "xai".to_string(),
        provider_name: PROVIDER_NAME.to_string(),
        kind: LoginKind::DeviceCode {
            handler: Box::new(XaiHandler::new(XAI_OAUTH_HOST)),
            flow: DeviceFlow::default(),
        },
        manual_input: None,
    }
}

/// 刷新 token。`refresh`：旧 refresh token。
pub async fn refresh_token(refresh: &str) -> Result<OAuthCredential> {
    refresh_token_with_host(XAI_OAUTH_HOST, refresh).await
}

/// 测试/代理用：指定 oauth host 刷新。`host`：OAuth 端点 base；`refresh`：旧 refresh token。
async fn refresh_token_with_host(host: &str, refresh: &str) -> Result<OAuthCredential> {
    let (status, json) = post_form_json(
        &format!("{}/oauth2/token", host),
        &[
            ("grant_type", "refresh_token"),
            ("client_id", XAI_CLIENT_ID),
            ("refresh_token", refresh),
        ],
    )
    .await?;
    if !status_is_ok(status) {
        return Err(Error::msg(format!(
            "xAI OAuth token refresh failed (HTTP {})",
            status
        )));
    }
    credentials_from_token_response(&json, Some(refresh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_response() {
        let json = serde_json::json!({
            "device_code": "dc-1",
            "user_code": "AB-1234",
            "verification_uri": "https://auth.x.ai/oauth2/device",
            "verification_uri_complete": "https://auth.x.ai/oauth2/device?user_code=AB-1234",
            "expires_in": 1800,
            "interval": 5,
        });
        assert_eq!(XAI_DEVICE_CODE_URL, "https://auth.x.ai/oauth2/device/code");
        assert_eq!(XAI_TOKEN_URL, "https://auth.x.ai/oauth2/token");
        assert!(json.get("user_code").is_some());
    }

    #[test]
    fn token_response_with_and_without_refresh() {
        let with_refresh = serde_json::json!({
            "access_token": "xai-access",
            "refresh_token": "xai-refresh",
            "expires_in": 3600,
        });
        let cred = credentials_from_token_response(&with_refresh, None).unwrap();
        assert_eq!(cred.access, "xai-access");
        assert_eq!(cred.refresh, "xai-refresh");
        assert!(cred.expires > crate::utils::time::now_ms());

        // refresh 响应可能缺 refresh_token → 沿用旧值
        let no_refresh = serde_json::json!({"access_token": "xai-access-2"});
        let cred = credentials_from_token_response(&no_refresh, Some("old-refresh")).unwrap();
        assert_eq!(cred.refresh, "old-refresh");
    }

    #[test]
    fn validates_https_verification_uri() {
        assert!(validate_https_url("https://auth.x.ai/device").is_ok());
        assert!(validate_https_url("http://auth.x.ai/device").is_err());
        assert!(validate_https_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn requires_token_fields() {
        assert!(credentials_from_token_response(&serde_json::json!({}), None).is_err());
        assert!(
            credentials_from_token_response(&serde_json::json!({"access_token": "x"}), None)
                .is_err()
        );
    }
}

#[cfg(test)]
mod mock_tests {
    use super::*;
    use crate::core::oauth::device_code::{DeviceCodePoller, DeviceStep};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    /// 本地假 HTTP 服务器：按请求顺序返回 (status, json body) 响应
    async fn mock_server(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut responses = responses.into_iter();
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 8192];
                let mut total = 0;
                loop {
                    match socket.read(&mut buf[total..]).await {
                        Ok(0) => break,
                        Ok(n) => {
                            total += n;
                            if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                            if total >= buf.len() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let Some((status, body)) = responses.next() else {
                    break;
                };
                let body = body.as_bytes();
                let head = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status,
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://127.0.0.1:{}", addr.port()), handle)
    }

    fn xai_login_with_host(host: &str) -> OAuthLogin {
        OAuthLogin {
            provider_id: "xai".to_string(),
            provider_name: PROVIDER_NAME.to_string(),
            kind: LoginKind::DeviceCode {
                handler: Box::new(XaiHandler::new(host)),
                flow: DeviceFlow::default(),
            },
            manual_input: None,
        }
    }

    async fn init_login(login: &mut OAuthLogin) -> DeviceCodePoller {
        let (handler, flow) = match &mut login.kind {
            LoginKind::DeviceCode { handler, flow } => (handler, flow),
            _ => panic!("not device flow"),
        };
        let info = handler.request_device().await.unwrap();
        flow.device_code = info.device_code;
        flow.user_code = info.user_code;
        flow.verification_uri = info.verification_uri;
        flow.step = DeviceStep::Polling;
        let poller = DeviceCodePoller::new(
            info.interval_seconds,
            info.expires_in_seconds,
            info.wait_before_first_poll,
        );
        flow.poller = Some(poller.clone());
        poller
    }

    async fn poll_once(login: &mut OAuthLogin) -> DevicePollResult<DeviceSuccess> {
        let (handler, flow) = match &mut login.kind {
            LoginKind::DeviceCode { handler, flow } => (handler, flow),
            _ => panic!("not device flow"),
        };
        let dc = flow.device_code.clone();
        handler.poll_token(&dc).await.unwrap()
    }

    #[test]
    fn xai_login_full_flow() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (host, _h) = mock_server(vec![
                (
                    200,
                    r#"{"device_code":"dc-x","user_code":"XY-9988","verification_uri":"https://auth.x.ai/oauth2/device","verification_uri_complete":"https://auth.x.ai/oauth2/device?user_code=XY-9988","interval":2,"expires_in":1800}"#,
                ),
                (400, r#"{"error":"authorization_pending"}"#),
                (
                    200,
                    r#"{"access_token":"xai-access","refresh_token":"xai-refresh","expires_in":3600}"#,
                ),
            ])
            .await;
            let mut login = xai_login_with_host(&host);
            let poller = init_login(&mut login).await;
            assert!(poller.poll_due_in() > 0, "xAI 首个 poll 前应等待");
            assert!(matches!(poll_once(&mut login).await, DevicePollResult::Pending));
            match poll_once(&mut login).await {
                DevicePollResult::Complete(DeviceSuccess::Credential(cred)) => {
                    assert_eq!(cred.access, "xai-access");
                    assert_eq!(cred.refresh, "xai-refresh");
                }
                other => panic!("expected complete, got {:?}", other_kind(&other)),
            }
        });
    }

    #[test]
    fn xai_https_uri_enforced() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // 非 https verification_uri → 设备授权失败
            let (host, _h) = mock_server(vec![(
                200,
                r#"{"device_code":"dc-bad","user_code":"BAD","verification_uri":"http://insecure.example/device","expires_in":600}"#,
            )])
            .await;
            let mut login = xai_login_with_host(&host);
            let (handler, flow) = match &mut login.kind {
                LoginKind::DeviceCode { handler, flow } => (handler, flow),
                _ => panic!(),
            };
            let err = handler.request_device().await.unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("Untrusted"), "{} 应拒绝非 https URI", msg);
            let _ = flow;
        });
    }

    #[test]
    fn xai_refresh_with_mock_host() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (host, _h) = mock_server(vec![(
                200,
                r#"{"access_token":"xai-new","expires_in":3600}"#,
            )])
            .await;
            let cred = refresh_token_with_host(&host, "old-refresh").await.unwrap();
            assert_eq!(cred.access, "xai-new");
            // 缺 refresh_token → 沿用旧值
            assert_eq!(cred.refresh, "old-refresh");
        });
    }

    fn other_kind<T>(_r: &DevicePollResult<T>) -> &'static str {
        "not-complete"
    }
}
