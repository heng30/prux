//! Kimi For Coding（Kimi Code 订阅）OAuth device-code 流程
//!
//! RFC 8628 device authorization grant against `https://auth.kimi.com`。
//! 端点：`/api/oauth/device_authorization` 发起；`/api/oauth/token` 轮询/刷新。
//! 轮询成功响应直接携带 access_token / refresh_token / expires_in（字段名非标准下划线）。

use super::{
    LoginKind, OAuthCredential, OAuthLogin,
    device_code::{
        DeviceCodeHandler, DeviceFlow, DeviceInfo, DevicePollResult, DeviceSuccess,
        PollTokenFuture, RequestDeviceFuture, post_form_json, trust_http_url,
    },
    simple_credential,
};
use crate::{
    error::{Error, Result},
    utils::{http::normalize_base_url, time::now_ms},
};
use serde_json::Value;

/// Kimi Code OAuth 公共客户端 ID
pub const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
/// 默认 OAuth host（可用 KIMI_CODE_OAUTH_HOST / KIMI_OAUTH_HOST 覆盖）
pub const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
/// 登录 UI 展示名
pub const PROVIDER_NAME: &str = "Kimi For Coding";
/// device code 默认有效期（秒）
pub const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
/// 默认轮询间隔（秒）
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;

/// Kimi Code 订阅 device-code 登录流程处理器，持有可覆盖的 OAuth host。
#[derive(Debug)]
struct KimiHandler {
    /// OAuth 端点 base（默认 auth.kimi.com，测试/代理可覆盖）
    host: String,
}

impl DeviceCodeHandler for KimiHandler {
    /// provider 标识固定为 `"kimi-coding"`。
    fn id(&self) -> &'static str {
        "kimi-coding"
    }

    /// 向 `{host}/api/oauth/device_authorization` 发起设备授权，解析出 device_code、
    /// user_code 与验证 URI（优先带用户码的完整 URI）。
    /// 响应缺字段或验证 URI 非 http(s) 时报错；interval/expires_in 缺失则用默认值。
    fn request_device(&self) -> RequestDeviceFuture<'_> {
        Box::pin(async move {
            let (status, json) = post_form_json(
                &format!("{}/api/oauth/device_authorization", self.host),
                &[("client_id", CLIENT_ID)],
            )
            .await?;
            if status >= 400 {
                return Err(Error::msg(format!(
                    "Kimi Code device authorization failed with status {}",
                    status
                )));
            }
            let get = |k: &str| json.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
            let device_code = get("device_code").ok_or_else(|| {
                Error::msg("Invalid Kimi Code device authorization response: missing device_code")
            })?;
            let user_code = get("user_code").ok_or_else(|| {
                Error::msg("Invalid Kimi Code device authorization response: missing user_code")
            })?;
            let verification_uri = get("verification_uri").unwrap_or_default();
            let verification_uri_complete = get("verification_uri_complete").unwrap_or_default();
            if !trust_http_url(&verification_uri) && !trust_http_url(&verification_uri_complete) {
                return Err(Error::msg(
                    "Untrusted verification_uri in Kimi Code device response",
                ));
            }
            let num = |k: &str| json.get(k).and_then(|v| v.as_u64());
            Ok(DeviceInfo {
                device_code,
                user_code,
                // 优先带用户码的完整 URI
                verification_uri: if trust_http_url(&verification_uri_complete) {
                    verification_uri_complete
                } else {
                    verification_uri
                },
                interval_seconds: Some(
                    num("interval")
                        .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)
                        .max(1),
                ),
                expires_in_seconds: Some(
                    num("expires_in")
                        .unwrap_or(DEVICE_CODE_TIMEOUT_SECONDS)
                        .max(1),
                ),
                wait_before_first_poll: true,
            })
        })
    }

    /// 轮询 `{host}/api/oauth/token`：成功且带 access_token 时返回 `Complete`（凭据），
    /// 否则按响应里的 `error` 字段映射为 Pending / SlowDown / Failed。
    fn poll_token<'a>(&'a self, device_code: &str) -> PollTokenFuture<'a> {
        let device_code = device_code.to_string();
        Box::pin(async move {
            let (status, json) = post_form_json(
                &format!("{}/api/oauth/token", self.host),
                &[
                    ("client_id", CLIENT_ID),
                    ("device_code", &device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
            )
            .await?;
            if status >= 500 {
                return Ok(DevicePollResult::Failed(format!(
                    "Kimi Code device token request failed with status {}",
                    status
                )));
            }
            if status == 200 && json.get("access_token").and_then(|v| v.as_str()).is_some() {
                return match parse_token_response(&json, "poll") {
                    Ok(cred) => Ok(DevicePollResult::Complete(DeviceSuccess::Credential(cred))),
                    Err(e) => Ok(DevicePollResult::Failed(e.to_string())),
                };
            }
            let error = json.get("error").and_then(|v| v.as_str()).unwrap_or("");
            match error {
                "authorization_pending" => Ok(DevicePollResult::Pending),
                "slow_down" => Ok(DevicePollResult::SlowDown {
                    interval_seconds: json.get("interval").and_then(|v| v.as_u64()),
                }),
                "expired_token" => Ok(DevicePollResult::Failed(
                    "Kimi Code device authorization expired. Please restart login.".to_string(),
                )),
                "access_denied" => Ok(DevicePollResult::Failed(
                    "Kimi Code login was denied.".to_string(),
                )),
                _ => Ok(DevicePollResult::Failed(format!(
                    "Kimi Code device token request failed (status {}){}",
                    status,
                    if error.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", error)
                    }
                ))),
            }
        })
    }
}

/// 解析 token 响应（字段 access_token/refresh_token/expires_in，空字符串视为缺失）。
/// `operation`：调用方场景（poll / refresh），仅用于错误文案。
fn parse_token_response(json: &Value, operation: &str) -> Result<OAuthCredential> {
    let access = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::msg(format!(
                "Kimi Code token {} missing access_token",
                operation
            ))
        })?;
    let refresh = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::msg(format!(
                "Kimi Code token {} missing refresh_token",
                operation
            ))
        })?;
    let expires_in = json
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .filter(|s| *s > 0)
        .ok_or_else(|| Error::msg(format!("Kimi Code token {} missing expires_in", operation)))?;

    Ok(simple_credential(
        access.to_string(),
        refresh.to_string(),
        now_ms() + expires_in * 1000,
    ))
}

/// 启动 kimi device-code 登录会话（同步；HTTP 初始化在首次轮询时完成）
pub fn start_device_flow() -> OAuthLogin {
    start_device_flow_with_host(oauth_host())
}

/// 测试/代理用：指定 oauth host 启动登录会话。`host`：OAuth 端点 base URL。
pub fn start_device_flow_with_host(host: impl Into<String>) -> OAuthLogin {
    OAuthLogin {
        provider_id: "kimi-coding".to_string(),
        provider_name: PROVIDER_NAME.to_string(),
        kind: LoginKind::DeviceCode {
            handler: Box::new(KimiHandler { host: host.into() }),
            flow: DeviceFlow::default(),
        },
        manual_input: None,
    }
}

/// 刷新 token（401/403/invalid_grant 报 unauthorized）。`refresh`：旧 refresh token。
pub async fn refresh_token(refresh: &str) -> Result<OAuthCredential> {
    let host = oauth_host();
    let (status, json) = post_form_json(
        &format!("{}/api/oauth/token", host),
        &[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
        ],
    )
    .await?;
    if status == 200 {
        return parse_token_response(&json, "refresh");
    }
    let error = json.get("error").and_then(|v| v.as_str()).unwrap_or("");
    if status == 401 || status == 403 || error == "invalid_grant" {
        return Err(Error::msg(format!(
            "Kimi Code token refresh unauthorized (status {})",
            status
        )));
    }
    Err(Error::msg(format!(
        "Kimi Code token refresh failed with status {}",
        status
    )))
}

/// 读取 OAuth host 环境变量（KIMI_CODE_OAUTH_HOST 优先，次 KIMI_OAUTH_HOST），
/// 经 normalize_base_url 归一化；未设置用 [`DEFAULT_OAUTH_HOST`]。
fn oauth_host() -> String {
    std::env::var("KIMI_CODE_OAUTH_HOST")
        .or_else(|_| std::env::var("KIMI_OAUTH_HOST"))
        .ok()
        .and_then(|v| normalize_base_url(&v))
        .unwrap_or_else(|| DEFAULT_OAUTH_HOST.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_token_response_fields() {
        let json = serde_json::json!({
            "access_token": "kim-access",
            "refresh_token": "kim-refresh",
            "expires_in": 3600,
        });
        let cred = parse_token_response(&json, "poll").unwrap();
        assert_eq!(cred.access, "kim-access");
        assert_eq!(cred.refresh, "kim-refresh");
        assert!(cred.expires > crate::utils::time::now_ms());
    }

    #[test]
    fn rejects_missing_token_fields() {
        assert!(parse_token_response(&serde_json::json!({}), "poll").is_err());
        assert!(
            parse_token_response(&serde_json::json!({"access_token": "x"}), "refresh").is_err()
        );
        assert!(
            parse_token_response(
                &serde_json::json!({"access_token": "", "refresh_token": "r", "expires_in": 100}),
                "poll"
            )
            .is_err(),
            "空 access 视为缺失"
        );
    }

    #[test]
    fn host_env_or_default() {
        let _ek = crate::test_support::env_key_lock();
        unsafe {
            std::env::remove_var("KIMI_CODE_OAUTH_HOST");
            std::env::remove_var("KIMI_OAUTH_HOST");
        }
        assert_eq!(oauth_host(), DEFAULT_OAUTH_HOST);
    }
}

#[cfg(test)]
mod mock_tests {
    use super::*;
    use crate::core::oauth::device_code::DeviceCodePoller;
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

    /// 驱动设备初始化（只发起 device 授权请求，返回 poller；轮询由测试控制）
    async fn init_login(login: &mut OAuthLogin) -> DeviceCodePoller {
        let (handler, flow) = match &mut login.kind {
            LoginKind::DeviceCode { handler, flow } => (handler, flow),
            _ => panic!("not device flow"),
        };
        let info = handler.request_device().await.unwrap();
        flow.device_code = info.device_code.clone();
        flow.user_code = info.user_code.clone();
        flow.verification_uri = info.verification_uri.clone();
        let poller = DeviceCodePoller::new(
            info.interval_seconds,
            info.expires_in_seconds,
            info.wait_before_first_poll,
        );
        flow.poller = Some(poller.clone());
        poller
    }

    /// 发起一次轮询并返回结果
    async fn poll_once(login: &mut OAuthLogin) -> DevicePollResult<DeviceSuccess> {
        let (handler, flow) = match &mut login.kind {
            LoginKind::DeviceCode { handler, flow } => (handler, flow),
            _ => panic!("not device flow"),
        };
        let dc = flow.device_code.clone();
        handler.poll_token(&dc).await.unwrap()
    }

    #[test]
    fn kimi_login_full_flow() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (host, _h) = mock_server(vec![
                (
                    200,
                    r#"{"device_code":"dc-1","user_code":"KIMI-ABC","verification_uri":"https://auth.kimi.com/device","verification_uri_complete":"https://auth.kimi.com/device?code=KIMI-ABC","interval":1,"expires_in":1800}"#,
                ),
                (200, r#"{"error":"authorization_pending"}"#),
                (
                    200,
                    r#"{"access_token":"kimi-access","refresh_token":"kimi-refresh","expires_in":3600}"#,
                ),
            ])
            .await;
            let mut login = start_device_flow_with_host(host);
            let poller = init_login(&mut login).await;
            assert!(poller.poll_due_in() <= 1000);
            // 第一次轮询 → pending
            assert!(matches!(
                poll_once(&mut login).await,
                DevicePollResult::Pending
            ));
            // 第二次轮询 → complete
            match poll_once(&mut login).await {
                DevicePollResult::Complete(DeviceSuccess::Credential(cred)) => {
                    assert_eq!(cred.access, "kimi-access");
                    assert_eq!(cred.refresh, "kimi-refresh");
                    assert_eq!(login.provider_id, "kimi-coding");
                    assert!(cred.expires > crate::utils::time::now_ms());
                }
                other => panic!("expected complete, got {:?}", other_kind(&other)),
            }
        });
    }

    #[test]
    fn kimi_slow_down_increases_interval() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (host, _h) = mock_server(vec![
                (
                    200,
                    r#"{"device_code":"dc-s","user_code":"KIMI-S","verification_uri":"https://auth.kimi.com/device","interval":1,"expires_in":1800}"#,
                ),
                (200, r#"{"error":"slow_down","interval":8}"#),
            ])
            .await;
            let mut login = start_device_flow_with_host(host);
            let mut poller = init_login(&mut login).await;
            match poll_once(&mut login).await {
                DevicePollResult::SlowDown { interval_seconds } => {
                    assert_eq!(interval_seconds, Some(8));
                    poller.on_slow_down(interval_seconds);
                    assert_eq!(poller.interval_seconds, 8);
                    assert!(poller.poll_due_in() > 0 && poller.poll_due_in() <= 8000);
                }
                other => panic!("expected slow_down, got {:?}", other_kind(&other)),
            }
        });
    }

    fn other_kind<T>(_r: &DevicePollResult<T>) -> &'static str {
        "not-complete"
    }

    #[test]
    fn kimi_refresh_uses_host_env() {
        let _ek = crate::test_support::env_key_lock();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (host, _h) = mock_server(vec![(
                200,
                r#"{"access_token":"kimi-new","refresh_token":"kimi-refresh-2","expires_in":3600}"#,
            )])
            .await;
            unsafe {
                std::env::set_var("KIMI_CODE_OAUTH_HOST", &host);
            }
            let cred = super::refresh_token("old-refresh").await.unwrap();
            assert_eq!(cred.access, "kimi-new");
            assert_eq!(cred.refresh, "kimi-refresh-2");
            unsafe {
                std::env::remove_var("KIMI_CODE_OAUTH_HOST");
            }
        });
    }
}
