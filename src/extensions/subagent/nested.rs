//! 嵌套子代理：子代理自己也能委派。
//!
//! ## 形状
//!
//! 委派工具的**名字**与顶层那三个完全一样（`Agent` / `get_subagent_result` / `steer_subagent`），
//! 因为对模型来说这就是同一件事。区别在作用域，而作用域是**运行时按调用者身份**决定的：
//!
//! - 核心默认把这三个名字从 child 的工具表里剔除（递归防护）。扩展为"允许嵌套"的那一层 child
//!   把它们**显式写进 `spec.tools`**，核心据此放行这一份（规则见 `agent_session.rs`）。
//! - handler 里用 [`super::manager::caller_of`] 认出"现在说话的是哪个 agent"（按它那一个取消标志
//!   的指针反查记录），于是：深度到顶就拒绝、只能看见**自己**派发的 child、父代理结束就把子孙掐掉。
//!
//! 也就是说："不允许嵌套"与"允许但限深"共用同一条实现，差别只在深度上限与身份判断——不给核心加字段，也不给模型两套名字。
//!
//! ## 深度上限
//!
//! 主会话 = 0、它的子代理 = 1、孙代理 = 2。
//! **`0`/`1` 等于关掉嵌套**：`1` 时子代理拿到工具也会在 handler 里被拒]
//! （用户可能在自己的 agent 文件里写了 `tools: Agent`，那句话不该绕过上限）。

use super::manager;

/// 嵌套委派工具的**名字**（与顶层工具同名，作用域在运行期按调用者决定）。
pub(crate) const NESTED_TOOL_NAMES: [&str; 3] = ["Agent", "get_subagent_result", "steer_subagent"];

/// 深度 `depth` 的 agent 还能不能再派一代（它的子代理会是 `depth + 1`）。
///
/// `max_depth` 由调用方从配置读进来（而不是在这里读全局配置）：
/// `build_spec` 因此是纯函数，测试可以钉住上限而不碰全局状态。
pub(crate) fn nesting_allowed(depth: u32, max_depth: u32) -> bool {
    depth.saturating_add(1) <= max_depth
}

/// 当前配置的上限（主会话 = 0）。
pub(crate) fn max_depth() -> u32 {
    manager::config().max_subagent_depth
}

/// 到顶时的工具错误（可读，且说清是哪个设置）。
pub(crate) fn depth_exceeded(caller_depth: u32) -> String {
    format!(
        "nesting limit reached: this agent is at depth {caller_depth} and maxSubagentDepth is {} \
         (main session = 0, its subagents = 1, …). Raise it in /agents → Settings, or do the work here.",
        max_depth()
    )
}
