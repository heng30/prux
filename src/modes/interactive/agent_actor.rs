//! agent actor 协议：UI 主循环 ↔ agent worker 的命令/事件通道
//!
//! 架构：
//! - agent worker：独立 tokio 任务**独占** `Agent`（move 进去，不包 Mutex）。
//!   `prompt`/`compact`/`navigate` 等 `&mut self` async 方法只被 worker 调用，不再有第二个线程碰。
//! - UI 主循环：持有 `App`，对 agent 的一切操作变成「发 `AgentCommand`」。
//!   UI → worker 方向用命令枚举；worker → 状态更新经 `run_in_event_loop` 投闭包直达 `&mut App`。
//! - busy 命令：worker 在回合间隙 drain 命令队列，自动排队执行。
//!
//! agent 流式事件沿用 `serde_json::Value`（agent 内部 emit 机制不变）

use crate::core::{provider::AgentMessage, skills::Skill};
use serde_json::Value;
use std::path::PathBuf;
use tokio::sync::mpsc;

/// UI → worker 命令发送端
pub type CommandTx = mpsc::UnboundedSender<AgentCommand>;

/// worker 接收端
pub type CommandRx = mpsc::UnboundedReceiver<AgentCommand>;

/// UI → worker 命令
#[derive(Debug)]
pub enum AgentCommand {
    /// 启动一轮 prompt（结果全经 sink 事件流）
    Prompt(String),
    /// 启动一轮含单条消息的 prompt
    PromptMessage(AgentMessage),
    /// 启动一轮多消息 prompt（steer 批 / 初始队列）
    PromptBatch(Vec<AgentMessage>),
    /// 切换模型（persist=true 时切换成功后写 settings 默认模型）
    SwitchModel {
        /// 目标 provider 名（如 deepseek、anthropic）。
        provider: String,
        /// 目标模型在 provider 目录中的标识。
        model_id: String,
        /// true 时切换成功后才写回 settings 作为默认模型。
        persist: bool,
    },
    /// 设置 thinking 级别（persist=true 时设置成功后写 settings 默认级别）
    SetThinking { level: String, persist: bool },
    /// 切换会话
    ResumeSession { path: String },
    /// 开始全新会话
    NewSession,
    /// 登录后即时应用凭据 / fallback 默认模型选择
    ApplyKey { provider: String, key: String },
    /// 扩展变更后重建工具
    RebuildTools,
    /// /session 统计
    SessionStats,
    /// 手动压缩
    Compact { instructions: Option<String> },
    /// 会话树导航（结果走消息流事件）
    Navigate { target_id: String, summarize: bool },
    /// 回退最后一次用户输入（活动分支叶子移到最后一条 user 消息之前；结果走 `on_rewind_done` 回执）
    Rewind,
    /// 设置会话名
    SetSessionName { name: String },
    /// 会话树
    Tree,
    /// 运行状态信息
    DebugInfo,
    /// 分享会话（theme 为当前 TUI 主题名，None 时回退 dark）
    Share { theme: Option<String> },
    /// 导出会话（theme 为当前 TUI 主题名，None 时回退 dark）
    Export {
        /// 导出目标路径；None 时由导出逻辑选默认文件名。
        out: Option<String>,
        /// 当前 TUI 主题名，None 时回退 dark。
        theme: Option<String>,
    },
    /// `/bug`：收集脱敏元数据 + 诊断，可选附 transcript / 模型摘要，导出 zip 到 cwd
    BugReport {
        /// 用户在描述面板输入的复现说明。
        hint: String,
        /// true 时把当前会话 transcript 一并打包。
        include_session: bool,
        /// true 时额外调模型生成一份摘要随包导出。
        include_summary: bool,
    },
    /// 设置/清除标签
    Label {
        /// 目标会话树条目 id。
        entry_id: String,
        /// Some 为设置标签，None 表示清除该条目的标签。
        label: Option<String>,
    },
    /// /reload：重载后的 context files + skills + system prompt 写入 worker 侧缓存并重建系统提示/工具
    /// （compose_tools 消费 agent.skills + rebuild_ctx，UI 侧的副本只用于展开）
    Reload {
        /// 重载后的项目上下文文件（路径，内容）对。
        context_files: Vec<(String, String)>,
        /// 重载后的技能列表，供 worker 重建工具与提示。
        skills: Vec<Skill>,
        /// 重载后的基础系统提示；None 表示沿用默认提示。
        system_prompt: Option<String>,
        /// 追加在系统提示末尾的附加内容；None 表示无附加。
        append_system_prompt: Option<String>,
    },
    /// /clone：按当前 leaf 复制活动分支到新会话（fork_at）。
    ForkClone,
    /// /fork 与修复面板共用：按 --fork 语义 fork 指定源。
    /// /fork 传当前会话路径 + dir=None；修复确认时路径已在面板截断修复。
    ForkFrom { path: String, dir: Option<PathBuf> },
    /// 向上下文追加一条消息但不触发回合（`!cmd` 输出进会话；idle 执行）
    AppendMessage { msg: AgentMessage },
    /// 向当前会话追加一条自定义条目（扩展持久化用；idle 执行，完成后经 sink 事件 `extension:entry_persisted` 回执）
    AppendCustomEntry {
        /// 扩展自定义的条目类型标签，回执时按此匹配。
        custom_type: String,
        /// 条目负载；None 表示该条目无数据（回执 null）。
        data: Option<Value>,
    },
    /// 僵尸操作面板确认后的恢复命令：面板选择前会话**未加载**；
    /// action=continue（只读容忍，直接打开）/ rewrite（先压平再打开）/ cancel（不打开，不发此命令）
    ResumeZombie { path: String, action: String },
    /// 取消当前回合
    Abort,
    /// 退出前收尾：丢弃进行中的回合并为所有未闭合 operation 补写 interrupted，
    /// 然后结束 worker 循环（进程退出路径调用，用户 Esc/Ctrl+C 后 /quit 不再残留僵尸）。
    Shutdown,
}

/// 创建命令通道
pub fn channels() -> (CommandTx, CommandRx) {
    mpsc::unbounded_channel()
}

/// UI 侧对 agent worker 的命令句柄。所有 handler / 闭包通过它发命令。
#[derive(Clone, Debug, Default)]
pub struct WorkerHandle {
    /// 命令发送端；`null()` 构造的测试句柄为 None，命令被直接丢弃。
    cmd_tx: Option<CommandTx>,
}

impl WorkerHandle {
    /// 绑定命令发送端，构造可向 agent worker 发命令的句柄。
    pub fn new(cmd_tx: CommandTx) -> Self {
        WorkerHandle {
            cmd_tx: Some(cmd_tx),
        }
    }

    /// 测试用：无通道的句柄（命令被丢弃，便于断言「不 panic 不阻塞」）
    pub fn null() -> Self {
        WorkerHandle { cmd_tx: None }
    }

    /// 发送命令；[`WorkerHandle::null`] 构造的无通道句柄会静默丢弃。
    pub fn send(&self, cmd: AgentCommand) {
        if let Some(tx) = &self.cmd_tx {
            _ = tx.send(cmd);
        }
    }

    /// 发送一条用户文本 prompt（触发一个回合）。
    pub fn prompt(&self, text: String) {
        self.send(AgentCommand::Prompt(text));
    }

    /// 发送单条消息 prompt。
    pub fn prompt_message(&self, msg: AgentMessage) {
        self.send(AgentCommand::PromptMessage(msg));
    }

    /// 一次发送多条消息 prompt（作为同一回合的批量输入）。
    pub fn prompt_batch(&self, msgs: Vec<AgentMessage>) {
        self.send(AgentCommand::PromptBatch(msgs));
    }

    /// 取消当前回合。
    pub fn abort(&self) {
        self.send(AgentCommand::Abort);
    }

    /// 向上下文追加一条消息但不触发回合（idle 时执行）。
    pub fn append_message(&self, msg: AgentMessage) {
        self.send(AgentCommand::AppendMessage { msg });
    }

    /// 向当前会话追加自定义条目（扩展持久化；经 `extension:entry_persisted` 事件回执）
    pub fn append_custom_entry(&self, custom_type: &str, data: Option<Value>) {
        self.send(AgentCommand::AppendCustomEntry {
            custom_type: custom_type.to_string(),
            data,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::AgentMessage;

    #[test]
    fn channels_are_wired() {
        let (cmd_tx, mut cmd_rx) = channels();
        cmd_tx.send(AgentCommand::Prompt("hi".into())).unwrap();
        assert!(matches!(
            cmd_rx.try_recv().unwrap(),
            AgentCommand::Prompt(t) if t == "hi"
        ));
    }

    #[test]
    fn agent_message_is_clonable_through_command() {
        let msg = AgentMessage::user_text("steer");
        let cmd = AgentCommand::PromptBatch(vec![msg.clone()]);
        match cmd {
            AgentCommand::PromptBatch(v) => assert_eq!(v[0].text(), "steer"),
            _ => panic!("wrong variant"),
        }
    }
}
