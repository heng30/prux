//! /login OAuth 交互（授权 URL / device code + 手动输入 + 回调等待）。
//!
//! 同步部分（start）：构造登录会话（PKCE / 回调服务器或 device-code 准备）、显示说明并打开输入面板。
//! 异步部分（poll）：主事件循环 tick 调用——优先处理用户手动提交的授权码/URL，
//! 否则推进流程（回调轮询 / device-code 轮询）；拿到凭据后写 auth.json、刷新模型目录。

use super::{app::App, panel::PanelKind};
use crate::{
    core::{
        auth,
        oauth::{
            self, LoginKind, OAuthCredential,
            device_code::{DeviceCodePoller, DevicePollResult, DeviceStep, DeviceSuccess},
        },
    },
    error::Result,
    modes::interactive::{agent_actor::AgentCommand, app::MsgLevel},
    utils::clipboard::write_clipboard,
};

pub use crate::core::oauth::OAuthLogin;

/// 启动 OAuth 登录。失败时提示并关闭面板。
pub fn start(st: &mut App) {
    let provider_id = st.login_provider_id.clone();
    open_login(st, oauth::start_login(&provider_id));
}

/// 启动「复制授权码」登录（anthropic）：不绑本地端口，用户在浏览器完成授权后
/// 把页面上的 `code#state` 粘回终端。失败时提示并关闭面板。
pub fn start_copy_code(st: &mut App) {
    let provider_id = st.login_provider_id.clone();
    open_login(st, oauth::start_copy_code_login(&provider_id));
}

/// 打开登录面板并显示授权地址（浏览器登录与复制授权码登录共用）。
/// `login`：已准备的登录会话；失败时提示并关闭面板。
fn open_login(st: &mut App, login: Result<OAuthLogin>) {
    let provider_name = st.login_provider_name.clone();
    let login = match login {
        Ok(l) => l,
        Err(e) => {
            st.push_msg(format!("OAuth setup failed: {}", e), MsgLevel::Error);
            st.panel.cancel();
            st.dirty = true;
            return;
        }
    };

    // 授权码流：自动复制授权网址到系统剪贴板（面板内提示 Ctrl+click 打开）
    let auth_url = login.auth_url();
    if !auth_url.is_empty() {
        let copied = write_clipboard(&auth_url).is_ok();
        if copied {
            // 复制成功提示已由 push_msg 推入消息流展示，status 不重复
            st.push_msg(
                format!("Login URL copied to clipboard: {}", auth_url),
                MsgLevel::Success,
            );
        } else {
            st.push_msg(
                format!(
                    "No clipboard access — Ctrl+click the URL below to open it in your browser: {}",
                    auth_url
                ),
                MsgLevel::Warning,
            );
        }
    }
    st.oauth_login = Some(login);
    st.panel.open(
        PanelKind::LoginOauth,
        format!("Login to {}", provider_name),
        Vec::new(),
    );
    st.dirty = true;
}

/// 取消 OAuth 登录（Esc / 失败清理）：关闭面板并释放回调端口。
pub fn cancel(st: &mut App) {
    if let Some(mut oauth) = st.oauth_login.take()
        && let LoginKind::AuthorizeCode { server, .. } = &mut oauth.kind
        && let Some(server) = server.as_mut()
    {
        server.shutdown();
    }
    st.panel.cancel();
    st.dirty = true;
}
/// 事件循环 tick：处理手动输入或回调。返回 true 表示登录流程已结束。
pub async fn poll(st: &mut App) -> bool {
    let Some(oauth) = st.oauth_login.as_mut() else {
        return false;
    };

    // 手动输入优先（用户 Enter 提交的授权码/URL；device AwaitInput 步骤的输入交给 poll_device）
    if let Some(input) = oauth.manual_input.take() {
        let awaiting = matches!(
            &oauth.kind,
            LoginKind::DeviceCode { flow, .. } if flow.step == DeviceStep::AwaitInput
        );
        if awaiting {
            oauth.manual_input = Some(input);
            return poll_device(st).await;
        }
        let parsed = oauth::parse_authorization_input(&input);
        let Some(code) = parsed.code else {
            st.push_msg(
                "Invalid authorization code or URL".to_string(),
                MsgLevel::Warning,
            );
            st.dirty = true;
            return false;
        };
        let verifier = match &oauth.kind {
            LoginKind::AuthorizeCode { verifier, .. }
            | LoginKind::AuthorizeCopyCode { verifier, .. } => verifier.clone(),
            LoginKind::DeviceCode { .. } => String::new(),
        };
        let expected = match &oauth.kind {
            LoginKind::AuthorizeCode { state, .. } | LoginKind::AuthorizeCopyCode { state, .. } => {
                state.clone()
            }
            _ => String::new(),
        };
        let redirect = oauth.redirect_uri_override().map(str::to_string);
        let state = parsed.state.unwrap_or(verifier.clone());
        return finish(
            st,
            code,
            state,
            &verifier,
            &expected,
            parsed.client_id,
            redirect.as_deref(),
        )
        .await;
    }

    // 回调服务器轮询（授权码流；端口被占用时无服务器，只能等粘贴）
    if let LoginKind::AuthorizeCode { server, .. } = &mut oauth.kind
        && let Some(server) = server.as_mut()
        && let Some(result) = oauth::poll_callback(server)
    {
        match result {
            Ok(cr) => {
                let verifier = match &oauth.kind {
                    LoginKind::AuthorizeCode { verifier, .. }
                    | LoginKind::AuthorizeCopyCode { verifier, .. } => verifier.clone(),
                    _ => String::new(),
                };
                let state = match &oauth.kind {
                    LoginKind::AuthorizeCode { state, .. }
                    | LoginKind::AuthorizeCopyCode { state, .. } => state.clone(),
                    _ => String::new(),
                };
                return finish(st, cr.code, cr.state, &verifier, &state, cr.client_id, None).await;
            }
            Err(msg) => {
                st.push_msg(format!("OAuth callback failed: {}", msg), MsgLevel::Error);
                cancel(st);
                return true;
            }
        }
    }

    // device-code 流：推进（首次初始化设备授权请求，之后按间隔轮询）
    if matches!(oauth.kind, LoginKind::DeviceCode { .. }) {
        return poll_device(st).await;
    }
    false
}

/// device-code 流推进（take 出会话避免借用冲突；未完成时放回，等下一 tick）。
/// 每个 tick 最多推进一步：Init（发起设备请求）或一次 Polling。
async fn poll_device(st: &mut App) -> bool {
    let Some(mut oauth) = st.oauth_login.take() else {
        return false;
    };

    // AwaitInput 步骤：等用户回答（如 copilot 企业域名）
    if let LoginKind::DeviceCode { flow, .. } = &oauth.kind
        && flow.step == DeviceStep::AwaitInput
    {
        return device_await_input(st, oauth).await;
    }

    // 第一步：发起设备授权请求
    if let LoginKind::DeviceCode { flow, .. } = &oauth.kind
        && flow.step == DeviceStep::Init
    {
        return device_init(st, oauth).await;
    }

    // 轮询 token 端点
    let (handler, flow) = match &mut oauth.kind {
        LoginKind::DeviceCode { handler, flow } if flow.step == DeviceStep::Polling => {
            (handler, flow)
        }
        _ => {
            st.oauth_login = Some(oauth);
            return false;
        }
    };
    let Some(poller) = flow.poller.as_mut() else {
        st.push_msg(
            "Device flow is not initialized".to_string(),
            MsgLevel::Warning,
        );
        st.panel.cancel();
        st.dirty = true;
        return true;
    };
    if poller.expired() {
        st.push_msg(format!(
            "Device flow timed out for {} after {} slow_down responses. Please sync your clock or restart login.",
            oauth.provider_name, poller.slow_down_count
        ), MsgLevel::Warning);
        st.panel.cancel();
        st.dirty = true;
        return true;
    }
    if poller.poll_due_in() > 0 {
        st.oauth_login = Some(oauth);
        return false;
    }

    // 轮询 token 端点并分发结果
    let result = handler.poll_token(&flow.device_code).await;
    handle_poll_result(st, oauth, result).await
}

/// 处理 device-code 轮询结果（Pending/SlowDown/Complete/Failed/Err）。
/// `oauth`：已从 `st` take 出的会话（返回前会放回或终结）；
/// `result`：本次轮询结果；返回 true 表示流程已结束。
async fn handle_poll_result(
    st: &mut App,
    mut oauth: OAuthLogin,
    result: Result<DevicePollResult<DeviceSuccess>>,
) -> bool {
    if let LoginKind::DeviceCode { handler, flow } = &mut oauth.kind
        && let Some(poller) = flow.poller.as_mut()
    {
        match result {
            Ok(DevicePollResult::Pending) => {
                poller.on_pending();
                st.dirty = true;
                st.oauth_login = Some(oauth);
                false
            }
            Ok(DevicePollResult::SlowDown { interval_seconds }) => {
                poller.on_slow_down(interval_seconds);
                st.dirty = true;
                st.oauth_login = Some(oauth);
                false
            }
            Ok(DevicePollResult::Complete(success)) => match success {
                DeviceSuccess::Credential(cred) => {
                    st.panel.close();
                    st.dirty = true;
                    save_credential(st, oauth, cred);
                    true
                }
                DeviceSuccess::Intermediate(value) => match handler.finalize(value).await {
                    Ok((cred, progress_msgs)) => {
                        for m in progress_msgs {
                            st.push_msg(m, MsgLevel::Info);
                        }
                        st.panel.close();
                        st.dirty = true;
                        save_credential(st, oauth, cred);
                        true
                    }
                    Err(e) => {
                        st.push_msg(format!("Login failed: {}", e), MsgLevel::Error);
                        st.panel.cancel();
                        st.dirty = true;
                        true
                    }
                },
            },
            Ok(DevicePollResult::Failed(msg)) => {
                st.push_msg(msg, MsgLevel::Error);
                st.panel.cancel();
                st.dirty = true;
                true
            }
            Err(e) => {
                st.push_msg(
                    format!("Device token request failed: {}", e),
                    MsgLevel::Error,
                );
                st.panel.cancel();
                st.dirty = true;
                true
            }
        }
    } else {
        // 会话已不在 device-code/Polling 状态：放回并结束（正常流程不可达）
        st.oauth_login = Some(oauth);
        false
    }
}

/// 处理 device-code AwaitInput 步骤：消费用户输入并推进到 Init。
/// `oauth`：已 take 出的会话；无输入时原样放回并等下一 tick。
async fn device_await_input(st: &mut App, mut oauth: OAuthLogin) -> bool {
    if let LoginKind::DeviceCode { handler, flow } = &mut oauth.kind {
        let input = oauth.manual_input.take();
        let Some(input) = input else {
            st.oauth_login = Some(oauth);
            return false;
        };
        if let Err(e) = handler.accept_input(flow, &input) {
            st.push_msg(format!("Login setup failed: {}", e), MsgLevel::Error);
            st.panel.cancel();
            st.dirty = true;
            return true;
        }
        flow.step = DeviceStep::Init;
        st.dirty = true;
        st.oauth_login = Some(oauth);
        return false;
    }
    st.oauth_login = Some(oauth);
    false
}

/// 处理 device-code Init 步骤：发起设备授权请求，成功后写入验证 URI（复制到剪贴板）。
/// `oauth`：已 take 出的会话；失败时关面板并结束流程。
async fn device_init(st: &mut App, mut oauth: OAuthLogin) -> bool {
    if let LoginKind::DeviceCode { handler, flow } = &mut oauth.kind {
        return match handler.request_device().await {
            Ok(info) => {
                flow.device_code = info.device_code;
                flow.user_code = info.user_code;
                flow.verification_uri = info.verification_uri;
                flow.poller = Some(DeviceCodePoller::new(
                    info.interval_seconds,
                    info.expires_in_seconds,
                    info.wait_before_first_poll,
                ));
                flow.step = DeviceStep::Polling;

                // device-code 流：拿到验证 URI 后复制到系统剪贴板
                let uri = flow.verification_uri.clone();
                let copied = write_clipboard(&uri).is_ok();

                if copied {
                    // 复制成功提示已由 push_msg 推入消息流展示，status 不重复
                    st.push_msg(
                        format!("Login URL copied to clipboard: {}", uri),
                        MsgLevel::Success,
                    );
                } else {
                    st.push_msg(format!(
                            "No clipboard access — Ctrl+click the URL below to open it in your browser: {}",
                            uri
                        ), MsgLevel::Warning);
                }
                st.dirty = true;
                st.oauth_login = Some(oauth);
                false
            }
            Err(e) => {
                st.push_msg(
                    format!("Device authorization failed: {}", e),
                    MsgLevel::Error,
                );
                st.panel.cancel();
                st.dirty = true;
                true
            }
        };
    }
    st.oauth_login = Some(oauth);
    false
}

/// 交换 token 并保存凭据；成功后清理状态并刷新模型目录。
/// `code`/`state`：授权码与回调 state；`verifier`：PKCE code_verifier；
/// `expected_state`：本地期望的 state（空串 = 不校验，如 OpenRouter）；
/// `client_id`：动态注册签发的 client id（仅 openai）；
/// `redirect_uri`：覆盖 provider 默认回调地址（仅复制授权码流用）。
/// 返回 true 表示流程已结束。
async fn finish(
    st: &mut App,
    code: String,
    state: String,
    verifier: &str,
    expected_state: &str,
    client_id: Option<String>,
    redirect_uri: Option<&str>,
) -> bool {
    let Some(oauth) = st.oauth_login.take() else {
        return false;
    };
    st.panel.close();
    st.dirty = true;

    // 授权码流 state 校验（expected_state 空表示不校验，如 OpenRouter）
    if !expected_state.is_empty() && state != expected_state {
        st.push_msg("OAuth state mismatch".to_string(), MsgLevel::Error);
        st.dirty = true;
        return true;
    }

    match oauth::exchange_login(
        &oauth.provider_id,
        &code,
        &state,
        verifier,
        client_id.as_deref(),
        redirect_uri,
    )
    .await
    {
        Ok(cred) => {
            save_credential(st, oauth, cred);
            true
        }
        Err(e) => {
            st.push_msg(
                format!("OAuth token exchange failed: {}", e),
                MsgLevel::Error,
            );
            st.dirty = true;
            true
        }
    }
}

/// 保存凭据、应用到当前 Agent、刷新模型目录。
/// `oauth`：已结束的登录会话（取 provider_id / 展示名）；`cred`：新凭据。
pub(crate) fn save_credential(st: &mut App, oauth: OAuthLogin, cred: OAuthCredential) {
    if let Err(e) = auth::write_oauth_credential(&oauth.provider_id, &cred) {
        st.push_msg(
            format!(
                "Failed to save credentials for {}: {}",
                oauth.provider_name, e
            ),
            MsgLevel::Error,
        );
        st.dirty = true;
        return;
    }

    st.push_msg(
        format!(
            "Saved OAuth account for {}. Credentials saved to {}",
            oauth.provider_name,
            crate::core::auth::auth_path().display()
        ),
        MsgLevel::Success,
    );

    // actor：即时应用（含 fallback 默认模型选择）由 worker 执行（ApplyKey）
    st.worker.send(AgentCommand::ApplyKey {
        provider: oauth.provider_id.clone(),
        key: cred.access.clone(),
    });

    // 登录成功后刷新模型目录
    st.pending_refresh.push_back(oauth.provider_id.clone());
    st.dirty = true;
}
