//! `update-check` 扩展：启动时异步检查 GitHub 最新 release，有新版本就在输入框下方弹出选择面板。
//!
//! 请求 `api.github.com/repos/<repo>/releases/latest` 取 `tag_name`，逐段比较数字版本；
//! 用户选「Don't ask again」时把该版本写进 `settings.json` 的
//! [`skipUpdateVersion`](crate::core::settings_manager::write_skip_update_version)，
//! 只抑制**这一个版本**——之后发布的更新版本会重新提示。
//!
//! 生命周期：
//! - **触发**：`on_session_start`（TUI 启动时由 `main` 分发一次）。
//!   离线（`--offline` / `PRUX_OFFLINE`）跳过；每次启动至多检查一次（进程级 AtomicBool）。
//! - **展示**：扩展覆盖层（[`OverlaySize::Inline`]：渲染在**输入框下方、底栏上方**），
//!   三个选项 `Open download page` / `Ignore` / `Don't ask again`；↑↓ 选择、Enter 确认、Esc / Ctrl+C 关闭。
//! - **动作**：`Open download page` 用浏览器打开 release 页并关闭；`Ignore` 只关掉本次提示
//!   （不落盘，下次启动仍提示）；`Don't ask again` 把版本写入 `settings.json` 后关闭。
//!
//! 网络失败静默忽略（启动期不该因为一次检查失败打扰用户），只在写盘失败时提示。

use crate::{
    APP_NAME,
    core::{
        extensions::{
            DockLine, DockSpan, Extension, ExtensionCommand, ExtensionHook, ExtensionMode,
            ExtensionTool, ExtensionUiRequest, OverlayEvent, OverlayKey, OverlaySize, OverlayView,
            SubcommandDef, UiNotifyLevel, next_ui_id, request_ui,
        },
        provider::AgentMessage,
        runtime,
        session_manager::Session,
        settings_manager,
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_UPDATE_CHECK, command_arg,
        util::{normalize_version, notify_text, version_greater},
    },
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
    utils::{
        glyphs::{DEF_ARROW_UP_DOWN, DEF_SELECTED_MARK},
        http,
    },
};
use serde_json::Value;
use std::{
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// 扩展名
const EXT: &str = "update-check";

/// 最新 release 的 API 端点（仓库 `heng30/prux`）
const LATEST_API: &str = "https://api.github.com/repos/heng30/prux/releases/latest";

/// 下载页（release 未带 `html_url` 时的回落地址）
const DOWNLOAD_PAGE: &str = "https://github.com/heng30/prux/releases/latest";

/// 单次检查超时（启动期不能拖太久；后台任务，失败静默）
const TIMEOUT: Duration = Duration::from_secs(15);

/// 选项文案（下标即顺序；渲染与按键分发共用，避免两处漂移）。
const OPTIONS: [&str; 3] = ["Open download page", "Ignore", "Don't ask again"];

/// 打开下载页选项的下标
const OPTION_OPEN: usize = 0;

/// 「不再提示」选项的下标
const OPTION_SKIP: usize = 2;

/// `/update` 子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "check",
        description: "Check for a newer release and print the version + download URL",
    },
    SubcommandDef {
        name: "open",
        description: "Open the latest release download page in your browser",
    },
];

/// 自声明工厂：linkme 分布式切片
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static UPDATE_CHECK_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_UPDATE_CHECK,
    make: || -> Arc<dyn Extension> { Arc::new(UpdateCheck) },
};

/// 已取回的最新 release。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Release {
    /// 版本号（已去掉前导 `v`）
    version: String,
    /// release 页面 URL（浏览器打开用）
    url: String,
}

/// 扩展 UI 状态（进程级单例）。
#[derive(Default)]
struct State {
    /// 当前覆盖层 id（None = 未显示）
    overlay_id: Option<u64>,
    /// 待提示的新版本（None = 无）
    notice: Option<Release>,
    /// 当前选中下标
    selected: usize,
}

/// 取扩展 UI 状态的进程级单例（惰性初始化）。
fn state() -> &'static Mutex<State> {
    /// 扩展 UI 状态（覆盖层 id、待提示的新版本、选中下标）的进程级单例。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 取状态锁；锁被毒化时取回内部值。
fn lock() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// `update-check` 扩展。
pub struct UpdateCheck;

impl Extension for UpdateCheck {
    /// 扩展名 `update-check`。
    fn name(&self) -> &str {
        EXT
    }

    /// 一句话说明：启动时检查新版本，并在输入框下方弹选择面板。
    fn description(&self) -> &str {
        "Check GitHub for a newer release on startup and prompt in a popup below the input box \
         (Open download page / Ignore / Don't ask again)"
    }

    /// 声明 `Minimal` = Minimal 及以上所有扩展模式都可用：版本提示与扩展模式无关。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Minimal]
    }

    /// 不提供模型工具（版本提示只面向用户）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/update` 命令及 `check` / `open` 子命令，并标记 `busy_safe`。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: "update".to_string(),
            description: "/update check (print new version + download URL) | /update open (open the download page)"
                .to_string(),
            busy_safe: true, // handler 只起异步任务 / 调浏览器，不锁 agent，忙碌时可安全执行
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 注册 `/update` 的 handler 到斜杠命令表。
    fn on_registered(&self) {
        register_slash_command(EXT, "update", command_update);
    }

    /// TUI 启动时由 `main` 分发一次：发起异步检查（离线 / 已检查过则跳过）。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], _session: Option<&Session>) {
        start_check();
    }

    /// 禁用时收起提示并清状态：禁用后框架不再轮询本扩展，`Closed` 回传也收不到，
    /// 不主动清理会让重新启用时旧提示（id 已失效）复活。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            clear(None);
        }
    }

    /// 只在提示打开时订阅覆盖层（否则框架每帧空转）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        if lock().overlay_id.is_some() {
            vec![ExtensionHook::Overlay]
        } else {
            Vec::new()
        }
    }

    /// 渲染提示覆盖层（标题 + 三个选项 + 底部按键提示）。
    ///
    /// `id` 不匹配当前提示或没有待提示版本时返回 `None`。
    fn overlay_view(&self, id: u64, _cols: u16) -> Option<OverlayView> {
        let st = lock();
        if st.overlay_id != Some(id) {
            return None;
        }

        let notice = st.notice.clone()?;
        let selected = st.selected.min(OPTIONS.len() - 1);

        let lines: Vec<DockLine> = OPTIONS
            .iter()
            .enumerate()
            .map(|(i, label)| option_line(label, i == selected))
            .collect();

        Some(OverlayView {
            title: format!(
                "Update available · v{} (current v{})",
                notice.version,
                env!("CARGO_PKG_VERSION")
            ),
            lines,
            footer: vec![
                (DEF_ARROW_UP_DOWN.to_string(), "choose".to_string()),
                ("Enter".to_string(), "select".to_string()),
                ("Esc".to_string(), "close".to_string()),
            ],
            input: None,
            editor: None,
            // 内联覆盖层：显示在输入框下方、底栏上方
            size: OverlaySize::Inline { max_rows: 8 },
            header: Vec::new(),
            messages: Vec::new(),
            selected: Some(selected),
            input_focus: false,
        })
    }

    /// 处理覆盖层事件：↑↓ 环绕移动、Enter 执行动作并关闭、Closed 清状态。
    ///
    /// 返回是否消费事件；Esc 等其它按键不消费，交给 TUI 默认语义。
    fn on_overlay_event(&self, id: u64, ev: &OverlayEvent) -> bool {
        match ev {
            OverlayEvent::Closed => clear(Some(id)),
            OverlayEvent::Key { key, .. } => match key {
                OverlayKey::Up => {
                    move_selection(id, -1);
                    true
                }
                OverlayKey::Down => {
                    move_selection(id, 1);
                    true
                }
                OverlayKey::Enter => {
                    activate(id);
                    true
                }
                // Esc 不消费：交给 TUI 默认语义关闭覆盖层（随后收到 Closed）
                _ => false,
            },
            // 无内联输入 / 编辑器：其余事件与本扩展无关
            _ => false,
        }
    }
}

/// 发起一次异步检查（每次进程至多一次）。
fn start_check() {
    /// 保证启动检查每个进程只发起一次的幂等标记。
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    // 离线模式不联网（`--offline` 不写 env，真源在 runtime）
    if runtime::offline() {
        return;
    }

    // 不在 tokio 运行时内（如部分单测直接调 on_session_start）时不发起检查，避免 panic
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };

    handle.spawn(async move {
        let Some(release) = fetch_latest().await else {
            return; // 网络/解析失败：静默
        };

        if !version_greater(&release.version, env!("CARGO_PKG_VERSION")) {
            return;
        }

        // 「Don't ask again」记录的正是该版本 → 不再提示；更新版本仍会提示
        if settings_manager::read_skip_update_version().as_deref() == Some(release.version.as_str())
        {
            return;
        }

        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.notice = Some(release);
            st.selected = 0;
        }
        request_ui(ExtensionUiRequest::ShowOverlay { id });
    });
}

/// 拉取最新 release：失败（网络 / 非 2xx / 无 tag）返回 None。
async fn fetch_latest() -> Option<Release> {
    let client = http::client_builder(None)
        .timeout(TIMEOUT)
        .user_agent(format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;

    let resp = client.get(LATEST_API).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let v: Value = resp.json().await.ok()?;
    let tag = v.get("tag_name").and_then(Value::as_str)?.trim();
    if tag.is_empty() {
        return None;
    }

    let url = v
        .get("html_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| DOWNLOAD_PAGE.to_string());

    Some(Release {
        version: normalize_version(tag),
        url,
    })
}

/// 一个选项行：选中项前置 `▶ ` 且整行 accent。
fn option_line(label: &str, selected: bool) -> DockLine {
    if selected {
        vec![
            DockSpan::new("accent", "#8abeb7", DEF_SELECTED_MARK),
            DockSpan::new("accent", "#8abeb7", label.to_string()),
        ]
    } else {
        vec![DockSpan::plain(format!("  {label}"))]
    }
}

/// ↑↓ 环绕移动选中项。
fn move_selection(id: u64, delta: isize) {
    let mut st = lock();
    if st.overlay_id != Some(id) || st.notice.is_none() {
        return;
    }
    st.selected = (st.selected as isize + delta).rem_euclid(OPTIONS.len() as isize) as usize;
}

/// Enter：按选中项执行动作，然后关闭提示。
fn activate(id: u64) {
    let (selected, notice) = {
        let st = lock();
        if st.overlay_id != Some(id) {
            return;
        }
        (st.selected, st.notice.clone())
    };
    let Some(notice) = notice else {
        return;
    };

    match selected {
        OPTION_OPEN => {
            // 与 OAuth 面板的 ctrl+click 一致：同步打开，失败时提示手动复制
            if webbrowser::open(&notice.url).is_err() {
                notify_text(
                    &format!("Failed to open browser. Open manually: {}", notice.url),
                    UiNotifyLevel::Warning,
                );
            }
        }
        OPTION_SKIP => {
            if let Err(e) = settings_manager::write_skip_update_version(&notice.version) {
                notify_text(
                    &format!("failed to save skipUpdateVersion: {e}"),
                    UiNotifyLevel::Warning,
                );
            }
        }
        // Ignore：只关闭本次提示，不落盘
        _ => {}
    }

    clear(Some(id));
    request_ui(ExtensionUiRequest::HideOverlay { id });
}

/// 关闭提示并清状态；`Some(id)` 时只清理匹配 id（避免误清新提示）。
/// 返回是否确实清理了当前提示。
fn clear(id: Option<u64>) -> bool {
    let mut st = lock();
    if let Some(id) = id
        && st.overlay_id != Some(id)
    {
        return false;
    }

    st.overlay_id = None;
    st.notice = None;
    st.selected = 0;
    true
}

/// `/update` handler：`check` 异步检查并输出，`open` 直接打开最新 release 下载页。
///
/// `open` **不比较版本**：用户自己判断是否需要更新（仓库未公开 / 想手动看 release 时也有用）。
fn command_update(st: &mut App, raw: &str) -> bool {
    match command_arg(raw).trim() {
        "" => st.push_msg(
            "Usage: /update check (check for a newer release) | /update open (open the download page)"
                .to_string(),
            MsgLevel::Info,
        ),
        "check" => start_manual_check(),
        "open" => open_download_page(),
        other => st.push_msg(
            format!("Unknown /update subcommand: {other}. Use `check` or `open`."),
            MsgLevel::Warning,
        ),
    }
    false
}

/// `/update check`：后台拉取最新 release，结果经聊天区系统消息输出。
fn start_manual_check() {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        notify_text(
            "Failed to check for updates: no async runtime.",
            UiNotifyLevel::Warning,
        );
        return;
    };

    handle.spawn(async move {
        let current = env!("CARGO_PKG_VERSION");
        match fetch_latest().await {
            Some(release) if version_greater(&release.version, current) => notify_text(
                &format!(
                    "Update available: v{} (current v{})\nDownload: {}",
                    release.version, current, release.url
                ),
                UiNotifyLevel::Info,
            ),
            Some(_) => notify_text(
                &format!("{APP_NAME} is up to date (v{current})."),
                UiNotifyLevel::Info,
            ),
            None => notify_text(
                "Failed to check for updates. Use /update open to visit the download page.",
                UiNotifyLevel::Warning,
            ),
        }
    });
}

/// `/update open`：用浏览器打开最新 release 页面（不比较版本）。
fn open_download_page() {
    if webbrowser::open(DOWNLOAD_PAGE).is_err() {
        notify_text(
            &format!("Failed to open browser. Open manually: {DOWNLOAD_PAGE}"),
            UiNotifyLevel::Warning,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{AUTH_TEST_LOCK, AgentDirGuard};

    fn lock_global() -> MutexGuard<'static, ()> {
        AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 直接注入一个待提示版本（跳过网络），返回覆盖层 id。
    fn seed(version: &str) -> u64 {
        let id = next_ui_id();
        let mut st = lock();
        st.overlay_id = Some(id);
        st.notice = Some(Release {
            version: version.to_string(),
            url: "https://example.test/prux/releases/tag/v9.9.9".to_string(),
        });
        st.selected = 0;
        id
    }

    fn drain_ui() {
        while crate::core::extensions::take_pending_ui().is_some() {}
    }

    #[test]
    fn version_greater_semantic() {
        assert!(version_greater("v1.1.0", "v1.0.2"));
        assert!(version_greater("v1.1.0", "V1.0.2"));
        assert!(version_greater("v1.10.0", "v1.9.0"), "多位数段应逐段比较");
        assert!(version_greater("1.0.3", "v1.0.2"), "不带 v 前缀也应可比较");
        assert!(!version_greater("V1.0.2", "v1.0.2"), "同版本不算更新");
        assert!(!version_greater("v1.0.2", "v1.1.0"), "旧版本不算更新");
        assert!(!version_greater("v1.0", "v1.0.2"), "位数少的不算更新");
        assert!(!version_greater("garbage", "v1.0.2"), "解析失败不算更新");
        assert!(
            version_greater("v1.2.0-rc1", "v1.1.9"),
            "预发布后缀只取数字段"
        );
    }

    #[test]
    fn option_line_marks_selection() {
        let selected = option_line("Ignore", true);
        assert!(selected[0].text.starts_with("▶"));
        assert_eq!(selected[1].text, "Ignore");
        let plain = option_line("Ignore", false);
        assert_eq!(plain[0].text, "  Ignore");
    }

    #[test]
    fn overlay_view_renders_three_options_below_input() {
        let _g = lock_global();
        let id = seed("1.2.0");
        let ext = UpdateCheck;

        assert_eq!(ext.hooks(), vec![ExtensionHook::Overlay]);
        let view = ext.overlay_view(id, 80).expect("认领自己的 id");
        assert_eq!(view.lines.len(), OPTIONS.len());
        assert!(matches!(view.size, OverlaySize::Inline { .. }));
        assert_eq!(view.selected, Some(0));
        assert!(view.title.contains("v1.2.0"));
        assert!(view.title.contains(env!("CARGO_PKG_VERSION")));
        assert!(view.footer.iter().any(|(k, _)| k == "Enter"));

        assert!(ext.overlay_view(id + 1, 80).is_none(), "不认领其它 id");
        clear(None);
    }

    #[test]
    fn selection_wraps_and_ignore_only_closes() {
        let _g = lock_global();
        let id = seed("1.2.0");
        let ext = UpdateCheck;
        drain_ui();

        // 上移环绕到最后一个
        assert!(ext.on_overlay_event(
            id,
            &OverlayEvent::Key {
                key: OverlayKey::Up,
                input: None
            }
        ));
        assert_eq!(lock().selected, OPTIONS.len() - 1);
        // 下移环绕回开头
        ext.on_overlay_event(
            id,
            &OverlayEvent::Key {
                key: OverlayKey::Down,
                input: None,
            },
        );
        assert_eq!(lock().selected, 0);

        // Ignore（下标 1）：只关闭，不落盘
        {
            let mut st = lock();
            st.selected = 1;
        }
        assert!(ext.on_overlay_event(
            id,
            &OverlayEvent::Key {
                key: OverlayKey::Enter,
                input: None
            }
        ));
        assert_eq!(lock().overlay_id, None, "动作后应关闭");
        // 关闭请求已入队
        assert!(matches!(
            crate::core::extensions::take_pending_ui(),
            Some(ExtensionUiRequest::HideOverlay { id: rid }) if rid == id
        ));
    }

    #[test]
    fn dont_ask_again_persists_skipped_version() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        drain_ui();

        let id = seed("1.2.0");
        {
            let mut st = lock();
            st.selected = OPTION_SKIP;
        }
        UpdateCheck.on_overlay_event(
            id,
            &OverlayEvent::Key {
                key: OverlayKey::Enter,
                input: None,
            },
        );

        assert_eq!(
            settings_manager::read_skip_update_version().as_deref(),
            Some("1.2.0"),
            "「Don't ask again」应把版本写进 settings.json"
        );
        assert_eq!(lock().overlay_id, None);
    }

    #[test]
    fn closed_event_clears_state() {
        let _g = lock_global();
        let id = seed("1.2.0");
        assert!(UpdateCheck.on_overlay_event(id, &OverlayEvent::Closed));
        assert_eq!(lock().overlay_id, None);
        assert!(!UpdateCheck.on_overlay_event(id, &OverlayEvent::Closed));
    }

    #[test]
    fn disabling_clears_pending_notice() {
        let _g = lock_global();
        let _ = seed("1.2.0");
        UpdateCheck.on_enabled_changed(false);
        assert_eq!(lock().overlay_id, None);
        assert!(lock().notice.is_none());
        assert!(UpdateCheck.hooks().is_empty());
    }

    #[test]
    fn skipped_version_is_not_reprompted() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        settings_manager::write_skip_update_version("1.2.0").unwrap();
        assert_eq!(
            settings_manager::read_skip_update_version().as_deref(),
            Some("1.2.0")
        );
        // 更新的版本仍然会提示
        assert!(version_greater("1.3.0", "1.2.0"));
        assert_eq!(normalize_version("v1.2.0"), "1.2.0");
    }

    #[test]
    fn update_command_declares_check_and_open() {
        let cmds = UpdateCheck.commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "update");
        assert!(cmds[0].busy_safe, "handler 不锁 agent，应 busy_safe");
        let names: Vec<&str> = cmds[0].subcommands.iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["check", "open"]);
    }

    /// 注册后 `/update` 才作为可用命令出现（存在性由已启用扩展声明决定）。
    #[test]
    fn registering_extension_exposes_update_command() {
        use crate::core::extensions::{
            command_busy_safe, command_provider, register_extension, unregister_extension,
        };
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        settings_manager::write_disabled_extensions(&[]).ok();

        register_extension(UpdateCheck);
        assert_eq!(command_provider("update").as_deref(), Some(EXT));
        assert!(command_busy_safe("update"));
        assert!(unregister_extension(EXT));
    }

    #[test]
    fn update_without_or_unknown_subcommand_prints_usage() {
        let _g = lock_global();
        let mut st = App::new();
        assert!(!command_update(&mut st, "update"), "不应退出 TUI");
        assert!(!st.system_messages.is_empty(), "无参应提示用法");

        let mut st2 = App::new();
        assert!(!command_update(&mut st2, "update bogus"));
        assert!(!st2.system_messages.is_empty(), "未知子命令应提示");
    }
}
