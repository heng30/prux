//! `ssh-remote` 扩展：远程工具族 + 端口隧道。
//!
//! **定位**：远程是「第二个文件系统 + 一台能跑命令的机器」，不是工作区。
//! 扩展只新增工具（`remote_exec` / `remote_read` / `remote_ls` / `remote_grep` /
//! `remote_sync` / `remote_pwd` / `remote_default`）与命令（`/ssh` / `/tunnel`），**不改写任何内置工具**——
//! 所以 `@` 补全、`!` 命令、git 状态、LSP、子代理看到的仍然只有本地，不存在
//! 「一半穿透、一半不穿透」的裂缝。
//!
//! 想访问远端资源时，两条正路：
//! 1. 显式工具：`remote_exec` 查日志/进程，`remote_read`/`remote_grep` 读远端文件；
//! 2. 隧道：`/tunnel add 15432:localhost:5432` 把远端服务映射到 `127.0.0.1`，
//!    之后本地一切（含 MCP server、数据库客户端）照旧连 localhost。
//!
//! **默认不启用**：它会建立出站连接并可能改动远端文件；七个工具都是
//! **延迟工具**（`ToolExposure::Deferred`），模型先用 `tool_search` 检索到它们，
//! **下一轮**才拿到完整声明。

mod auth;
mod command;
mod connect;
mod exec;
mod fs;
mod sshcfg;
mod state;
mod tool;
mod transport;
mod tunnel;

#[cfg(test)]
mod e2e_test;

use crate::{
    core::{
        extensions::{
            DockLine, DockSpan, Extension, ExtensionCommand, ExtensionHook, ExtensionMode,
            ExtensionTool, ExtensionUiRequest, OverlayEvent, OverlayView, ToolExecCtx,
            UiNotifyLevel, request_ui,
        },
        provider::AgentMessage,
        session_manager::Session,
        tools::{ToolError, ToolResult},
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_SSH_REMOTE,
        ssh_remote::state::RemoteConfig, tool_search,
    },
    modes::interactive::handlers::register_host_command,
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{sync::Arc, time::Duration};

/// 扩展名
const EXT: &str = "ssh-remote";

/// 状态栏里显示的最大行数（多了会把底栏挤爆）
const DOCK_MAX_LINES: usize = 4;

/// 工厂静态（启动时由 `register_all` 收集）
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static SSH_REMOTE_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_SSH_REMOTE,
    make: || Arc::new(SshRemote),
};

/// 扩展实现
pub struct SshRemote;

impl Extension for SshRemote {
    /// 扩展名
    fn name(&self) -> &str {
        EXT
    }

    /// 面板里的一句话说明
    fn description(&self) -> &str {
        "Remote SSH toolset: remote_exec/read/ls/grep/sync/pwd/default plus local port tunnels; built-in tools stay local"
    }

    /// 仅在 Dev 模式下可用（All 与 Creator 之外更低的级别；按当前需要不覆盖 Creator）。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 前置工具：七个 `remote_*` 都是延迟工具，模型得先用 `tool_search` 才能发现它们
    /// （`/extension` 二级详情页按此展示 `requires`）。
    fn prerequisite_tools(&self) -> Vec<String> {
        vec![tool_search::TOOL_NAME.to_string()]
    }

    /// 会建立出站连接、可能改动远端文件，必须显式开启
    fn default_enabled(&self) -> bool {
        false
    }

    /// 模型侧工具：`remote_exec` / `remote_read` / `remote_ls` / `remote_grep` / `remote_sync` /
    /// `remote_pwd` / `remote_default`
    fn tools(&self) -> Vec<ExtensionTool> {
        tool::tools()
    }

    /// 用户侧命令：`/ssh` 与 `/tunnel`
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![command::command(), command::tunnel_command()]
    }

    /// 停靠面板（连接与隧道）+ 每轮预算归零 + 凭据输入框
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![
            ExtensionHook::Dock,
            ExtensionHook::AgentEvent,
            ExtensionHook::Overlay,
        ]
    }

    /// 每轮开始归零远程输出预算（`turn_max_bytes` 是「每轮」语义）
    fn on_agent_event(&self, event: &Value) {
        if event.get("type").and_then(|v| v.as_str()) == Some("turn_start") {
            transport::reset_turn_budget();
        }
    }

    /// 注册 `/ssh` 与 `/tunnel` 的执行入口
    fn on_registered(&self) {
        register_host_command(EXT, command::CMD, command::handle);
        register_host_command(EXT, command::TUNNEL_CMD, command::handle_tunnel);
    }

    /// 启用状态变化：禁用时断开全部连接并停掉隧道
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            transport::disconnect_all();
            tunnel::stop_all();
        }
    }

    /// 执行远程工具
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        ctx: ToolExecCtx,
    ) -> BoxFuture<'static, std::result::Result<ToolResult, ToolError>> {
        Box::pin(async move { tool::dispatch(&name, &args, &ctx.cwd).await })
    }

    /// 认证输入框（密码 / 私钥口令）：每帧拉取，内容由 `auth` 模块持有
    fn overlay_view(&self, id: u64, cols: u16) -> Option<OverlayView> {
        auth::overlay_view(id, cols)
    }

    /// 认证输入框的按键（逐键回传，扩展自己维护明文缓冲并渲染掩码）
    fn on_overlay_event(&self, id: u64, ev: &OverlayEvent) -> bool {
        auth::on_overlay_event(id, ev)
    }

    /// 停靠面板：已连接端点与活跃隧道；都没了返回空（面板自动隐藏）
    fn dock_lines(&self) -> Vec<DockLine> {
        let connected = transport::connected();
        let tunnels = tunnel::active();
        if connected.is_empty() && tunnels.is_empty() {
            return Vec::new();
        }

        let mut lines: Vec<DockLine> = vec![vec![DockSpan::new("muted", "#999999", "ssh remote")]];
        for ep in connected.iter().take(DOCK_MAX_LINES) {
            lines.push(vec![DockSpan::new("accent", "#8abeb7", ep.display())]);
        }

        if !tunnels.is_empty() {
            let text = tunnels
                .iter()
                .map(|t| format!("127.0.0.1:{}", t.local_port))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(vec![DockSpan::plain(format!("tunnels: {text}"))]);
        }
        lines
    }

    /// 会话开始：只恢复端口隧道（连接本身按需建立，不预热）
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], _session: Option<&Session>) {
        if RemoteConfig::load().tunnels.is_empty() {
            return;
        }

        tokio::spawn(async move {
            // 让 TUI 先渲染完第一帧再连，避免启动期网络波动糊住界面
            tokio::time::sleep(Duration::from_millis(200)).await;
            for (spec, result) in tunnel::restore_all().await {
                match result {
                    Ok(port) => request_ui(ExtensionUiRequest::Notify {
                        text: format!("SSH tunnel restored: 127.0.0.1:{port} ({spec})"),
                        level: UiNotifyLevel::Info,
                    }),
                    Err(e) => request_ui(ExtensionUiRequest::Notify {
                        text: format!("SSH tunnel {spec} failed: {e}"),
                        level: UiNotifyLevel::Warning,
                    }),
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::ssh_remote::state::TestRootGuard;

    /// 端点 JSON 形状能被 serde 往返（老配置迁移不会炸）
    #[test]
    fn endpoint_json_roundtrip() {
        let value = serde_json::json!({
            "command": "ssh root@gpu",
            "host": "gpu",
            "user": "root",
            "port": 22,
            "note": "H100",
            "cwd": "/srv/x"
        });
        let ep: state::Endpoint = serde_json::from_value(value).unwrap();
        assert_eq!(ep.key(), "root@gpu:22");
        assert_eq!(ep.cwd, "/srv/x");
        assert!(ep.display().contains("H100"));
    }

    /// 七个工具与两个命令都要真的声明出来（面板与提示词依赖这个清单）
    #[test]
    fn declares_seven_tools_and_two_commands() {
        let _l = state::TEST_ROOT_LOCK.blocking_lock();
        let _g = TestRootGuard::temp();
        let names: Vec<String> = SshRemote.tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names.len(), 7);
        for expected in [
            tool::EXEC,
            tool::READ,
            tool::LS,
            tool::GREP,
            tool::SYNC,
            tool::PWD,
            tool::DEFAULT,
        ] {
            assert!(names.iter().any(|n| n == expected), "缺少工具 {expected}");
        }
        let cmds: Vec<String> = SshRemote.commands().into_iter().map(|c| c.name).collect();
        assert_eq!(cmds, vec![command::CMD, command::TUNNEL_CMD]);
        assert!(!SshRemote.default_enabled(), "必须默认关闭");
        assert_eq!(
            SshRemote.prerequisite_tools(),
            vec!["tool_search"],
            "延迟工具的前置工具必须是 tool_search"
        );
    }

    /// 没连接也没隧道时停靠面板必须为空（面板据此自动隐藏）
    #[test]
    fn dock_is_empty_without_connections() {
        let _l = state::TEST_ROOT_LOCK.blocking_lock();
        let _g = TestRootGuard::temp();
        transport::disconnect_all();
        tunnel::stop_all();
        assert!(SshRemote.dock_lines().is_empty());
    }
}
