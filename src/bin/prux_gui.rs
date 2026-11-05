//! prux-gui：桌面 GUI 二进制（`--features gui`）。
//!
//! 与 `prux`（TUI）共用 [`prux::cli::startup::bootstrap`]；区别是 gpui 必须独占主线程，
//! 所以 tokio runtime 由本文件手动创建并保持存活，agent worker 跑在它上面。
//! `--tui` 时不进 GUI，改跑交互 TUI（参数解析仍由 bootstrap 里的 clap 完成）。

// release 版不显示控制台窗口；debug 版保留控制台，便于看日志与 `--tui` 调试。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use prux::{cli::startup::bootstrap, modes::tui};

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start tokio runtime");

    let startup = runtime.block_on(bootstrap());

    if startup.tui {
        runtime.block_on(tui::run_from_startup(startup));
        return;
    }

    prux::modes::gui::run(startup, runtime.handle());
}
