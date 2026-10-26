//! prux 入口（TUI 二进制）：`bootstrap()` 装配完成 → 终端检查 → 交互 TUI。

use prux::{APP_NAME, cli::startup::bootstrap, modes::interactive};
use std::io::IsTerminal;

/// 进程入口：会话/模型/工具已由 [`bootstrap`] 装配好，这里只做终端检查与模式分派。
#[tokio::main]
async fn main() {
    let startup = bootstrap().await;

    // TUI 模式需要交互终端：stdin/stdout 非 TTY 时无法渲染
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!(
            "Error: {APP_NAME} only supports the interactive TUI mode, which requires stdin and stdout to be a TTY.",
        );
        std::process::exit(1);
    }

    if let Err(e) = interactive::run_interactive(
        startup.agent,
        startup.initial_messages,
        startup.prompt_templates,
        startup.theme_override.as_deref(),
        startup.no_themes,
        startup.reload_ctx,
        startup.verbose,
    )
    .await
    {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
