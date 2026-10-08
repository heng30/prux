//! 扩展 UI 请求总线
//!
//! 扩展经 [`request_ui`] 提交 UI 请求，
//! TUI 事件循环每轮取一个（[`take_pending_ui`]）处理：Select → 打开通用自定义面板；
//! Notify → 聊天区系统消息。面板 Enter/Esc 结果经 [`Extension::on_ui_choice`] 回传（按请求 id 分发），
//! 返回 `Some(text)` 即触发下一轮 prompt。核心只提供队列与 id 分配，不感知具体扩展。

use crate::core::provider::AgentMessage;
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Mutex, OnceLock},
};

/// 通知级别（TUI 映射到自身 MsgLevel）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiNotifyLevel {
    /// 普通信息，用于中性提示。
    Info,
    /// 操作成功，如设置已生效。
    Success,
    /// 错误，表示操作失败或异常。
    Error,
    /// 警告，提示可能有问题但未失败。
    Warning,
}

/// 通知里的带样式文本片段：`fg` 为主题键（"text"/"dim"/"success"/"muted" 等）
/// 或 hex（如 "#8abeb7"），None = 跟随消息级别默认色。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichSpan {
    /// 前景色：主题键或 hex 色值；None = 跟随消息级别默认色。
    pub fg: Option<String>,
    /// 该片段要渲染的文本。
    pub text: String,
}

impl RichSpan {
    /// 使用默认色（跟随消息级别配色）的片段。
    pub fn plain(text: impl Into<String>) -> Self {
        RichSpan {
            fg: None,
            text: text.into(),
        }
    }

    /// 使用指定前景色的片段：`key` 为主题键（如 "success"）或 hex 色值。
    pub fn fg(key: &str, text: impl Into<String>) -> Self {
        RichSpan {
            fg: Some(key.to_string()),
            text: text.into(),
        }
    }
}

/// 选择面板的一个选项。
///
/// `text` 是**回传串**（扩展按首个 token 的 key 分发，见 `choice_key`），
/// `fg` 只是**展示色**：主题键（如 `"success"`/`"accent"`/`"text"`）或 hex（`"#b5bd68"`），
/// `None` 走面板默认配色（未选中 = 默认色，选中 = accent）。
/// 展示色不进回传串，扩展改配色不会影响分发匹配。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    /// 选项文本：既在面板里显示，也作为选择结果回传。
    pub text: String,
    /// 展示用前景色（主题键或 hex）；None = 面板默认配色。
    pub fg: Option<String>,
}

impl SelectOption {
    /// 默认配色的选项。
    pub fn new(text: impl Into<String>) -> Self {
        SelectOption {
            text: text.into(),
            fg: None,
        }
    }

    /// 指定展示色（主题键或 hex）的选项。
    pub fn styled(text: impl Into<String>, fg: &str) -> Self {
        SelectOption {
            text: text.into(),
            fg: Some(fg.to_string()),
        }
    }
}

/// 扩展发起的 UI 请求
#[derive(Debug, Clone)]
pub enum ExtensionUiRequest {
    /// 选择面板：弹出通用自定义选择面板，Enter/Esc 结果经 on_ui_choice 回传
    Select {
        /// 请求 id，Enter/Esc 结果按它回传给发起扩展。
        id: u64,
        /// 选择面板的标题。
        title: String,
        /// 供选择的候选选项。
        options: Vec<SelectOption>,
    },
    /// 纯通知（聊天区系统消息，无后续交互）
    Notify { text: String, level: UiNotifyLevel },
    /// 富文本通知（聊天区系统消息）：spans 逐段配色，丢弃纯文本里无法渲染的标记
    NotifyRich {
        /// 逐段携带配色的文本片段。
        spans: Vec<RichSpan>,
        /// 通知级别，决定默认配色与展示样式。
        level: UiNotifyLevel,
    },
    /// 扩展自定义消息块（聊天区卡片）：TUI 按 `custom_type` 反查渲染（见
    /// [`super::Extension::render_custom_message`]），扩展自行决定是否同时
    /// `PersistSessionEntry` 落盘（落盘后由扩展在 `on_session_switched` 重放）。
    CustomMessage {
        /// 卡片左侧标签（如 `[subagent]`）
        label: String,
        /// 渲染分派键（扩展自己的命名空间，如 `subagent-completion`）
        custom_type: String,
        /// 渲染载荷（JSON；扩展自定形状）
        data: Value,
    },
    /// 请求打开扩展覆盖层（内容由 [`super::overlay::overlay_view`] 按需拉取）。
    /// TUI 打开后每帧拉取内容、把按键经 `on_overlay_event` 回传；`id` 由扩展分配
    /// （与 `Select` 的 `next_ui_id` 同一 id 空间，但**不复用** `Select` 的回传通道）。
    ShowOverlay { id: u64 },
    /// 扩展自己关闭覆盖层（如列表已空）。TUI 关闭时会回调 `OverlayEvent::Closed`。
    HideOverlay { id: u64 },
    /// 请求显示停靠面板（状态栏上方可滚动面板）：扩展在"需要立即可见"时请求，
    /// TUI 置 `dock_visible = true`；是否有内容由渲染层结合各扩展 `dock_lines` 判定。
    ShowDock,
    /// 请求打开指定扩展的设置面板（占据输入框区域，对齐 /settings）：
    /// 一次只展示 `ext` 一个扩展的设置项（扩展名不存在 / 未声明设置时面板为空）。
    /// TUI 显示期间接管键盘（type to search；↑/↓/Space/Enter；Esc/Ctrl+C 隐藏）。
    ShowSettings { ext: String },
    /// 扩展自主续跑：投一条消息并触发下一轮 prompt。
    /// TUI 处理时若仍忙碌（用户 steer/follow-up 已 drained 并启动新一轮）则跳过，
    /// 避免延续抢跑用户输入；空闲则入 `initial_queue` 由 `take_next_action` 发送。
    /// `Box` 避免枚举因内联 AgentMessage 而特大。
    Continuation { message: Box<AgentMessage> },
    /// 扩展请求中止当前回合（如 loop-detect 检测到死循环）：
    /// TUI 转 `AgentCommand::Abort` 丢弃进行中的回合，并像 Esc 一样复位本地
    /// busy/流式状态；与 Esc 的区别是不取回用户排队输入。
    AbortRun,
    /// 扩展请求「中断并重开一轮」（如 hang-detect 检测到假死）：
    /// 先按 [`Self::AbortRun`] 中止进行中的回合，再把运行中 steer 注入箱里的排队消息
    /// 与 `message` 合成一个 user 批次，经 `AgentCommand::PromptBatch` 立即作为新一轮
    /// prompt 投出。**不动**本地 follow-up 队列：follow-up 的语义是「本轮 settle 后才发送」，
    /// 保持中断前的行为，由新一轮结算后的 `drain_next_batch` 自然取走。
    /// `Box` 避免枚举因内联 AgentMessage 而特大。
    RestartRun { message: Box<AgentMessage> },
    /// 扩展请求回退最后一次用户输入（`/rewind`）：TUI 转 `AgentCommand::Rewind`，
    /// worker 把活动分支叶子移到最后一条 user 消息之前——该消息与之后产生的所有输出
    /// 一起离开模型上下文（历史仍保留在会话树上，可用 `/tree` 找回）。
    /// `restore_to_editor` 决定被回退的用户输入是否回填输入框（true = 回填供修改重发，
    /// false = 丢弃）；结果经 `App::on_rewind_done` 回执。
    RewindLastUserInput { restore_to_editor: bool },
    /// 请求重建工具列表（扩展工具可见性变化后，如 goal 激活/暂停时
    /// `get_goal`/`update_goal` 的显隐切换需要下一轮 compose 生效）。
    /// TUI 转 `AgentCommand::RebuildTools`；worker 空闲时执行，下一轮生效。
    RebuildTools,
    /// 向当前会话追加自定义条目（扩展持久化；经 `append_custom_entry` 落盘，
    /// 完成后由 worker 发 `extension:entry_persisted` 事件回执）。
    /// 供无 `App` 访问的扩展路径（on_agent_event / 工具执行）使用。
    PersistSessionEntry { custom_type: String, data: Value },
}

/// 进程内常驻的扩展 UI 请求队列，首次调用时懒加载。
fn queue() -> &'static Mutex<VecDeque<ExtensionUiRequest>> {
    /// 扩展 UI 请求队列：每轮 TUI 至多取出并处理一个请求，先到先得。
    static Q: OnceLock<Mutex<VecDeque<ExtensionUiRequest>>> = OnceLock::new();
    Q.get_or_init(Default::default)
}

/// 全局自增请求 id（Select 面板回传时按 id 匹配扩展）
pub fn next_ui_id() -> u64 {
    /// 全局自增的 UI 请求 id，Select 面板回传时按 id 匹配发起扩展。
    static ID: AtomicU64 = AtomicU64::new(1);
    ID.fetch_add(1, Ordering::Relaxed)
}

/// 扩展提交 UI 请求（入队；每轮至多处理一个，先到先得）
pub fn request_ui(req: ExtensionUiRequest) {
    queue().lock().unwrap().push_back(req);
}

/// 请求显示停靠面板（幂等）：队列里已有 `ShowDock` 时不重复入队。
///
/// 一次 fan-out 会派发几十个 agent，每个都想"展示 dock"；若逐个入队，
/// 队列里的完成通知（`Continuation`/`CustomMessage`）会被挤到几十轮之后
/// （TUI 每轮只取一个请求）。展示意图本来就是幂等的，合并不丢信息。
pub fn request_show_dock() {
    let mut q = queue().lock().unwrap();
    if q.iter().any(|r| matches!(r, ExtensionUiRequest::ShowDock)) {
        return;
    }
    q.push_back(ExtensionUiRequest::ShowDock);
}

/// TUI 取走下一个待处理请求（取走即消费）
pub fn take_pending_ui() -> Option<ExtensionUiRequest> {
    queue().lock().unwrap().pop_front()
}

/// 按 `custom_type` 找认领它的扩展并渲染卡片内容；无扩展认领返回 `None`（TUI 退化为纯文本块，不丢信息）。
pub fn render_custom_message(custom_type: &str, data: &Value) -> Option<Vec<RichSpan>> {
    for ext in super::registered() {
        if let Some(spans) = ext.render_custom_message(custom_type, data) {
            return Some(spans);
        }
    }
    None
}

/// 面板 Enter/Esc 结果按 id 回传：遍历已启用扩展调用 [`Extension::on_ui_choice`]，
/// 返回第一个 `Some(text)` 作为 follow-up prompt（无则 None）。
pub fn submit_ui_choice(id: u64, choice: Option<String>) -> Option<String> {
    for ext in super::registered() {
        if let Some(text) = ext.on_ui_choice(id, choice.clone()) {
            return Some(text);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `request_show_dock` 幂等：队列里已有一条 ShowDock 时不重复入队
    /// （一次 fan-out 派发几十个 agent 时，重复请求会把完成通知挤到几十轮之后）。
    #[test]
    fn show_dock_requests_are_deduped() {
        // UI 队列是进程级全局态，与 plan-mode/goal/subagent 的测试串行。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while take_pending_ui().is_some() {}

        request_show_dock();
        request_show_dock();
        request_ui(ExtensionUiRequest::RebuildTools);
        request_show_dock();

        let mut dock = 0;
        let mut others = 0;
        while let Some(req) = take_pending_ui() {
            match req {
                ExtensionUiRequest::ShowDock => dock += 1,
                _ => others += 1,
            }
        }
        assert_eq!(dock, 1, "重复的展示意图合并为一条");
        assert_eq!(others, 1, "其它请求不受影响");

        // 上一条被取走后，新的展示请求可以再次入队
        request_show_dock();
        assert!(matches!(
            take_pending_ui(),
            Some(ExtensionUiRequest::ShowDock)
        ));
    }
}
