//! 内置工具系统。 分层（工具三层设计）：
//!
//! - [`index`] 定义 `ToolDef`（描述层，只告诉模型工具长什么样）与注册表；
//! - `read` / `write` / `edit` / `bash` / `grep` / `find` / `ls` 是实现层；
//! - 执行统一返回 [`ToolResult`] / [`ToolError`]，错误被编码为消息，不打断 Agent 循环。

mod bash;
mod edit;
mod find;
mod grep;
mod ls;
mod operations;
mod read;
mod write;

pub(crate) mod index;

#[cfg(windows)]
mod powershell;

pub use operations::{DefaultToolOperations, ToolOperations, ToolStat};

pub use bash::{
    BashOutcome, BashSessionEnv, OnChunkCallback, abort_all_bash, abort_bash, execute_bash,
    execute_bash_with, execute_bash_with_env,
};
pub use edit::{execute_edit, preview_edit_diff};
pub use find::execute_find;
pub use grep::execute_grep;
pub use index::{
    ToolDef, ToolError, ToolExecutionMode, ToolResult, ToolResultAttachment, all_tool_names,
    apply_prepare_arguments, resolve_tool_path, tool_def, tool_defs, tool_execution_mode,
    validate_tool_arguments,
};
pub use ls::execute_ls;
pub use read::{ReadImageOptions, execute_read, execute_read_with_options};
pub use write::execute_write;

#[cfg(windows)]
pub use powershell::{execute_powershell, execute_powershell_result, find_powershell};
