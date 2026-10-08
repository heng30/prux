//! CLI 业务层：参数解析、文件参数、初始消息、会话选择、模型列表、项目信任

pub mod args;
pub mod auth_command;
pub mod file_processor;
pub mod initial_message;
pub mod list_models;
pub mod mcp_command;
pub mod project_trust;
pub mod session_picker;

pub use args::{Args, Command};
