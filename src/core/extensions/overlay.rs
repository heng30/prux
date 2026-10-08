//! 扩展覆盖层接口：输入区上方（Inline）或全屏（Fullscreen）的自持面板。
//!
//! 与 [`super::dock`] 的分工：
//! - **dock** = 只读汇总条（扩展被动提供行，用户不能在其中导航）；
//! - **overlay** = 交互面板（扩展**自持**选中项/滚动状态，按键回传给扩展）。
//!
//! 与 `Select` 面板（`PanelKind::Custom`）的分工：`Select` 是核心定义语义的"选一个"，
//! 覆盖层是扩展定义语义的任意交互面（列表导航、实时会话查看、内联输入）。
//!
//! 数据流（拉取式，与 dock 一致）：
//! 1. 扩展 `request_ui(ShowOverlay { id })` 请求打开；
//! 2. TUI 每帧调 [`overlay_view`] 拉取当前内容（扩展只读自身状态，不加 agent 锁）；
//! 3. 按键经 [`overlay_event`] 回传（扩展返回 `true` 表示已消费，未消费走 TUI 默认语义）；
//! 4. 扩展可 `request_ui(HideOverlay { id })` 自行关闭；TUI 关闭时回调
//!    [`OverlayEvent::Closed`] 让扩展清理状态。
//!
//! 锁序约定：`overlay_view` / `overlay_event` 都在 TUI 线程调用，实现里**不得**
//! 加 agent 锁（与 `/agents` 的 `busy_safe = true` 同一条约束）。

use super::{dock::DockLine, hooks::ExtensionHook, registry};
use crate::core::provider::AgentMessage;

/// 覆盖层尺寸档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlaySize {
    /// 输入框上方的内联面板（行数上限；内容超出由扩展/滚动处理）
    Inline { max_rows: u16 },
    /// 选择器面板样式：占据**输入框区域**（主页输入框隐藏），内容上下各一条 `─` 分割线，
    /// 与核心的 `Select` 面板（`/model` `/session` `/agents list` 等）视觉一致。
    Panel { max_rows: u16 },
    /// 全屏覆盖：整个聊天区 + 状态栏 + 输入框区域都交给覆盖层
    Fullscreen,
}

/// 覆盖层底部的内联输入行（如查看器里的 steer 输入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayInput {
    /// 输入行左侧标签（如 `steer>`）
    pub label: String,
    /// 当前文本（扩展持有真值；TUI 编辑后回传）
    pub value: String,
    /// 空输入时的提示（占位）
    pub placeholder: String,
}

/// 覆盖层里的多行编辑器（编辑 agent 文件正文、向导的系统提示等）。
///
/// 与 [`OverlayInput`] 的分工：单行输入把文本编辑完全交给扩展（字符/退格逐键回传），
/// 多行编辑器则由**核心持编辑缓冲与光标**，扩展只给初值与标签、在保存时收到整段文本
/// （[`OverlayEvent::EditorSubmit`]）。这样左/右/删除/换行/翻页这些真正需要编辑器的键
/// 才有地方落，扩展也不必自己实现文本编辑器。
///
/// `generation` 是"重新装载"信号：扩展换了编辑对象（或想丢弃本地编辑）时自增，
/// 核心只在 generation 变化时用 `value` 覆盖当前缓冲；同一 generation 的每帧拉取
/// 不会重置用户正在敲的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayEditor {
    /// 编辑器上方的标签（如文件路径 / `System prompt`）
    pub label: String,
    /// 初次装载（或 generation 变化）时的文本
    pub value: String,
    /// 文本为空时的提示（占位）
    pub placeholder: String,
    /// 可见高度（行数）；内容超出时随光标滚动。
    /// `0` = 填满剩余区域（Fullscreen 编辑器用）
    pub rows: u16,
    /// 重新装载信号（见类型文档）
    pub generation: u64,
}

/// 一帧覆盖层内容（TUI 每帧拉取；扩展返回 `None` 表示不再需要该覆盖层）。
#[derive(Debug, Clone)]
pub struct OverlayView {
    /// 覆盖层标题（面板边框或内联头部展示）。
    pub title: String,
    /// **固定区**行：标题行下方、不随滚动移动（查看器的状态头/路径/活动行/回显）。
    pub header: Vec<DockLine>,
    /// **滚动区**的预渲染行（复用 dock 的带样式行类型）。
    pub lines: Vec<DockLine>,
    /// **滚动区**中需要 TUI 渲染的消息（主页同一条管线）。渲染后拼在 `lines` 之后。
    /// 扩展层拿不到 `Theme`，所以消息由 TUI 侧渲染，而不是扩展自己排版。
    pub messages: Vec<AgentMessage>,
    /// 底部键位提示：`(键, 说明)`，TUI 拼成一行
    pub footer: Vec<(String, String)>,
    /// 底部内联输入行；无需输入时为 None。
    pub input: Option<OverlayInput>,
    /// 多行编辑器（与 `input` 互斥使用；同时给出时优先渲染编辑器）
    pub editor: Option<OverlayEditor>,
    /// 尺寸档位：Inline（输入框上方）/ Panel / Fullscreen。
    pub size: OverlaySize,
    /// 当前选中行下标（列表类覆盖层用；TUI 会滚动以保持它在视口内）
    pub selected: Option<usize>,
    /// 是否请求聚焦内联输入。TUI 只在**变化沿**响应（false→true 聚焦、true→false 失焦），
    /// 因此用户可以靠打字自行聚焦而不被每帧重置。
    pub input_focus: bool,
}

impl OverlayView {
    /// 便捷构造：只有标题与内容行（Inline 尺寸、无输入）。
    pub fn inline(title: impl Into<String>, lines: Vec<DockLine>, max_rows: u16) -> Self {
        OverlayView {
            title: title.into(),
            header: Vec::new(),
            lines,
            messages: Vec::new(),
            footer: Vec::new(),
            input: None,
            editor: None,
            size: OverlaySize::Inline { max_rows },
            selected: None,
            input_focus: false,
        }
    }
}

/// 覆盖层按键（与终端库解耦；TUI 侧翻译 crossterm 事件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayKey {
    /// 上方向键。
    Up,
    /// 下方向键。
    Down,
    /// 左方向键。
    Left,
    /// 右方向键。
    Right,
    /// 上翻页键（PageUp）。
    PageUp,
    /// 下翻页键（PageDown）。
    PageDown,
    /// Home：跳到列表或文本开头。
    Home,
    /// End：跳到列表或文本末尾。
    End,
    /// 回车：提交内联输入或确认选中项。
    Enter,
    /// 退出键：未被扩展消费时 TUI 关闭覆盖层。
    Esc,
    /// 退格：删除光标前一个字符。
    Backspace,
    /// Delete：删除光标处的字符。
    Delete,
    /// Tab：切换焦点或补全。
    Tab,
    /// 可打印字符（含中文等非 ASCII）
    Char(char),
    /// 其它键（扩展一般忽略）
    Other,
}

/// 回传给扩展的覆盖层事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayEvent {
    /// 按键；`input` 是当前内联输入内容（无输入行时为 None）
    Key {
        /// 被 TUI 翻译后的按键。
        key: OverlayKey,
        /// 该按键发生时内联输入框的文本；无输入行时为 None。
        input: Option<String>,
    },
    /// 内联输入提交（Enter）
    Submit { text: String },
    /// 多行编辑器保存（Ctrl+S）：携带完整文本，扩展据此落盘/推进
    EditorSubmit { text: String },
    /// 覆盖层被 TUI 关闭（Esc / 会话切换 / 扩展注销）：扩展应清理状态
    Closed,
}

/// 拉取指定覆盖层的内容：遍历**已启用**且声明了 [`ExtensionHook::Overlay`] 的扩展，
/// 返回第一个认领 `id` 的快照。无扩展认领时返回 None（TUI 视为"该覆盖层已消失"）。
/// 拉取覆盖层内容。`cols` 是终端宽度——渲染器要知道它才能自己排版（如两栏），
/// 而核心的 `OverlayView` 只是一列行，不认识"栏"。
pub fn overlay_view(id: u64, cols: u16) -> Option<OverlayView> {
    for ext in registry::registered() {
        if !ext.hooks().contains(&ExtensionHook::Overlay) {
            continue;
        }
        if let Some(view) = ext.overlay_view(id, cols) {
            return Some(view);
        }
    }
    None
}

/// 把事件回传给拥有该覆盖层的扩展；返回 `true` 表示扩展已消费。
///
/// 未认领 `id` 时返回 `false`（TUI 走默认语义：Esc 关闭）。
pub fn overlay_event(id: u64, ev: OverlayEvent) -> bool {
    for ext in registry::registered() {
        if !ext.hooks().contains(&ExtensionHook::Overlay) {
            continue;
        }
        if ext.on_overlay_event(id, &ev) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{Extension, register_extension, unregister_extension};

    struct OverlayExt;

    impl Extension for OverlayExt {
        fn name(&self) -> &str {
            "overlay-test-ext"
        }
        fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
            Vec::new()
        }
        fn hooks(&self) -> Vec<ExtensionHook> {
            vec![ExtensionHook::Overlay]
        }
        fn overlay_view(&self, id: u64, _cols: u16) -> Option<OverlayView> {
            (id == 7).then(|| OverlayView::inline("t", Vec::new(), 5))
        }
        fn on_overlay_event(&self, id: u64, ev: &OverlayEvent) -> bool {
            matches!(
                (id, ev),
                (
                    7,
                    OverlayEvent::Key {
                        key: OverlayKey::Char('s'),
                        ..
                    }
                )
            )
        }
    }

    /// 只声明 Dock（不声明 Overlay）的扩展不应被轮询。
    struct DockOnlyExt;

    impl Extension for DockOnlyExt {
        fn name(&self) -> &str {
            "overlay-dock-only-ext"
        }
        fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
            Vec::new()
        }
        fn hooks(&self) -> Vec<ExtensionHook> {
            vec![ExtensionHook::Dock]
        }
        fn overlay_view(&self, _id: u64, _cols: u16) -> Option<OverlayView> {
            panic!("未声明 Overlay 的扩展不应被轮询");
        }
    }

    #[test]
    fn view_and_event_dispatch_respect_hook_gating_and_ownership() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        register_extension(OverlayExt);
        register_extension(DockOnlyExt);

        let view = overlay_view(7, 80).expect("认领 id=7");
        assert_eq!(view.title, "t");
        assert_eq!(view.size, OverlaySize::Inline { max_rows: 5 });
        assert!(overlay_view(8, 80).is_none(), "未认领的 id 返回 None");

        assert!(overlay_event(
            7,
            OverlayEvent::Key {
                key: OverlayKey::Char('s'),
                input: None
            }
        ));
        assert!(!overlay_event(
            7,
            OverlayEvent::Key {
                key: OverlayKey::Char('x'),
                input: None
            }
        ));
        assert!(
            !overlay_event(9, OverlayEvent::Closed),
            "无主事件返回 false"
        );

        unregister_extension("overlay-test-ext");
        unregister_extension("overlay-dock-only-ext");
    }
}
