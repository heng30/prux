//! `prux auth login|logout`：不起交互会话的凭据子命令。
//!
//! - `prux auth login <provider> [--api-key] [--no-browser]`
//! - `prux auth logout <provider>`
//!
//! 走的是与 TUI `/login` 同一套 core 实现（`core::oauth` 的 `start_login` /
//! `exchange_login`），只是把面板驱动的 UI 换成按行读 stdin。

use crate::core::{
    auth::{auth_path, remove_auth, write_auth_key, write_oauth_credential},
    login_registry::{api_key_provider_ids, oauth_provider_ids, provider_name},
    oauth::{
        self, AuthorizationInput, LoginKind, OAuthLogin,
        device_code::{DeviceCodePoller, DevicePollResult, DeviceStep, DeviceSuccess},
    },
};
use std::io::{BufRead, Write};

/// 登录形态：订阅账号（OAuth）还是粘贴 API key。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMode {
    /// OAuth：浏览器/设备码流程（`/login` 的账号登录）
    OAuth,
    /// 粘贴 API key 写入 auth.json
    ApiKey,
}

/// 选登录形态：`--api-key` 强制 API key；否则只要该 provider 支持 OAuth 就走 OAuth。
///
/// `provider`：login_registry 的 provider id；`force_api_key`：命令行给了 `--api-key`。
/// 两边都不支持时返回 `None`（调用方报「未知 provider」）。
pub fn pick_mode(provider: &str, force_api_key: bool) -> Option<LoginMode> {
    let oauth = oauth_provider_ids().contains(&provider);
    let api_key = api_key_provider_ids().contains(&provider);
    if force_api_key {
        return api_key.then_some(LoginMode::ApiKey);
    }
    if oauth {
        Some(LoginMode::OAuth)
    } else if api_key {
        Some(LoginMode::ApiKey)
    } else {
        None
    }
}

/// 跑一次登录，返回进程退出码（0 成功）。
///
/// `provider`：login_registry 的 provider id；`mode`：[`pick_mode`] 的结果；
/// `no_browser`：给了 `--no-browser`，只打印 URL 不尝试启动浏览器。
///
/// OAuth 部分在**独立线程**里建自己的运行时：`run_auth_command` 可能已经在 GUI 的
/// 运行时里被调用，就地 `Runtime::new().block_on()` 会 panic。
pub fn run_login(provider: &str, mode: LoginMode, no_browser: bool) -> i32 {
    match mode {
        LoginMode::ApiKey => login_with_api_key(provider),
        LoginMode::OAuth => {
            let provider = provider.to_string();
            std::thread::spawn(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("Error: cannot start async runtime: {}", e);
                        return 1;
                    }
                };
                rt.block_on(login_with_oauth(&provider, no_browser))
            })
            .join()
            .unwrap_or(1)
        }
    }
}

/// 删除该 provider 在 auth.json 里的凭据，返回进程退出码。
pub fn run_logout(provider: &str) -> i32 {
    match remove_auth(provider) {
        Ok(()) => {
            println!("Logged out of {}.", provider_name(provider));
            0
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            1
        }
    }
}

/// 读一行 stdin（去首尾空白）；EOF 或空行返回 `None`。
fn read_line() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => {
            let trimmed = line.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        }
        Err(_) => None,
    }
}

/// 提示并读一行；提示写 stderr（stdout 留给脚本可解析的输出）。
fn prompt(label: &str) -> Option<String> {
    // 标签自带冒号时不再补一个（授权码流的标签就带）
    if label.ends_with(':') {
        eprint!("{} ", label);
    } else {
        eprint!("{}: ", label);
    }
    _ = std::io::stderr().flush();
    read_line()
}

/// 粘贴 API key 并写入 auth.json。
fn login_with_api_key(provider: &str) -> i32 {
    let name = provider_name(provider);
    let Some(key) = prompt(&format!("Paste your API key for {name}")) else {
        eprintln!("Error: no API key given.");
        return 1;
    };
    match write_auth_key(provider, &key) {
        Ok(()) => {
            println!("Saved API key for {name} to {}.", auth_path().display());
            0
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            1
        }
    }
}

/// 打开浏览器（`no_browser` 或启动失败时只打印提示）。
fn open_browser(url: &str, no_browser: bool) {
    if no_browser {
        return;
    }
    if webbrowser::open(url).is_err() {
        eprintln!("Could not open a browser — open the URL above manually.");
    }
}

/// 保存 OAuth 凭据并提示。
fn save_credential(provider: &str, cred: oauth::OAuthCredential) -> i32 {
    match write_oauth_credential(provider, &cred) {
        Ok(()) => {
            println!(
                "Saved OAuth account for {} to {}.",
                provider_name(provider),
                auth_path().display()
            );
            0
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            1
        }
    }
}

/// OAuth 登录：设备码流按间隔轮询，授权码流要求粘回回调 URL 或 `code#state`。
async fn login_with_oauth(provider: &str, no_browser: bool) -> i32 {
    let mut login = match oauth::start_login(provider) {
        Ok(login) => login,
        Err(e) => {
            eprintln!("Error: {}", e);
            return 1;
        }
    };

    if login.is_device_code() {
        return device_flow(&mut login, no_browser).await;
    }
    authorize_flow(&mut login, no_browser).await
}

/// 设备码流：可能先要用户回答一次（如企业域名），再打印 URL + 码并轮询到凭据。
async fn device_flow(login: &mut OAuthLogin, no_browser: bool) -> i32 {
    let input_prompt = login.prompt_text().unwrap_or_default();
    let LoginKind::DeviceCode { handler, flow } = &mut login.kind else {
        return 1;
    };

    // 少数 provider 需要一步人工输入（copilot 企业域名）
    if flow.step == DeviceStep::AwaitInput {
        let Some(answer) = prompt(&input_prompt) else {
            eprintln!("Error: login cancelled.");
            return 1;
        };
        if let Err(e) = handler.accept_input(flow, &answer) {
            eprintln!("Error: {}", e);
            return 1;
        }
    }

    let info = match handler.request_device().await {
        Ok(info) => info,
        Err(e) => {
            eprintln!("Error: device authorization failed: {}", e);
            return 1;
        }
    };
    flow.device_code = info.device_code.clone();
    flow.user_code = info.user_code.clone();
    flow.verification_uri = info.verification_uri.clone();
    let mut poller = DeviceCodePoller::new(
        info.interval_seconds,
        info.expires_in_seconds,
        info.wait_before_first_poll,
    );

    eprintln!(
        "Open {} in your browser and enter the code: {}",
        info.verification_uri, info.user_code
    );
    open_browser(&info.verification_uri, no_browser);

    loop {
        if poller.expired() {
            eprintln!("Error: device code expired. Run the command again.");
            return 1;
        }
        let wait = poller.poll_due_in();
        if wait > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
        }
        match handler.poll_token(&flow.device_code).await {
            Ok(DevicePollResult::Pending) => poller.on_pending(),
            Ok(DevicePollResult::SlowDown { interval_seconds }) => {
                poller.on_slow_down(interval_seconds)
            }
            Ok(DevicePollResult::Complete(success)) => {
                let cred = match success {
                    DeviceSuccess::Credential(cred) => cred,
                    DeviceSuccess::Intermediate(value) => match handler.finalize(value).await {
                        Ok((cred, msgs)) => {
                            for m in msgs {
                                eprintln!("{}", m);
                            }
                            cred
                        }
                        Err(e) => {
                            eprintln!("Error: login failed: {}", e);
                            return 1;
                        }
                    },
                };
                return save_credential(&login.provider_id, cred);
            }
            Ok(DevicePollResult::Failed(reason)) => {
                eprintln!("Error: login failed: {}", reason);
                return 1;
            }
            Err(e) => {
                eprintln!("Error: login failed: {}", e);
                return 1;
            }
        }
    }
}

/// 授权码流：打印授权 URL，然后**同时**等两种输入 —— 本机浏览器回调（授权码流绑了本地端口时）
/// 与用户粘回的 URL / `code#state` / 裸 code。
async fn authorize_flow(login: &mut oauth::OAuthLogin, no_browser: bool) -> i32 {
    let copy_code = login.is_copy_code();
    let url = login.auth_url();
    let label = oauth::authorization_input_label(&login.provider_id, copy_code);
    let redirect_uri = login.redirect_uri_override().map(str::to_string);

    println!(
        "{}",
        oauth::authorization_instruction(&login.provider_id, copy_code)
    );
    println!("{}", url);
    open_browser(&url, no_browser);

    // stdin 放独立线程：主循环还要轮询回调服务器，两边都阻塞在同一线程会互相饿死
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(prompt(label));
    });

    // 取出 PKCE / state / 回调服务器：之后要整体借用 `login` 去换 token
    let (verifier, state, mut server) = match &mut login.kind {
        LoginKind::AuthorizeCode {
            verifier,
            state,
            server,
            ..
        } => (verifier.clone(), state.clone(), server.take()),
        LoginKind::AuthorizeCopyCode {
            verifier, state, ..
        } => (verifier.clone(), state.clone(), None),
        LoginKind::DeviceCode { .. } => return 1,
    };
    let verifier = verifier.as_str();
    let state = state.as_str();

    loop {
        // 本机浏览器重定向会自己送到，不必粘贴（端口被占用时 server 为 None → 只能粘贴）
        if let Some(result) = server.as_mut().and_then(oauth::poll_callback) {
            match result {
                Ok(cr) => {
                    let input = AuthorizationInput {
                        code: Some(cr.code),
                        state: Some(cr.state),
                        client_id: cr.client_id,
                    };
                    return finish(login, input, verifier, state, redirect_uri.as_deref()).await;
                }
                Err(msg) => {
                    eprintln!("Error: OAuth callback failed: {}", msg);
                    return 1;
                }
            }
        }
        match rx.try_recv() {
            Ok(Some(input)) => {
                return finish(
                    login,
                    oauth::parse_authorization_input(&input),
                    verifier,
                    state,
                    redirect_uri.as_deref(),
                )
                .await;
            }
            Ok(None) => {
                eprintln!("Error: login cancelled.");
                return 1;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                eprintln!("Error: login cancelled.");
                return 1;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

/// 校验 state 并换 token。
/// `input`：回调或粘贴解析出的授权码；`verifier`/`state`：本次授权请求的 PKCE 与 state；
/// `redirect_uri`：复制授权码流必须带上的一致回调地址。
async fn finish(
    login: &oauth::OAuthLogin,
    input: AuthorizationInput,
    verifier: &str,
    state: &str,
    redirect_uri: Option<&str>,
) -> i32 {
    let AuthorizationInput {
        code,
        state: pasted_state,
        client_id,
    } = input;
    let Some(code) = code else {
        eprintln!("Error: could not find an authorization code in the input.");
        return 1;
    };
    let pasted_state = pasted_state.unwrap_or_default();
    if !state.is_empty() && pasted_state != state {
        eprintln!("Error: OAuth state mismatch.");
        return 1;
    }
    match oauth::exchange_login(
        &login.provider_id,
        &code,
        &pasted_state,
        verifier,
        client_id.as_deref(),
        redirect_uri,
    )
    .await
    {
        Ok(cred) => save_credential(&login.provider_id, cred),
        Err(e) => {
            eprintln!("Error: token exchange failed: {}", e);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 已知 provider 的形态判别：订阅型走 OAuth，纯 API key 型走粘贴，未知 provider 报错。
    #[test]
    fn pick_mode_follows_registry() {
        assert_eq!(pick_mode("anthropic", false), Some(LoginMode::OAuth));
        assert_eq!(pick_mode("deepseek", false), Some(LoginMode::ApiKey));
        assert_eq!(pick_mode("anthropic", true), Some(LoginMode::ApiKey));
        assert_eq!(pick_mode("no-such-provider", false), None);
    }
}
