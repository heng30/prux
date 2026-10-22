//! `computer` 扩展：操作真实的桌面（截图、鼠标键盘、窗口、无障碍控件）。
//!
//! 设计要点：
//!
//! - **默认关闭**，用 `/computer on` 打开、`/computer off` 关掉；关闭时这一族工具的 schema
//!   不从上下文中发出（`filter_extension_tools` 返回空），开启后下一轮才出现在工具表里。
//!   开关只在进程内有效：重启回到关闭。
//! - **平台后端**在 `backend/`（截图/窗口/输入/启动），**无障碍通道**在 `a11y/`
//!   （Linux 用 AT-SPI2，Windows 用 UI Automation，macOS 用 AXUIElement）。
//! - 每次截图登记一个**帧**（`frame`）：模型可以按图内像素给坐标，由这里投影回屏幕坐标；
//!   帧对应的窗口移动过就拒绝，而不是照着旧图乱点。
//!
//! Linux 后端只支持 **Wayland**：截图优先 `grim`（缺省时用 `xcap`），窗口列表来自
//! `swaymsg`（Wayland 没有平台中立的窗口枚举），输入走 `enigo` 的虚拟键盘/指针协议。

pub mod a11y;
pub mod backend;
pub mod input;
pub mod ops;
pub mod schema;
pub mod state;

use crate::{
    core::{
        extensions::{
            Extension, ExtensionCommand, ExtensionTool, ForkProjectInfo, SubcommandDef, ToolExecCtx,
        },
        tools::{ToolError, ToolResult},
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_COMPUTER},
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::{Arc, OnceLock};

/// 扩展名（`/extension` 面板与诊断里用）。
const EXT: &str = "computer";

/// 斜杠命令名（不含 `/`）。
const CMD: &str = "computer";

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static COMPUTER_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_COMPUTER,
    make: || -> Arc<dyn Extension> { Arc::new(Computer) },
};

/// `computer` 扩展主体。
pub struct Computer;

impl Extension for Computer {
    /// 返回固定扩展名 `computer`。
    fn name(&self) -> &str {
        EXT
    }

    /// `/extension` 面板的一句话描述。
    fn description(&self) -> &str {
        "Drive the real desktop: screenshots, mouse/keyboard, windows and the accessibility tree (computer_* tools)."
    }

    /// 声明移植来源。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "dsh-computer-use".to_string(),
            plugin_version: "1.39.5".to_string(),
            url: "https://github.com/LJL454".to_string(),
        })
    }

    /// 默认关闭：把鼠标键盘交给模型需要用户显式打开。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 提供 `/computer` 命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Turn desktop control on/off, or show its status: /computer on|off|status"
                .to_string(),
            busy_safe: true,
            subcommands: vec![
                SubcommandDef {
                    name: "on",
                    description: "Enable computer control for this process",
                },
                SubcommandDef {
                    name: "off",
                    description: "Release held input and disable computer control",
                },
                SubcommandDef {
                    name: "status",
                    description: "Show whether computer control is on and which backend is in use",
                },
            ],
        }]
    }

    /// 绑定 `/computer` 的执行入口。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_computer);
    }

    /// 提供 14 个 `computer_*` 工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        schema::tools()
    }

    /// 关闭时这一族工具不进上下文（省 schema token，也让模型看不到不可用的能力）。
    fn filter_extension_tools(&self, tools: Vec<ExtensionTool>) -> Vec<ExtensionTool> {
        if state::enabled() { tools } else { Vec::new() }
    }

    /// 扩展被整体禁用时一并关掉能力并释放按住输入。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            state::set_enabled(false);
            let _ = input::release_all();
        }
    }

    /// 按工具名分发。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        ctx: ToolExecCtx,
    ) -> BoxFuture<'static, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            // 子代理的权限范围在启动时就固定了，不能从会话内部放宽；一台真实的鼠标键盘
            // 正是最不该经"派个子代理"绕过去的权限。
            if ctx.depth > 0 || ctx.agent_id.is_some() {
                return Err(ToolError(
                    "computer control is not available to subagents: a delegated agent's scope is fixed when it starts and cannot be widened from inside. Hand this step back to the parent agent.".to_string(),
                ));
            }
            if !state::enabled() {
                return Err(ToolError(
                    "computer control is off: run /computer on first (it is off by default, and this is a real mouse and keyboard)"
                        .to_string(),
                ));
            }
            if !backend_available() {
                return Err(ToolError(format!(
                    "computer control has no usable backend here ({}); on Linux this needs a Wayland session, a screenshot tool (grim) or xcap, and `swaymsg` for window enumeration",
                    backend::describe()
                )));
            }
            ops::dispatch(&name, &args).await
        })
    }
}

/// 后端可用性只探测一次：探测本身要跑 `xcap::Monitor::all()`，不该每调一次工具付一遍。
fn backend_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(backend::available)
}

/// `/computer` 命令实现。
fn command_computer(st: &mut App, raw: &str) -> bool {
    let arg = crate::extensions::command_arg(raw)
        .trim()
        .to_ascii_lowercase();
    match arg.as_str() {
        "" | "status" => {
            st.push_msg(status_text(), MsgLevel::Info);
        }
        "on" => {
            if !backend_available() {
                st.push_msg(
                    format!(
                        "computer control cannot be enabled here: {}",
                        backend::describe()
                    ),
                    MsgLevel::Warning,
                );
                return false;
            }
            state::set_enabled(true);
            crate::core::extensions::request_ui(
                crate::core::extensions::ExtensionUiRequest::RebuildTools,
            );
            st.push_msg(
                format!("computer control on ({})", backend::describe()),
                MsgLevel::Success,
            );
        }
        "off" => {
            let released = input::release_all().unwrap_or_default();
            state::set_enabled(false);
            crate::core::extensions::request_ui(
                crate::core::extensions::ExtensionUiRequest::RebuildTools,
            );
            st.push_msg(
                if released.is_empty() {
                    "computer control off".to_string()
                } else {
                    format!("computer control off; released: {}", released.join(", "))
                },
                MsgLevel::Success,
            );
        }
        other => {
            st.push_msg(
                format!("usage: /computer on|off|status (got {other:?})"),
                MsgLevel::Warning,
            );
        }
    }
    false
}

/// 状态文本（命令与诊断共用）。
fn status_text() -> String {
    let enabled = state::enabled();
    let mut lines = vec![format!(
        "computer control: {}",
        if enabled { "on" } else { "off" }
    )];
    lines.push(format!(
        "backend: {} (available: {})",
        backend::describe(),
        backend_available()
    ));
    if let Ok(displays) = backend::displays() {
        lines.push(format!(
            "displays: {}",
            backend::render_displays(&displays).replace('\n', "; ")
        ));
    }
    lines.push(format!(
        "accessibility: {}",
        if a11y::available() {
            "available"
        } else {
            "unavailable"
        }
    ));
    lines.push(if enabled {
        "tools: computer_* are visible; /computer off releases held input and hides them"
            .to_string()
    } else {
        "run /computer on to expose the computer_* tools".to_string()
    });
    lines.join("\n")
}
