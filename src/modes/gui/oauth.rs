//! `/login` 的 OAuth 流程：授权 URL / device code + 手动粘贴 + 回调等待。
//!
//! 与 TUI 的 `tui::oauth_flow` 用同一套 `core::oauth` 接口，区别只在驱动方式：
//! TUI 由事件循环 tick 推进，这里整段流程跑在一个后台任务里，进度经通道送回视图轮询
//! （视图侧的落点是 `app::GuiApp::poll_oauth`）。

use crate::core::oauth::{
    self, LoginKind, OAuthCredential, OAuthLogin,
    device_code::{DeviceCodePoller, DevicePollResult, DeviceStep, DeviceSuccess},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender},
    },
    time::Duration,
};

/// 两次推进之间的间隔：回调连接检查与 device-code 轮询节奏都由它决定。
const TICK: Duration = Duration::from_millis(300);

/// 流程推给视图的一条进度。
pub enum OauthEvent {
    /// 更新登录面板的内容
    Show(OauthPanel),
    /// 一条进度/提示消息
    Note(String),
    /// 流程结束：成功带回 provider id 与凭据，失败带回错误文案
    Done(Result<(String, OAuthCredential), String>),
}

/// 登录面板一帧的内容（与 TUI 的 `LoginOauth` 面板同构）。
#[derive(Default)]
pub struct OauthPanel {
    /// device-code 的用户码（授权码流或等待中为 `None`）
    pub code: Option<String>,
    /// 授权地址 / 验证 URI：点击用浏览器打开，落面板时自动复制到剪贴板
    pub url: Option<String>,
    /// 说明或等待文案（`None` = 不显示；内部可含换行）
    pub note: Option<String>,
    /// 输入行标签（`None` = 不显示输入行，空串 = 有输入行但不要标签）
    pub input_label: Option<String>,
    /// 是否允许提交空输入（device 的 `AwaitInput`，空值代表用默认值）
    pub allow_empty: bool,
}

/// 一步推进的结果。
enum Step {
    /// 还没结束，等下一 tick
    Wait,
    /// 流程结束
    Done(Result<(String, OAuthCredential), String>),
}

/// 跑完整段登录流程，结束（成功 / 失败 / 被取消）后返回。
///
/// `input`：面板里用户提交的授权码（device `AwaitInput` 步骤是提问的回答）；
/// `cancel`：视图关掉面板时置位，置位后释放回调端口并退出；
/// `events`：送给视图的进度通道（视图侧丢了接收端就自然结束）。
pub async fn run(
    mut login: OAuthLogin,
    input: Receiver<String>,
    cancel: Arc<AtomicBool>,
    events: Sender<OauthEvent>,
) {
    show_initial(&login, &events);
    loop {
        if cancel.load(Ordering::Relaxed) {
            shutdown(&mut login);
            return;
        }
        // 一 tick 内连敲了多次只留最后一条（用户回车后会清空输入框）
        while let Ok(text) = input.try_recv() {
            login.manual_input = Some(text);
        }
        match step(&mut login, &events).await {
            Step::Wait => tokio::time::sleep(TICK).await,
            Step::Done(result) => {
                _ = events.send(OauthEvent::Done(result));
                return;
            }
        }
    }
}

/// 首帧：授权码流显示授权地址（顺手复制到剪贴板），device 流显示等待文案或提问。
fn show_initial(login: &OAuthLogin, events: &Sender<OauthEvent>) {
    _ = events.send(OauthEvent::Show(
        if let Some(prompt) = login.prompt_text() {
            let placeholder = login.prompt_placeholder();
            OauthPanel {
                note: Some(if placeholder.is_empty() {
                    prompt
                } else {
                    format!("{prompt}\n{placeholder}")
                }),
                input_label: Some(String::new()),
                allow_empty: true,
                ..OauthPanel::default()
            }
        } else if let Some((user_code, uri)) = login.device_code_info() {
            device_panel(&user_code, &uri)
        } else if login.is_device_code() {
            OauthPanel {
                note: Some("Requesting device code.".to_string()),
                ..OauthPanel::default()
            }
        } else {
            OauthPanel {
                url: Some(login.auth_url()),
                note: Some(
                    oauth::authorization_instruction(&login.provider_id, login.is_copy_code())
                        .to_string(),
                ),
                input_label: Some(
                    oauth::authorization_input_label(&login.provider_id, login.is_copy_code())
                        .to_string(),
                ),
                allow_empty: false,
                ..OauthPanel::default()
            }
        },
    ));
}

/// device-code 流的面板内容：用户码 + 验证地址 + 等待文案。
fn device_panel(user_code: &str, uri: &str) -> OauthPanel {
    OauthPanel {
        code: Some(user_code.to_string()),
        url: Some(uri.to_string()),
        note: Some(
            "Enter the code above at the URL in your browser, then return here.\n\nWaiting for authorization..."
                .to_string(),
        ),
        ..OauthPanel::default()
    }
}

/// 取消：释放授权码流的本地回调端口，避免下次登录撞端口。
fn shutdown(login: &mut OAuthLogin) {
    if let LoginKind::AuthorizeCode {
        server: Some(server),
        ..
    } = &mut login.kind
    {
        server.shutdown();
    }
}

/// 推进一步：优先处理用户提交的输入，其次回调连接 / device-code 轮询。
async fn step(login: &mut OAuthLogin, events: &Sender<OauthEvent>) -> Step {
    let provider_id = login.provider_id.clone();
    let provider_name = login.provider_name.clone();
    let verifier = match &login.kind {
        LoginKind::AuthorizeCode { verifier, .. }
        | LoginKind::AuthorizeCopyCode { verifier, .. } => verifier.clone(),
        LoginKind::DeviceCode { .. } => String::new(),
    };
    let expected_state = match &login.kind {
        LoginKind::AuthorizeCode { state, .. } | LoginKind::AuthorizeCopyCode { state, .. } => {
            state.clone()
        }
        LoginKind::DeviceCode { .. } => String::new(),
    };
    let redirect = login.redirect_uri_override().map(str::to_string);

    // 1. 用户提交的输入：device 的 AwaitInput 是回答提问，其余是授权码/URL
    if let Some(text) = login.manual_input.take() {
        if let LoginKind::DeviceCode { handler, flow } = &mut login.kind
            && flow.step == DeviceStep::AwaitInput
        {
            if let Err(error) = handler.accept_input(flow, &text) {
                return Step::Done(Err(format!("Login setup failed: {error}")));
            }
            flow.step = DeviceStep::Init;
            return Step::Wait;
        }

        let parsed = oauth::parse_authorization_input(&text);
        let Some(code) = parsed.code else {
            return Step::Done(Err("Invalid authorization code or URL".to_string()));
        };
        let state = parsed.state.unwrap_or_else(|| verifier.clone());
        return exchange(
            &provider_id,
            &code,
            &state,
            &verifier,
            &expected_state,
            parsed.client_id,
            redirect.as_deref(),
        )
        .await;
    }

    match &mut login.kind {
        // 2. 授权码流：本地回调服务器收到浏览器跳转
        LoginKind::AuthorizeCode {
            server: Some(server),
            ..
        } => {
            if let Some(result) = oauth::poll_callback(server) {
                return match result {
                    Ok(code) => {
                        exchange(
                            &provider_id,
                            &code.code,
                            &code.state,
                            &verifier,
                            &expected_state,
                            code.client_id,
                            None,
                        )
                        .await
                    }
                    Err(message) => Step::Done(Err(format!("OAuth callback failed: {message}"))),
                };
            }
        }
        // 3. device-code 流：发起设备授权请求，之后按间隔轮询
        LoginKind::DeviceCode { handler, flow } => match flow.step {
            DeviceStep::Init => {
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
                        _ = events.send(OauthEvent::Show(device_panel(
                            &flow.user_code,
                            &flow.verification_uri,
                        )));
                        Step::Wait
                    }
                    Err(error) => Step::Done(Err(format!("Device authorization failed: {error}"))),
                };
            }
            DeviceStep::Polling => {
                let Some(poller) = flow.poller.as_mut() else {
                    return Step::Done(Err("Device flow is not initialized".to_string()));
                };
                if poller.expired() {
                    return Step::Done(Err(format!(
                        "Device flow timed out for {} after {} slow_down responses. Please sync your clock or restart login.",
                        provider_name, poller.slow_down_count
                    )));
                }
                if poller.poll_due_in() > 0 {
                    return Step::Wait;
                }
                let result = handler.poll_token(&flow.device_code).await;
                return match result {
                    Ok(DevicePollResult::Pending) => {
                        if let Some(poller) = flow.poller.as_mut() {
                            poller.on_pending();
                        }
                        Step::Wait
                    }
                    Ok(DevicePollResult::SlowDown { interval_seconds }) => {
                        if let Some(poller) = flow.poller.as_mut() {
                            poller.on_slow_down(interval_seconds);
                        }
                        Step::Wait
                    }
                    Ok(DevicePollResult::Complete(DeviceSuccess::Credential(cred))) => {
                        Step::Done(Ok((provider_id, cred)))
                    }
                    Ok(DevicePollResult::Complete(DeviceSuccess::Intermediate(value))) => {
                        match handler.finalize(value).await {
                            Ok((cred, progress)) => {
                                for message in progress {
                                    _ = events.send(OauthEvent::Note(message));
                                }
                                Step::Done(Ok((provider_id, cred)))
                            }
                            Err(error) => Step::Done(Err(format!("Login failed: {error}"))),
                        }
                    }
                    Ok(DevicePollResult::Failed(message)) => Step::Done(Err(message)),
                    Err(error) => Step::Done(Err(format!("Device token request failed: {error}"))),
                };
            }
            DeviceStep::AwaitInput | DeviceStep::Finished => {}
        },
        // 端口被占用（无本地回调服务器）：只能等用户粘贴
        LoginKind::AuthorizeCode { server: None, .. } | LoginKind::AuthorizeCopyCode { .. } => {}
    }
    Step::Wait
}

/// 用授权码换 token。`expected_state` 非空时校验 state（空 = 不校验，如 OpenRouter）。
async fn exchange(
    provider_id: &str,
    code: &str,
    state: &str,
    verifier: &str,
    expected_state: &str,
    client_id: Option<String>,
    redirect_uri: Option<&str>,
) -> Step {
    if !expected_state.is_empty() && state != expected_state {
        return Step::Done(Err("OAuth state mismatch".to_string()));
    }
    match oauth::exchange_login(
        provider_id,
        code,
        state,
        verifier,
        client_id.as_deref(),
        redirect_uri,
    )
    .await
    {
        Ok(cred) => Step::Done(Ok((provider_id.to_string(), cred))),
        Err(error) => Step::Done(Err(format!("OAuth token exchange failed: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 授权码流：面板要带可点击的 URL 与输入行标签（TUI 同款文案）。
    #[test]
    fn authorization_panel_carries_url_and_input_label() {
        let login = oauth::start_copy_code_login("anthropic").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        show_initial(&login, &tx);
        let OauthEvent::Show(panel) = rx.recv().unwrap() else {
            panic!("expected Show");
        };
        assert_eq!(panel.url.as_deref(), Some(login.auth_url().as_str()));
        assert!(panel.input_label.is_some());
        assert!(panel.note.unwrap().contains("code#state"));
    }

    /// device-code 流：用户码与验证 URI 各就各位，且不显示输入行。
    #[test]
    fn device_panel_carries_code_and_url() {
        let panel = device_panel("ABCD-1234", "https://example.com/device");
        assert_eq!(panel.code.as_deref(), Some("ABCD-1234"));
        assert_eq!(panel.url.as_deref(), Some("https://example.com/device"));
        assert!(panel.input_label.is_none());
    }
}
