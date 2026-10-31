//! prux-gui：桌面 GUI 二进制（`--features gui`）。
//!
//! 与 `prux`（TUI）共用 [`prux::cli::startup::bootstrap`]；区别是 gpui 必须独占主线程，
//! 所以 tokio runtime 由本文件手动创建并保持存活，agent worker 跑在它上面。
//! `--tui` 时不进 GUI，改跑交互 TUI（参数解析仍由 bootstrap 里的 clap 完成）。

use prux::{APP_NAME, cli::startup::bootstrap, modes::interactive};
use std::io::IsTerminal;

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start tokio runtime");

    let startup = runtime.block_on(bootstrap());

    if startup.tui {
        // TUI 需要交互终端：stdin/stdout 非 TTY 时无法渲染
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            eprintln!(
                "Error: {APP_NAME} only supports the interactive TUI mode, which requires stdin and stdout to be a TTY.",
            );
            std::process::exit(1);
        }

        let result = runtime.block_on(interactive::run_interactive(
            startup.agent,
            startup.initial_messages,
            startup.prompt_templates,
            startup.theme_override.as_deref(),
            startup.no_themes,
            startup.reload_ctx,
            startup.verbose,
        ));

        if let Err(e) = result {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }

        return;
    }

    prux::modes::gui::run(startup, runtime.handle());
}
