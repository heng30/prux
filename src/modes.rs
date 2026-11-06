//! 运行模式：交互 TUI（`tui`）与桌面 GUI（`gui`）
//!
//! 依赖方向：modes 只依赖 core/ 与 cli/，不反向依赖。

#[cfg(feature = "gui")]
pub mod gui;

pub mod cwd_store;
pub mod slash;
pub mod tui;

use std::sync::atomic::{AtomicBool, Ordering};

/// 本次进程是否跑桌面 GUI（`prux-gui` 二进制）。
///
/// 启动装配（`cli::startup`）在选中的会话损坏时要决定「能不能弹修复面板」，
/// 而那两个二进制共用同一份装配代码：TUI 在无终端时必须报错退出，
/// GUI 则没有终端也照样能弹面板。
static GUI_BINARY: AtomicBool = AtomicBool::new(false);

/// 标记本次进程是桌面 GUI；`prux-gui` 在 `bootstrap()` 之前调用。
pub fn set_gui_binary() {
    GUI_BINARY.store(true, Ordering::Relaxed);
}

/// 本次进程是否为桌面 GUI。
pub fn is_gui_binary() -> bool {
    GUI_BINARY.load(Ordering::Relaxed)
}
