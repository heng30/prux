//! `rpc` 扩展：把 prux 变成任意语言都能驱动的长期子进程（JSONL 上的 RPC）。
//!
//! **问题**：想从别的语言、IDE 或自建界面里用同一套模型、工具与会话历史，只有两条路——
//! 要么内嵌 prux 的 Rust API（跨语言就别想了），要么自己再写一遍 agent loop。
//!
//! **本扩展**：注入 `prux rpc` 子命令，在 stdin / stdout 上讲「一行一条 JSON」：
//! stdin 是命令，stdout 是命令响应 + agent 事件流 + 扩展 UI 记录。子命令入口由本扩展提供，
//! `bootstrap()` 把**已装配好的** Agent 交给它——模型链、工具表、会话文件、扩展钩子与 TUI
//! 完全相同，客户端与 `prux` 看到的是同一份 `agent_dir()`。
//!
//! 边界：
//! - 回合（`prompt`）与手动压缩（`compact`）会独占 Agent：期间只接 `steer` / `follow_up` /
//!   `abort` / `clear_queue` / `abort_bash`，其余命令回一句「正在处理」；
//! - `bash` 命令只执行并回结果，**不写进会话上下文**（prux 没有 `bashExecution` 条目）；
//! - `abort_retry` 是幂等空操作：重试退避在回合内部，`abort` 已经能停整轮；
//! - 关停靠关闭 stdin；不处理 SIGTERM（客户端该走「关 stdin 再等」这条路径）。
//!
//! 默认不启用但**没有任何工具与钩子注册**：不开 `prux rpc` 时它什么都不做。

mod host;
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
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_RPC},
};
use futures_util::future::BoxFuture;
use std::sync::Arc;

/// 扩展名（注册名，也是 `/extension` 面板里的名字）。
pub(super) const EXT: &str = "rpc";

/// 子命令名（`prux rpc`）。
pub(super) const CMD: &str = "rpc";

/// 子命令的一行说明（`prux --help` 的 Commands 段）。
const DESCRIPTION: &str =
    "Serve the JSON-lines RPC protocol on stdin/stdout (headless integration)";

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static RPC_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_RPC,
    make: || -> Arc<dyn Extension> { Arc::new(Rpc) },
};

/// `rpc` 扩展。
pub struct Rpc;

impl Extension for Rpc {
    /// 扩展注册名。
    fn name(&self) -> &str {
        EXT
    }

    /// 面板里的功能说明。
    fn description(&self) -> &str {
        concat!(
            "JSON-lines RPC server: `",
            env!("CARGO_PKG_NAME"),
            " rpc` speaks one JSON record per line on stdin/stdout — \
         commands in, responses plus the full agent event stream out — so any language or IDE can \
         drive this same agent. Prompt/compact hold the agent; steer, follow_up, abort, \
         clear_queue and abort_bash stay available while they run."
        )
    }

    /// 仅 `Dev` 及以上模式可用（与 `acp` 同挡位）：进程级集成属于开发场景。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 默认不启用：`prux rpc` 是给外部程序接入用的，属按需开启的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不给模型加工具：本扩展只在 `prux rpc` 进程里工作。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 注入 `prux rpc` 子命令。
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

/// 子命令入口：Agent 已就绪，跑 RPC 服务端直到 stdin 关闭。
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
    fn extension_injects_the_rpc_subcommand_without_tools() {
        let ext = Rpc;
        assert_eq!(ext.name(), "rpc");
        assert!(!ext.default_enabled(), "默认不启用：进程级集成属按需开启");
        assert!(ext.tools().is_empty(), "本扩展不给模型加工具");
        assert!(ext.hooks().is_empty(), "本扩展不订阅任何钩子");
        let subs = ext.subcommands();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].name, "rpc");
        assert!(!subs[0].description.is_empty(), "--help 需要一行说明");
        assert_eq!(
            ext.modes(),
            vec![ExtensionMode::Dev],
            "进程级集成只在开发模式可用"
        );
    }
}
