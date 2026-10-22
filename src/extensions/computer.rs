//! `computer` 扩展：操作真实的桌面（截图、鼠标键盘、窗口、无障碍控件）。
//!
//! 设计要点：
//!
//! - **默认不启用**，在 `/extension` 面板里打开本扩展就是授权（持久化），没有第二个运行时开关；
//!   扩展被禁用时立即释放仍按住的键与鼠标键，并清掉帧。
//! - **14 个工具全部是延迟工具**（`ToolExposure::Deferred`，见 `schema::tools`）：描述与参数 schema
//!   平时不进上下文，模型先经 `tool_search` 检索到 `computer_*`，**下一轮**才拿到完整声明。
//!   授权门仍在：未启用的扩展根本不在 `registered()` 里，工具名解析不到（“Tool not found”）。
//!   因此本扩展隐式依赖 `tool-search` 扩展：本扩展没声明 `modes()`（仅 `All` 模式可用），
//!   `tool-search` 声明 `[Dev, Creator]`，二者交集目前恰好是 `All`。若哪天给本扩展加上
//!   `modes()`，要么一并放宽 `tool-search`，否则工具会变成“永远搜不到”。
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
            Extension, ExtensionCommand, ExtensionTool, ForkProjectInfo, ToolExecCtx,
            is_deferred_tool_activated,
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

    /// 默认关闭：把鼠标键盘交给模型需要用户显式打开（在 `/extension` 面板里启用即授权）。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 提供 `/computer` 命令（只读诊断）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description:
                "Show the desktop-control backend, displays and which computer_* tools are loaded"
                    .to_string(),
            busy_safe: true,
            subcommands: Vec::new(),
        }]
    }

    /// 绑定 `/computer` 的执行入口。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_computer);
    }

    /// 提供 14 个 `computer_*` 工具（全部声明为延迟工具，见 [`schema::tools`]）。
    fn tools(&self) -> Vec<ExtensionTool> {
        schema::tools()
    }

    /// 扩展被禁用时释放仍按住的输入并清掉帧（真正的安全收尾）。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            state::reset();
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

/// `/computer` 命令实现：只读诊断，没有开关。
fn command_computer(st: &mut App, raw: &str) -> bool {
    let arg = crate::extensions::command_arg(raw).trim();
    match arg.to_ascii_lowercase().as_str() {
        "" | "status" => st.push_msg(status_text(), MsgLevel::Info),
        other => st.push_msg(
            format!("usage: /computer (got {other:?})"),
            MsgLevel::Warning,
        ),
    }
    false
}

/// 状态文本（命令与诊断共用）。
///
/// 工具是否已进入工具表看的是延迟激活集（`tool_search` 命中后才算“已载入”），
/// 这是唯一需要模型/用户知道的“现在能不能直接调”的事实。
fn status_text() -> String {
    let mut lines = vec![format!(
        "backend: {} (available: {})",
        backend::describe(),
        backend_available()
    )];
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

    let tools = schema::tools();
    let loaded: Vec<&str> = tools
        .iter()
        .map(|t| t.name.as_str())
        .filter(|name| is_deferred_tool_activated(name))
        .collect();
    lines.push(if loaded.is_empty() {
        format!(
            "tools: 0/{} loaded; they are deferred tools, the model loads them with tool_search",
            tools.len()
        )
    } else {
        format!(
            "tools: {}/{} loaded: {}",
            loaded.len(),
            tools.len(),
            loaded.join(", ")
        )
    });
    lines.join("\n")
}
