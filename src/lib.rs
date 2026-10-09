//! Rust 版 pi（@earendil-works/pi-coding-agent）
//!
//! # 目录结构（严格对齐 pi/packages/coding-agent/src）
//! - [`cli`]      —— CLI 业务：参数解析、文件参数、初始消息、会话选择、模型列表、项目信任
//! - [`core`]     —— 核心：模型调用、Agent 循环、会话、压缩、工具、trait 扩展（对应 dist/core/）
//! - [`modes`]    —— 运行模式：交互 TUI（对应 dist/modes/）
//! - [`utils`]    —— 通用工具：diff、mime、路径、截断（对应 dist/utils/）
//! - [`mermaid_text`] —— 内嵌的第三方库（`mermaid-text` 0.57.0 源码），把 Mermaid
//!   源码渲染为 Unicode 盒图；不参与下面的分层，仅被 [`modes`] 使用
//!
//! # API 分层（参考 pi 三层架构，依赖方向必须单向向上）
//!    1. **底层 ai**：[`core::provider`] 定义统一消息/模型/工具类型与流式调用，
//!       不依赖任何其他模块（对应 pi-ai）。
//!
//!    2. **中层 agent-core**：[`core::agent_session`]、[`core::session_manager`]、
//!       [`core::compaction`]、[`core::event_bus`]、[`core::messages`]
//!       负责 Agent 循环、状态、事件、会话历史与上下文压缩，不知道具体业务（对应 pi-agent-core）。
//!
//!    3. **顶层业务**：[`cli`]、[`modes`]、[`core::tools`]、[`core::skills`] 等组装成，
//!       具体编程助手（对应 pi-coding-agent）。

pub mod cli;
pub mod core;
pub mod error;
pub mod extensions;
pub mod modes;
pub mod utils;
pub mod vendor;

// 测试接缝（线程本地 agent_dir/HOME override、超时护栏）：
// 单元测试经 cfg(test)、集成测试经 self dev-dependency 的 test-support feature 编译。
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use cli::args::{Args, Command};
pub use core::agent_loop::AgentLoop;
pub use core::agent_session::Agent;
pub use core::provider::{AgentMessage, ContentBlock, ModelConfig, StreamEvent, Usage};
pub use core::session_manager::Session;
pub use vendor::mermaid_text;

/// 应用名（取自 Cargo 包名），用于配置目录、日志前缀等。
pub const APP_NAME: &str = env!("CARGO_PKG_NAME");
/// 项目作用域名（`.prux`），用于项目级配置目录命名。
pub const PROJECT_SCOPE_NAME: &str = concat!(".", env!("CARGO_PKG_NAME"));

/// 内嵌资源：`assets/` 目录
#[macro_export]
macro_rules! embedded {
    ($rel:literal) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/", $rel))
    };
}
