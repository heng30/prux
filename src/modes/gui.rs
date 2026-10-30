//! GUI 运行模式（gpui）：与 TUI 平行的桌面界面。
//!
//! 依赖方向同 `interactive`：只向下依赖 `core` / `cli`，不反向依赖。
//! 与 agent 的一切接触都走 [`bridge`]：命令经 `AgentCommand` 下发，
//! 状态经 agent 的 JSON 事件流上行，因此 `core` 与 TUI 侧都不需要为 GUI 改动。

mod app;
mod bridge;
mod code;
mod components;
mod composer;
mod giff;
mod markdown;
mod overlay;
mod sidebar;
mod suggest;
mod text_edit;
mod theme;
mod transcript;

use crate::{
    APP_NAME, cli::startup::Startup, core, core::settings_manager, embedded_bytes,
    modes::interactive::agent_actor, utils::async_task,
};
use app::GuiApp;
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBackgroundAppearance,
    WindowBounds, WindowOptions, px, size,
};
use std::{borrow::Cow, sync::Arc};
use theme::Theme;
use tokio::{runtime::Handle, sync::Mutex, sync::mpsc::unbounded_channel};

/// 默认窗口尺寸（逻辑像素）。
const DEFAULT_WINDOW_SIZE: (f32, f32) = (1180.0, 780.0);

/// 窗口最小尺寸（逻辑像素）：再小侧栏与输入框会挤在一起。
const MIN_WINDOW_SIZE: (f32, f32) = (760.0, 560.0);

/// 内嵌字体：思源黑体 CN（默认界面字体，随二进制发布，不依赖系统装字体）。
const UI_FONT_BYTES: &[u8] = embedded_bytes!("fonts/SourceHanSansCN.otf");

/// GUI 入口。**必须由主线程调用**（gpui 独占主线程事件循环）。
///
/// `rt` 是调用方持有的 tokio runtime：agent worker 跑在它上面，
/// 调用方必须在 `run` 返回前保持 runtime 存活。
pub fn run(startup: Startup, rt: &Handle) {
    theme::set_ui_scale(settings_manager::read_settings_ui_scale());

    // 视图线程没有 tokio 运行时：登记它，扩展的一次性异步任务（`/btw`、`/recap` 等）才能跑起来
    async_task::install(rt.clone());

    // GUI 自己记住的明暗优先；没存过才用 --theme / settings.theme，最后回落暗色
    let gui_theme = settings_manager::read_settings_theme_gui();
    let theme = Theme::new(Theme::is_dark_theme(
        gui_theme.as_deref().or(startup.theme_override.as_deref()),
    ));
    let (event_tx, event_rx) = unbounded_channel();
    let (cmd_tx, cmd_rx) = agent_actor::channels();

    // 运行中 steer 注入箱与 agent 共享：忙碌时提交的消息由 run_loop 在回合内注入
    let steer_inbox = startup.agent.runtime_steer_inbox.clone();

    rt.spawn(bridge::worker::run(
        Arc::new(Mutex::new(startup.agent)),
        cmd_rx,
        event_tx,
    ));

    let initial_messages = startup.initial_messages;

    // `-a/--approve` / `--no-approve`：显式给了信任决定就不再弹启动信任面板
    let trust_override = startup.reload_ctx.trust_override;

    // 扩展的 UI 请求（`ask_prompt` / `ShowOverlay`…）只在 ui_ready 为真时才发出
    core::extensions::set_ui_ready(true);

    Application::new().run(move |cx: &mut App| {
        _ = cx
            .text_system()
            .add_fonts(vec![Cow::Borrowed(UI_FONT_BYTES)]);

        let bounds = Bounds::centered(
            None,
            size(px(DEFAULT_WINDOW_SIZE.0), px(DEFAULT_WINDOW_SIZE.1)),
            cx,
        );
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(APP_NAME.into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(MIN_WINDOW_SIZE.0), px(MIN_WINDOW_SIZE.1))),
            window_background: WindowBackgroundAppearance::Opaque,
            app_id: Some(APP_NAME.to_owned()),
            ..Default::default()
        };

        let view = cx.new(|cx| {
            GuiApp::new(
                theme,
                cmd_tx,
                event_rx,
                initial_messages,
                steer_inbox,
                trust_override,
                cx,
            )
        });

        _ = cx.open_window(options, move |window, cx| {
            // Wayland 后端不会用 `TitlebarOptions::title` 设标题，得显式设一次
            window.set_window_title(APP_NAME);
            let focus = view.read(cx).focus_handle(cx);
            window.focus(&focus);
            view
        });
    });

    core::extensions::set_ui_ready(false);
}
