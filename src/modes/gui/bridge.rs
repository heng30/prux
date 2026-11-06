//! 与 agent 的桥：命令下行、事件上行。

pub mod worker;

use crate::core::{
    model_scope::ScopedModel, prompt_templates::PromptTemplate, provider::AgentMessage,
    session_manager::SessionRepairPlan, skills::Skill,
};
use serde_json::Value;
use std::path::PathBuf;

/// worker → GUI 的上行事件。
pub enum GuiEvent {
    /// agent 内核 JSON 事件（形状与 TUI 消费的完全一致：`message_start` /
    /// `message_update` / `tool_execution_start` / `agent_settled` …）。
    Agent(Value),
    /// 一轮回合以错误结束（`prompt` 返回 Err，内核事件流不会覆盖这类失败）。
    Error(String),
    /// 一次性提示（导出路径、命令回执等），GUI 直接显示成一条 note。
    Notice(String),
    /// 会话历史（切换/新建会话后推一次，GUI 据此重建 transcript）
    History(Vec<AgentMessage>),
    /// 本会话可用的技能与提示词模板：worker 启动与 `/reload` 后各推一次。
    ///
    /// 带正文（技能是 SKILL.md 路径、模板是模板正文），因为 `/` 补全要名字、
    /// 提交时的展开要正文——忙碌排队（steer / follow-up）在界面侧排队，
    /// 不先展开就会把 `/模板名` 原样塞给模型。
    PromptAssets {
        /// 已加载的技能（展开 `/skill:名` 时读 `path`）
        skills: Vec<Skill>,
        /// 已加载的提示词模板（展开 `/模板名 参数`）
        templates: Vec<PromptTemplate>,
    },
    /// 要打开的会话有 ≥2 个崩溃残留的未关闭 operation：先弹
    /// Continue / Rewrite / Cancel 面板，确认后再装配（同 TUI）。
    SessionZombie {
        /// 含未关闭 operation 的会话文件
        path: PathBuf,
        /// 未完成 operation 的条数（含当前 run）
        count: usize,
    },
    /// 要打开的会话中间行损坏：弹修复面板（抢救 / 截断 / 取消），
    /// 确认后写回文件再重新打开（同 TUI 的修复面板）。
    SessionRepair {
        /// 损坏的会话文件
        path: PathBuf,
        /// 修复方案（含两条写回路径的内容）
        plan: SessionRepairPlan,
    },
    /// `/rewind` 回退成功：重建 transcript，并按 GUI 记录的选择把输入回填输入框
    Rewound {
        /// 回退后的消息历史（回退第一条 user 消息时为空）
        messages: Vec<AgentMessage>,
        /// 被回退的用户输入原文（`/rewind` 的「回填编辑器」选项用它）
        removed_input: String,
    },
    /// 会话状态快照：每次命令执行后与每个回合结束时推送，GUI 据此刷新顶栏/面板。
    Snapshot(Box<Snapshot>),
}

/// 会话状态快照（GUI 需要的全部「非流式」状态）。
pub struct Snapshot {
    /// 当前模型所属 provider（如 `anthropic`）。
    pub provider: String,
    /// 当前模型标识。
    pub model: String,
    /// 当前 thinking 级别；模型不支持思考时为 None。
    pub thinking: Option<String>,
    /// 该模型可选的 thinking 级别（含 `off`）。
    pub thinking_levels: Vec<String>,
    /// `--models` / `settings.enabledModels` 解析出的 scope（有序，可带 thinking）；
    /// 非空时 `Ctrl+P` 只在其中循环，空则退回全部已配置模型。
    pub model_cycle: Vec<ScopedModel>,
    /// 会话文件路径；内存会话（`--no-session`）为 None。
    pub session_path: Option<String>,
    /// 会话名（`/name` 设置过才有；未命名或内存会话为 None）——侧栏行标题随它变
    pub session_name: Option<String>,
    /// 会话中的消息条数。
    pub messages: usize,
    /// 当前会话树的叶子条目 id（`/label` 等按 id 操作时用）；
    /// 内存会话或空树为 None。
    pub leaf_entry_id: Option<String>,
    /// 当前激活的工具名。
    pub tools: Vec<String>,
    /// 工作目录。
    pub cwd: String,
    /// 会话累计用量（输入/输出/缓存/费用）。
    pub usage: UsageTotals,
}

/// 会话累计用量。
#[derive(Default)]
pub struct UsageTotals {
    /// 未命中缓存的输入 token 合计。
    pub input: u64,
    /// 输出 token 合计。
    pub output: u64,
    /// 命中缓存的输入 token 合计。
    pub cache_read: u64,
    /// 写入缓存的 token 合计。
    pub cache_write: u64,
    /// 费用合计，单位美元。
    pub cost: f64,
    /// 最后一次请求的合计 token（≈ 当前上下文占用）。
    pub context_tokens: u64,
    /// 当前模型的上下文窗口（token）；窗口未知时为 0，此时不显示占用率。
    pub context_window: u64,
}
