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
mod markdown;
mod overlay;
mod panel;
mod sidebar;
mod suggest;
mod text_edit;
mod theme;
mod transcript;

use crate::APP_NAME;
use crate::cli::startup::Startup;
use crate::core::settings_manager;
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBackgroundAppearance,
    WindowBounds, WindowOptions, px, size,
};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tokio::sync::mpsc::unbounded_channel;

/// 默认窗口尺寸（逻辑像素）。
const DEFAULT_WINDOW_SIZE: (f32, f32) = (1180.0, 780.0);
/// 窗口最小尺寸（逻辑像素）：再小侧栏与输入框会挤在一起。
const MIN_WINDOW_SIZE: (f32, f32) = (760.0, 560.0);

/// GUI 入口。**必须由主线程调用**（gpui 独占主线程事件循环）。
///
/// `rt` 是调用方持有的 tokio runtime：agent worker 跑在它上面，
/// 调用方必须在 `run` 返回前保持 runtime 存活。
pub fn run(startup: Startup, rt: &Handle) {
    // 界面缩放先落地：首帧就按用户上次选的档位渲染，不然先小后大，闪一下
    theme::set_ui_scale(settings_manager::read_settings_ui_scale());
    let theme = theme::Theme::new(is_dark_theme(startup.theme_override.as_deref()));

    let (event_tx, event_rx) = unbounded_channel();
    let (cmd_tx, cmd_rx) = crate::modes::interactive::agent_actor::channels();
    rt.spawn(bridge::worker::run(
        Arc::new(Mutex::new(startup.agent)),
        cmd_rx,
        event_tx,
    ));

    let initial_messages = startup.initial_messages;

    Application::new().run(move |cx: &mut App| {
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

        let view = cx.new(|cx| app::GuiApp::new(theme, cmd_tx, event_rx, initial_messages, cx));

        _ = cx.open_window(options, move |window, cx| {
            let focus = view.read(cx).focus_handle(cx);
            window.focus(&focus);
            view
        });
    });
}

/// GUI 只有明暗两套令牌：`--theme light`（或 settings 里的亮色主题名）走亮色，其余暗色。
fn is_dark_theme(name: Option<&str>) -> bool {
    match name {
        Some(name) => !name.to_lowercase().contains("light"),
        None => true,
    }
}
