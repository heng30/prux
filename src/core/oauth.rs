//! OAuth 2.0 登录后端：授权码 + PKCE / device-code 通用抽象。
//!
//! 层次：
//! - 本模块：共用类型（OAuthCredential / PKCE / 回调服务器 / 授权输入解析）、
//!   登录会话与按 provider 分发的 start / exchange / refresh 入口。
//! - [`device_code`]：RFC 8628 device authorization grant 通用轮询器。
//! - [`anthropic`]、[`kimi`]、[`xai`]、[`copilot`]、[`openrouter`]、[`openai_chatgpt`]：
//!   各 provider 的流程实现（授权码流或 device-code 流）。
//!
//! TUI 交互层（modes/interactive/oauth_flow）负责组合：显示授权 URL / device code、
//! 手动粘贴兜底、轮询推进、完成后写 auth.json。

pub mod anthropic;
pub mod copilot;
pub mod device_code;
pub mod kimi;
pub mod openai_chatgpt;
pub mod openrouter;
pub mod xai;

use crate::{
    error::{Error, Result},
    utils::http,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use device_code::DeviceCodeHandler;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Write,
    net::{TcpListener, TcpStream},
    time::Duration,
};

/// 默认 token 过期偏移：提前 5 分钟视为过期
pub const EXPIRY_SKEW_MS: u64 = 5 * 60 * 1000;

/// 默认回调路径
pub const CALLBACK_PATH: &str = "/callback";

/// 授权回调结果
#[derive(Debug, Clone)]
pub struct CodeResult {
    /// 授权码（token 交换用）
    pub code: String,
    /// 防 CSRF 的 state，需与发起授权时的一致
    pub state: String,
    /// 动态注册签发的 client id（仅 openai 回调返回）
    pub client_id: Option<String>,
}

/// PKCE 参数对
#[derive(Debug, Clone)]
pub struct Pkce {
    /// 随机 code_verifier（32 字节 base64url，43 字符），token 交换时回传
    pub verifier: String,
    /// verifier 的 S256 摘要（base64url），随授权请求发送
    pub challenge: String,
}

/// OAuth 凭据（auth.json 中 type=oauth 条目）。
///
/// `refresh` 为空字符串表示无刷新机制（如 OpenRouter 的永久 API key）；
/// `expires` 为 u64::MAX 表示永不过期。
/// 附加字段（enterprise_url / available_model_ids / client_id / scopes）为可选，按 provider 使用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    /// 访问令牌（请求时放入 Authorization 头）
    pub access: String,
    /// 刷新令牌；空串表示无刷新机制（OpenRouter 永久 key）
    pub refresh: String,
    /// 过期时刻（epoch ms），提前 5 分钟视为过期
    pub expires: u64,
    /// 企业版域名（仅 github-copilot；如 company.ghe.com）
    #[serde(
        rename = "enterpriseUrl",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub enterprise_url: Option<String>,
    /// 可用模型 id 白名单（仅 github-copilot；None = 不过滤）
    #[serde(
        rename = "availableModelIds",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub available_model_ids: Option<Vec<String>>,
    /// 动态注册签发的 client id（仅 openai；刷新时回传）
    #[serde(rename = "clientId", default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// 授权 scope 列表（仅 openai；记录签发时的授权范围）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
}

impl OAuthCredential {
    /// 给定时刻（epoch ms）是否已过期
    pub fn expired(&self, now_ms: u64) -> bool {
        self.expires <= now_ms
    }

    /// 是否有可用的刷新路径（refresh 非空）
    pub fn refreshable(&self) -> bool {
        !self.refresh.is_empty()
    }

    /// 永不过期（OpenRouter 永久 key）
    pub fn is_permanent(&self) -> bool {
        self.expires == u64::MAX
    }
}

/// OAuthCredential 构造默认值
impl Default for OAuthCredential {
    /// 全字段空/零值：空令牌、`expires = 0`（即已过期），provider 附加字段为 `None`。
    fn default() -> Self {
        OAuthCredential {
            access: String::new(),
            refresh: String::new(),
            expires: 0,
            enterprise_url: None,
            available_model_ids: None,
            client_id: None,
            scopes: None,
        }
    }
}

/// 常规 access/refresh/expires 凭据（无 provider 附加字段）。
///
/// `access`：访问令牌；`refresh`：刷新令牌（空 = 无）；
/// `expires`：过期时刻（epoch ms）。
pub fn simple_credential(
    access: impl Into<String>,
    refresh: impl Into<String>,
    expires: u64,
) -> OAuthCredential {
    OAuthCredential {
        access: access.into(),
        refresh: refresh.into(),
        expires,
        enterprise_url: None,
        available_model_ids: None,
        client_id: None,
        scopes: None,
    }
}

/// 生成 `n` 字节密码学随机数（PKCE verifier / state / 回调路径随机段）。
pub(crate) fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf)
        .map_err(|source| Error::msg(format!("getrandom for PKCE: {source}")))?;
    Ok(buf)
}

/// 生成 PKCE verifier（32 随机字节 base64url）与 S256 challenge
pub fn generate_pkce() -> Result<Pkce> {
    let verifier_bytes = random_bytes(32)?;
    let verifier = URL_SAFE_NO_PAD.encode(&verifier_bytes);
    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hash);
    Ok(Pkce {
        verifier,
        challenge,
    })
}

/// 本地回调服务器：绑定回调端口，收到合法回调后发出一份 code。
/// 同步 bind（handler 非异步）；事件循环用 [`poll_callback`] 非阻塞轮询。
///
/// `port=0` 时由系统分配临时端口（OpenRouter 等），实际端口经 [`callback_url`] 获取。
/// `require_state=false` 用于无 state 校验的端点（OpenRouter）。
#[derive(Debug)]
pub struct CallbackServer {
    /// 监听 127.0.0.1 的 TCP 监听器（非阻塞）
    pub listener: TcpListener,
    /// 是否已结算：收到合法回调、出错或主动关闭后不再接受连接
    settled: bool,
    /// 期望的回调路径（如 /callback）
    path: String,
    /// 是否强制校验 state 参数（false 用于 OpenRouter）
    require_state: bool,
}

impl CallbackServer {
    /// 绑定回调端口。`port=0` 表示临时端口；端口被占用时立即报错。
    /// 默认路径 /callback + 要求 state 校验（anthropic）。
    pub fn bind(port: u16) -> Result<CallbackServer> {
        CallbackServer::bind_with(port, CALLBACK_PATH, true)
    }

    /// 绑定回调端口（自定义路径 + 可选 state 校验）。
    /// `port`：监听端口（0 = 系统分配）；`path`：期望的回调路径；
    /// `require_state=false` 时回调只需 code（OpenRouter）。
    pub fn bind_with(port: u16, path: &str, require_state: bool) -> Result<CallbackServer> {
        CallbackServer::bind_raw(port, path, require_state).map_err(|source| {
            Error::msg(format!(
                "failed to bind OAuth callback server on 127.0.0.1:{}. \
                 Close the process holding the port or set a different callback port.",
                source
            ))
        })
    }

    /// 绑定回调端口，保留底层 `io::Error`。
    /// 调用方需要区分具体错误（如 `AddrInUse`）时用它，否则用 [`CallbackServer::bind_with`]。
    pub fn bind_raw(port: u16, path: &str, require_state: bool) -> std::io::Result<CallbackServer> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        Ok(CallbackServer {
            listener,
            settled: false,
            path: path.to_string(),
            require_state,
        })
    }

    /// 实际监听端口（bind(0) 后为系统分配值）
    pub fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// 回调 URL（手动粘贴与浏览器回调共用）
    pub fn callback_url(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port(), self.path)
    }

    /// 主动关闭监听（登录结束/取消时）；TcpListener 随自身 drop 释放端口
    pub fn shutdown(&mut self) {
        self.settled = true;
    }
}

/// 事件循环 tick 时调用：非阻塞处理挂起的回调连接。
/// 返回 None 表示暂无回调；Some(Ok(code,state)) 表示合法回调；
/// Some(Err(msg)) 表示处理错误（已回错误页）。
/// `server`：本地回调服务器；一次 tick 内会处理完所有挂起连接。
pub fn poll_callback(
    server: &mut CallbackServer,
) -> Option<std::result::Result<CodeResult, String>> {
    if server.settled {
        return None;
    }

    loop {
        let (mut socket, _) = match server.listener.accept() {
            Ok(pair) => pair,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
            Err(e) => return Some(Err(format!("callback accept failed: {}", e))),
        };
        socket.set_read_timeout(Some(Duration::from_secs(3))).ok();
        socket.set_write_timeout(Some(Duration::from_secs(3))).ok();
        let mut buf = [0u8; 8192];
        let n = match std::io::Read::read(&mut socket, &mut buf) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if n == 0 {
            continue;
        }
        let head = String::from_utf8_lossy(&buf[..n]);
        let request_line = head.lines().next().unwrap_or("");
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_string();
        let (path_only, query) = match path.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (path, String::new()),
        };
        if path_only != server.path {
            let body = error_html("Callback path not found", Some(&path_only));
            write_sync_http(&mut socket, 404, &body);
            continue;
        }
        let params: HashMap<String, String> = query
            .split('&')
            .filter(|s| !s.is_empty())
            .filter_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                Some((
                    http::url_decode(k).unwrap_or_default(),
                    http::url_decode(v).unwrap_or_default(),
                ))
            })
            .collect();
        if let Some(error) = params.get("error") {
            // provider 以 authorization error 重定向时必须带描述失败，不能只回错误页然后继续无限等回调。
            let description = params
                .get("error_description")
                .filter(|d| !d.is_empty())
                .unwrap_or(error);
            let body = error_html(
                "Authorization not completed",
                Some(&format!("error={}", description)),
            );
            write_sync_http(&mut socket, 400, &body);
            server.settled = true;
            return Some(Err(format!("authorization failed: {}", description)));
        }
        let (Some(code), state) = (params.get("code"), params.get("state")) else {
            let body = error_html("Missing code or state parameter", None);
            write_sync_http(&mut socket, 400, &body);
            continue;
        };

        // 无 state 校验的端点（OpenRouter）：state 可为空
        let state = if server.require_state {
            let Some(state) = state.cloned() else {
                let body = error_html("Missing state parameter", None);
                write_sync_http(&mut socket, 400, &body);
                continue;
            };
            state
        } else {
            state.cloned().unwrap_or_default()
        };

        let result = CodeResult {
            code: code.clone(),
            state,
            client_id: params.get("client_id").cloned(),
        };
        let body = success_html();
        write_sync_http(&mut socket, 200, &body);
        server.settled = true;
        return Some(Ok(result));
    }
}

/// 渲染回调结果页（深色卡片）。`title` 为浏览器标题，`heading` 为卡片标题，
/// `message` 为正文，`details` 为可选等宽字体补充信息（已 HTML 转义）。
fn oauth_html(title: &str, heading: &str, message: &str, details: Option<&str>) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let details_html = details
        .map(|d| format!("<p class=\"details\">{}</p>", esc(d)))
        .unwrap_or_default();
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8" /><title>{}</title>
<style>
  body {{ margin:0; min-height:100vh; display:flex; align-items:center; justify-content:center;
         background:#09090b; color:#fafafa; font-family: ui-sans-serif, system-ui, sans-serif; }}
  .card {{ max-width: 420px; padding: 24px; border: 1px solid #27272a; border-radius: 12px; }}
  h1 {{ font-size: 18px; }} p {{ color: #a1a1aa; line-height: 1.5; }}
  .details {{ font-family: ui-monospace, monospace; font-size: 12px; word-break: break-all; }}
</style></head>
<body><div class="card"><h1>{}</h1><p>{}</p>{}</div></body></html>"#,
        esc(title),
        esc(heading),
        esc(message),
        details_html
    )
}

/// 登录成功页（提示可关闭窗口）
pub fn success_html() -> String {
    oauth_html(
        "Login complete",
        "Login complete",
        "You can close this window and return to the terminal.",
        None,
    )
}

/// 登录失败页。`message` 为失败原因，`details` 为可选细节。
pub fn error_html(message: &str, details: Option<&str>) -> String {
    oauth_html(
        "Login failed",
        message,
        "Please return to the terminal for more details.",
        details,
    )
}

/// 同步写一个最简 HTTP 响应（Connection: close）后随 socket 关闭结束。
/// `status` 仅支持 200/400/404，其余归为 Error。
fn write_sync_http(socket: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let bytes = body.as_bytes();
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        reason,
        bytes.len()
    );
    _ = socket.write_all(head.as_bytes());
    _ = socket.write_all(bytes);
}

/// 手动粘贴的授权输入
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthorizationInput {
    /// 授权码
    pub code: Option<String>,
    /// state（裸 code 粘贴时为 None）
    pub state: Option<String>,
    /// 动态注册签发的 client id（openai 回调 URL 带）
    pub client_id: Option<String>,
}

/// 解析手动粘贴的授权码/回调 URL。
///
/// 支持：完整回调 URL（取 query 中的 code/state/client_id）、
/// `code#state`、`code=..&state=..`、以及裸 code。
pub fn parse_authorization_input(input: &str) -> AuthorizationInput {
    let value = input.trim();
    if value.is_empty() {
        return AuthorizationInput::default();
    }
    // URL 形式
    if value.contains("://") {
        let (_, query) = value.split_once('?').unwrap_or((value, ""));
        let mut out = AuthorizationInput::default();
        for kv in query.split('&').filter(|s| !s.is_empty()) {
            if let Some((k, v)) = kv.split_once('=') {
                let decoded = || http::url_decode(v).unwrap_or_default();
                match k {
                    "code" => out.code = Some(decoded()),
                    "state" => out.state = Some(decoded()),
                    "client_id" => out.client_id = Some(decoded()),
                    _ => {}
                }
            }
        }
        return out;
    }

    // code#state 或 code=..&state=.. 或裸 code
    if value.contains('#') {
        let (code, state) = value.split_once('#').unwrap();
        return AuthorizationInput {
            code: Some(code.to_string()),
            state: Some(state.to_string()),
            client_id: None,
        };
    }
    if value.contains("code=") {
        let mut out = AuthorizationInput::default();
        for kv in value.split('&').filter(|s| !s.is_empty()) {
            if let Some((k, v)) = kv.split_once('=') {
                let decoded = || http::url_decode(v).unwrap_or_default();
                match k {
                    "code" => out.code = Some(decoded()),
                    "state" => out.state = Some(decoded()),
                    "client_id" => out.client_id = Some(decoded()),
                    _ => {}
                }
            }
        }
        return out;
    }
    AuthorizationInput {
        code: Some(value.to_string()),
        state: None,
        client_id: None,
    }
}

/// 登录流程类型：授权码（PKCE）+ 回调 / device-code 轮询
#[derive(Debug)]
pub enum LoginKind {
    /// 授权码 + PKCE：等待浏览器回调或用户粘贴。
    AuthorizeCode {
        /// 完整授权 URL（含 PKCE challenge 与 state）
        auth_url: String,
        /// PKCE code_verifier（token 交换用）
        verifier: String,
        /// state（空 = 不校验；anthropic/openai 用 verifier 本身）
        state: String,
        /// 本地回调服务器；None = 回调端口被占用，退化为纯粘贴 redirect URL
        server: Option<CallbackServer>,
    },
    /// 复制授权码登录：不绑本地端口，浏览器回调落在 provider 侧页面，
    /// 用户把页面上的 `code#state` 粘回终端。
    AuthorizeCopyCode {
        /// 完整授权 URL（含 PKCE challenge 与 state）
        auth_url: String,
        /// PKCE code_verifier（token 交换用）
        verifier: String,
        /// 授权请求里的 state；粘贴回来的 state 必须与它一致
        state: String,
        /// token 交换时必须与授权请求一致的 provider 侧回调地址
        redirect_uri: String,
    },
    /// device-code：显示 user_code + verification_uri，按间隔轮询。
    DeviceCode {
        /// provider 的 device-code 处理器（初始化/轮询实现）
        handler: Box<dyn DeviceCodeHandler + Send>,
        /// 轮询状态机（user_code、轮询间隔、当前步骤等）
        flow: device_code::DeviceFlow,
    },
}

impl std::fmt::Display for LoginKind {
    /// 输出流程类型名：`"authorize-code"` / `"authorize-copy-code"` / `"device-code"`。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoginKind::AuthorizeCode { .. } => write!(f, "authorize-code"),
            LoginKind::AuthorizeCopyCode { .. } => write!(f, "authorize-copy-code"),
            LoginKind::DeviceCode { .. } => write!(f, "device-code"),
        }
    }
}

/// 登录会话
#[derive(Debug)]
pub struct OAuthLogin {
    /// provider 标识（如 anthropic / github-copilot）
    pub provider_id: String,
    /// provider 展示名（如 Anthropic）
    pub provider_name: String,
    /// 本次登录的流程类型与中间状态
    pub kind: LoginKind,
    /// 用户手动提交的授权码/URL（tick 处理）
    pub manual_input: Option<String>,
}

impl OAuthLogin {
    /// 授权 URL（授权码流与复制授权码流；device-code 返回空串）
    pub fn auth_url(&self) -> String {
        match &self.kind {
            LoginKind::AuthorizeCode { auth_url, .. }
            | LoginKind::AuthorizeCopyCode { auth_url, .. } => auth_url.clone(),
            LoginKind::DeviceCode { .. } => String::new(),
        }
    }

    /// 是否是「复制授权码」流程（无本地回调，只能等用户粘贴 `code#state`）
    pub fn is_copy_code(&self) -> bool {
        matches!(&self.kind, LoginKind::AuthorizeCopyCode { .. })
    }

    /// token 交换时覆盖 provider 默认 `redirect_uri` 的值；
    /// 复制授权码流用 provider 侧回调地址（本地回调流返回 None，用 provider 默认）。
    pub fn redirect_uri_override(&self) -> Option<&str> {
        match &self.kind {
            LoginKind::AuthorizeCopyCode { redirect_uri, .. } => Some(redirect_uri),
            _ => None,
        }
    }

    /// device-code 展示信息（未经初始化时返回 None）
    pub fn device_code_info(&self) -> Option<(String, String)> {
        match &self.kind {
            LoginKind::DeviceCode { flow, .. } if !flow.user_code.is_empty() => {
                Some((flow.user_code.clone(), flow.verification_uri.clone()))
            }
            _ => None,
        }
    }

    /// 流程是否已结束（成功/失败/取消）
    pub fn finished(&self) -> bool {
        match &self.kind {
            LoginKind::DeviceCode { flow, .. } => flow.step.is_finished(),
            LoginKind::AuthorizeCode { .. } | LoginKind::AuthorizeCopyCode { .. } => false,
        }
    }

    /// 是否 device-code 流（含尚未初始化的 Init 状态；区别于授权码流）
    pub fn is_device_code(&self) -> bool {
        matches!(&self.kind, LoginKind::DeviceCode { .. })
    }

    /// 当前是否在等待用户回答提问（copilot 企业域名；AwaitInput 步骤）
    pub fn awaiting_input(&self) -> bool {
        matches!(
            &self.kind,
            LoginKind::DeviceCode { flow, .. } if flow.step == device_code::DeviceStep::AwaitInput
        )
    }

    /// AwaitInput 步骤的提问文案（其他状态返回 None）
    pub fn prompt_text(&self) -> Option<String> {
        match &self.kind {
            LoginKind::DeviceCode { flow, .. }
                if flow.step == device_code::DeviceStep::AwaitInput =>
            {
                (!flow.prompt.is_empty()).then(|| flow.prompt.clone())
            }
            _ => None,
        }
    }

    /// AwaitInput 步骤的输入占位示例（如 copilot "e.g., company.ghe.com"；无则空）
    pub fn prompt_placeholder(&self) -> String {
        match &self.kind {
            LoginKind::DeviceCode { flow, .. } => flow.placeholder.clone(),
            _ => String::new(),
        }
    }

    /// 当前需要展示的进度/说明文本（无则空串）
    pub fn progress(&self) -> Option<String> {
        match &self.kind {
            LoginKind::DeviceCode { .. } => None,
            LoginKind::AuthorizeCode { .. } | LoginKind::AuthorizeCopyCode { .. } => None,
        }
    }
}

/// 启动登录会话（同步准备；device-code 的 HTTP 初始化在首次轮询时异步完成）。
/// provider 未实现登录流程时报错。
///
/// `provider_id`：login_registry 中的 provider 标识（如 anthropic / github-copilot）。
pub fn start_login(provider_id: &str) -> Result<OAuthLogin> {
    match provider_id {
        "anthropic" => anthropic::start_authorize_flow(),
        "kimi-coding" => Ok(kimi::start_device_flow()),
        "xai" => Ok(xai::start_device_flow()),
        "github-copilot" => Ok(copilot::start_device_flow()),
        "openrouter" => openrouter::start_authorize_flow(),
        "openai" => openai_chatgpt::start_authorize_flow(),
        _ => Err(Error::msg(format!(
            "OAuth login flow not implemented for provider: {}",
            provider_id
        ))),
    }
}

/// 该 provider 是否需要让用户先选登录方式（浏览器回调 / 复制授权码）。
pub fn supports_copy_code_login(provider_id: &str) -> bool {
    matches!(provider_id, "anthropic")
}

/// 启动「复制授权码」登录（不绑本地端口）。provider 未实现时报错。
pub fn start_copy_code_login(provider_id: &str) -> Result<OAuthLogin> {
    match provider_id {
        "anthropic" => anthropic::start_copy_code_flow(),
        _ => Err(Error::msg(format!(
            "copy code login not implemented for provider: {}",
            provider_id
        ))),
    }
}

/// 用授权码换 token（按 provider 分发）。
///
/// `code`：授权码；`state`：回调 state（用作校验/回传）；
/// `verifier`：PKCE code_verifier；`client_id`：仅动态注册的 provider（openai）使用；
/// `redirect_uri`：覆盖 provider 默认回调地址（仅复制授权码流用，其余传 `None`）。
pub async fn exchange_login(
    provider_id: &str,
    code: &str,
    state: &str,
    verifier: &str,
    client_id: Option<&str>,
    redirect_uri: Option<&str>,
) -> Result<OAuthCredential> {
    match provider_id {
        "anthropic" => anthropic::exchange_code(code, state, verifier, redirect_uri).await,
        "openrouter" => openrouter::exchange_code(code, verifier).await,
        "openai" => openai_chatgpt::exchange_code(code, verifier, client_id).await,
        _ => Err(Error::msg(format!(
            "OAuth token exchange not implemented for provider: {}",
            provider_id
        ))),
    }
}

/// 按 provider 刷新过期凭据。返回新凭据（写入由调用方负责）。
/// `cred`：旧凭据（refresh token / client_id 等字段以它为输入）。
pub async fn refresh_oauth(provider_id: &str, cred: &OAuthCredential) -> Result<OAuthCredential> {
    match provider_id {
        "anthropic" => anthropic::refresh_token(&cred.refresh).await,
        "kimi-coding" => kimi::refresh_token(&cred.refresh).await,
        "xai" => xai::refresh_token(&cred.refresh).await,
        "github-copilot" => copilot::refresh_token(cred).await,
        "openrouter" => Ok(cred.clone()),
        "openai" => openai_chatgpt::refresh_token(cred).await,
        _ => Err(Error::msg(format!(
            "OAuth token refresh not implemented for provider: {}",
            provider_id
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_generates_verifier_and_challenge() {
        let pkce = generate_pkce().unwrap();
        assert!(!pkce.verifier.is_empty());
        assert!(!pkce.challenge.is_empty());
        // verifier: 32 字节 base64url (43 chars, no padding)
        assert_eq!(pkce.verifier.len(), 43);
        assert!(!pkce.verifier.contains('='));
        // challenge 是 verifier 的 SHA-256 → 32 字节 → 43 chars base64url
        assert_eq!(pkce.challenge.len(), 43);
        // 确定性：相同 verifier 产出相同 challenge
        let hash = Sha256::digest(pkce.verifier.as_bytes());
        assert_eq!(pkce.challenge, URL_SAFE_NO_PAD.encode(hash));
    }

    #[test]
    fn parse_manual_input_variants() {
        // 裸 code
        let input = parse_authorization_input("qqqq-cccc");
        assert_eq!(input.code.as_deref(), Some("qqqq-cccc"));
        assert!(input.state.is_none());
        // code#state
        let input = parse_authorization_input("code1#state1");
        assert_eq!(input.code.as_deref(), Some("code1"));
        assert_eq!(input.state.as_deref(), Some("state1"));
        // code=...&state=...
        let input = parse_authorization_input("code=xyz&state=abc");
        assert_eq!(input.code.as_deref(), Some("xyz"));
        assert_eq!(input.state.as_deref(), Some("abc"));
        // URL 形式（回调重放粘贴）
        let input =
            parse_authorization_input("http://127.0.0.1:53692/callback?code=abc123&state=xyz~");
        assert_eq!(input.code.as_deref(), Some("abc123"));
        assert_eq!(input.state.as_deref(), Some("xyz~"));
        assert!(input.client_id.is_none());
        // openai 动态注册回调：带签发的 client_id
        let input = parse_authorization_input(
            "http://127.0.0.1:1455/auth/callback?code=c1&state=s1&client_id=client-issued",
        );
        assert_eq!(input.client_id.as_deref(), Some("client-issued"));
        // 空白
        let input = parse_authorization_input("  ");
        assert!(input.code.is_none());
        assert!(input.state.is_none());
    }

    #[test]
    fn credential_expiry_logic() {
        let c = OAuthCredential {
            access: "a".into(),
            refresh: "r".into(),
            expires: 1000,
            enterprise_url: None,
            available_model_ids: None,
            client_id: None,
            scopes: None,
        };
        assert!(c.expired(1001));
        assert!(!c.expired(999));
        // 空 refresh → 不可刷新
        let permanent = OAuthCredential {
            access: "k".into(),
            refresh: String::new(),
            expires: u64::MAX,
            enterprise_url: None,
            available_model_ids: None,
            client_id: None,
            scopes: None,
        };
        assert!(!permanent.refreshable());
        assert!(permanent.is_permanent());
        // u64::MAX - 1 远大于任何真实时钟，永不过期
        assert!(!permanent.expired(u64::MAX - 1));
    }

    #[test]
    fn login_session_exposes_auth_url() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let login = anthropic::start_authorize_flow().unwrap();
        assert_eq!(login.provider_name, "Anthropic");
        let url = login.auth_url();
        assert!(url.starts_with(anthropic::AUTHORIZE_URL));
    }
}
#[cfg(test)]
mod callback_tests {
    use super::*;
    use std::io::Read as _;

    /// 串行化占用回调端口的测试：与 auth 流程测试共用全局锁，
    /// 避免并行时互相抢端口导致 bind 失败 → panic → 锁污染连锁
    fn port_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn hit_async(port: u16, url: &str) -> std::thread::JoinHandle<String> {
        let url = url.to_string();
        std::thread::spawn(move || {
            let mut resp = String::new();
            if let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                let _ = stream.write_all(
                    format!(
                        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                        url
                    )
                    .as_bytes(),
                );
                let mut buf = [0u8; 65536];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => resp.push_str(&String::from_utf8_lossy(&buf[..n])),
                        Err(_) => break,
                    }
                }
            }
            resp
        })
    }

    #[test]
    fn callback_returns_code_and_success_page() {
        let _g = port_lock();
        let Ok(mut server) = CallbackServer::bind(0) else {
            eprintln!("skip: callback port occupied");
            return;
        };
        let port = server.port();
        assert!(port > 0, "temporary port should be usable");
        let result = poll_callback(&mut server);
        assert!(result.is_none(), "should return None when no connection");
        drop(server); // 释放端口

        let mut server = CallbackServer::bind(0).unwrap();
        let port = server.port();
        // 无关路径：起线程请求，poll 处理 404 后返回 None
        let t = hit_async(port, "/other");
        std::thread::sleep(std::time::Duration::from_millis(80));
        let code_result = poll_callback(&mut server);
        assert!(code_result.is_none(), "non-callback path should be ignored");
        let resp = t.join().unwrap();
        assert!(resp.contains("404 Not Found"));
        // 合法回调
        let t = hit_async(port, "/callback?code=abc&state=xyz");
        std::thread::sleep(std::time::Duration::from_millis(80));
        let code_result = poll_callback(&mut server);
        let cr = code_result
            .expect("valid callback should settle")
            .expect("no error");
        assert_eq!(cr.code, "abc");
        assert_eq!(cr.state, "xyz");
        let resp = t.join().unwrap();
        assert!(resp.contains("200 OK"));
        assert!(resp.contains("Login complete"));
        drop(server);
    }

    #[test]
    fn callback_rejects_missing_params() {
        let _g = port_lock();
        let Ok(mut server) = CallbackServer::bind(0) else {
            eprintln!("skip: callback port occupied");
            return;
        };
        let port = server.port();
        // 缺 code → 400 错误页，poll 不返回结果（继续等）
        let t = hit_async(port, "/callback?state=only");
        std::thread::sleep(std::time::Duration::from_millis(80));
        let _ = poll_callback(&mut server);
        let r = t.join().unwrap();
        assert!(r.contains("400 Bad Request"));
        assert!(r.contains("Missing code or state parameter"));
        // 有 code 缺 state（require_state=true 端点）→ 400 “缺少 state 参数”
        let t = hit_async(port, "/callback?code=only");
        std::thread::sleep(std::time::Duration::from_millis(80));
        let _ = poll_callback(&mut server);
        let r = t.join().unwrap();
        assert!(r.contains("400 Bad Request"));
        assert!(r.contains("Missing state parameter"));
        // provider 以 authorization error 重定向 → 立即失败（pi：不再回错误页后无限等待）
        let t = hit_async(
            port,
            "/callback?error=access_denied&error_description=User+denied",
        );
        std::thread::sleep(std::time::Duration::from_millis(80));
        let err = poll_callback(&mut server)
            .expect("error redirect should settle")
            .expect_err("error redirect should fail");
        assert_eq!(err, "authorization failed: User denied");
        let r = t.join().unwrap();
        assert!(r.contains("400 Bad Request"));
        assert!(r.contains("User denied"));
        drop(server);
    }

    #[test]
    fn callback_port_bind_fails_when_occupied() {
        let _g = port_lock();
        // 占用端口后 bind 应报错
        let Ok(occupied) = std::net::TcpListener::bind(("127.0.0.1", 0)) else {
            eprintln!("skip: bind failed unexpectedly");
            return;
        };
        let err = CallbackServer::bind(occupied.local_addr().unwrap().port()).unwrap_err();
        assert!(err.to_string().contains("failed to bind"));
        drop(occupied);
    }
}
