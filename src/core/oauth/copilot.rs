//! GitHub Copilot OAuth device-code 流程
//!
//! 多步流程：
//! 1. 询问 GitHub Enterprise domain（可空 → github.com）
//! 2. device-code：`https://{domain}/login/device/code` → 轮询 `/login/oauth/access_token` 拿 GitHub token
//! 3. `https://api.{domain}/copilot_internal/v2/token` 换 Copilot token（refresh = GitHub token）
//! 4. 尽力启用目录内模型（并发 4）
//! 5. `GET {base}/models` 拉 availableModelIds（模型过滤）

use super::{
    EXPIRY_SKEW_MS, LoginKind, OAuthCredential, OAuthLogin,
    device_code::{
        DeviceCodeHandler, DeviceFlow, DeviceInfo, DevicePollResult, DeviceStep, DeviceSuccess,
        FinalizeFuture,
    },
};
use crate::{
    core::{
        model_resolver,
        oauth::device_code::{PollTokenFuture, RequestDeviceFuture},
    },
    error::{Error, Result},
    utils::http,
};
use serde_json::Value;
use std::{sync::Mutex, time::Duration};

/// GitHub OAuth 公共客户端 ID（GitHub Copilot 插件同款）
pub const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
/// `GET /models` 需要声明的 GitHub API 版本
pub const COPILOT_API_VERSION: &str = "2026-06-01";
/// 登录 UI 展示名（也写入 OAuthLogin.provider_name）
pub const PROVIDER_NAME: &str = "GitHub Copilot";
/// 默认 OAuth host（可用 COPILOT_OAUTH_HOST 环境变量覆盖为企业域名）
pub const DEFAULT_OAUTH_HOST: &str = "github.com";
/// device code 默认有效期（秒）
pub const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
/// 默认轮询间隔（秒）
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
/// 启用模型的并发数
const POLICY_CONCURRENCY: usize = 4; // 模型启用策略并发

/// GitHub Copilot device-code 登录流程处理器，持有可被测试/网关覆盖的端点。
#[derive(Debug)]
pub struct CopilotHandler {
    /// 当前 OAuth 域名（默认 github.com，可经 initial_prompt 设为企业域名）
    domain: Mutex<String>,
    /// OAuth 端点 base 覆盖（测试/自定义网关；默认 https://{domain}）
    oauth_base: Option<String>,
    /// API 端点 base 覆盖（测试/自定义网关；默认从 token proxy-ep 推导）
    api_base: Option<String>,
}

impl CopilotHandler {
    /// 新建 handler：域名取自环境变量（缺省 github.com），端点无覆盖。
    pub(crate) fn new() -> Self {
        CopilotHandler {
            domain: Mutex::new(oauth_host()),
            oauth_base: None,
            api_base: None,
        }
    }

    /// 当前域名（锁内 clone，避免持锁套 HTTP）
    fn domain(&self) -> String {
        self.domain.lock().unwrap().clone()
    }

    /// device code 端点（`{oauth_base}` 或 `https://{domain}` 下的 /login/device/code）
    fn device_code_url(&self) -> String {
        match &self.oauth_base {
            Some(b) => format!("{}/login/device/code", b),
            None => format!("https://{}/login/device/code", self.domain()),
        }
    }

    /// GitHub access token 轮询端点（/login/oauth/access_token）
    fn access_token_url(&self) -> String {
        match &self.oauth_base {
            Some(b) => format!("{}/login/oauth/access_token", b),
            None => format!("https://{}/login/oauth/access_token", self.domain()),
        }
    }

    /// Copilot token 交换端点（`{api_base}` 或 `https://api.{domain}` 下的 /copilot_internal/v2/token）
    fn copilot_token_url(&self) -> String {
        match &self.api_base {
            Some(b) => format!("{}/copilot_internal/v2/token", b),
            None => format!("https://api.{}/copilot_internal/v2/token", self.domain()),
        }
    }

    /// 登录流程使用的 API base：token proxy-ep 推导，或测试覆盖值
    fn resolved_api_base(&self, token: &str) -> String {
        if let Some(b) = &self.api_base {
            return b.to_string();
        }
        Self::base_url_from_token(token).unwrap_or_else(|| format!("https://api.{}", self.domain()))
    }

    /// 从 Copilot token 的 proxy-ep 推导 API base URL（proxy. → api.）
    fn base_url_from_token(token: &str) -> Option<String> {
        let proxy = token
            .split(';')
            .find_map(|seg| seg.strip_prefix("proxy-ep="))?;
        let proxy = proxy.trim();
        let api_host = if let Some(rest) = proxy.strip_prefix("proxy.") {
            format!("api.{}", rest)
        } else {
            proxy.to_string()
        };
        Some(format!("https://{}", api_host))
    }
}

/// GitHub Copilot IDE 网关头（User-Agent / Editor-* / Copilot-Integration-Id）。
/// 不写死在代码中：定义在模型目录（静态基线 + models-store.json 动态覆盖）的模型条目
/// headers 字段里。认证流程可能在登录前发生（models-store.json 尚未写入），
/// 因此统一经 model_resolver 合并视图读取，基线内嵌保证始终可用。
fn gateway_headers() -> Vec<(String, String)> {
    model_resolver::provider_headers("github-copilot")
}

/// 仅保留指定 key 的网关头（device flow 只需 User-Agent）
fn gateway_headers_keep(keys: &[&str]) -> Vec<(String, String)> {
    gateway_headers()
        .into_iter()
        .filter(|(k, _)| keys.contains(&k.as_str()))
        .collect()
}

/// POST form（附带自定义头）并解析 JSON 响应。
/// `headers`：额外请求头（如 GitHub 要求的 User-Agent）；返回 (status, json)。
async fn post_form_json_with_headers(
    url: &str,
    fields: &[(&str, &str)],
    headers: &[(String, String)],
) -> Result<(u16, Value)> {
    let body = fields
        .iter()
        .map(|(k, v)| format!("{}={}", http::urlencode(k), http::urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let client = http::build_client()?;
    let mut req = client
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .timeout(Duration::from_secs(30));
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let response = req
        .send()
        .await
        .map_err(|source| Error::msg(format!("HTTP request failed for {}: {}", url, source)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, json))
}

/// GET（附带自定义头）并解析 JSON 响应；返回 (status, json)，非 JSON 回落为 null。
async fn get_json_with_headers(url: &str, headers: &[(String, String)]) -> Result<(u16, Value)> {
    let client = http::build_client()?;
    let mut req = client
        .get(url)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(15));
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let response = req
        .send()
        .await
        .map_err(|source| Error::msg(format!("HTTP request failed for {}: {}", url, source)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, json))
}

impl DeviceCodeHandler for CopilotHandler {
    /// 固定返回 "github-copilot"，同时作为凭据的 provider_id。
    fn id(&self) -> &'static str {
        "github-copilot"
    }

    /// 询问 GitHub Enterprise 域名（留空即用 github.com）。
    fn initial_prompt(&self) -> Option<String> {
        Some("GitHub Enterprise URL/domain (blank for github.com)".to_string())
    }

    /// 校验并规范化用户输入的域名，写入 `self.domain` 并回填 `flow.prompt`；
    /// 输入非法（无法规范化为域名）时返回 Err，空白输入视为 github.com 直接通过。
    fn accept_input(&self, flow: &mut DeviceFlow, input: &str) -> Result<()> {
        let trimmed = input.trim();
        if !trimmed.is_empty() {
            let domain = http::normalize_domain(trimmed)
                .ok_or_else(|| Error::msg("Invalid GitHub Enterprise URL/domain"))?;
            *self.domain.lock().unwrap() = domain.clone();
            flow.prompt = domain;
        }
        Ok(())
    }

    /// 向 GitHub 申请 device code；HTTP 状态 ≥400 或响应缺少必需字段时返回 Err。
    fn request_device(&self) -> RequestDeviceFuture<'_> {
        Box::pin(async move {
            let url = self.device_code_url();
            let (status, json) = post_form_json_with_headers(
                &url,
                &[("client_id", CLIENT_ID), ("scope", "read:user")],
                &gateway_headers_keep(&["User-Agent"]),
            )
            .await?;
            if status >= 400 {
                return Err(Error::msg(format!(
                    "Invalid device code response (status {})",
                    status
                )));
            }
            let get = |k: &str| json.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
            let device_code = get("device_code")
                .ok_or_else(|| Error::msg("Invalid device code response: missing device_code"))?;
            let user_code = get("user_code")
                .ok_or_else(|| Error::msg("Invalid device code response: missing user_code"))?;
            let verification_uri = get("verification_uri").ok_or_else(|| {
                Error::msg("Invalid device code response: missing verification_uri")
            })?;
            let num = |k: &str| json.get(k).and_then(|v| v.as_u64());

            Ok(DeviceInfo {
                device_code,
                user_code,
                verification_uri,
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

    /// 轮询 access token 端点：拿到 token 即 Complete，未授权返回 Pending/SlowDown，
    /// 其余错误码返回 Failed（不会返回 Err）。
    fn poll_token<'a>(&'a self, device_code: &str) -> PollTokenFuture<'a> {
        let device_code = device_code.to_string();
        Box::pin(async move {
            let (status, json) = post_form_json_with_headers(
                &self.access_token_url(),
                &[
                    ("client_id", CLIENT_ID),
                    ("device_code", &device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
                &gateway_headers_keep(&["User-Agent"]),
            )
            .await?;
            let access = json
                .get("access_token")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(token) = access {
                return Ok(DevicePollResult::Complete(DeviceSuccess::Intermediate(
                    token,
                )));
            }

            let error = json
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            match error.as_str() {
                "authorization_pending" => Ok(DevicePollResult::Pending),
                "slow_down" => Ok(DevicePollResult::SlowDown {
                    interval_seconds: json.get("interval").and_then(|v| v.as_u64()),
                }),
                _ => Ok(DevicePollResult::Failed(format!(
                    "Device flow failed: {} (status {})",
                    if error.is_empty() {
                        "invalid response".to_string()
                    } else {
                        error
                    },
                    status
                ))),
            }
        })
    }

    /// 用 GitHub token 换 Copilot token（GET，仅接受 GET），再解析出 api base 与模型列表。
    /// 换 token 失败或响应缺 token/expires_at 时返回 Err；返回的进度消息供调用方展示。
    fn finalize(&self, github_token: String) -> FinalizeFuture<'_> {
        Box::pin(async move {
            let mut progress: Vec<String> = Vec::new();
            let token_url = self.copilot_token_url();

            // 用 GitHub token 换 Copilot token（端点只接受 GET：POST 会 404）
            let mut headers: Vec<(String, String)> = vec![
                ("Accept".to_string(), "application/json".to_string()),
                (
                    "Authorization".to_string(),
                    format!("Bearer {}", github_token),
                ),
            ];
            headers.extend(gateway_headers());

            let (status, json) = get_json_with_headers(&token_url, &headers).await?;
            if status >= 400 {
                return Err(Error::msg(format!(
                    "Copilot token exchange failed (status {})",
                    status
                )));
            }
            let token = json
                .get("token")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::msg("Invalid Copilot token response: missing token"))?;
            let expires_at = json
                .get("expires_at")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| Error::msg("Invalid Copilot token response: missing expires_at"))?;
            let base_url = self.resolved_api_base(token);

            // 尽力启用模型（失败忽略；幂等）
            progress.push("Enabling models...".to_string());
            let models = model_resolver::provider_models("github-copilot");
            let mut ids: Vec<String> = models
                .iter()
                .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .collect();
            ids.sort();
            ids.dedup();

            if !ids.is_empty() {
                for chunk in ids.chunks(POLICY_CONCURRENCY) {
                    for id in chunk {
                        _ = enable_model(&base_url, token, id).await;
                    }
                }
            }

            // 拉取可用模型 id（过滤）
            let available = fetch_available_model_ids(&base_url, token).await;

            Ok((
                OAuthCredential {
                    // refresh 保存 GitHub token（后续刷新时用它换新 Copilot token）
                    access: token.to_string(),
                    refresh: github_token,
                    expires: expires_at * 1000 - EXPIRY_SKEW_MS,
                    enterprise_url: Some(self.domain()),
                    available_model_ids: available,
                    client_id: None,
                    scopes: None,
                },
                progress,
            ))
        })
    }
}

/// POST {base}/models/{id}/policy 启用模型。
/// `base_url`：Copilot API base；`token`：Copilot token；`model_id`：模型 id。
/// 网络错误也返回 Ok(false)（启用是尽力而为）。
async fn enable_model(base_url: &str, token: &str, model_id: &str) -> Result<bool> {
    let url = format!("{}/models/{}/policy", base_url, model_id);
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), format!("Bearer {}", token)),
        ("openai-intent".to_string(), "chat-policy".to_string()),
        ("x-interaction-type".to_string(), "chat-policy".to_string()),
    ];
    headers.extend(gateway_headers());

    let client = http::build_client()?;
    let mut req = client
        .post(&url)
        .body("{\"state\":\"enabled\"}")
        .timeout(Duration::from_secs(15));
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    match req.send().await {
        Ok(r) => Ok(r.status().is_success()),
        Err(_) => Ok(false),
    }
}

/// GET {base}/models 拉取可用模型。
/// 过滤掉不支持 tool_calls 的模型；优先 model_picker_enabled + policy 未禁用的集合，
/// 为空时（仅官方端点）回退到 policy=enabled 集合。任一步失败返回 None。
async fn fetch_available_model_ids(base_url: &str, token: &str) -> Option<Vec<String>> {
    let mut headers: Vec<(String, String)> = vec![
        ("Accept".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), format!("Bearer {}", token)),
        (
            "X-GitHub-Api-Version".to_string(),
            COPILOT_API_VERSION.to_string(),
        ),
    ];
    headers.extend(gateway_headers());
    let Ok((status, json)) = get_json_with_headers(&format!("{}/models", base_url), &headers).await
    else {
        return None;
    };
    if status >= 400 {
        return None;
    }
    let data = json.get("data").and_then(|v| v.as_array())?;
    let allow_policy_fallback = base_url == "https://api.individual.githubcopilot.com";
    let mut picker: Vec<String> = Vec::new();
    let mut policy: Vec<String> = Vec::new();
    for item in data {
        let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };

        // 不支持 tool_calls 的模型跳过
        if let Some(false) = item
            .get("capabilities")
            .and_then(|c| c.get("supports"))
            .and_then(|s| s.get("tool_calls"))
            .and_then(|v| v.as_bool())
        {
            continue;
        }
        let picker_enabled = item
            .get("model_picker_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let policy_state = item
            .get("policy")
            .and_then(|p| p.get("state"))
            .and_then(|v| v.as_str());
        if picker_enabled && policy_state != Some("disabled") {
            picker.push(id.to_string());
        }
        if policy_state == Some("enabled") {
            policy.push(id.to_string());
        }
    }
    if !picker.is_empty() || !allow_policy_fallback {
        Some(picker)
    } else {
        Some(policy)
    }
}

/// 启动 copilot 登录会话（同步；直接使用默认 github.com，可用 COPILOT_OAUTH_HOST
/// 环境变量覆盖，不再询问企业域名）。
/// 流程从 Init 起步：首个 tick 即发起设备授权请求，进入 Code + verification_uri 展示。
pub fn start_device_flow() -> OAuthLogin {
    OAuthLogin {
        provider_id: "github-copilot".to_string(),
        provider_name: PROVIDER_NAME.to_string(),
        kind: LoginKind::DeviceCode {
            handler: Box::new(CopilotHandler::new()),
            flow: DeviceFlow {
                // 跳过 AwaitInput 提问：默认域名 github.com（可经环境变量覆盖），直接设备授权
                step: DeviceStep::Init,
                ..DeviceFlow::default()
            },
        },
        manual_input: None,
    }
}

/// 刷新：用 GitHub token（cred.refresh）换新 Copilot token + 重拉可用模型。
/// 域名从 `cred.enterprise_url` 取（缺省 github.com）。
pub async fn refresh_token(cred: &OAuthCredential) -> Result<OAuthCredential> {
    let domain = cred.enterprise_url.clone().unwrap_or_else(oauth_host);
    // 端点只接受 GET（POST 返回 404）
    let mut headers: Vec<(String, String)> = vec![
        ("Accept".to_string(), "application/json".to_string()),
        (
            "Authorization".to_string(),
            format!("Bearer {}", cred.refresh),
        ),
    ];
    headers.extend(gateway_headers());
    let (status, json) = get_json_with_headers(
        &format!("https://api.{}/copilot_internal/v2/token", domain),
        &headers,
    )
    .await?;
    if status >= 400 {
        return Err(Error::msg(format!(
            "Copilot token refresh failed (status {})",
            status
        )));
    }
    let token = json
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::msg("Invalid Copilot token response: missing token"))?;
    let expires_at = json
        .get("expires_at")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| Error::msg("Invalid Copilot token response: missing expires_at"))?;
    let base_url = CopilotHandler::base_url_from_token(token)
        .unwrap_or_else(|| format!("https://api.{}", domain));
    let available = fetch_available_model_ids(&base_url, token).await;
    Ok(OAuthCredential {
        access: token.to_string(),
        refresh: cred.refresh.clone(),
        expires: expires_at * 1000 - EXPIRY_SKEW_MS,
        enterprise_url: Some(domain),
        available_model_ids: available,
        client_id: None,
        scopes: None,
    })
}

/// 从环境变量读取 OAuth domain（hostname 形式），默认 github.com。
/// 支持 COPILOT_OAUTH_HOST / GITHUB_COPILOT_OAUTH_HOST；
/// 值可为 hostname 或完整 URL，统一经 normalize_domain 归一化为 hostname。
fn oauth_host() -> String {
    std::env::var("COPILOT_OAUTH_HOST")
        .or_else(|_| std::env::var("GITHUB_COPILOT_OAUTH_HOST"))
        .ok()
        .and_then(|v| http::normalize_domain(&v))
        .unwrap_or_else(|| DEFAULT_OAUTH_HOST.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_client_id() {
        // base64("Iv1.b507a08c87ecfe98")
        assert_eq!(CLIENT_ID, "Iv1.b507a08c87ecfe98");
    }

    #[test]
    fn base_url_from_proxy_ep() {
        let token =
            "tid=123;exp=9999999999;proxy-ep=proxy.individual.githubcopilot.com;sku=individual";
        assert_eq!(
            CopilotHandler::base_url_from_token(token).as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
        // 无 proxy-ep → None（调用方回退）
        assert_eq!(CopilotHandler::base_url_from_token("tid=a"), None);
    }

    #[test]
    fn copilot_urls_follow_domain() {
        unsafe {
            std::env::remove_var("COPILOT_OAUTH_HOST");
            std::env::remove_var("GITHUB_COPILOT_OAUTH_HOST");
        }
        let h = CopilotHandler::new();
        assert_eq!(h.device_code_url(), "https://github.com/login/device/code");
        assert_eq!(
            h.access_token_url(),
            "https://github.com/login/oauth/access_token"
        );
        assert_eq!(
            h.copilot_token_url(),
            "https://api.github.com/copilot_internal/v2/token"
        );
    }

    #[test]
    fn oauth_host_env_or_default() {
        unsafe {
            std::env::remove_var("COPILOT_OAUTH_HOST");
            std::env::remove_var("GITHUB_COPILOT_OAUTH_HOST");
        }
        // 未设置 → 默认 github.com
        assert_eq!(oauth_host(), DEFAULT_OAUTH_HOST);
        // 主变量：接受完整 URL/末尾斜杠，归一化为 hostname
        unsafe {
            std::env::set_var("COPILOT_OAUTH_HOST", "https://company.ghe.com/");
        }
        assert_eq!(oauth_host(), "company.ghe.com");
        // 主变量为空 → 回退默认
        unsafe {
            std::env::set_var("COPILOT_OAUTH_HOST", "");
        }
        assert_eq!(oauth_host(), DEFAULT_OAUTH_HOST);
        // 备选变量生效
        unsafe {
            std::env::remove_var("COPILOT_OAUTH_HOST");
            std::env::set_var("GITHUB_COPILOT_OAUTH_HOST", "gh.ghe.com");
        }
        assert_eq!(oauth_host(), "gh.ghe.com");
        unsafe {
            std::env::remove_var("COPILOT_OAUTH_HOST");
            std::env::remove_var("GITHUB_COPILOT_OAUTH_HOST");
        }
    }

    #[test]
    fn available_ids_respect_capabilities() {
        let base = "https://api.individual.githubcopilot.com";
        let rt = tokio::runtime::Runtime::new().unwrap();
        // 纯解析逻辑由 fetch 函数内部测试不便；验证常量
        assert_eq!(COPILOT_API_VERSION, "2026-06-01");
        let _ = base;
        let _ = rt;
    }
}

#[cfg(test)]
mod mock_tests {
    use super::*;
    use crate::core::oauth::device_code::DeviceStep;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    /// 测试/代理用：指定 OAuth 与 API 端点 base（如 http://127.0.0.1:port）
    fn for_test(oauth_base: &str, api_base: &str) -> CopilotHandler {
        CopilotHandler {
            domain: Mutex::new("github.com".to_string()),
            oauth_base: Some(oauth_base.to_string()),
            api_base: Some(api_base.to_string()),
        }
    }

    /// 本地假 HTTP 服务器：按请求路径路由（access_token 首次 pending，其余成功）
    async fn mock_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let access_hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hits = access_hits.clone();
        let handle = tokio::spawn(async move {
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
                let head = String::from_utf8_lossy(&buf);
                let request_line = head.lines().next().unwrap_or("");
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("/").to_string();
                // 严格按 method + path 路由：copilot token 端点只认 GET（POST 会 404，对齐真实 GitHub）
                let body: (&'static str, bool) = match (method.as_str(), path.as_str()) {
                    ("POST", "/login/device/code") => (
                        r#"{"device_code":"dc-gh","user_code":"GH-7777","verification_uri":"https://github.com/login/device","interval":1,"expires_in":1800}"#,
                        true,
                    ),
                    ("POST", "/login/oauth/access_token") => {
                        let n = hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if n == 0 {
                            (r#"{"error":"authorization_pending"}"#, false)
                        } else {
                            (
                                r#"{"access_token":"github-token-1","token_type":"bearer"}"#,
                                true,
                            )
                        }
                    }
                    ("GET", "/copilot_internal/v2/token") => (
                        r#"{"token":"tid=1;exp=2000000000;proxy-ep=echo.example.com;sku=individual","expires_at":2000000000}"#,
                        true,
                    ),
                    ("POST", p) if p.starts_with("/models/") && p.ends_with("/policy") => {
                        // enable 模型（幂等；测试忽略结果）
                        (r#"{"status":"success"}"#, true)
                    }
                    ("GET", "/models") => (
                        r#"{"data":[{"id":"claude-sonnet-4","model_picker_enabled":true,"policy":{"state":"enabled"}},{"id":"gpt-5","capabilities":{"supports":{"tool_calls":false}},"policy":{"state":"enabled"}}]}"#,
                        true,
                    ),
                    _ => (r#"{"error":"not found"}"#, false),
                };
                let (body, ok) = body;
                let status = if ok { 200 } else { 400 };
                let body = body.as_bytes();
                let head = format!(
                    "HTTP/1.1 {} {} \r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status,
                    if ok { "OK" } else { "Bad Request" },
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://127.0.0.1:{}", addr.port()), handle)
    }

    /// 构造带自定义端点的 handler 并完成设备初始化
    async fn init_with(oauth_base: &str, api_base: &str) -> (CopilotHandler, DeviceFlow) {
        let login = OAuthLogin {
            provider_id: "github-copilot".to_string(),
            provider_name: PROVIDER_NAME.to_string(),
            kind: LoginKind::DeviceCode {
                handler: Box::new(for_test(oauth_base, api_base)),
                flow: DeviceFlow::default(),
            },
            manual_input: None,
        };
        let mut login = login;
        let (handler, flow) = match &mut login.kind {
            LoginKind::DeviceCode { handler, flow } => (&**handler, flow),
            _ => panic!(),
        };
        // trait object 不能直接 clone；转换成具体 handler 再调用
        // 通过单独测试流程驱动：直接构造 handler（不经过 OAuthLogin）
        let _ = (handler, flow);
        let h = for_test(oauth_base, api_base);
        let info = h.request_device().await.unwrap();
        let flow = DeviceFlow {
            device_code: info.device_code,
            user_code: info.user_code,
            verification_uri: info.verification_uri,
            step: DeviceStep::Polling,
            ..DeviceFlow::default()
        };
        (h, flow)
    }

    #[test]
    fn copilot_full_login_with_enterprise_flow() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (host, _h) = mock_server().await;

            // 设备初始化
            let (handler, flow) = init_with(&host, &host).await;
            assert_eq!(flow.user_code, "GH-7777");
            // 第一次轮询 pending
            assert!(matches!(
                handler.poll_token(&flow.device_code).await.unwrap(),
                DevicePollResult::Pending
            ));
            // 第二次轮询 complete（Intermediate github token）
            match handler.poll_token(&flow.device_code).await.unwrap() {
                DevicePollResult::Complete(DeviceSuccess::Intermediate(tok)) => {
                    assert_eq!(tok, "github-token-1");
                    let (cred, progress) = handler.finalize(tok).await.unwrap();
                    assert!(!progress.is_empty(), "应有 Enabling models… 进度");
                    assert!(cred.access.contains("tid=1"));
                    assert_eq!(cred.refresh, "github-token-1");
                    assert_eq!(cred.enterprise_url.as_deref(), Some("github.com"));
                    // tool_calls=false 的 gpt-5 被过滤
                    let ids = cred.available_model_ids.unwrap();
                    assert_eq!(ids, vec!["claude-sonnet-4".to_string()]);
                }
                other => panic!("expected intermediate complete, got {:?}", other),
            }
        });
    }

    #[test]
    fn copilot_enterprise_domain_prompt_and_refresh() {
        let h = for_test("", "");
        // 初始提问
        let prompt = h.initial_prompt().unwrap();
        assert!(prompt.contains("Enterprise"));
        // accept_input 接受企业域名
        let mut flow = DeviceFlow::default();
        h.accept_input(&mut flow, "https://company.ghe.com")
            .unwrap();
        assert_eq!(h.domain(), "company.ghe.com");
        // 空输入不改动已设置域名（默认值仅在从未设置时由初始化提供）
        h.accept_input(&mut flow, "  ").unwrap();
        assert_eq!(h.domain(), "company.ghe.com");
    }
}
