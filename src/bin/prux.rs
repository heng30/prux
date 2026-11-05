//! prux 入口（TUI 二进制）：`bootstrap()` 装配完成 → 交互 TUI。

use prux::{cli::startup::bootstrap, modes::tui};

#[tokio::main]
async fn main() {
    tui::run_from_startup(bootstrap().await).await;
}
