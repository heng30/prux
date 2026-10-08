//! 扩展覆盖层的 TUI 侧状态（内容由扩展持有，这里只持"快照 + 滚动 + 渲染缓存 + 内联输入 + 选择"）。
//!
//! 生命周期：
//! - `ShowOverlay { id }` → [`open`]，之后每帧 [`sync`] 拉取最新内容；
//! - 扩展返回 `None` / 发出 `HideOverlay` / 用户 Esc → [`close`]（并回传 [`OverlayEvent::Closed`] 让扩展清理状态）；
//! - 会话切换（`/new` `/resume` …）也走 [`close`]：覆盖层展示的是某会话的子代理。
//!
//! 内容分两部分：
//! - `view.lines` 是扩展**预渲染**的 `DockLine`（列表类覆盖层），渲染期转成 ratatui 行；
//! - `view.messages` 是需要 TUI 用**主页同一条管线**渲染的 `AgentMessage`（查看器），
//!   在 [`sync`] 里带缓存渲染并存入 [`OverlayState::message_lines`]。
//!
//! 鼠标：覆盖层自己处理滚轮、右侧滚动条拖动、内容拖选与中键复制。滚动语义是
//! **顶部偏移**（0 = 顶部），与主页消息区"距底部偏移"相反，因此坐标/拖动映射各写一份。

use crate::{
    core::extensions::{self, OverlayEvent, OverlayKey, OverlaySize, OverlayView},
    modes::interactive::{
        app::App, editor::Editor, line_input::InputBox, render::messages::MessageCache,
    },
    utils::display::char_width,
};
use ratatui::{layout::Rect, text::Line};

/// 编辑器覆盖层里除正文以外的固定行数（标签行 + 标签后的空行），与 [`super::render::overlay`] 保持一致。
pub(crate) const EDITOR_CHROME_ROWS: usize = 2;

/// `OverlaySize::Panel` 上下两条 `─` 分割线占用的行数。
pub(crate) const PANEL_BORDER_ROWS: usize = 2;

/// 边缘自动滚动每 tick 滚动行数（与主页消息区一致）。
const DRAG_EDGE_STEP: usize = 2;
/// 指针停留在边缘的 tick 数（满阈值才持续滚动，避免选择边缘行时误触）。
const EDGE_DWELL_TICKS: u8 = 3;

/// 覆盖层内容的鼠标选择 / 滚动条拖拽状态。
///
/// 行号是**内容全局行**（`scroll + 视口行`），滚动/追加时稳定。
#[derive(Debug, Clone, Default)]
pub struct OverlaySel {
    /// 选择区间：(起始行, 起始字符, 结束行, 结束字符)
    pub sel: Option<(usize, usize, usize, usize)>,
    /// 拖拽进行中
    pub dragging: bool,
    /// 最近一次拖拽事件的屏幕行（边缘自动滚动 tick 用）
    pub drag_row: Option<u16>,
    /// 指针停留在边缘的 tick 计数
    pub edge_dwell: u8,
    /// 最近一帧内容区屏幕起始行 / 高度
    pub content_y: u16,
    /// 最近一帧内容区可显示的行数，供滚轮与选择坐标换算
    pub content_rows: u16,
    /// 最近一帧可见行的纯文本（命中测试用）
    pub text: Vec<String>,
    /// 滚动条所在列（渲染时记录；无滚动条为 None）
    pub scroll_x: Option<u16>,
    /// 最近一帧滚动条 thumb 在 track 内的相对区间 (start, len)
    pub scroll_thumb: Option<(usize, usize)>,
    /// 滚动条拖拽中
    pub scroll_dragging: bool,
    /// 滚动条按下基准：(按下 row, 抓取偏移)
    pub scroll_press: Option<(u16, usize)>,
}

/// 覆盖层状态。
pub struct OverlayState {
    /// 扩展分配的 id（内容拉取与事件回传都用它）
    pub id: u64,
    /// 最近一帧的快照
    pub view: OverlayView,
    /// 顶部行偏移
    pub scroll: usize,
    /// 内联输入（`view.input` 为 Some 时可见；扩展持有真值，这里持编辑中的副本）
    pub input: InputBox,
    /// 多行编辑器（`view.editor` 为 Some 时激活；核心持编辑缓冲与光标）
    pub editor: Editor,
    /// 编辑器缓冲对应的 generation（None = 无编辑器）
    editor_generation: Option<u64>,
    /// 输入行是否处于激活态（false = 按键走导航，输入只显示标签/占位）
    pub input_active: bool,
    /// 最近一帧渲染区域（鼠标命中测试）
    pub area: Option<Rect>,
    /// 最近一帧的内容区行数（滚轮滚动用）
    pub content_rows: usize,
    /// 上一帧的 `view.selected`（只在它**变化**时自动滚动，避免手动滚动被拉回选中行）
    last_selected: Option<usize>,
    /// 上一帧扩展请求的输入聚焦态（只在变化沿动作）
    prev_focus_request: bool,
    /// 滚动区里由 `view.messages` 渲染出的行（主页管线；带缓存）
    pub message_lines: Vec<Line<'static>>,
    /// `view.messages` 的渲染缓存
    msg_cache: MessageCache,
    /// 选择 + 滚动条拖拽状态
    pub sel: OverlaySel,
}

impl OverlayState {
    /// 按 overlay 视图构造状态：输入框回填 `view.input` 初值、编辑器清空，
    /// 滚动/选择/渲染缓存取默认值（`last_selected`、`prev_focus_request` 作为变化沿基准）。
    fn new(id: u64, view: OverlayView) -> Self {
        let mut input = InputBox::new();
        if let Some(i) = &view.input {
            input.set_value(&i.value);
        }
        let mut editor = Editor::new();
        editor.clear();
        OverlayState {
            id,
            view,
            scroll: 0,
            input,
            editor,
            editor_generation: None,
            input_active: false,
            area: None,
            content_rows: 1,
            last_selected: None,
            prev_focus_request: false,
            message_lines: Vec::new(),
            msg_cache: MessageCache::default(),
            sel: OverlaySel::default(),
        }
    }

    /// 滚动区总行数（预渲染行 + 渲染后的消息行）。
    pub fn len(&self) -> usize {
        self.view.lines.len() + self.message_lines.len()
    }

    /// 内容为空（空列表的覆盖层仍显示标题/底栏，故此处只用于 clippy 的成对约定）。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 视口内可见行数（至少 1 行，避免除零/死循环）。
    pub fn viewport(&self) -> usize {
        self.content_rows.max(1)
    }

    /// 最大合法顶部偏移。
    pub fn max_scroll(&self) -> usize {
        self.len().saturating_sub(self.viewport())
    }

    /// 是否需要滚动条（内容超视口）。
    pub fn scrollable(&self) -> bool {
        self.len() > self.viewport()
    }

    /// 内联输入当前文本（`None` = 该覆盖层没有输入行）。
    pub fn input_text(&self) -> Option<String> {
        self.view.input.as_ref().map(|_| self.input.value.clone())
    }

    /// 是否存在多行编辑器。
    pub fn has_editor(&self) -> bool {
        self.view.editor.is_some()
    }

    /// 取内容全局行 `g` 的纯文本（选择提取用；DockLine 用原始文本，消息行用渲染后的 span 文本）。
    fn content_text_at(&self, g: usize) -> Option<String> {
        if g < self.view.lines.len() {
            return Some(
                self.view.lines[g]
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<String>(),
            );
        }
        let i = g - self.view.lines.len();
        self.message_lines.get(i).map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
    }
}

/// 打开（或替换）覆盖层。
pub fn open(st: &mut App, id: u64, view: OverlayView) {
    // 换主：先告知旧覆盖层"已被替换"，否则它的状态永远等不到 Closed
    if let Some(old) = st.overlay.as_ref().map(|o| o.id)
        && old != id
    {
        extensions::overlay_event(old, OverlayEvent::Closed);
    }
    st.overlay = Some(OverlayState::new(id, view));
    st.dirty = true;
}

/// 关闭覆盖层（回传 [`OverlayEvent::Closed`]）。
pub fn close(st: &mut App) {
    if let Some(o) = st.overlay.take() {
        extensions::overlay_event(o.id, OverlayEvent::Closed);
        st.dirty = true;
    }
}

/// 每帧刷新：拉取内容、渲染消息、保持滚动位置合理、同步扩展持有的输入值。
///
/// 扩展不再认领该 id（返回 `None`）时自动关闭（对齐 dock 的"内容为空自动隐藏"）。
pub fn sync(st: &mut App, cols: u16) {
    let Some(id) = st.overlay.as_ref().map(|o| o.id) else {
        return;
    };
    let Some(view) = extensions::overlay_view(id, cols) else {
        close(st);
        return;
    };

    // 内容区渲染宽度：右侧恒留 1 列给滚动条（与渲染层一致）。
    let avail = (cols as usize).saturating_sub(1).max(1);

    let App {
        overlay,
        theme,
        expand_all,
        show_thinking,
        dirty,
        ..
    } = st;
    let Some(state) = overlay.as_mut() else {
        return;
    };

    // 更新前的内容长度与贴底判定：新增内容到达时，只有原本就在底部才继续贴底
    // （覆盖层没有持久 follow；上滚即停止跟随，与主页一致）。
    let prev_len = state.len();
    let was_at_bottom = state.scroll + state.viewport() >= prev_len;

    // 消息行：带缓存渲染（追加式列表只渲染新增项）
    let mut cache = std::mem::take(&mut state.msg_cache);
    let msg_lines = cache.render(&view.messages, avail, theme, *expand_all, *show_thinking);
    state.msg_cache = cache;

    // 内容区行数：Inline/Panel 由 max_rows 扣掉边框/标题/底栏/输入行；Fullscreen 由布局在
    // 渲染时回填，这里沿用上一帧的值（首帧先用一个保守值）。
    let rows = match view.size {
        OverlaySize::Inline { max_rows } => {
            inline_content_rows(&view, msg_lines.len(), max_rows, 0)
        }
        OverlaySize::Panel { max_rows } => {
            inline_content_rows(&view, msg_lines.len(), max_rows, PANEL_BORDER_ROWS)
        }
        OverlaySize::Fullscreen => state.content_rows.max(1),
    };

    // 多行编辑器：只在 generation 变化时装载（同一 generation 的每帧拉取不重置用户编辑）。
    match view.editor.as_ref().map(|e| e.generation) {
        Some(generation) if state.editor_generation != Some(generation) => {
            if let Some(ed) = &view.editor {
                state.editor.clear();
                state.editor.insert_text(&ed.value);
                state.editor.cursor_line = 0;
                state.editor.cursor_col = 0;
            }
            state.editor_generation = Some(generation);
        }
        Some(_) => {}
        None => state.editor_generation = None,
    }

    // 输入真值以扩展为准（扩展在 Submit 后清空 value）；但**编辑中不覆盖**用户正在敲的文本。
    if let (Some(ext_input), true) = (&view.input, !state.input_active) {
        let cur = state.input.value.clone();
        if ext_input.value != cur {
            state.input.set_value(&ext_input.value);
        }
    }
    let gained_input = state.view.input.is_none() && view.input.is_some();
    let lost_input = state.view.input.is_some() && view.input.is_none();
    let focus_edge = view.input_focus != state.prev_focus_request;
    let focus_request = view.input_focus;

    state.view = view;
    state.message_lines = msg_lines;
    state.content_rows = rows.max(1);
    if lost_input {
        state.input_active = false;
        state.input.clear();
    }
    if gained_input {
        // 新出现的输入行：聚焦它（查看器里按 i 打开 steer 输入）
        state.input_active = true;
    }
    if focus_edge {
        // 扩展显式请求聚焦/失焦（只在变化沿响应，避免覆盖用户的"打字即聚焦"）
        state.input_active = focus_request;
        state.prev_focus_request = focus_request;
    }

    update_scroll(state, was_at_bottom);
    *dirty = true;
}

/// 更新滚动位置（纯函数，便于测试）：
/// - "更新前已贴底" → 重新贴底（查看器持续输出时保持跟随；上滚后不再跟随）；
/// - 否则若扩展声明的 `selected` **发生了变化**（键盘导航），保证它进入视口。
///   手动滚轮/拖动滚动条不改变 `selected`，因此不会被拉回选中行（否则滚动条一拖就回弹）。
/// - 最后夹取到合法范围。
pub fn update_scroll(state: &mut OverlayState, was_at_bottom: bool) {
    let viewport = state.viewport();
    if was_at_bottom {
        state.scroll = state.max_scroll();
    } else if let Some(sel) = state.view.selected
        && state.last_selected != Some(sel)
    {
        if sel < state.scroll {
            state.scroll = sel;
        } else if sel >= state.scroll + viewport {
            state.scroll = sel + 1 - viewport;
        }
    }
    state.last_selected = state.view.selected;
    state.scroll = state.scroll.min(state.max_scroll());
}

/// 内联/面板覆盖层应占的总行数（含边框/标题/底栏/输入行）；Fullscreen 返回 0（由布局另行分配）。
pub fn outer_rows(st: &App) -> usize {
    let Some(o) = st.overlay.as_ref() else {
        return 0;
    };
    match o.view.size {
        OverlaySize::Fullscreen => 0,
        OverlaySize::Inline { max_rows } => {
            (o.len() + chrome_rows(&o.view, 0)).min(max_rows.max(3) as usize)
        }
        OverlaySize::Panel { max_rows } => {
            (o.len() + chrome_rows(&o.view, PANEL_BORDER_ROWS)).min(max_rows.max(3) as usize)
        }
    }
}

/// 覆盖层除内容外的固定行数（标题/分割线/空行/底栏/输入行/编辑器）。
/// 面板样式无标题行，但有上/中/下 3 个空行（见 [`super::render::overlay`] 的布局）。
fn chrome_rows(view: &OverlayView, border_rows: usize) -> usize {
    let editor = view
        .editor
        .as_ref()
        .map(|e| EDITOR_CHROME_ROWS + e.rows as usize)
        .unwrap_or(0);
    let title_or_blanks = if border_rows > 0 { 3 } else { 1 };
    border_rows
        + title_or_blanks
        + usize::from(!view.footer.is_empty())
        + usize::from(view.input.is_some())
        + editor
}

/// Inline/Panel 覆盖层的内容区行数（总高 = 内容 + 固定行，上限 `max_rows`）。
fn inline_content_rows(
    view: &OverlayView,
    message_rows: usize,
    max_rows: u16,
    border_rows: usize,
) -> usize {
    (max_rows.max(3) as usize)
        .saturating_sub(chrome_rows(view, border_rows))
        .max(1)
        .min((view.lines.len() + message_rows).max(1))
}

/// 内联输入提交（把当前文本回传扩展；是否清空由扩展决定，下一帧 sync 同步）。
///
/// 提交后**立即清空并退出输入态**：扩展通常只把 `OverlayInput.value` 置空，
/// 而 [`sync`] 在 `input_active` 时不会用扩展值覆盖本地编辑，所以若不复位，
/// 用户按下 Enter 后会看到文本原样留在输入框里（像是"回车没反应"）；
/// 失焦后下一帧 sync 也会正常拿到扩展给的新值。
pub fn submit_input(st: &mut App) {
    let Some(state) = st.overlay.as_mut() else {
        return;
    };

    let text = state.input.value.clone();
    let id = state.id;
    state.input.clear();
    state.input_active = false;
    extensions::overlay_event(id, OverlayEvent::Submit { text });
    st.dirty = true;
}

/// 多行编辑器保存（Ctrl+S）：把完整文本回传扩展。
pub fn submit_editor(st: &mut App) {
    let Some(state) = st.overlay.as_ref() else {
        return;
    };
    let text = state.editor.expanded_text();
    let id = state.id;
    extensions::overlay_event(id, OverlayEvent::EditorSubmit { text });
    st.dirty = true;
}

/// 按键 → [`OverlayEvent::Key`]（含当前输入内容），回传扩展；返回是否被扩展消费。
///
/// 被消费即置 `dirty`：扩展往往只改自己持有的状态（列表选中项、检查器选中行），
/// 内容要等下一帧 `sync` 拉取才可见。若这里不置位，空闲时 `should_repaint()` 恒为false，
/// 事件循环不会重绘，键盘在列表/检查器里就"看起来无效"
/// （除选中项不动外，`p`/`s`/`r`/`x`/`c` 等控制键的界面反馈同样丢失）。
pub fn send_key(st: &mut App, key: OverlayKey) -> bool {
    let Some((id, input)) = st.overlay.as_ref().map(|s| (s.id, s.input_text())) else {
        return false;
    };

    let consumed = extensions::overlay_event(id, OverlayEvent::Key { key, input });
    if consumed {
        st.dirty = true;
    }
    consumed
}

/// 鼠标滚轮滚动（`delta` 为行数增量，向上为负）。
pub fn scroll_by(st: &mut App, delta: isize) {
    let Some(state) = st.overlay.as_mut() else {
        return;
    };
    let max = state.max_scroll() as isize;
    let next = (state.scroll as isize + delta).clamp(0, max);
    if next as usize != state.scroll {
        state.scroll = next as usize;
        st.dirty = true;
    }
}

/// 指针是否落在"覆盖层应独占鼠标"的区域。
///
/// 全屏覆盖层接管整屏（主页消息区/停靠面板根本没渲染，不能让陈旧命中状态参与）；
/// inline 覆盖层只吃自己矩形内的鼠标，区域外仍归主页。
pub fn captures_mouse(st: &App, col: u16, row: u16) -> bool {
    let Some(o) = st.overlay.as_ref() else {
        return false;
    };
    if o.view.size == OverlaySize::Fullscreen {
        return true;
    }
    o.area
        .is_some_and(|a| col >= a.x && col < a.x + a.width && row >= a.y && row < a.y + a.height)
}

/// 左键按下：滚动条列优先（抓取/跳转），否则开始内容拖选。
pub fn left_down(st: &mut App, col: u16, row: u16) {
    let Some(state) = st.overlay.as_mut() else {
        return;
    };
    // 多行编辑器覆盖层不参与内容选择（编辑器自己管光标）。
    if state.has_editor() {
        return;
    }
    state.sel.dragging = false;
    state.sel.scroll_dragging = false;
    state.sel.scroll_press = None;
    state.sel.drag_row = None;
    state.sel.edge_dwell = 0;

    if state.sel.scroll_x == Some(col) {
        state.sel.sel = None;
        state.sel.scroll_dragging = true;

        if scrollbar_press_on_thumb(state, row) {
            let off = row
                .saturating_sub(state.sel.content_y)
                .saturating_sub(state.sel.scroll_thumb.map(|(s, _)| s as u16).unwrap_or(0));
            state.sel.scroll_press = Some((row, off as usize));
        } else {
            scrollbar_click_jump(state, row);
        }
        st.dirty = true;
        return;
    }

    match hit(state, col, row) {
        Some((line, ch)) => {
            state.sel.sel = Some((line, ch, line, ch));
            state.sel.dragging = true;
        }
        None => state.sel.sel = None,
    }
    st.dirty = true;
}

/// 左键拖动：滚动条抓取拖动优先，否则内容拖选（视口边缘自动滚动）。
pub fn left_drag(st: &mut App, col: u16, row: u16) {
    let Some(state) = st.overlay.as_mut() else {
        return;
    };
    if state.has_editor() {
        return;
    }
    if state.sel.scroll_dragging {
        if state.sel.scroll_press.is_some() {
            scrollbar_drag_to(state, row);
        }
        st.dirty = true;
        return;
    }
    if !state.sel.dragging {
        return;
    }

    state.sel.drag_row = Some(row);
    let y = state.sel.content_y;
    let h = state.sel.content_rows.max(1);
    let at_edge = row <= y || row.saturating_add(1) >= y + h;
    if !at_edge {
        state.sel.edge_dwell = 0;
    }
    if let Some((line, ch)) = hit(state, col, row) {
        set_drag_point(state, line, ch);
        st.dirty = true;
        return;
    }
    if (row < y && scroll_edge(state, true)) || (row >= y + h && scroll_edge(state, false)) {
        st.dirty = true;
    }
}

/// 左键松开：结束拖选与滚动条抓取。
pub fn left_up(st: &mut App) {
    let Some(state) = st.overlay.as_mut() else {
        return;
    };
    state.sel.dragging = false;
    state.sel.scroll_dragging = false;
    state.sel.scroll_press = None;
    state.sel.drag_row = None;
    state.sel.edge_dwell = 0;
    st.dirty = true;
}

/// 覆盖层是否正在拖拽（内容选择或滚动条抓取）。
pub fn is_dragging(st: &App) -> bool {
    st.overlay
        .as_ref()
        .is_some_and(|o| o.sel.dragging || o.sel.scroll_dragging)
}

/// 清除选择（中键复制后调用）。
pub fn clear_selection(st: &mut App) {
    if let Some(state) = st.overlay.as_mut() {
        state.sel.sel = None;
        state.sel.dragging = false;
    }
    st.dirty = true;
}

/// 覆盖层输入行是否处于聚焦态（中键粘贴/字符写入的前置条件）。
pub fn input_focused(st: &App) -> bool {
    st.overlay
        .as_ref()
        .is_some_and(|o| o.view.input.is_some() && o.input_active)
}

/// 把文本粘进覆盖层输入行（单行控件：换行压成空格）。
pub fn paste_into_input(st: &mut App, text: &str) {
    let Some(state) = st.overlay.as_mut() else {
        return;
    };
    if state.view.input.is_some() && state.input_active {
        let flat = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
        state.input.insert_text(&flat);
        st.dirty = true;
    }
}

/// 覆盖层当前选中的文本（内容全局行区间重建）。
pub fn selected_text(st: &App) -> Option<String> {
    let state = st.overlay.as_ref()?;
    let (ls, cs, le, ce) = state.sel.sel?;
    let (ls, cs, le, ce) = if (ls, cs) <= (le, ce) {
        (ls, cs, le, ce)
    } else {
        (le, ce, ls, cs)
    };

    let mut out = String::new();
    for g in ls..=le {
        let Some(text) = state.content_text_at(g) else {
            continue;
        };
        let chars: Vec<char> = text.chars().collect();
        let a = if g == ls { cs.min(chars.len()) } else { 0 };
        let b = if g == le {
            ce.min(chars.len())
        } else {
            chars.len()
        };
        if a >= b {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.extend(chars[a..b].iter());
    }

    (!out.is_empty()).then_some(out)
}

/// 事件循环 tick：拖选时指针停在内容区边缘 → 滞留满阈值后持续自动滚动。
/// 返回是否发生了滚动。
pub fn auto_scroll_tick(st: &mut App) -> bool {
    let Some(state) = st.overlay.as_mut() else {
        return false;
    };
    if !state.sel.dragging {
        state.sel.edge_dwell = 0;
        return false;
    }
    let Some(row) = state.sel.drag_row else {
        state.sel.edge_dwell = 0;
        return false;
    };
    let y = state.sel.content_y;
    let h = state.sel.content_rows.max(1);
    let at_top = row <= y;
    let at_bottom = row.saturating_add(1) >= y + h;
    if !at_top && !at_bottom {
        state.sel.edge_dwell = 0;
        return false;
    }
    if state.sel.edge_dwell < EDGE_DWELL_TICKS {
        state.sel.edge_dwell += 1;
        return false;
    }
    scroll_edge(state, at_top)
}

/// 命中测试：屏幕 (col,row) → 内容全局行号 + 字符偏移（内容区内）。
fn hit(state: &OverlayState, col: u16, row: u16) -> Option<(usize, usize)> {
    let y = state.sel.content_y;
    let h = state.sel.content_rows.max(1);
    if row < y || row >= y + h {
        return None;
    }
    let line = (row - y) as usize;
    let text = state.sel.text.get(line)?;
    let mut w = 0usize;
    for (i, c) in text.chars().enumerate() {
        if col as usize <= w {
            return Some((state.scroll + line, i));
        }
        w += char_width(c);
    }
    Some((state.scroll + line, text.chars().count()))
}

/// 拖拽时更新选择终点（起始点不动）。
fn set_drag_point(state: &mut OverlayState, line: usize, ch: usize) {
    if let Some((ls, cs, _, _)) = state.sel.sel {
        state.sel.sel = Some((ls, cs, line, ch));
    }
}

/// 拖拽边缘滚动一步：`up=true` 上滚（显示更早内容，scroll 减小），
/// `up=false` 下滚（scroll 增大）；选中终点钉在新视口边缘行。
/// 返回是否实际滚动（已到边缘返回 false）。
fn scroll_edge(state: &mut OverlayState, up: bool) -> bool {
    let max = state.max_scroll();
    if up {
        if state.scroll == 0 {
            return false;
        }
        state.scroll = state.scroll.saturating_sub(DRAG_EDGE_STEP);
        let start = state.scroll;
        set_drag_point(state, start, 0);
    } else {
        if state.scroll >= max {
            return false;
        }
        state.scroll = (state.scroll + DRAG_EDGE_STEP).min(max);
        let end_line = (state.scroll + state.viewport()).saturating_sub(1);
        set_drag_point(state, end_line, usize::MAX);
    }
    true
}

/// 按下位置是否落在 thumb 区间内（抓取而非跳转）。
fn scrollbar_press_on_thumb(state: &OverlayState, row: u16) -> bool {
    let Some((start, len)) = state.sel.scroll_thumb else {
        return false;
    };
    let rel = row
        .saturating_sub(state.sel.content_y)
        .min(state.sel.content_rows.saturating_sub(1)) as usize;
    rel >= start && rel < start + len
}

/// 由 thumb 目标顶行（track 内相对位置）反算顶部偏移。
///
/// 渲染端把两端映射到 `position = 0` / `position = total-1`（保证 thumb 贴顶/贴底），
/// 这里取其精确反函数：`position = rel*denom/track`，`scroll = position*max_scroll/(total-1)`。
/// 因此拖动时 thumb 始终跟手，不会因为线性近似而在抓起时跳变。
fn scroll_for_thumb_top(state: &OverlayState, rel: usize) -> usize {
    let track = state.sel.content_rows.max(1) as usize;
    let total = state.len();
    let max_scroll = state.max_scroll();
    if max_scroll == 0 || total <= 1 {
        return 0;
    }

    let denom = (total - 1 + track).max(1);
    let thumb_len =
        ((track as f64 * track as f64 / denom as f64).round() as usize).clamp(1, track.max(1));
    let max_start = track.saturating_sub(thumb_len);
    let rel = rel.min(max_start);

    // thumb 已到 track 底 → 内容结尾（保证拖到底能看到最后一行）
    if rel >= max_start {
        return max_scroll;
    }

    let position = (rel * denom) as f64 / track as f64;
    let scroll = (position * max_scroll as f64 / (total - 1) as f64).round() as usize;
    scroll.min(max_scroll)
}

/// 点击 track 空白：thumb 顶对齐点击行。覆盖层是顶部偏移语义：
/// track 顶 → scroll=0（内容开头），track 底 → scroll=max_scroll（内容结尾）。
fn scrollbar_click_jump(state: &mut OverlayState, row: u16) {
    let rel = row.saturating_sub(state.sel.content_y) as usize;
    let scroll = scroll_for_thumb_top(state, rel);
    state.scroll = scroll;
    state.sel.scroll_press = Some((row, 0));
}

/// 抓取拖动映射：thumb 顶目标 = 鼠标行 - grabOffset（按下时固定，保持跟手不跳变），
/// 再用 [`scroll_for_thumb_top`] 反算 scroll。
fn scrollbar_drag_to(state: &mut OverlayState, row: u16) {
    let Some((_, grab_offset)) = state.sel.scroll_press else {
        return;
    };
    let rel = row
        .saturating_sub(state.sel.content_y)
        .saturating_sub(grab_offset as u16) as usize;
    let scroll = scroll_for_thumb_top(state, rel);
    state.scroll = scroll;
}

/// 渲染层回填内容区几何 + 可见行文本（鼠标命中/选择用）。
pub fn record_content_geometry(
    state: &mut OverlayState,
    content_y: u16,
    content_rows: u16,
    text: Vec<String>,
) {
    state.sel.content_y = content_y;
    state.sel.content_rows = content_rows;
    state.sel.text = text;
}

/// 渲染层回填滚动条几何（`None` = 无滚动条）。
pub fn record_scrollbar(
    state: &mut OverlayState,
    scroll_x: Option<u16>,
    thumb: Option<(usize, usize)>,
) {
    state.sel.scroll_x = scroll_x;
    state.sel.scroll_thumb = thumb;
}

/// 选择高亮区间（内容全局行），供渲染层套用。
pub fn selection_range(state: &OverlayState) -> Option<(usize, usize, usize, usize)> {
    let (ls, cs, le, ce) = state.sel.sel?;
    Some(if (ls, cs) <= (le, ce) {
        (ls, cs, le, ce)
    } else {
        (le, ce, ls, cs)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{DockSpan, OverlayEditor, OverlayInput, OverlayView};

    fn view(lines: usize, selected: Option<usize>) -> OverlayView {
        OverlayView {
            title: "Agents".to_string(),
            lines: (0..lines)
                .map(|i| vec![DockSpan::plain(format!("line{i}"))])
                .collect(),
            footer: vec![("Esc".to_string(), "close".to_string())],
            input: None,
            editor: None,
            size: OverlaySize::Inline { max_rows: 6 },
            selected,
            input_focus: false,
            ..OverlayView::inline("x", Vec::new(), 6)
        }
    }

    fn with_overlay(id: u64, v: OverlayView) -> App {
        let mut st = App::new();
        open(&mut st, id, v);
        st
    }

    #[test]
    fn inline_rows_account_for_chrome_and_cap() {
        let v = view(20, None);
        // 上限 6：扣标题 1 + 底栏 1 → 内容 4 行
        assert_eq!(inline_content_rows(&v, 0, 6, 0), 4);
        let mut no_footer = v.clone();
        no_footer.footer.clear();
        assert_eq!(inline_content_rows(&no_footer, 0, 6, 0), 5);
        let mut with_input = no_footer.clone();
        with_input.input = Some(OverlayInput {
            label: "steer>".to_string(),
            value: String::new(),
            placeholder: String::new(),
        });
        assert_eq!(inline_content_rows(&with_input, 0, 6, 0), 4);
        // 内容少于可用行数时按内容给高
        let short = view(2, None);
        assert_eq!(inline_content_rows(&short, 0, 6, 0), 2);
        // 消息行计入内容高度（不足上限时按内容给高，超出则被 max_rows 截）
        assert_eq!(inline_content_rows(&short, 1, 6, 0), 3);
        assert_eq!(inline_content_rows(&short, 3, 6, 0), 4);
        // 面板样式：多扣上下两条分割线与 3 个空行（上限 6 时内容只剩 1 行）
        assert_eq!(inline_content_rows(&v, 0, 6, PANEL_BORDER_ROWS), 1);
        assert_eq!(inline_content_rows(&v, 0, 12, PANEL_BORDER_ROWS), 6);
    }

    #[test]
    fn scroll_clamps_and_tracks_selection() {
        let mut st = with_overlay(1, view(20, None));
        {
            let o = st.overlay.as_mut().unwrap();
            o.content_rows = 4;
            assert_eq!(o.max_scroll(), 16);
            assert!(o.scrollable());
            o.scroll = 99;
        }
        scroll_by(&mut st, 0);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 16, "滚轮越界夹回");
        scroll_by(&mut st, -3);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 13);
    }

    #[test]
    fn stick_to_bottom_only_when_already_at_bottom() {
        // 原本贴底 → 新内容到达后继续贴底
        let mut st = with_overlay(1, view(20, None));
        {
            let o = st.overlay.as_mut().unwrap();
            o.content_rows = 5;
            o.scroll = 15; // 贴底（15 + 5 == 20）
        }
        update_scroll(st.overlay.as_mut().unwrap(), true);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 15);
        // 内容变长后贴底位置随之下移
        {
            let o = st.overlay.as_mut().unwrap();
            o.view
                .lines
                .extend((0..5).map(|i| vec![DockSpan::plain(format!("extra{i}"))]));
        }
        update_scroll(st.overlay.as_mut().unwrap(), true);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 20, "贴底跟随新行");

        // 不在底部：不贴底，保持原位置
        let mut st = with_overlay(2, view(20, None));
        st.overlay.as_mut().unwrap().content_rows = 6;
        st.overlay.as_mut().unwrap().scroll = 3;
        update_scroll(st.overlay.as_mut().unwrap(), false);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 3, "上滚后不再跟随");

        // selected 在视口下方 → 滚到它可见
        let mut st = with_overlay(3, view(20, Some(12)));
        st.overlay.as_mut().unwrap().content_rows = 5;
        update_scroll(st.overlay.as_mut().unwrap(), false);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 8, "选中行贴底对齐");
        // selected 在视口上方 → 上滚到它
        st.overlay.as_mut().unwrap().scroll = 10;
        st.overlay.as_mut().unwrap().view.selected = Some(3);
        update_scroll(st.overlay.as_mut().unwrap(), false);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 3);
    }

    /// 手动滚动（滚轮/滚动条）不应被“选中行跟随”拉回：只有 selected **变化**时才跟随。
    #[test]
    fn manual_scroll_is_not_yanked_back_to_unchanged_selection() {
        let mut st = with_overlay(1, view(40, Some(20)));
        st.overlay.as_mut().unwrap().content_rows = 5;

        // 首帧：跟随到选中行
        update_scroll(st.overlay.as_mut().unwrap(), false);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 16);

        // 手动上滚（选中行不变）→ 不被拉回
        st.overlay.as_mut().unwrap().scroll = 0;
        update_scroll(st.overlay.as_mut().unwrap(), false);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 0, "选中行未变，不跟随");

        // 选中行变化 → 重新跟随
        st.overlay.as_mut().unwrap().view.selected = Some(30);
        update_scroll(st.overlay.as_mut().unwrap(), false);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 26);
    }

    /// Inline 覆盖层的编辑器占位：标题 1 + 底栏 1 + 标签 1 + `rows`（上限 max_rows）。
    #[test]
    fn outer_rows_accounts_for_the_editor_region() {
        let mut v = view(3, None);
        v.editor = Some(OverlayEditor {
            label: "file".to_string(),
            value: String::new(),
            placeholder: String::new(),
            rows: 4,
            generation: 1,
        });
        let st = with_overlay(1, v);
        // 内容3 + 标题1 + 底栏1 + 标签与空行2 + 编辑器4 = 11，被 max_rows=6 截
        assert_eq!(outer_rows(&st), 6);
    }

    /// 扩展消费按键后必须置 `dirty`：否则空闲时 `should_repaint()` 为 false，
    /// 扩展改掉的选中项/检查器状态不会被重绘，键盘在列表里就"看起来无效"。
    #[test]
    fn consumed_key_marks_the_app_dirty() {
        use crate::core::extensions::{
            Extension, ExtensionHook, register_extension, unregister_extension,
        };

        struct ConsumingExt;

        impl Extension for ConsumingExt {
            fn name(&self) -> &str {
                "overlay-dirty-test-ext"
            }
            fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
                Vec::new()
            }
            fn hooks(&self) -> Vec<ExtensionHook> {
                vec![ExtensionHook::Overlay]
            }
            fn overlay_view(&self, id: u64, _cols: u16) -> Option<OverlayView> {
                (id == 77).then(|| OverlayView::inline("t", Vec::new(), 5))
            }
            fn on_overlay_event(&self, id: u64, ev: &OverlayEvent) -> bool {
                id == 77 && matches!(ev, OverlayEvent::Key { .. })
            }
        }

        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        register_extension(ConsumingExt);

        let mut st = App::new();
        open(&mut st, 77, OverlayView::inline("t", Vec::new(), 5));
        st.dirty = false;
        assert!(send_key(&mut st, OverlayKey::Down), "扩展应消费按键");
        assert!(st.dirty, "被消费的按键必须触发重绘");

        // 无主覆盖层：不消费就不置 dirty（`send_key` 的返回值与 dirty 必须一致）
        let mut st2 = App::new();
        open(&mut st2, 78, OverlayView::inline("t", Vec::new(), 5));
        st2.dirty = false;
        let consumed = send_key(&mut st2, OverlayKey::Down);
        assert_eq!(st2.dirty, consumed, "dirty 只在被消费时置位");

        unregister_extension("overlay-dirty-test-ext");
    }

    #[test]
    fn submit_sends_current_input_text_and_resets_the_box() {
        let mut v = view(3, None);
        v.input = Some(OverlayInput {
            label: "steer>".to_string(),
            value: String::new(),
            placeholder: "type…".to_string(),
        });
        let mut st = with_overlay(42, v);
        st.overlay.as_mut().unwrap().input.insert_text("hello");
        st.overlay.as_mut().unwrap().input_active = true;
        assert_eq!(
            st.overlay.as_ref().unwrap().input_text().as_deref(),
            Some("hello")
        );
        submit_input(&mut st); // 无扩展认领 → 不 panic（事件被丢弃）
        // 提交后清空并退出输入态：否则回车看起来"没反应"（文本原地不动）
        assert_eq!(st.overlay.as_ref().unwrap().input.value, "");
        assert!(!st.overlay.as_ref().unwrap().input_active);
    }

    /// 扩展在打开时请求聚焦输入（`input_focus: true`）→ 首帧 `sync` 后输入即激活。
    #[test]
    fn input_focus_request_is_applied_on_the_first_sync() {
        use crate::core::extensions::{
            Extension, ExtensionHook, register_extension, unregister_extension,
        };

        struct FocusExt;

        impl Extension for FocusExt {
            fn name(&self) -> &str {
                "overlay-focus-test-ext"
            }
            fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
                Vec::new()
            }
            fn hooks(&self) -> Vec<ExtensionHook> {
                vec![ExtensionHook::Overlay]
            }
            fn overlay_view(&self, id: u64, _cols: u16) -> Option<OverlayView> {
                (id == 1_000_077).then(|| OverlayView {
                    input: Some(OverlayInput {
                        label: "resume>".to_string(),
                        value: String::new(),
                        placeholder: String::new(),
                    }),
                    input_focus: true,
                    ..OverlayView::inline("t", Vec::new(), 5)
                })
            }
        }

        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        register_extension(FocusExt);

        let mut st = App::new();
        let v = extensions::overlay_view(1_000_077, 80).expect("认领");
        open(&mut st, 1_000_077, v);
        sync(&mut st, 80);
        assert!(
            st.overlay.as_ref().unwrap().input_active,
            "input_focus 应在首帧聚焦输入（直接打字就生效）"
        );

        unregister_extension("overlay-focus-test-ext");
    }

    /// 滚动条拖动映射：覆盖层是顶部偏移语义（track 顶 = 内容开头）。
    #[test]
    fn overlay_scrollbar_drag_maps_to_top_offset() {
        let mut st = with_overlay(1, view(120, None));
        {
            let o = st.overlay.as_mut().unwrap();
            o.content_rows = 10;
            record_content_geometry(o, 0, 10, Vec::new());
        }
        let max = st.overlay.as_ref().unwrap().max_scroll(); // 110

        // 点击 track 顶 → 内容开头
        {
            let o = st.overlay.as_mut().unwrap();
            scrollbar_click_jump(o, 0);
        }
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 0, "track 顶 = 开头");
        // 点击 track 底 → 内容结尾
        {
            let o = st.overlay.as_mut().unwrap();
            scrollbar_click_jump(o, 9);
        }
        assert_eq!(st.overlay.as_ref().unwrap().scroll, max, "track 底 = 结尾");

        // 抓取 thumb 拖到 track 顶 → 开头
        {
            let o = st.overlay.as_mut().unwrap();
            o.scroll = 50;
            o.sel.scroll_press = Some((5, 0));
            scrollbar_drag_to(o, 0);
        }
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 0, "拖到顶 = 开头");
        {
            let o = st.overlay.as_mut().unwrap();
            o.sel.scroll_press = Some((5, 0));
            scrollbar_drag_to(o, 9);
        }
        assert_eq!(st.overlay.as_ref().unwrap().scroll, max, "拖到底 = 结尾");
    }

    /// 抓住 thumb 并拖到同一行不应跳变（拖动是渲染端 position 映射的精确反函数）。
    #[test]
    fn grabbing_the_thumb_does_not_jump() {
        let mut st = with_overlay(1, view(1000, None));
        {
            let o = st.overlay.as_mut().unwrap();
            o.content_rows = 30;
            record_content_geometry(o, 0, 30, Vec::new());
        }
        let max = st.overlay.as_ref().unwrap().max_scroll();
        let (total, track) = (1000usize, 30usize);
        let denom = (total - 1 + track).max(1);
        let thumb_len =
            ((track as f64 * track as f64 / denom as f64).round() as usize).clamp(1, track);

        for scroll in [0usize, 100, 500, max] {
            // 渲染端 position 映射 → thumb_start
            let position = if max == 0 {
                0
            } else {
                ((total - 1) as f64 * (scroll as f64 / max as f64)).round() as usize
            };
            let thumb_start = ((position as f64 * track as f64 / denom as f64).round() as usize)
                .min(track - thumb_len);

            // 抓住 thumb 顶（grab_offset=0），拖到同一行
            {
                let o = st.overlay.as_mut().unwrap();
                o.scroll = scroll;
                o.sel.scroll_press = Some((thumb_start as u16, 0));
                scrollbar_drag_to(o, thumb_start as u16);
            }
            let after = st.overlay.as_ref().unwrap().scroll;
            assert!(
                after.abs_diff(scroll) <= 1,
                "scroll={scroll} thumb_start={thumb_start} → after={after}"
            );
        }
    }

    /// 选择提取：跨行区间按内容全局行切片。
    #[test]
    fn selected_text_slices_content_lines() {
        let mut st = with_overlay(1, view(3, None));
        {
            let o = st.overlay.as_mut().unwrap();
            o.sel.sel = Some((0, 5, 1, 3));
        }
        // line0 = "line0"，从第 5 字符起为空；line1 = "line1"，取前 3 字符
        let text = selected_text(&st).unwrap_or_default();
        assert!(text.contains("lin"), "{text:?}");
    }

    /// 全屏覆盖层独占鼠标；inline 只吃自己矩形内。
    #[test]
    fn captures_mouse_respects_overlay_size() {
        let mut st = App::new();
        open(
            &mut st,
            1,
            OverlayView {
                size: OverlaySize::Fullscreen,
                ..OverlayView::inline("t", Vec::new(), 5)
            },
        );
        assert!(captures_mouse(&st, 0, 0), "全屏任意位置都归覆盖层");
        assert!(captures_mouse(&st, 200, 200));

        let mut st2 = App::new();
        open(&mut st2, 1, OverlayView::inline("t", Vec::new(), 5));
        st2.overlay.as_mut().unwrap().area = Some(Rect::new(0, 5, 40, 5));
        assert!(captures_mouse(&st2, 10, 6), "inline 矩形内");
        assert!(!captures_mouse(&st2, 10, 20), "inline 矩形外归主页");
    }
}
