//! 运行模式：交互 TUI（`interactive`）与桌面 GUI（`gui`）
//!
//! 依赖方向：modes 只依赖 core/ 与 cli/，不反向依赖。

#[cfg(feature = "gui")]
pub mod gui;

pub mod interactive;
