//! 认证凭据：密码、私钥口令、交互式输入。
//!
//! 凭据只在**进程内存**里缓存，从不落盘、不进命令行参数、不进会话记录。来源按以下顺序：
//!
//! 1. 进程内缓存（同一次运行里重连、再次连接直接复用）；
//! 2. `-o PasswordFile=<path>`（权限必须是 0600，否则拒绝读取）；
//! 3. 环境变量 `PRUX_SSH_PASSWORD`（登录密码）/ `PRUX_SSH_KEY_PASSPHRASE`（私钥口令）；
//! 4. TUI 掩码输入（`Overlay` 覆盖层，逐键回传、扩展自己维护明文缓冲并渲染 `*`）。
//!
//! 交互输入走覆盖层而不是内联输入框：`OverlayInput` 的文本由核心持有并原样显示，
//! 密码会明文出现在屏幕上；逐键回传（`OverlayKey::Char`）才能自己掩码。

use crate::{
    core::extensions::{
        self, DockSpan, ExtensionUiRequest, OverlayEvent, OverlayKey, OverlaySize, OverlayView,
        UiNotifyLevel, next_ui_id,
    },
    utils::paths::expand_tilde_path,
};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tokio::sync::oneshot;

/// 交互输入的超时（用户离开时不要让后台连接永久挂着）
const PROMPT_TIMEOUT: Duration = Duration::from_secs(180);

/// 进行中的交互输入
static PROMPT: OnceLock<Mutex<Option<PromptState>>> = OnceLock::new();

/// 凭据缓存：(端点标识, 种类) → 明文
static SECRETS: OnceLock<Mutex<HashMap<(String, SecretKind), String>>> = OnceLock::new();

/// 凭据种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKind {
    /// 登录密码
    Password,
    /// 私钥口令
    KeyPassphrase,
}

impl SecretKind {
    /// 环境变量名
    fn env_var(self) -> &'static str {
        match self {
            SecretKind::Password => "PRUX_SSH_PASSWORD",
            SecretKind::KeyPassphrase => "PRUX_SSH_KEY_PASSPHRASE",
        }
    }
}

/// 缓存容器
fn secrets() -> &'static Mutex<HashMap<(String, SecretKind), String>> {
    SECRETS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 交互输入的进行中状态
struct PromptState {
    /// 覆盖层 id
    id: u64,
    /// 凭据种类（决定缓存键与提示语）
    kind: SecretKind,
    /// 端点标识（缓存键）
    scope: String,
    /// 展示给用户的提示（如 `password for root@gpu:22`）
    prompt: String,
    /// 已输入的明文（渲染时替换成 `*`）
    buffer: String,
    /// 结果回传口（提交或取消时消费）
    reply: Option<oneshot::Sender<Option<String>>>,
}

/// 进行中状态容器
fn prompt_slot() -> &'static Mutex<Option<PromptState>> {
    PROMPT.get_or_init(|| Mutex::new(None))
}

/// 取已缓存的凭据
pub fn cached(scope: &str, kind: SecretKind) -> Option<String> {
    secrets()
        .lock()
        .unwrap()
        .get(&(scope.to_string(), kind))
        .cloned()
}

/// 写入缓存
pub fn remember(scope: &str, kind: SecretKind, value: &str) {
    secrets()
        .lock()
        .unwrap()
        .insert((scope.to_string(), kind), value.to_string());
}

/// 清空某端点的全部缓存（`/ssh remove <target>`）
pub fn forget(scope: &str) -> usize {
    let mut map = secrets().lock().unwrap();
    let before = map.len();
    map.retain(|(key, _), _| key != scope);
    before - map.len()
}

/// 清空所有端点的缓存
pub fn forget_all() -> usize {
    let mut map = secrets().lock().unwrap();
    let count = map.len();
    map.clear();
    count
}

/// [`acquire`] 的结果。
///
/// 区分「没有凭据」「有凭据但被拒」「用户主动取消」：被拒时必须把原因带给调用方，
/// 否则用户只会看到「认证失败」，不知道真正的问题是密码文件权限过宽；
/// 取消则要能被调用方识别出来，不能退化成一条认证错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquired {
    /// 取到凭据
    Found(String),
    /// 没有可用来源（非 TUI、超时、环境变量为空）
    Missing,
    /// 用户在交互输入框里主动取消（`Esc` / 关闭覆盖层）：不是错误，不要报认证失败
    Cancelled,
    /// 有来源但不可用（文件权限过宽、文件为空等），附原因
    Rejected(String),
}

/// 需要凭据时调用：按「缓存 → 文件 → 环境变量 → 交互输入」取一个值。
pub async fn acquire(
    scope: &str,
    kind: SecretKind,
    prompt: &str,
    password_file: Option<&str>,
) -> Acquired {
    if let Some(value) = cached(scope, kind) {
        return Acquired::Found(value);
    }

    if let Some(path) = password_file {
        match read_secret_file(path) {
            Ok(value) => {
                remember(scope, kind, &value);
                return Acquired::Found(value);
            }
            Err(e) => return Acquired::Rejected(e),
        }
    }

    if let Ok(value) = std::env::var(kind.env_var())
        && !value.is_empty()
    {
        remember(scope, kind, &value);
        return Acquired::Found(value);
    }

    if !extensions::ui_ready() {
        return Acquired::Missing;
    }

    match ask_overlay(scope, kind, prompt).await {
        Acquired::Found(value) if !value.is_empty() => {
            remember(scope, kind, &value);
            Acquired::Found(value)
        }
        Acquired::Found(_) => Acquired::Missing,
        other => other,
    }
}

/// 读凭据文件：取第一行，要求权限为 0600（Unix）
pub fn read_secret_file(path: &str) -> Result<String, String> {
    let expanded = expand_tilde_path(path.to_string());
    let meta = std::fs::metadata(&expanded).map_err(|e| format!("{}: {e}", expanded.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "{} has mode {:03o}; run `chmod 600 {}`",
                expanded.display(),
                mode,
                expanded.display()
            ));
        }
    }

    #[cfg(not(unix))]
    let _ = &meta;

    let text =
        std::fs::read_to_string(&expanded).map_err(|e| format!("{}: {e}", expanded.display()))?;
    let first = text.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return Err(format!("{} is empty", expanded.display()));
    }
    Ok(first.to_string())
}

/// 弹出掩码输入框并等待结果。
///
/// 用户按 `Esc` / 关闭覆盖层时返回 [`Acquired::Cancelled`]：调用方据此区分
/// 「用户主动放弃」与「根本没有输入渠道」，不要把它当成认证失败。
async fn ask_overlay(scope: &str, kind: SecretKind, prompt: &str) -> Acquired {
    let id = next_ui_id();
    let (tx, rx) = oneshot::channel();

    {
        // 同一时刻只允许一个输入框：后来的请求直接失败，不覆盖前者
        let mut slot = prompt_slot().lock().unwrap();
        if slot.is_some() {
            return Acquired::Missing;
        }

        *slot = Some(PromptState {
            id,
            kind,
            scope: scope.to_string(),
            prompt: prompt.to_string(),
            buffer: String::new(),
            reply: Some(tx),
        });
    }

    extensions::request_ui(ExtensionUiRequest::ShowOverlay { id });

    let result = tokio::time::timeout(PROMPT_TIMEOUT, rx).await;
    match result {
        Ok(Ok(Some(value))) => Acquired::Found(value),
        Ok(Ok(None)) => {
            extensions::request_ui(ExtensionUiRequest::Notify {
                text: "SSH authentication cancelled".to_string(),
                level: UiNotifyLevel::Warning,
            });
            Acquired::Cancelled
        }
        // 超时或 UI 已销毁：清掉状态，避免卡住后续请求
        _ => {
            prompt_slot().lock().unwrap().take();
            extensions::request_ui(ExtensionUiRequest::HideOverlay { id });
            if !extensions::ui_ready() {
                return Acquired::Missing;
            }

            extensions::request_ui(ExtensionUiRequest::Notify {
                text: format!("SSH credential prompt timed out ({prompt})"),
                level: UiNotifyLevel::Warning,
            });
            Acquired::Missing
        }
    }
}

/// 渲染进行中的密码输入框（`Extension::overlay_view` 调用；TUI 线程，每帧一次）
pub fn overlay_view(id: u64, _cols: u16) -> Option<OverlayView> {
    let slot = prompt_slot().lock().unwrap();
    let state = slot.as_ref().filter(|s| s.id == id)?;
    let masked = "*".repeat(state.buffer.chars().count());
    Some(OverlayView {
        title: "SSH authentication".to_string(),
        header: vec![
            vec![DockSpan::new("accent", "#8abeb7", " SSH authentication")],
            vec![DockSpan::plain("")],
            vec![DockSpan::plain(state.prompt.clone())],
        ],
        lines: vec![vec![DockSpan::new("accent", "#8abeb7", masked)]],
        messages: Vec::new(),
        footer: vec![
            ("Enter".to_string(), "confirm".to_string()),
            ("Esc".to_string(), "cancel".to_string()),
        ],
        input: None,
        editor: None,
        size: OverlaySize::Panel { max_rows: 10 },
        selected: None,
        input_focus: false,
    })
}

/// 处理进行中的密码输入框的按键（`Extension::on_overlay_event` 调用）
pub fn on_overlay_event(id: u64, ev: &OverlayEvent) -> bool {
    let mut slot = prompt_slot().lock().unwrap();
    let Some(state) = slot.as_mut().filter(|s| s.id == id) else {
        return false;
    };

    match ev {
        OverlayEvent::Key { key, .. } => match key {
            OverlayKey::Char(c) => {
                state.buffer.push(*c);
                true
            }
            OverlayKey::Backspace => {
                state.buffer.pop();
                true
            }
            OverlayKey::Delete => {
                state.buffer.clear();
                true
            }
            OverlayKey::Enter => {
                let (kind, scope) = (state.kind, state.scope.clone());
                let buffer = std::mem::take(&mut state.buffer);
                let reply = state.reply.take();
                // 先交出槽位再回传：回传后等待方可能立刻发起下一次输入
                drop(slot);
                if let Some(reply) = reply {
                    _ = reply.send(Some(buffer.clone()));
                }

                extensions::request_ui(ExtensionUiRequest::HideOverlay { id });
                // 提交即缓存（`acquire` 也会缓存，这里冗余一次以防等待端被丢弃）
                if !buffer.is_empty() {
                    remember(&scope, kind, &buffer);
                }
                true
            }
            OverlayKey::Esc => {
                let reply = state.reply.take();
                drop(slot);

                if let Some(reply) = reply {
                    _ = reply.send(None);
                }

                prompt_slot().lock().unwrap().take();
                extensions::request_ui(ExtensionUiRequest::HideOverlay { id });
                true
            }
            _ => true,
        },
        OverlayEvent::Closed => {
            let reply = state.reply.take();
            drop(slot);
            if let Some(reply) = reply {
                _ = reply.send(None);
            }
            prompt_slot().lock().unwrap().take();
            true
        }
        // 没有内联输入行，提交事件不会出现
        _ => false,
    }
}

/// 串行化操作凭据缓存的测试。
///
/// 缓存是进程级单例：并行的用例互相 `forget_all()` 会把对方的条目清掉。
/// 同步用例用 `blocking_lock()`，e2e 用 `lock().await`。
#[cfg(test)]
pub static AUTH_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// 凭据缓存是进程级单例：所有涉缓存的用例都放在同一个测试里顺序跑，
    /// 否则 `forget_all()` 会互相清空（并行测试共享同一张表）。
    #[test]
    fn cache_scopes_kinds_and_forgets() {
        let _l = AUTH_TEST_LOCK.blocking_lock();
        forget_all();
        let scope = "root@cache-test:22";
        assert!(cached(scope, SecretKind::Password).is_none());

        remember(scope, SecretKind::Password, "s3cret");
        remember(scope, SecretKind::KeyPassphrase, "pass");
        assert_eq!(
            cached(scope, SecretKind::Password).as_deref(),
            Some("s3cret")
        );
        assert_eq!(
            cached(scope, SecretKind::KeyPassphrase).as_deref(),
            Some("pass")
        );
        // 不同端点互不影响
        remember("other@host:22", SecretKind::Password, "x");
        assert_eq!(forget(scope), 2);
        assert!(cached(scope, SecretKind::Password).is_none());
        assert_eq!(
            cached("other@host:22", SecretKind::Password).as_deref(),
            Some("x")
        );
        assert_eq!(forget_all(), 1);
        assert!(cached("other@host:22", SecretKind::Password).is_none());
    }

    /// 密码输入面板必须占据输入区（Panel）：若用 Inline，主页输入框会留在面板下方，
    /// 但按键全被覆盖层吃掉，用户看到的是个打不了字的框。
    #[test]
    fn prompt_panel_takes_over_the_input_area() {
        let _l = AUTH_TEST_LOCK.blocking_lock();
        let (tx, _rx) = oneshot::channel();
        *prompt_slot().lock().unwrap() = Some(PromptState {
            id: 7,
            kind: SecretKind::Password,
            scope: "blue@ubuntu:22".to_string(),
            prompt: "password for blue@ubuntu:22".to_string(),
            buffer: "abc".to_string(),
            reply: Some(tx),
        });

        let view = overlay_view(7, 80).expect("进行中应有覆盖层");
        assert_eq!(view.size, OverlaySize::Panel { max_rows: 10 });
        assert!(
            view.header
                .iter()
                .any(|line| line.iter().any(|s| s.text.contains("SSH authentication"))),
            "面板无标题行，标题要塞进固定区"
        );
        assert!(
            view.header
                .iter()
                .any(|line| line.iter().any(|s| s.text.contains("blue@ubuntu:22"))),
            "固定区要写清这是谁的口令"
        );
        let title: String = view.header[0].iter().map(|s| s.text.as_str()).collect();
        assert_eq!(title, " SSH authentication", "标题独占首行");
        assert!(
            view.header[1].iter().all(|s| s.text.is_empty()),
            "标题与口令提示之间留一行空行"
        );
        let prompt: String = view.header[2].iter().map(|s| s.text.as_str()).collect();
        assert_eq!(prompt, "password for blue@ubuntu:22", "口令提示另起一行");
        assert_eq!(view.lines[0][0].text, "***", "密码必须掩码");

        prompt_slot().lock().unwrap().take();
    }

    #[test]
    fn secret_file_rejects_wide_permissions() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!("prux-secret-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("pw");
            std::fs::write(&path, "hunter2\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let err = read_secret_file(&path.to_string_lossy()).unwrap_err();
            assert!(err.contains("chmod 600"), "{err}");

            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(
                read_secret_file(&path.to_string_lossy()).unwrap(),
                "hunter2"
            );
            _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// 文件来源优先于交互，并写入缓存（`acquire` 的短路顺序）
    #[tokio::test]
    async fn reads_password_from_file() {
        let _l = AUTH_TEST_LOCK.lock().await;
        let dir = std::env::temp_dir().join(format!("prux-acq-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pw");
        std::fs::write(&path, "file-secret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        forget_all();
        let value = acquire(
            "u@acq:1",
            SecretKind::Password,
            "password",
            Some(&path.to_string_lossy()),
        )
        .await;
        assert_eq!(value, Acquired::Found("file-secret".to_string()));
        assert_eq!(
            cached("u@acq:1", SecretKind::Password).as_deref(),
            Some("file-secret")
        );
        forget_all();
        _ = std::fs::remove_dir_all(&dir);
    }
}
