//! OpenRouter OAuth PKCE 流程
//!
//! OpenRouter 用授权码交换一个**永久用户控制的 API key**（POST /api/v1/auth/keys），
//! 而不是 access/refresh 对。回调由临时端口的一次性 loopback 服务器处理，
//! 与手动粘贴（远程/headless）竞速。授权 URL 无 state（callback_url 本身带随机端口与路径）。

use super::{CallbackServer, LoginKind, OAuthCredential, OAuthLogin, generate_pkce, random_bytes};
use crate::{
    error::{Error, Result},
    utils::http,
};
use serde_json::Value;
use std::time::Duration;

/// 授权端点（浏览器打开，带 callback_url + code_challenge）
pub const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
/// key 交换端点（授权码 → 永久 API key）
pub const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
/// 回调路径前缀（后接随机 UUID，避免被猜中）
pub const CALLBACK_PATH_PREFIX: &str = "/oauth/callback/";
/// 登录 UI 展示名
pub const PROVIDER_NAME: &str = "OpenRouter";

/// 启动 OpenRouter 授权码登录会话：临时端口 + 随机回调路径 + 无 state
///（callback_url 自带随机端口与路径，已足防止串入）。
pub fn start_authorize_flow() -> Result<OAuthLogin> {
    let pkce = generate_pkce()?;
    let rand = random_bytes(16)?;
    let uuid = rand
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let hex = format!("{:02x}", b);
            if matches!(i, 4 | 6 | 8 | 10) {
                format!("-{}", hex)
            } else {
                hex
            }
        })
        .collect::<String>();
    let path = format!("/oauth/callback/{}", uuid);
    let server = CallbackServer::bind_with(0, &path, false)?;
    let callback_url = server.callback_url();
    let auth_url = format!(
        "{}?callback_url={}&code_challenge={}&code_challenge_method=S256",
        AUTHORIZE_URL,
        http::urlencode(&callback_url),
        http::urlencode(&pkce.challenge)
    );
    Ok(OAuthLogin {
        provider_id: "openrouter".to_string(),
        provider_name: PROVIDER_NAME.to_string(),
        kind: LoginKind::AuthorizeCode {
            auth_url,
            verifier: pkce.verifier,
            state: String::new(),
            server: Some(server),
        },
        manual_input: None,
    })
}

/// 用授权码换永久 API key（响应 {key}）。
/// `code`：授权码；`verifier`：PKCE code_verifier。
pub async fn exchange_code(code: &str, verifier: &str) -> Result<OAuthCredential> {
    exchange_code_at(TOKEN_URL, code, verifier).await
}

/// 测试/代理用：指定交换端点。`token_url`：key 交换端点。
async fn exchange_code_at(token_url: &str, code: &str, verifier: &str) -> Result<OAuthCredential> {
    let body = serde_json::json!({
        "code": code,
        "code_verifier": verifier,
        "code_challenge_method": "S256",
    });
    let client = http::build_client()?;
    let response = client
        .post(token_url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|source| Error::msg(format!("OpenRouter key exchange failed: {}", source)))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::msg(format!(
            "OpenRouter OAuth key exchange failed (HTTP {}){}",
            status,
            if text.is_empty() {
                String::new()
            } else {
                format!(": {}", text)
            }
        )));
    }
    let v: Value = serde_json::from_str(&text).map_err(|source| Error::Json {
        context: "OpenRouter OAuth returned invalid JSON".to_string(),
        source,
    })?;
    let key = v
        .get("key")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::msg("OpenRouter OAuth response carries no \"key\""))?;

    // 永久 key：无 refresh、永不过期
    Ok(OAuthCredential {
        access: key.to_string(),
        refresh: String::new(),
        expires: u64::MAX,
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
    fn auth_url_contains_callback_and_challenge() {
        let login = start_authorize_flow().unwrap();
        let url = login.auth_url();
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("callback_url="));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        // 无 state 校验
        assert!(!url.contains("state="));
        // 临时端口回调 URL
        match &login.kind {
            LoginKind::AuthorizeCode { server, state, .. } => {
                assert_eq!(state.as_str(), "");
                assert!(server.as_ref().expect("回调服务器必绑").port() > 0);
            }
            _ => panic!("expected authorize flow"),
        }
        // verifier 非空（PKCE 交换需要）
        match &login.kind {
            LoginKind::AuthorizeCode { verifier, .. } => {
                assert!(!verifier.is_empty());
            }
            _ => panic!(),
        }
    }

    async fn mock_key_server(payload: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
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
            let body = payload.as_bytes();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body).await;
            let _ = socket.shutdown().await;
        });
        (format!("http://127.0.0.1:{}", addr.port()), handle)
    }

    #[test]
    fn exchange_network_exchanges_permanent_key() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (url, _h) = mock_key_server(r#"{"key":"sk-or-permanent-1"}"#).await;
            let cred = exchange_code_at(&url, "code-1", "verifier-1")
                .await
                .unwrap();
            assert_eq!(cred.access, "sk-or-permanent-1");
            assert_eq!(cred.refresh, "");
            assert!(cred.is_permanent());
            assert!(!cred.refreshable());
        });
    }

    #[test]
    fn exchange_parses_key_only() {
        // 通过 mock 服务器验证 exchange（TOKEN_URL 固定，测内部解析不可行；
        // 此处验证凭据构造形态：永久 key）
        let cred = exchange_parse_key(serde_json::json!({"key": "sk-or-xxx"})).unwrap();
        assert_eq!(cred.access, "sk-or-xxx");
        assert_eq!(cred.refresh, "");
        assert!(cred.is_permanent());
        assert!(!cred.refreshable());
    }

    fn exchange_parse_key(v: serde_json::Value) -> Result<OAuthCredential> {
        let key = v
            .get("key")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::msg("OpenRouter OAuth response carries no \"key\""))?;
        Ok(OAuthCredential {
            access: key.to_string(),
            refresh: String::new(),
            expires: u64::MAX,
            enterprise_url: None,
            available_model_ids: None,
            client_id: None,
            scopes: None,
        })
    }
}
