//! `acp` 扩展：把 prux 变成编辑器的 agent 后端（Agent Client Protocol）。
//!
//! **问题**：prux 只能自己开 TUI。想在 Zed / VS Code 里用同一套模型、工具与会话历史，
//! 就得让编辑器直接驱动 prux 的 Agent，而不是再写一遍 agent loop。
//!
//! **本扩展**：注入 `prux acp` 子命令，在 stdin / stdout 上讲 ACP（NDJSON + JSON-RPC 2.0）。
//! 子命令入口由本扩展提供，`bootstrap()` 把**已装配好的** Agent 交给它——模型链、工具表、
//! 会话文件、扩展钩子与 TUI 完全相同，`prux` 与编辑器看到的是同一份 `agent_dir()`。
//!
//! 支持的方法：`initialize` / `authenticate` / `session/new` / `session/prompt` /
//! `session/cancel`，流式输出经 `session/update` 通知（文本增量、思考增量、工具调用）。
//!
//! 边界：
//! - **单会话**：`session/new` 返回 bootstrap 已经打开的会话，编辑器的多个面板共享同一个 Agent
//!   （prux 的会话切换是 TUI 语义，不在 ACP 里做第二套）；
//! - 不支持 `session/load`（历史在会话文件里，客户端不需要重放）；
//! - prompt 只取文本块：图片 / 音频 / 嵌入资源在能力声明里已标不支持。
//!
//! 默认启用但**没有任何工具与钩子注册**：不开 `prux acp` 时它什么都不做。

mod protocol;
mod server;

use crate::{
    APP_NAME,
    core::{
        agent_session::Agent,
        extensions::{
            Extension, ExtensionMode, ExtensionSubcommand, ExtensionTool,
            register_subcommand_handler,
        },
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_ACP},
};
use futures_util::future::BoxFuture;
use std::sync::Arc;

/// 扩展名（注册名，也是 `/extension` 面板里的名字）。
pub(super) const EXT: &str = "acp";

/// 子命令名（`prux acp`）。
pub(super) const CMD: &str = "acp";

/// 子命令的一行说明（`prux --help` 的 Commands 段）。
const DESCRIPTION: &str =
    "Serve the Agent Client Protocol on stdin/stdout (for editors such as Zed)";

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static ACP_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_ACP,
    make: || -> Arc<dyn Extension> { Arc::new(Acp) },
};

/// `acp` 扩展。
pub struct Acp;

impl Extension for Acp {
    /// 扩展注册名。
    fn name(&self) -> &str {
        EXT
    }

    /// 面板里的功能说明。
    fn description(&self) -> &str {
        "Agent Client Protocol server: `prux acp` speaks ACP (NDJSON + JSON-RPC 2.0) on \
         stdin/stdout so editors such as Zed can drive this same agent — same model chain, tools, \
         session file and extension hooks. Single session; no session/load."
    }

    /// 仅 `Dev` 及以上模式可用（与 `vet` 同挡位）：编辑器集成属于开发场景。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 默认不启用：`prux acp` 是给编辑器接入用的，属按需开启的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不给模型加工具：本扩展只在 `prux acp` 进程里工作。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 注入 `prux acp` 子命令。
    fn subcommands(&self) -> Vec<ExtensionSubcommand> {
        vec![ExtensionSubcommand {
            name: CMD,
            description: DESCRIPTION,
        }]
    }

    /// 接线子命令入口。
    fn on_registered(&self) {
        register_subcommand_handler(EXT, CMD, handle);
    }
}

/// 子命令入口：Agent 已就绪，跑 ACP 服务端直到 stdin 关闭。
///
/// `-h` / `--help` 由这里自己回（子命令在 clap 里声明了 `disable_help_flag`），
/// 因为 `prux --help` 只列名字，用法得说清楚。
fn handle(agent: Agent, args: Vec<String>) -> BoxFuture<'static, i32> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("Usage: {} {CMD}\n\n{DESCRIPTION}", APP_NAME);
        return Box::pin(async { 0 });
    }

    Box::pin(server::run(agent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_injects_the_acp_subcommand_without_tools() {
        let ext = Acp;
        assert_eq!(ext.name(), "acp");
        assert!(!ext.default_enabled(), "默认不启用：编辑器接入属按需开启");
        assert!(ext.tools().is_empty(), "本扩展不给模型加工具");
        assert!(ext.hooks().is_empty(), "本扩展不订阅任何钩子");
        let subs = ext.subcommands();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].name, "acp");
        assert!(!subs[0].description.is_empty(), "--help 需要一行说明");
        assert_eq!(
            ext.modes(),
            vec![ExtensionMode::Dev],
            "编辑器集成只在开发模式可用"
        );
    }
}
