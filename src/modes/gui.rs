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

use crate::{
    APP_NAME,
    cli::startup::Startup,
    core::{self, settings_manager},
    embedded_bytes, extensions,
    modes::interactive::agent_actor,
    utils::paths::{cwd, home_dir},
};
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBackgroundAppearance,
    WindowBounds, WindowOptions, px, size,
};
use std::{borrow::Cow, collections::HashSet, sync::Arc};
use theme::Theme;
use tokio::{runtime::Handle, sync::Mutex, sync::mpsc::unbounded_channel};

/// 扫描全部技能并算出给定过滤规则下的启用集合（`/skills` 面板用）。
///
/// 返回全部技能行与启用集合；两条路径都用
/// `skills::load_skills`，所以「勾选框说开着」和「实际加载到」必然一致。
/// 技能行：`(名称, 描述, 来源, 是否禁模型调用, SKILL.md 路径)`。
pub(crate) type SkillRow = (String, String, String, bool, String);

/// `/skills` 面板需要的数据：全部技能行 + 当前规则下启用的技能名。
pub(crate) type SkillsSnapshot = (Vec<SkillRow>, HashSet<String>);

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

    let theme = Theme::new(is_dark_theme(startup.theme_override.as_deref()));
    let (event_tx, event_rx) = unbounded_channel();
    let (cmd_tx, cmd_rx) = agent_actor::channels();

    rt.spawn(bridge::worker::run(
        Arc::new(Mutex::new(startup.agent)),
        cmd_rx,
        event_tx,
    ));

    let initial_messages = startup.initial_messages;

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

        let view = cx.new(|cx| app::GuiApp::new(theme, cmd_tx, event_rx, initial_messages, cx));

        _ = cx.open_window(options, move |window, cx| {
            // Wayland 后端不会用 `TitlebarOptions::title` 设标题，得显式设一次
            window.set_window_title(APP_NAME);
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

/// 扫描技能并返回快照（`rules` 是未落盘的草稿规则，启用态按它实算）。
pub(crate) fn skills_snapshot(rules: &[String]) -> SkillsSnapshot {
    let home_dir = home_dir();
    let cwd = cwd().to_string_lossy().to_string();
    let agent_dir = settings_manager::agent_dir();
    let bundled = extensions::skills::bundled_skill_names();

    let load = |filter: &[String]| {
        core::skills::load_skills(
            &cwd,
            &agent_dir,
            &home_dir,
            &[],
            &bundled,
            true,
            true,
            filter,
        )
    };

    let all = load(&[]);
    let enabled: HashSet<String> = load(rules).into_iter().map(|skill| skill.name).collect();
    let rows = all
        .into_iter()
        .map(|skill| {
            (
                skill.name,
                skill.description,
                skill.source,
                skill.disable_model_invocation,
                skill.path.to_string_lossy().to_string(),
            )
        })
        .collect();
    (rows, enabled)
}

/// `/skills` 详情页（Tab）的行：启用态 + SKILL.md 头信息 + 来源 + 路径。
///
/// 技能不存在时返回空（调用方据此不打开详情页）。
pub(crate) fn skill_detail_lines(rules: &[String], name: &str) -> Vec<String> {
    let (all, enabled) = skills_snapshot(rules);
    let Some(row) = all.iter().find(|row| row.0 == name) else {
        return Vec::new();
    };
    vec![
        format!("  name: {name}"),
        format!("  enabled: {}", enabled.contains(name)),
        format!("  disable-model-invocation: {}", row.3),
        format!("  source: {}", row.2),
        format!("  path: {}", row.4),
        String::new(),
        format!("  description: {}", row.1),
    ]
}
