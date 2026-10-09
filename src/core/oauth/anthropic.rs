//! Anthropic（Claude Pro/Max 订阅）OAuth 授权码 + PKCE 流程
//!
//! 两种登录方式（用户二选一）：
//! 1. **浏览器登录**（默认）：本地回调服务器（127.0.0.1:53692/callback）等授权回调；
//!    端口被占用时退化为纯粘贴 redirect URL。
//! 2. **复制授权码登录**：不绑端口，`redirect_uri` 用 Anthropic 侧页面
//!    [`COPY_CODE_REDIRECT_URI`]，用户在页面上复制 `code#state` 粘回终端。
//!
//! 两条路径共用 PKCE 生成与 token 交换（区别只在 `redirect_uri`）与刷新逻辑。

use super::{
    CALLBACK_PATH, CallbackServer, EXPIRY_SKEW_MS, LoginKind, OAuthCredential, OAuthLogin, Pkce,
    generate_pkce,
};
use crate::{
    error::{Error, Result},
    utils::{http, time::now_ms},
};
use serde_json::{Value, json};

/// Anthropic OAuth 公共客户端 ID（Claude Code 同款，固定值）
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// 授权端点（浏览器打开）
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
/// token 端点（换码 / 刷新共用，JSON 请求体）
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// 回调端口（注册在 Anthropic 侧的固定端口，不可改）
pub const CALLBACK_PORT: u16 = 53692;
/// 「复制授权码」登录的回调地址：由 Anthropic 侧页面承接，不指向本地端口
pub const COPY_CODE_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
/// 申请的权限范围（created API key / 用户信息 / 推理等）
pub const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// 浏览器登录的固定回调 URL（token 交换时需与授权请求中的 redirect_uri 一致）
pub fn redirect_uri() -> String {
    format!("http://127.0.0.1:{}{}", CALLBACK_PORT, CALLBACK_PATH)
}

/// 拼接授权 URL。`pkce` 提供 code_challenge；`redirect` 为回调地址
/// （浏览器登录用本地固定端口，端口被占用时仍用注册的固定端口；复制授权码登录用 provider 侧地址）。
fn authorization_url(pkce: &Pkce, redirect: &str) -> String {
    let params = [
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", redirect),
        ("scope", SCOPES),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
        ("state", &pkce.verifier),
    ];
    let qs: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", http::urlencode(k), http::urlencode(v)))
        .collect();
    format!("{}?{}", AUTHORIZE_URL, qs.join("&"))
}

/// 启动 anthropic 浏览器登录会话（同步）：生成 PKCE + 绑定回调端口。
/// 53692 优先（便于容器/SSH 端口转发），被占用时让 OS 另取一个空闲 loopback 端口——
/// Anthropic 接受任意 loopback 端口；两个端口都绑不上才退化为纯粘贴 redirect URL。
pub fn start_authorize_flow() -> Result<OAuthLogin> {
    let pkce = generate_pkce()?;
    let server = CallbackServer::bind(CALLBACK_PORT)
        .or_else(|_| CallbackServer::bind(0))
        .ok();
    let port = server.as_ref().map(|s| s.port()).unwrap_or(CALLBACK_PORT);
    let redirect = format!("http://127.0.0.1:{}{}", port, CALLBACK_PATH);

    Ok(OAuthLogin {
        provider_id: "anthropic".to_string(),
        provider_name: "Anthropic".to_string(),
        kind: LoginKind::AuthorizeCode {
            auth_url: authorization_url(&pkce, &redirect),
            verifier: pkce.verifier.clone(),
            state: pkce.verifier,
            server,
        },
        manual_input: None,
    })
}

/// 启动 anthropic「复制授权码」登录会话：不绑端口，回调地址用 [`COPY_CODE_REDIRECT_URI`]，
/// 用户在 Anthropic 页面上复制 `code#state` 粘回终端。
pub fn start_copy_code_flow() -> Result<OAuthLogin> {
    let pkce = generate_pkce()?;
    Ok(OAuthLogin {
        provider_id: "anthropic".to_string(),
        provider_name: "Anthropic".to_string(),
        kind: LoginKind::AuthorizeCopyCode {
            auth_url: authorization_url(&pkce, COPY_CODE_REDIRECT_URI),
            verifier: pkce.verifier.clone(),
            state: pkce.verifier,
            redirect_uri: COPY_CODE_REDIRECT_URI.to_string(),
        },
        manual_input: None,
    })
}

/// 构造授权码交换的请求体（纯函数，便于断言 `redirect_uri` 的选择）。
/// `redirect`：本次授权请求用的回调地址，必须与之一致。
fn exchange_body(code: &str, state: &str, verifier: &str, redirect: &str) -> Value {
    json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": redirect,
        "code_verifier": verifier,
    })
}

/// 用授权码换 token。`code`：授权码；`state`：授权请求里的 state（即 verifier）；
/// `verifier`：PKCE code_verifier；`redirect_override`：覆盖默认回调地址
/// （`None` = 浏览器登录的本地固定端口，`Some` = 复制授权码流的 provider 侧地址）。
pub async fn exchange_code(
    code: &str,
    state: &str,
    verifier: &str,
    redirect_override: Option<&str>,
) -> Result<OAuthCredential> {
    let redirect = redirect_override
        .map(str::to_string)
        .unwrap_or_else(redirect_uri);
    post_token(exchange_body(code, state, verifier, &redirect)).await
}

/// 用 refresh_token 刷新。`refresh`：旧 refresh token。
pub async fn refresh_token(refresh: &str) -> Result<OAuthCredential> {
    post_token(json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh,
    }))
    .await
}

/// POST 到 [`TOKEN_URL`]（JSON 体，换码与刷新共用），解析 access/refresh/expires_in。
/// `body`：完整 JSON 请求体（grant_type 不同）。
async fn post_token(body: Value) -> Result<OAuthCredential> {
    let client = http::build_client()?;
    let response = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|source| Error::msg(format!("OAuth token request failed: {}", source)))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::msg(format!(
            "OAuth token request failed. status={}; url={}; body={}",
            status, TOKEN_URL, text
        )));
    }
    let v: Value = serde_json::from_str(&text).map_err(|source| Error::Json {
        context: "OAuth token response is not valid JSON".to_string(),
        source,
    })?;
    let access = v
        .get("access_token")
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::msg("OAuth token response missing access_token"))?;
    let refresh = v
        .get("refresh_token")
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::msg("OAuth token response missing refresh_token"))?;
    let expires_in = v.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(3600);
    Ok(OAuthCredential {
        access: access.to_string(),
        refresh: refresh.to_string(),
        expires: now_ms() + expires_in * 1000 - EXPIRY_SKEW_MS, // 提前 5 分钟视为过期
        enterprise_url: None,
        available_model_ids: None,
        client_id: None,
        scopes: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_url_contains_expected_params() {
        let pkce = super::super::generate_pkce().unwrap();
        let url = authorization_url(&pkce, &redirect_uri());
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(&format!("code_challenge={}", pkce.challenge)));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("state={}", pkce.verifier)));
        assert!(url.contains(&format!(
            "redirect_uri={}",
            http::urlencode(&format!(
                "http://127.0.0.1:{}{}",
                CALLBACK_PORT,
                super::super::CALLBACK_PATH
            ))
        )));
    }

    /// 2.3：复制授权码登录不绑端口，回调地址用 Anthropic 侧页面，且粘贴回来的 state 就是 PKCE verifier。
    #[test]
    fn copy_code_flow_uses_provider_redirect_without_callback_server() {
        let login = start_copy_code_flow().expect("复制授权码登录应总能启动");
        let LoginKind::AuthorizeCopyCode {
            auth_url,
            verifier,
            state,
            redirect_uri,
        } = &login.kind
        else {
            panic!("expected copy-code flow: {:?}", login.kind);
        };
        assert_eq!(redirect_uri, COPY_CODE_REDIRECT_URI);
        assert!(auth_url.contains(&http::urlencode(COPY_CODE_REDIRECT_URI)));
        assert!(
            !auth_url.contains("127.0.0.1"),
            "复制授权码流不得把本地端口写进授权 URL: {auth_url}"
        );
        assert_eq!(state, verifier);
        assert!(login.is_copy_code());
        assert_eq!(
            login.redirect_uri_override(),
            Some(COPY_CODE_REDIRECT_URI),
            "token 交换必须回传同一个 redirect_uri"
        );
    }

    /// token 交换请求体的 `redirect_uri`：浏览器流用本地端口，复制授权码流用覆盖值。
    #[test]
    fn exchange_body_follows_redirect_override() {
        let local = exchange_body("c", "s", "v", &redirect_uri());
        assert_eq!(
            local["redirect_uri"].as_str(),
            Some(redirect_uri().as_str()),
            "默认必须与浏览器流的授权请求一致"
        );

        let copy = exchange_body("c", "s", "v", COPY_CODE_REDIRECT_URI);
        assert_eq!(
            copy["redirect_uri"].as_str(),
            Some(COPY_CODE_REDIRECT_URI),
            "复制授权码流必须回传 provider 侧回调地址"
        );
        assert_eq!(copy["code"].as_str(), Some("c"));
        assert_eq!(copy["state"].as_str(), Some("s"));
        assert_eq!(copy["code_verifier"].as_str(), Some("v"));
        assert_eq!(copy["grant_type"].as_str(), Some("authorization_code"));
    }

    /// pi #10571：53692 被占用（Hyper-V/WSL 端口排除等）时回落到 OS 分配的空闲 loopback 端口，
    /// 用**真实端口**拼 redirect_uri，而不是放弃浏览器回调。
    #[test]
    fn occupied_callback_port_falls_back_to_a_free_loopback_port() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 先占住固定回调端口；端口已被其它进程占用时同样满足前提
        let occupied = std::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT));
        let login = start_authorize_flow().expect("端口被占用也应能启动登录");
        match &login.kind {
            LoginKind::AuthorizeCode {
                server, auth_url, ..
            } => {
                let server = server.as_ref().expect("端口被占用时应回落到空闲端口");
                let port = server.port();
                assert_ne!(port, CALLBACK_PORT, "回落后不能还是被占用的端口");
                assert!(
                    auth_url.contains(&http::urlencode(&format!(
                        "http://127.0.0.1:{}{}",
                        port,
                        super::super::CALLBACK_PATH
                    ))),
                    "授权 URL 必须用真实绑定的端口：{auth_url}"
                );
            }
            _ => panic!("expected authorize-code flow"),
        }
        drop(login);
        drop(occupied);
    }
}
