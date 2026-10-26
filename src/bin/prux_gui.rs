//! prux-gui：桌面 GUI 二进制（`--features gui`）。
//!
//! 与 `prux`（TUI）共用 [`prux::cli::startup::bootstrap`]；区别是 gpui 必须独占主线程，
//! 所以 tokio runtime 由本文件手动创建并保持存活，agent worker 跑在它上面。

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start tokio runtime");

    let startup = runtime.block_on(prux::cli::startup::bootstrap());
    prux::modes::gui::run(startup, runtime.handle());
}
