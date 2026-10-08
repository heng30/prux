//! OpenAI（ChatGPT 订阅）OAuth：Sign in with ChatGPT
//!
//! 公共客户端动态注册：授权请求先带占位 `client_id`（[`DYNAMIC_CLIENT_ID`]），
//! OpenAI 在回调地址里回传实际签发的 client_id —— 刷新 token 时必须原样回传，
//! 因此它随凭据一起落盘（[`OAuthCredential::client_id`]）。
//!
//! 授权码流 + PKCE，回调 `127.0.0.1:1455/auth/callback`；端口被占用（Codex CLI 或另一个
//! 未完成的登录）时**直接报错**：若无监听却继续，浏览器回调会落到抢到端口的进程并报
//! “OAuth state mismatch”，登录永远等不到结果。签发的 access token 直接用于 api.openai.com。

use super::{CallbackServer, LoginKind, OAuthCredential, OAuthLogin, generate_pkce, random_bytes};
use crate::{
    core::settings_manager,
    error::{Error, Result},
    utils::{display::is_uuid, http, time::now_ms},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use std::time::Duration;

/// 登录 UI 展示名（也写入 OAuthLogin.provider_name）
pub const PROVIDER_NAME: &str = "OpenAI";
/// 每次登录都注册一个新 client；OpenAI 在回调里回传签发的 client id
pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
/// 动态注册时上报的客户端名称（显示在 OpenAI 侧）
pub const AGENT_NAME_HINT: &str = "Prux";
/// 授权端点
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
/// token 端点（换码与刷新共用，form 编码）
pub const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
/// token 绑定的资源（api.openai.com）
pub const RESOURCE: &str = "https://api.openai.com/v1";
/// 回调端口（与 Codex CLI 共用，不可改）
pub const CALLBACK_PORT: u16 = 1455;
/// 回调路径
pub const CALLBACK_PATH: &str = "/auth/callback";
/// 固定回调 URL（已注册，需与授权请求/换码时一致）
pub const REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";
/// 直连 api.openai.com 必需的 scope（缺失则拒绝入库）
pub const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// 授权请求的完整 scope 列表
pub const SCOPE: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// 真实过期前提前这么久刷新，避免请求刚发出 token 就失效
const EXPIRY_MARGIN_MS: u64 = 3 * 60 * 1000;

/// 生成 32 字节随机值的 base64url 编码（PKCE state / nonce）。
fn random_value() -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes(32)?))
}

/// OpenAI 以稳定 URI 标识每个安装（agent host）：`urn:uuid:<uuid>`。
fn agent_host_id(device_id: &str) -> Result<String> {
    let device_id = device_id.trim();
    if !is_uuid(device_id) {
        return Err(Error::msg(
            "Sign in with ChatGPT requires a device ID (UUID) for this installation",
        ));
    }
    Ok(format!("urn:uuid:{}", device_id.to_ascii_lowercase()))
}

/// 启动 Sign in with ChatGPT 授权码登录会话（同步）。
pub fn start_authorize_flow() -> Result<OAuthLogin> {
    let device_id = settings_manager::get_or_create_device_id()?;
    let host_id = agent_host_id(&device_id)?;
    let pkce = generate_pkce()?;
    let state = random_value()?;
    let nonce = random_value()?;

    // 回调端口固定 1455（与 Codex CLI 共用）：被占用时必须报错，不能静默退化为粘贴 URL
    let server = match CallbackServer::bind_raw(CALLBACK_PORT, CALLBACK_PATH, true) {
        Ok(server) => Some(server),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            return Err(Error::msg(format!(
                "Port {} is in use, probably by an unfinished login in another session or by \
                 the Codex CLI. Cancel that login and try again.",
                CALLBACK_PORT
            )));
        }
        Err(e) => {
            return Err(Error::msg(format!(
                "failed to bind OAuth callback server: {e}"
            )));
        }
    };

    let params = [
        ("client_id", DYNAMIC_CLIENT_ID),
        ("agent_name_hint", AGENT_NAME_HINT),
        ("ext_agent_host_id", host_id.as_str()),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("resource", RESOURCE),
        ("scope", SCOPE),
        ("state", state.as_str()),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("nonce", nonce.as_str()),
    ];
    let qs: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", http::urlencode(k), http::urlencode(v)))
        .collect();

    Ok(OAuthLogin {
        provider_id: "openai".to_string(),
        provider_name: PROVIDER_NAME.to_string(),
        kind: LoginKind::AuthorizeCode {
            auth_url: format!("{}?{}", AUTHORIZE_URL, qs.join("&")),
            verifier: pkce.verifier,
            state,
            server,
        },
        manual_input: None,
    })
}

/// 用授权码换 token。`client_id` 来自回调 URL（OpenAI 动态签发），缺失即报错。
/// `code`：授权码；`verifier`：PKCE code_verifier。
pub async fn exchange_code(
    code: &str,
    verifier: &str,
    client_id: Option<&str>,
) -> Result<OAuthCredential> {
    let client_id = require_client_id(client_id)?;
    let (status, json) = post_form(
        TOKEN_URL,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", REDIRECT_URI),
            ("resource", RESOURCE),
        ],
    )
    .await?;
    require_success(status, &json, "exchange")?;

    // 不用 id_token 识别用户，但它是 token 响应契约的一部分
    let has_id_token = json
        .get("id_token")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());

    if !has_id_token {
        return Err(Error::msg(
            "OpenAI OAuth token response did not contain an ID token",
        ));
    }

    credential_from_token(&json, client_id, "exchange")
}

/// 刷新 token（凭据里必须带签发的 client id）。`cred`：旧凭据。
pub async fn refresh_token(cred: &OAuthCredential) -> Result<OAuthCredential> {
    let client_id = require_client_id(cred.client_id.as_deref()).map_err(|_| {
        Error::msg(
            "Stored OpenAI OAuth credential does not contain an issued client ID; reconnect ChatGPT",
        )
    })?;

    let (status, json) = post_form(
        TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", &cred.refresh),
            ("resource", RESOURCE),
        ],
    )
    .await?;

    require_success(status, &json, "refresh")?;
    credential_from_token(&json, client_id, "refresh")
}

/// 取动态注册签发的 client id；缺失/空白即报错（提示用户粘贴完整回调 URL）。
fn require_client_id(client_id: Option<&str>) -> Result<&str> {
    client_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::msg(
                "OpenAI OAuth registration callback did not contain an issued client ID; \
                 paste the full callback URL from the browser",
            )
        })
}

/// 非 2xx 状态码时按 `operation` 组装错误（带原始 body）。
fn require_success(status: u16, json: &Value, operation: &str) -> Result<()> {
    if status < 400 {
        return Ok(());
    }
    let body = json.to_string();
    Err(Error::msg(format!(
        "OpenAI OAuth token {} failed ({}): {}",
        operation, status, body
    )))
}

/// 从 token 响应构造凭据：必须带 direct token scope（否则无法直连 api.openai.com）。
/// `json`：token 响应；`client_id`：本次使用的动态 client id；
/// `operation`：exchange / refresh，仅用于错误文案。
fn credential_from_token(
    json: &Value,
    client_id: &str,
    operation: &str,
) -> Result<OAuthCredential> {
    let field = |name: &str| -> Result<String> {
        json.get(name)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| Error::msg(format!("OpenAI OAuth token response has invalid {name}")))
    };
    let access = field("access_token")?;
    let refresh = field("refresh_token")?;
    let scope = field("scope")?;
    let expires_in = json
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            Error::msg(format!(
                "OpenAI OAuth token {operation} response has invalid expires_in"
            ))
        })?;

    let scopes: Vec<String> = scope.split_whitespace().map(|s| s.to_string()).collect();
    if !scopes.iter().any(|s| s == DIRECT_TOKEN_SCOPE) {
        return Err(Error::msg(format!(
            "OpenAI OAuth grant did not include {}",
            DIRECT_TOKEN_SCOPE
        )));
    }

    Ok(OAuthCredential {
        access,
        refresh,
        expires: now_ms() + expires_in * 1000 - EXPIRY_MARGIN_MS,
        enterprise_url: None,
        available_model_ids: None,
        client_id: Some(client_id.to_string()),
        scopes: Some(scopes),
    })
}

/// form-urlencoded POST，返回 (状态码, JSON)；非 JSON 响应体回落到 null。
/// `fields`：form 字段（键值均自动 urlencode）。
async fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<(u16, Value)> {
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

    fn token_response(scope: &str) -> Value {
        serde_json::json!({
            "access_token": "at-1",
            "refresh_token": "rt-1",
            "id_token": "it-1",
            "expires_in": 3600,
            "scope": scope,
        })
    }

    #[test]
    fn credential_requires_direct_token_scope() {
        let cred = credential_from_token(&token_response(SCOPE), "client-1", "test").unwrap();
        assert_eq!(cred.client_id.as_deref(), Some("client-1"));
        assert_eq!(
            cred.scopes.as_deref(),
            Some(
                [
                    "openid",
                    "profile",
                    "email",
                    "offline_access",
                    "resource.invoke",
                    "chatgpt.tokens.use.direct"
                ]
                .map(|s| s.to_string())
                .as_slice()
            )
        );
        assert!(cred.expires > now_ms());

        // 缺 direct scope → 报错
        let err = credential_from_token(&token_response("openid profile"), "c", "test");
        assert!(err.is_err());

        // 缺字段 → 报错
        assert!(credential_from_token(&serde_json::json!({}), "c", "test").is_err());
        let mut missing_expires = token_response(SCOPE);
        missing_expires
            .as_object_mut()
            .unwrap()
            .remove("expires_in");
        assert!(credential_from_token(&missing_expires, "c", "test").is_err());
    }

    #[test]
    fn client_id_is_required_from_callback() {
        assert!(require_client_id(Some(" issued ")).is_ok());
        assert!(require_client_id(Some("  ")).is_err());
        assert!(require_client_id(None).is_err());
    }

    #[test]
    fn agent_host_id_requires_uuid() {
        let id = "3F2504E0-4F89-41D3-9A0C-0305E82C3301";
        assert_eq!(
            agent_host_id(id).unwrap(),
            "urn:uuid:3f2504e0-4f89-41d3-9a0c-0305e82c3301"
        );
        assert!(agent_host_id("not-a-uuid").is_err());
        assert!(agent_host_id("").is_err());
    }

    #[test]
    fn auth_url_contains_expected_params() {
        // 1455 回调端口全局唯一：与其它 browser flow 用例串行
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        // 走 dispatch 入口，顺带覆盖 oauth::start_login 的 openai 分支
        let login = crate::core::oauth::start_login("openai").unwrap();
        assert_eq!(login.provider_id, "openai");
        assert_eq!(login.provider_name, "OpenAI");
        let url = login.auth_url();
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("client_id=dynamic_agent_client"));
        assert!(url.contains("agent_name_hint=Prux"));
        assert!(url.contains("ext_agent_host_id=urn%3Auuid%3A"));
        assert!(url.contains("resource=https%3A%2F%2Fapi.openai.com%2Fv1"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("nonce="));
        match &login.kind {
            LoginKind::AuthorizeCode { server, state, .. } => {
                assert_eq!(state.len(), 43);
                // 端口可用时必须真正监听 1455（不可用则 start_login 已经报错）
                let server = server.as_ref().expect("端口可用时应绑定回调服务器");
                assert_eq!(server.port(), CALLBACK_PORT);
            }
            _ => panic!("expected authorize flow"),
        }
        // 首次登录生成本机 deviceId 并写入全局设置
        assert!(settings_manager::read_settings_device_id().is_some());
    }

    #[test]
    fn occupied_callback_port_fails_instead_of_degrading() {
        // 1455 回调端口全局唯一：与其它 browser flow 用例串行
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        // 先占住 1455，再发起登录：应直接报错而不是退化为粘贴 URL
        let held = std::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).unwrap();
        let err = crate::core::oauth::start_login("openai").unwrap_err();
        assert!(
            err.to_string().contains("is in use"),
            "unexpected error: {err}"
        );
        drop(held);
    }
}
