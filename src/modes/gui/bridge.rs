//! 与 agent 的桥：命令下行、事件上行。

pub mod worker;

use serde_json::Value;

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
    History(Vec<crate::core::provider::AgentMessage>),
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
    pub model_cycle: Vec<crate::core::model_scope::ScopedModel>,
    /// 会话名；未命名为 None。
    pub session_name: Option<String>,
    /// 会话文件路径；内存会话（`--no-session`）为 None。
    pub session_path: Option<String>,
    /// 会话中的消息条数。
    pub messages: usize,
    /// 当前会话树的叶子条目 id（`/label` 等按 id 操作时用）；
    /// 内存会话或空树为 None。
    pub leaf_entry_id: Option<String>,
    /// 当前激活的工具名。
    pub tools: Vec<String>,
    /// 工作目录。
    pub cwd: String,
    /// 本会话可用的技能：`(名称, 描述, 来源)`。
    pub skills: Vec<(String, String, String)>,
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
}
