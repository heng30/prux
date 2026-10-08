//! `downloader` 扩展：`/download` 手动安装 fd / rg，并提供 dock 工具段。
//!
//! - **默认启用**（工具可用性属于开箱能力）：禁用后命令与 dock 段一起消失；
//!   启动时的缺失提示不受影响（那条路径在 [`crate::core::tools_manager`]，只提示不下载）。
//! - `/download`（无参数）= 切换本扩展 dock 段显隐（对齐 plan-mode `/todos` /
//!   proxy `/proxy`）：段**默认隐藏**（fd/rg 是后台依赖，不该因为别的扩展开了面板就
//!   顺带出现）；面板未开时显示本段并打开面板，面板已开时切换本段显隐。
//!   不动 `/dock` 的全局面板开关——隐藏本段后面板若无其它段，渲染时自动收起。
//!   触发下载（`/download fd|rg`）会显示本段，让进度可见。
//! - `/download fd|rg`：缺失 → 直接下载；已存在 → 弹选择面板（重装 / 取消）；
//!   已在下载中 → 只提示；未知工具 / 多余参数 → 报错。状态来自 `tools_manager` 快照。
//! - handler 不锁 agent（只起后台下载 + 动 dock 段显隐），故 `busy_safe`。

use crate::{
    core::{
        extensions::{
            self, DockLine, DockSpan, Extension, ExtensionCommand, ExtensionHook, ExtensionMode,
            ExtensionTool, ExtensionUiRequest, SelectOption, SubcommandDef, UiNotifyLevel,
            next_ui_id,
        },
        runtime,
        tools_manager::{self, ToolSnapshot, ToolSource, ToolState, ToolStatus},
    },
    extensions::{
        ACCENT, DIM, ERROR, EXTENSION_FACTORIES, ExtensionFactory, MUTED, PRIORITY_DOWNLOADER,
        SUCCESS, command_arg,
        util::{choice_key, download_spinner},
    },
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
};
use std::sync::{Arc, Mutex, OnceLock};

/// 扩展名
const EXT: &str = "downloader";

/// 命令名
const CMD: &str = "download";

/// 子命令候选（`/download ` 后的二级候选）。
///
/// [`SubcommandDef::name`] 只能是 `'static` 字面量，无法从
/// [`tools_manager::tool_names`] 生成，故在此硬编码，并由
/// [`tests::subcommands_match_tool_roster`] 钉住两边一致。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "fd",
        description: "Download/install fd (backing the find tool)",
    },
    SubcommandDef {
        name: "rg",
        description: "Download/install ripgrep (backing the grep tool)",
    },
];

/// 自声明工厂：linkme 分布式切片
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static DOWNLOADER_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_DOWNLOADER,
    make: || -> Arc<dyn Extension> { Arc::new(Downloader) },
};

/// 停靠段可见性（内存态，不持久化）。默认隐藏：fd/rg 是后台依赖，
/// 别的扩展打开停靠面板时不应顺带把它们的状态带出来；`/download` 无参切换本段显隐，
/// 触发下载时也会显示本段。
fn dock_shown_state() -> &'static Mutex<bool> {
    /// 本扩展 dock 段可见性的进程级内存态，默认隐藏且不持久化。
    static S: OnceLock<Mutex<bool>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(false))
}

/// 本扩展 dock 段当前是否可见（`hooks()` / `dock_lines()` / `wants_redraw()` 共用）
fn is_dock_shown() -> bool {
    *dock_shown_state().lock().unwrap_or_else(|e| e.into_inner())
}

/// 设置本扩展 dock 段可见性（进程级内存态，不持久化）。
fn set_dock_shown(shown: bool) {
    *dock_shown_state().lock().unwrap_or_else(|e| e.into_inner()) = shown;
}

/// 待确认的选择面板：`(请求 id, 工具名)`。
///
/// 选择面板回传只有 `(id, 选项串)`，没有上下文，故按 id 记住要装哪个工具。
fn pending() -> &'static Mutex<Option<(u64, String)>> {
    /// 待确认重装的工具（请求 id → 工具名），供选择面板回传时按 id 认领。
    static P: OnceLock<Mutex<Option<(u64, String)>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// `downloader` 扩展（自身无状态：工具状态住在 [`tools_manager`]）
#[derive(Default)]
pub struct Downloader;

impl Extension for Downloader {
    /// 扩展名 `downloader`。
    fn name(&self) -> &str {
        EXT
    }

    /// 一句话说明：`/download` 安装 fd/rg，裸命令切换 dock 段显隐。
    fn description(&self) -> &str {
        "Install the external tools prux uses (fd, rg) with /download; bare /download \
         toggles a dock section (hidden by default) showing their current state"
    }

    /// 仅在 Dev / Creator 模式下提供。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 段隐藏时退订 Dock（框架不再每帧空转采集）
    fn hooks(&self) -> Vec<ExtensionHook> {
        if is_dock_shown() {
            vec![ExtensionHook::Dock]
        } else {
            Vec::new()
        }
    }

    /// 不提供模型工具（下载只由用户经 `/download` 触发）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/download` 命令及其 `fd` / `rg` 子命令候选，并标记 `busy_safe`。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: format!(
                "/{CMD} [{}] downloads a tool; bare /{CMD} toggles the tools dock section",
                tools_manager::tool_names().join("|")
            ),
            busy_safe: true, // handler 只起后台下载 + 动 dock 段显隐，不锁 agent
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 工具段：标题行 + 每个工具一行（每帧采集，只读内存快照）；段隐藏时返回空。
    fn dock_lines(&self) -> Vec<DockLine> {
        if !is_dock_shown() {
            return Vec::new();
        }

        let spinner = download_spinner();
        tools_manager::with_snapshots(|snaps| dock_lines_for(snaps, spinner))
    }

    /// 段可见且有工具在下载 → 请求每帧重绘（spinner 才会转）。
    fn wants_redraw(&self) -> bool {
        is_dock_shown() && tools_manager::any_downloading()
    }

    /// 注册 `/download` 的 handler 到斜杠命令表。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_download);
    }

    /// 确认面板回传：按 id 认领待重装的工具，选 `download` 才起下载。
    ///
    /// 返回 `None`（必须如此，返回文本会被当成 follow-up prompt）；面板被 Esc 关闭时静默取消。
    fn on_ui_choice(&self, id: u64, choice: Option<String>) -> Option<String> {
        let tool = take_pending(id)?;
        let key = choice.unwrap_or_default(); // Esc 关闭面板 → choice 为 None → 静默取消

        if choice_key(&key) == "download" {
            spawn_download(&tool);
        }

        None // 必须返回 None：返回文本会被当作 follow-up prompt 起新回合
    }
}

/// `/download` 的参数 → 动作。
///
/// 纯函数（快照经闭包注入）：命令 handler 只做「执行动作」，全部分支都能离线测试。
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// 无参数：切换本扩展 dock 段显隐
    ToggleDock,
    /// 参数不合法（消息可直接展示）
    Reject(String),
    /// 离线：拒绝下载（工具名）
    Offline(String),
    /// 已在下载中：只提示（工具名）
    Busy(String),
    /// 工具已存在：弹确认面板（工具名）
    Confirm(String),
    /// 直接下载（工具名）
    Download(String),
}

/// 把 `/download` 的原始参数解析成 [`Action`]：空 → 切换 dock 段，
/// 单个已知工具名按离线 / 下载中 / 已存在 / 缺失分派，其余报用法或未知工具。
fn plan_action(
    arg: &str,
    offline: bool,
    snapshot: impl Fn(&str) -> Option<ToolSnapshot>,
) -> Action {
    let arg = arg.trim();
    if arg.is_empty() {
        return Action::ToggleDock;
    }

    // 只接受单个工具名：多余参数不猜（`/download fd rg` 报用法）
    let mut parts = arg.split_whitespace();
    let (Some(name), None) = (parts.next(), parts.next()) else {
        return Action::Reject(usage_message(arg));
    };

    let tool = name.to_ascii_lowercase();
    if !tools_manager::is_known_tool(&tool) {
        return Action::Reject(usage_message(name));
    }

    if offline {
        return Action::Offline(tool);
    }

    match snapshot(&tool) {
        Some(snap) if snap.state == ToolState::Downloading => Action::Busy(tool),
        // 「已存在」看实际是否可用（而非只看 state：重装失败后旧副本可能仍在）
        Some(snap) if snap.path.is_some() => Action::Confirm(tool),
        _ => Action::Download(tool),
    }
}

/// 参数不合法的提示：单个未知名字给可用清单，多个 token 给用法
fn usage_message(arg: &str) -> String {
    if arg.split_whitespace().count() > 1 {
        format!("Usage: /{CMD} [{}]", tools_manager::tool_names().join("|"))
    } else {
        tools_manager::unknown_tool_message(arg)
    }
}

/// `/download`：无参数切换本扩展 dock 段显隐；`fd` / `rg` 下载或确认重装。
fn command_download(st: &mut App, raw: &str) -> bool {
    let action = plan_action(
        command_arg(raw),
        runtime::offline(),
        tools_manager::snapshot,
    );

    match action {
        Action::ToggleDock => toggle_dock_section(st),
        Action::Reject(message) => st.push_msg(message, MsgLevel::Error),
        Action::Offline(tool) => {
            st.push_msg(
                tools_manager::offline_skip_message(&tool),
                MsgLevel::Warning,
            );
        }
        Action::Busy(tool) => st.push_msg(
            format!(
                "{} is already downloading",
                tools_manager::display_name(&tool)
            ),
            MsgLevel::Info,
        ),
        Action::Confirm(tool) => {
            // `plan_action` 判定「已存在」就必然有快照；取不到只可能是并发刷新，放弃即可
            if let Some(snap) = tools_manager::snapshot(&tool) {
                open_confirm_panel(&tool, &snap);
            }
        }
        Action::Download(tool) => spawn_download(&tool),
    }
    false
}

/// 切换本扩展 dock 段的显隐（对齐 plan-mode `/todos` / proxy `/proxy`）：
/// 面板未开 → 显示本段并打开面板；面板已开 → 切换本段显隐。
/// 面板开关本身归 `/dock`；隐藏本段后若无其它段，下一帧渲染自动收起面板。
fn toggle_dock_section(st: &mut App) {
    if !st.dock_visible {
        set_dock_shown(true);
        st.dock_visible = true;
        st.dock_offset = 0;
        st.dirty = true;
        return;
    }

    set_dock_shown(!is_dock_shown());
    st.dirty = true;
}

/// 工具已存在 → 弹确认面板（并记下 pending，供 [`Extension::on_ui_choice`] 认领）
fn open_confirm_panel(tool: &str, snap: &ToolSnapshot) {
    let id = next_ui_id();
    *pending().lock().unwrap_or_else(|e| e.into_inner()) = Some((id, tool.to_string()));
    extensions::request_ui(confirm_panel_request(tool, snap, id));
}

/// 组装确认面板请求。
/// 选项串按扩展约定 `"<key>  <说明>"`（回传后取首个 token 当 key，见 [`choice_key`]）。
fn confirm_panel_request(tool: &str, snap: &ToolSnapshot, id: u64) -> ExtensionUiRequest {
    ExtensionUiRequest::Select {
        id,
        title: format!(
            "{} already installed — re-download?",
            tools_manager::display_name(tool)
        ),
        options: vec![
            SelectOption::new(format!("download  {}", confirm_action_label(tool, snap))),
            SelectOption::new("cancel  Cancel"),
        ],
    }
}

/// 「重装」那一行的说明：按现有副本来源区分（覆盖本地副本 / 装一份本地的压过 PATH）
fn confirm_action_label(tool: &str, snap: &ToolSnapshot) -> String {
    let local = tools_manager::tool_local_path(tool);
    match (snap.source, snap.path.as_deref()) {
        (Some(ToolSource::Path), Some(path)) => {
            format!(
                "Install to {} (shadows {})",
                local.display(),
                path.display()
            )
        }
        _ => format!("Re-download (overwrite {})", local.display()),
    }
}

/// 取走并校验待确认请求（id 不匹配 = 不是本扩展的面板）
fn take_pending(id: u64) -> Option<String> {
    let mut guard = pending().lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some((pending_id, tool)) if *pending_id == id => {
            let tool = tool.clone();
            *guard = None;
            Some(tool)
        }
        _ => None,
    }
}

/// 起一次后台下载：进度经 `tools_manager` 快照进 dock，结果经 `Notify` 进聊天区。
/// 这是唯一的 fd/rg 下载入口（启动路径只探测提示，不自动下载）。
fn spawn_download(tool: &str) {
    let tool = tool.to_string();
    set_dock_shown(true);
    extensions::request_show_dock();

    tokio::spawn(async move {
        tools_manager::download_now(&tool, &|status: ToolStatus| {
            let level = if status.is_warning() {
                UiNotifyLevel::Warning
            } else {
                UiNotifyLevel::Info
            };

            extensions::request_ui(ExtensionUiRequest::Notify {
                text: status.message().to_string(),
                level,
            });
        })
        .await;
    });
}

/// 构建工具段：标题行 + 每个工具一行（行数 = 工具数 + 1，恒非空）
fn dock_lines_for(snaps: &[ToolSnapshot], spinner: &str) -> Vec<DockLine> {
    let mut lines: Vec<DockLine> = Vec::with_capacity(snaps.len() + 1);
    lines.push(vec![DockSpan::plain("Downloader")]);

    for snap in snaps {
        lines.push(tool_line(snap, spinner));
    }

    lines
}

/// 一行工具：`  ● fd      10.2.0  ~/.prux/bin/fd`
fn tool_line(snap: &ToolSnapshot, spinner: &str) -> DockLine {
    let (mark, style) = match snap.state {
        ToolState::Downloading => (spinner.to_string(), ACCENT),
        ToolState::Present => ("●".to_string(), SUCCESS),
        ToolState::Failed => ("✗".to_string(), ERROR),
        ToolState::Missing => ("○".to_string(), DIM),
    };
    let detail = match snap.state {
        ToolState::Downloading => "downloading…".to_string(),
        ToolState::Failed => format!(
            "failed: {}",
            first_line(snap.last_error.as_deref().unwrap_or("unknown error"))
        ),
        ToolState::Missing => format!("/{CMD} {}", snap.tool),
        ToolState::Present => snap
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    };

    let mut spans = vec![
        DockSpan::plain("  "),
        DockSpan::new(style.0, style.1, format!("{mark} ")),
        DockSpan::new("text", "", format!("{:<8}", snap.tool)),
        DockSpan::new(
            "muted",
            MUTED.1,
            format!("{:<8}", snap.version.as_deref().unwrap_or("—")),
        ),
        DockSpan::new("dim", DIM.1, detail),
    ];
    // PATH 里的系统副本：标明来源，避免与 `~/.prux/bin` 的本地安装混淆
    if snap.state == ToolState::Present && snap.source == Some(ToolSource::Path) {
        spans.push(DockSpan::new("muted", MUTED.1, " (system)"));
    }
    spans
}

/// 错误首行（完整错误另有一条 Warning 进聊天区）
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(
        tool: &str,
        state: ToolState,
        path: Option<&str>,
        source: Option<ToolSource>,
        version: Option<&str>,
        error: Option<&str>,
    ) -> ToolSnapshot {
        ToolSnapshot {
            tool: tool.to_string(),
            state,
            path: path.map(std::path::PathBuf::from),
            source,
            version: version.map(str::to_string),
            last_error: error.map(str::to_string),
        }
    }

    fn present_fd() -> ToolSnapshot {
        snap(
            "fd",
            ToolState::Present,
            Some("/home/u/.prux/bin/fd"),
            Some(ToolSource::AgentBin),
            Some("10.2.0"),
            None,
        )
    }

    /// 一行 DockSpan 的纯文本（断言行内容用）
    fn text(line: &DockLine) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    fn none(_: &str) -> Option<ToolSnapshot> {
        None
    }

    #[test]
    fn subcommands_match_tool_roster() {
        let declared: Vec<&str> = SUBCOMMANDS.iter().map(|s| s.name).collect();
        assert_eq!(
            declared,
            tools_manager::tool_names(),
            "子命令候选必须与 tools_manager 的工具清单一致"
        );
    }

    #[test]
    fn plan_action_covers_every_branch() {
        assert_eq!(plan_action("", false, none), Action::ToggleDock);
        assert_eq!(plan_action("   ", false, none), Action::ToggleDock);

        assert_eq!(
            plan_action("fd", false, none),
            Action::Download("fd".into())
        );
        assert_eq!(
            plan_action("FD", false, none),
            Action::Download("fd".into())
        );
        assert_eq!(
            plan_action("  rg  ", false, none),
            Action::Download("rg".into())
        );

        assert_eq!(plan_action("fd", true, none), Action::Offline("fd".into()));

        // 已存在（本地安装）→ 确认面板
        assert_eq!(
            plan_action("fd", false, |_| Some(present_fd())),
            Action::Confirm("fd".into())
        );

        // 重装失败但旧副本仍在（PATH）→ 仍按「已存在」弹面板
        let failed_present = |_: &str| {
            Some(snap(
                "fd",
                ToolState::Failed,
                Some("/usr/bin/fd"),
                Some(ToolSource::Path),
                Some("10.2.0"),
                Some("boom"),
            ))
        };
        assert_eq!(
            plan_action("fd", false, failed_present),
            Action::Confirm("fd".into())
        );

        // 下载中 → 只提示，不弹面板、不重下
        let downloading =
            |_: &str| Some(snap("fd", ToolState::Downloading, None, None, None, None));
        assert_eq!(
            plan_action("fd", false, downloading),
            Action::Busy("fd".into())
        );

        // 失败且无副本 → 直接重下
        let failed = |_: &str| {
            Some(snap(
                "fd",
                ToolState::Failed,
                None,
                None,
                None,
                Some("boom"),
            ))
        };
        assert_eq!(
            plan_action("fd", false, failed),
            Action::Download("fd".into())
        );
    }

    #[test]
    fn reject_messages_name_the_tool_or_the_usage() {
        assert_eq!(
            plan_action("git", false, none),
            Action::Reject("Unknown tool \"git\". Available: fd, rg".to_string())
        );
        assert_eq!(
            plan_action("fd rg", false, none),
            Action::Reject("Usage: /download [fd|rg]".to_string())
        );
    }

    #[test]
    fn confirm_label_splits_by_source() {
        let local = present_fd();
        let label = confirm_action_label("fd", &local);
        assert!(label.starts_with("Re-download (overwrite "), "{label}");
        assert!(label.ends_with("/fd)"), "{label}");

        let system = snap(
            "rg",
            ToolState::Present,
            Some("/usr/bin/rg"),
            Some(ToolSource::Path),
            Some("14.1.1"),
            None,
        );
        let label = confirm_action_label("rg", &system);
        assert!(label.starts_with("Install to "), "{label}");
        assert!(label.ends_with("(shadows /usr/bin/rg)"), "{label}");
    }

    #[test]
    fn choice_key_takes_first_token() {
        assert_eq!(
            choice_key("download  Re-download (overwrite /x)"),
            "download"
        );
        assert_eq!(choice_key("cancel  Cancel"), "cancel");
        assert_eq!(choice_key(""), "");
    }

    /// 确认面板：标题 + 两条选项（选项串约定 `<key>  <说明>`）
    #[test]
    fn confirm_panel_offers_download_and_cancel() {
        match confirm_panel_request("fd", &present_fd(), 7) {
            ExtensionUiRequest::Select { id, title, options } => {
                let options: Vec<String> = options.into_iter().map(|o| o.text).collect();
                assert_eq!(id, 7);
                assert_eq!(title, "fd already installed — re-download?");
                assert_eq!(options.len(), 2, "{options:?}");
                assert_eq!(choice_key(&options[0]), "download");
                assert!(
                    options[0].contains("Re-download (overwrite "),
                    "{}",
                    options[0]
                );
                assert_eq!(options[1], "cancel  Cancel");
            }
            other => panic!("应为选择面板：{other:?}"),
        }
    }

    /// 裸 /download 切换本扩展 dock 段的显隐（不控制整个面板，对齐 plan-mode /todos）；
    /// 段默认隐藏，故别的扩展开面板时不会顺带带出 fd/rg 状态。
    #[test]
    fn download_without_args_toggles_own_dock_section() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_dock_shown(false); // 默认态；其它测试可能改过进程级共享状态
        let mut st = App::new();
        assert!(!st.dock_visible);
        assert!(!is_dock_shown(), "段默认隐藏（面板因别的扩展打开也不出现）");

        // 面板未开 → 显示本段并打开面板（不把已可见的段关上）
        assert!(!command_download(&mut st, "/download"));
        assert!(st.dock_visible, "裸 /download 应打开面板");
        assert!(is_dock_shown(), "段应变为可见");
        assert!(st.dirty);

        // 面板已开 → 只隐藏本段，面板开关不动
        st.dirty = false;
        command_download(&mut st, "/download");
        assert!(st.dock_visible, "隐藏本段不应关闭面板开关");
        assert!(!is_dock_shown(), "第二次裸 /download 应隐藏本段");
        assert!(st.dirty);

        // 再切一次 → 重新显示本段
        command_download(&mut st, "/download");
        assert!(st.dock_visible);
        assert!(is_dock_shown(), "第三次应重新显示本段");

        set_dock_shown(false); // 复位进程级共享状态
    }

    /// 段隐藏时 hooks 退订 Dock、dock_lines 返回空（面板不再为本段占位）
    #[test]
    fn hidden_section_stops_providing_dock_content() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_dock_shown(false);
        assert!(Downloader.hooks().is_empty(), "默认隐藏 → 不订阅 Dock");
        assert!(Downloader.dock_lines().is_empty(), "默认不提供内容");

        set_dock_shown(true);
        assert!(Downloader.hooks().contains(&ExtensionHook::Dock));
        assert!(!Downloader.dock_lines().is_empty(), "可见时恒有标题行");

        set_dock_shown(false);
        assert!(Downloader.hooks().is_empty(), "隐藏后退订 Dock");
        assert!(Downloader.dock_lines().is_empty(), "隐藏时不提供内容");

        set_dock_shown(false);
    }

    /// 参数不合法：只报错，不弹面板、不动 dock 开关
    #[test]
    fn invalid_args_report_to_chat_without_touching_the_panel() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut st = App::new();

        command_download(&mut st, "/download git");
        command_download(&mut st, "/download fd rg");

        let msgs: Vec<(MsgLevel, String)> = st
            .system_messages
            .iter()
            .map(|(_, m)| (m.level, crate::modes::interactive::app::sys_msg_text(m)))
            .collect();
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert_eq!(msgs[0].0, MsgLevel::Error);
        assert_eq!(msgs[0].1, "Unknown tool \"git\". Available: fd, rg");
        assert_eq!(msgs[1].1, "Usage: /download [fd|rg]");
        assert!(!st.dock_visible, "报错不应打开面板");
    }

    /// 命令声明接入注册表：默认启用（工具可用性属于开箱能力）→ `/download` 立刻可分发
    /// 且 busy_safe，禁用后命令与 dock 段一起消失。
    /// （注册表是进程级全局态，持 AUTH_TEST_LOCK 与其它碰注册表的测试串行。）
    #[test]
    fn command_surfaces_only_when_enabled() {
        use crate::core::extensions::{
            ExtensionMode, command_busy_safe, command_provider, is_extension_enabled,
            register_extension, registered_commands, set_extension_enabled, set_extension_mode,
            unregister_extension,
        };

        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        // 清掉同进程里其它测试可能残留的注册，保证本测试自带干净起点
        unregister_extension(EXT);
        register_extension(Downloader);

        // 默认启用（命令可分发；dock 段默认隐藏，靠 /download 显示）：
        // 命令立刻可分发，且声明了 busy_safe
        // （handler 不锁 agent）
        assert!(is_extension_enabled(EXT), "downloader 应默认启用");
        assert_eq!(command_provider(CMD).as_deref(), Some(EXT));
        assert!(command_busy_safe(CMD));
        let declared = registered_commands()
            .into_iter()
            .find(|c| c.name == CMD)
            .expect("命令应出现在候选列表");
        assert!(declared.busy_safe);
        assert_eq!(
            declared
                .subcommands
                .iter()
                .map(|s| s.name)
                .collect::<Vec<_>>(),
            tools_manager::tool_names()
        );

        // 禁用后命令消失（dock 段与命令一起下线）
        set_extension_enabled(EXT, false);
        assert!(command_provider(CMD).is_none());
        assert!(
            !registered_commands().iter().any(|c| c.name == CMD),
            "禁用后不应再出现候选"
        );

        assert!(unregister_extension(EXT));
    }

    #[test]
    fn dock_lines_render_every_state() {
        let snaps = vec![
            present_fd(),
            snap(
                "rg",
                ToolState::Present,
                Some("/usr/bin/rg"),
                Some(ToolSource::Path),
                Some("14.1.1"),
                None,
            ),
            snap("miss", ToolState::Missing, None, None, None, None),
            snap("busy", ToolState::Downloading, None, None, None, None),
            snap(
                "bad",
                ToolState::Failed,
                None,
                None,
                Some("10.2.0"),
                Some("network unreachable\nsecond line"),
            ),
        ];
        let lines = dock_lines_for(&snaps, "*");
        assert_eq!(lines.len(), 6, "标题行 + 每工具一行");
        assert_eq!(text(&lines[0]), "Downloader");
        assert_eq!(text(&lines[1]), "  ● fd      10.2.0  /home/u/.prux/bin/fd");
        assert_eq!(text(&lines[2]), "  ● rg      14.1.1  /usr/bin/rg (system)");
        assert_eq!(text(&lines[3]), "  ○ miss    —       /download miss");
        assert_eq!(text(&lines[4]), "  * busy    —       downloading…");
        assert_eq!(
            text(&lines[5]),
            "  ✗ bad     10.2.0  failed: network unreachable"
        );
    }
}
