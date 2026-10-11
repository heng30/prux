//! `/debug` 中需要 agent 侧数据的子命令
//!
//! TUI 侧的 `App` 不持有 agent（`Agent` 由 worker 独占），这些子命令一律经
//! [`AgentCommand`] 请 worker 汇总，再经 `command_notice` 回执到聊天区系统消息
//! （只给人看，不进模型上下文）。

use crate::modes::interactive::{agent_actor::AgentCommand, app::App};

/// `/debug info`：请 worker 汇总 agent 运行状态（provider/model/api、自动压缩/重试开关、
/// steer/follow-up 模式、会话 id 与活动分支叶子）。
pub(super) fn info(st: &mut App) {
    st.worker.send(AgentCommand::DebugInfo);
}

/// `/debug system-prompt`：请 worker 回执当前系统提示词原文（空提示词时给一行提示语）。
pub(super) fn system_prompt(st: &mut App) {
    st.worker.send(AgentCommand::SystemPrompt);
}
