//! 扩展 UI 请求总线
//!
//! 扩展经 [`request_ui`] 提交 UI 请求，
//! TUI 事件循环每轮取一个（[`take_pending_ui`]）处理：Select → 打开通用自定义面板；
//! Notify → 聊天区系统消息。面板 Enter/Esc 结果经 [`Extension::on_ui_choice`] 回传（按请求 id 分发），
//! 返回 `Some(text)` 即触发下一轮 prompt。核心只提供队列与 id 分配，不感知具体扩展。
//!
//! # 需要「等用户作答」的扩展：用 [`ask_prompt`]
//!
//! `request_ui` 是发完即忘；只有 [`Extension::on_ui_choice`] 回调，没有「等一下再继续」。
//! 工具前门控那类必须当场拿到答案的扩展，用 [`ask_prompt`]：它入队同一个 Select 面板，
//! 然后 await 结果（可选择超时）。await 期间**不占线程**，future 被 drop（回合被 abort）
//! 即等于取消；没有 TUI 事件循环（非交互模式）时立即返回 `None`，调用方据此走 fail-closed 分支。

use crate::core::provider::AgentMessage;
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot::Sender;

/// 事件循环是否就绪（TUI 在监听扩展 UI 请求）。
///
/// 由 TUI 主循环在装/拆事件队列时置位。为 `false` 表示没有可交互的面板——
/// 非交互运行、测试、以及启动早期的窗口期都属于这一类，
/// 此时 [`ask_prompt`] 立刻返回 `None` 而不是傻等。
static UI_READY: AtomicBool = AtomicBool::new(false);

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
    /// 状态栏临时提示（不进聊天区）：文案写到状态栏，过期自动消失。
    /// 用于错误这类「一闪而过、不该留在对话里」的反馈。
    Status { text: String },
    /// 浮层 toast：几秒后自动消失的角标提示，`level` 决定配色。
    /// 宿主有浮层就画浮层（桌面 GUI），没有就退回状态栏（TUI）。
    Toast {
        /// 提示正文。
        text: String,
        /// 通知级别，决定浮层配色。
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

/// 设置事件循环就绪状态（TUI 主循环调用）。
pub fn set_ui_ready(ready: bool) {
    UI_READY.store(ready, Ordering::Relaxed);
}

/// 事件循环是否就绪（可供 [`ask_prompt`] 之外需要判断「有没有人看面板」的调用方使用）。
pub fn ui_ready() -> bool {
    UI_READY.load(Ordering::Relaxed)
}

/// 取等待表的锁；持锁线程 panic 过（中毒）时取回内部数据继续用。
///
/// 这里绝不能因为中毒就放弃：等待表一旦失效，所有扩展提问都会静默返回 `None`，
/// 表现为「门控突然全部拒绝」，排查起来毫无线索。
fn waiters_guard() -> MutexGuard<'static, HashMap<u64, Sender<Option<String>>>> {
    waiters().lock().unwrap_or_else(|e| e.into_inner())
}

/// 是否已经有提问在等用户作答。
///
/// 面板一次只展示一个：别处再发提问会把当前那个顶掉（被顶掉的一方收到取消）。
/// 需要「一次只问一个」的调用方先查这个，自己决定是排队还是立刻放弃。
pub fn ask_pending() -> bool {
    !waiters_guard().is_empty()
}

/// 正在等待某个提问结果的唤醒通道：请求 id → oneshot 发送端。
fn waiters() -> &'static Mutex<HashMap<u64, Sender<Option<String>>>> {
    /// 提问等待表：id → oneshot 发送端（[`ask_prompt`] 插入，[`submit_ui_choice`] 取走）。
    static W: OnceLock<Mutex<HashMap<u64, Sender<Option<String>>>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

/// [`ask_prompt`] 的等待表条目守卫：无论正常返回、超时还是 future 被 drop，都要注销。
///
/// future 被 drop（回合 abort / 面板被替换）时不会执行 `ask_prompt` 的收尾代码，
/// 只能靠 Drop 清掉槽位；否则等待表会一直挂着那个 id。
struct WaiterGuard(u64);

impl Drop for WaiterGuard {
    /// 从等待表里注销本条 id（发送端已失效时忽略）。
    fn drop(&mut self) {
        waiters_guard().remove(&self.0);
    }
}

/// 向用户提问并等待作答（异步版 Select 面板）。
///
/// 入队一条与 [`ExtensionUiRequest::Select`] 同形的请求，然后 await 用户的选择：
///
/// - 返回 `Some(text)`：用户选中的选项文本（与 [`SelectOption::text`] 相同）；
/// - 返回 `None`：用户按 Esc 取消、面板被别处替换、`timeout` 到期，
///   或者**没有 TUI 事件循环**（非交互模式）。
///
/// 调用方必须自己决定 `None` 的含义（门控类扩展应当按 fail-closed 处理）。
/// 等待期间不持有任何其它锁，因此不会被 UI 线程的操作卡住；回合被 abort 时
/// future 被 drop 即等价于取消（面板若还开着，用户按 Esc 即可关闭）。
pub async fn ask_prompt(
    title: impl Into<String>,
    options: Vec<SelectOption>,
    timeout: Option<Duration>,
) -> Option<String> {
    if !ui_ready() || options.is_empty() {
        return None;
    }

    let id = next_ui_id();
    let (tx, rx) = tokio::sync::oneshot::channel();
    waiters_guard().insert(id, tx);
    let _guard = WaiterGuard(id);

    request_ui(ExtensionUiRequest::Select {
        id,
        title: title.into(),
        options,
    });

    // 发送端被丢弃只发生在异常路径（正常取消走 submit_ui_choice(id, None)）。
    let wait = async { rx.await.unwrap_or_default() };
    match timeout {
        Some(limit) => tokio::time::timeout(limit, wait).await.ok().flatten(),
        None => wait.await,
    }
}

/// 面板 Enter/Esc 结果按 id 回传：遍历已启用扩展调用 [`Extension::on_ui_choice`]，
/// 返回第一个 `Some(text)` 作为 follow-up prompt（无则 None）。
pub fn submit_ui_choice(id: u64, choice: Option<String>) -> Option<String> {
    // 提问通道优先：命中就唤醒 ask_prompt 的等待者，且不触发 follow-up prompt
    // （发起方自己会接着往下走，再塞一条用户消息就是重复）。
    if let Some(tx) = waiters_guard().remove(&id) {
        _ = tx.send(choice);
        return None;
    }

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

    /// 跑一个 async 测试体（用独立 runtime，避免与其它测试共享全局 runtime）。
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(fut)
    }

    /// 取走队列里第一条 Select 请求的 id（丢弃其它请求）。
    ///
    /// 用真实 sleep 轮询而不是 `yield_now`：spawn 出去的任务跑在 runtime 的 worker
    /// 线程上，空转让出的时间不足以让它把请求入队。
    async fn take_select_id() -> u64 {
        for _ in 0..200 {
            if let Some(req) = take_pending_ui() {
                if let ExtensionUiRequest::Select { id, .. } = req {
                    return id;
                }
                continue;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no Select request was queued");
    }

    /// `ask_prompt` 的结果由 `submit_ui_choice` 送达，并且不触发 follow-up prompt。
    #[test]
    fn ask_prompt_resolves_through_submit() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while take_pending_ui().is_some() {}
        set_ui_ready(true);

        let answer = block_on(async {
            let waiter = tokio::spawn(async {
                ask_prompt(
                    "title",
                    vec![SelectOption::new("a"), SelectOption::new("b")],
                    Some(Duration::from_secs(5)),
                )
                .await
            });
            let id = take_select_id().await;
            assert!(ask_pending(), "等待表应已登记");
            // 命中提问通道时返回 None：发起方自己接着走，不该再塞一条用户消息。
            assert_eq!(submit_ui_choice(id, Some("b".to_string())), None);
            waiter.await.expect("waiter task")
        });

        assert_eq!(answer.as_deref(), Some("b"));
        assert!(!ask_pending(), "答复送达后等待表应清空");
        set_ui_ready(false);
    }

    /// 取消（Esc）就是 `submit_ui_choice(id, None)`。
    #[test]
    fn ask_prompt_cancel_returns_none() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while take_pending_ui().is_some() {}
        set_ui_ready(true);

        let answer = block_on(async {
            let waiter = tokio::spawn(async {
                ask_prompt("title", vec![SelectOption::new("a")], None).await
            });
            let id = take_select_id().await;
            submit_ui_choice(id, None);
            waiter.await.expect("waiter task")
        });

        assert_eq!(answer, None);
        set_ui_ready(false);
    }

    /// 没有 TUI 事件循环时立即返回 None（调用方据此走 fail-closed）。
    #[test]
    fn ask_prompt_without_ui_returns_immediately() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_ui_ready(false);
        let answer = block_on(ask_prompt(
            "title",
            vec![SelectOption::new("a")],
            Some(Duration::from_secs(5)),
        ));
        assert_eq!(answer, None);
    }

    /// 超时按未作答处理，并清掉等待表条目。
    #[test]
    fn ask_prompt_times_out() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while take_pending_ui().is_some() {}
        set_ui_ready(true);

        let answer = block_on(ask_prompt(
            "title",
            vec![SelectOption::new("a")],
            Some(Duration::from_millis(10)),
        ));
        assert_eq!(answer, None);
        assert!(!ask_pending(), "超时后等待表应清空");
        set_ui_ready(false);
    }
}
