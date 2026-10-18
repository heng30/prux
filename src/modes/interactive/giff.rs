//! `/giff` 审阅视图：浏览工作树未提交改动（文件列表 + 双栏 / 统一 diff）。
//!
//! **只读**——本视图不修改任何文件，也不执行 rebase 或提交。
//!
//! 生命周期由 TUI 侧掌握：`/giff` 采集数据后把 [`GiffView`] 装进 `App`，渲染与按键
//! 分流见 [`crate::modes::interactive::render`] 与 `handlers`；按 `q` 退出时视图转成
//! [`GiffRestore`] 留在 `App` 上，下次 `/giff` 重新采集数据但恢复上次看的文件与滚动位置。

mod diff;
mod types;

pub(super) mod render;

#[cfg(test)]
mod tests;

use crate::modes::interactive::theme::{Theme, color};
use ratatui::{layout::Rect, style::Color};
use std::collections::HashMap;

pub use diff::{FileChanges, LineChange, collect_uncommitted};
pub use types::{Pane, ViewMode};

/// 视图用到的颜色集合：全部取自当前生效主题，只是「取色快照」
/// （不新建主题、不落盘、不可配置），主题切换后每帧重建。
pub struct GiffStyle {
    /// 强调色：徽标、获得焦点的窗格标题。
    pub accent: Color,
    /// 获得焦点的窗格边框色。
    pub border_focused: Color,
    /// 未获得焦点的窗格边框色。
    pub border_dim: Color,
    /// 正文前景色（终端默认前景）。
    pub fg_normal: Color,
    /// 次要文本：计数、说明、diff 行号列。
    pub fg_dim: Color,
    /// 高亮正文：当前文件名、模态框按键徽章文字。
    pub fg_bright: Color,
    /// 新增行的前景色（含文件列表里的 `+n` 统计）。
    pub fg_added: Color,
    /// 删除行的前景色（含文件列表里的 `-n` 统计）。
    pub fg_removed: Color,
    /// 底部键位行里的按键名颜色。
    pub fg_key: Color,
    /// diff 内容区行号列颜色。
    pub fg_line_num: Color,
    /// 新增标记 `+` 的颜色。
    pub fg_added_marker: Color,
    /// 删除标记 `-` 的颜色。
    pub fg_removed_marker: Color,
    /// header 行背景色（底部键位行同用）。
    pub bg_header: Color,
    /// 文件列表选中行背景色。
    pub bg_selection: Color,
    /// 新增行的整行背景色。
    pub bg_added: Color,
    /// 删除行的整行背景色。
    pub bg_removed: Color,
    /// 帮助模态框背景色。
    pub bg_modal: Color,
    /// 模态框外区域的压暗色；主题没有「压暗背景」语义键，取终端默认背景（不压暗）。
    pub bg_modal_dim: Color,
    /// 模态框边框色。
    pub border_modal: Color,
    /// 按键徽章背景色。
    pub bg_key_badge: Color,
    /// 模态框内分隔线颜色。
    pub fg_separator: Color,
    /// 按键徽章文字色。
    pub fg_badge: Color,
}

impl GiffStyle {
    /// 从当前主题解析出本视图需要的全部颜色；主题缺某个键时用给定 fallback，
    /// 键值是空串（终端默认色）时映射为 [`Color::Reset`]。
    pub fn from_theme(theme: &Theme) -> Self {
        let c = |key: &str, fallback: &str| {
            let value = theme.get(key, fallback);
            if value.is_empty() {
                Color::Reset
            } else {
                color(&value)
            }
        };

        Self {
            accent: c("accent", "violet"),
            border_focused: c("borderAccent", "#8abeb7"),
            border_dim: c("borderMuted", "#666666"),
            fg_normal: c("text", ""),
            fg_dim: c("dim", "#666666"),
            fg_bright: c("text", ""),
            fg_added: c("toolDiffAdded", "green"),
            fg_removed: c("toolDiffRemoved", "red"),
            fg_key: c("accent", "violet"),
            fg_line_num: c("dim", "#666666"),
            fg_added_marker: c("toolDiffAdded", "green"),
            fg_removed_marker: c("toolDiffRemoved", "red"),
            bg_header: c("selectedBg", "#2a2a3c"),
            bg_selection: c("selectedBg", "#2a2a3c"),
            bg_added: c("toolSuccessBg", "#1e3a2a"),
            bg_removed: c("toolErrorBg", "#3a1e1e"),
            bg_modal: c("customMessageBg", "#2a1e3a"),
            bg_modal_dim: Color::Reset,
            border_modal: c("borderAccent", "violet"),
            bg_key_badge: c("selectedBg", "#3a3a5c"),
            fg_separator: c("borderMuted", "#666666"),
            fg_badge: c("text", ""),
        }
    }
}

/// `/giff` 的跨次保留状态：按 `q` 退出后留在 `App` 上，下次 `/giff` 用它恢复
/// 「上次查看的文件、各文件滚动位置、焦点窗格、双栏/统一视图」。
///
/// 只保留位置类信息，不保留 diff 数据——数据每次重新采集（工作树可能已变），
/// 因此上次的文件若已不在本次改动里，就退回到第一个文件。
#[derive(Debug, Clone, Default)]
pub struct GiffRestore {
    /// 上次选中的文件路径；`None` = 还没有历史或上次没有文件。
    file: Option<String>,
    /// 文件路径 → 滚动行偏移（只有仍存在的文件会被恢复）。
    scroll_positions: HashMap<String, usize>,
    /// 上次的焦点窗格；`None` = 无历史，用默认（文件列表）。
    focused_pane: Option<Pane>,
    /// 上次的视图模式；`None` = 无历史，用默认（双栏）。
    view_mode: Option<ViewMode>,
}

/// `/giff` 审阅视图的全部状态。
pub struct GiffView {
    /// 各文件的左右行序列（每次 `/giff` 重新采集）。
    file_changes: FileChanges,
    /// 左侧标签（如 `HEAD`）。
    left_label: String,
    /// 右侧标签（如 `Working Tree`）。
    right_label: String,
    /// 按路径排序后的文件列表。
    file_names: Vec<String>,
    /// 当前选中文件在 [`Self::file_names`] 中的下标；列表为空时为 0。
    current_file_idx: usize,
    /// 文件路径 → 滚动行偏移。
    scroll_positions: HashMap<String, usize>,
    /// 当前焦点窗格。
    focused_pane: Pane,
    /// 双栏 / 统一视图。
    view_mode: ViewMode,
    /// 帮助模态是否打开（打开期间只接受关闭键）。
    show_help: bool,
    /// 是否处于输入态：`true` 时主页输入框显示并接收按键，`Esc` 回到导航。
    pub input_active: bool,
    /// 颜色快照（每帧由当前主题重建）。
    pub(super) style: GiffStyle,
    /// 缓存的 syntect 主题（语法高亮用），随主题指纹失效。
    pub(super) syntect_theme: Option<syntect::highlighting::Theme>,
    /// 生成 [`Self::syntect_theme`] 时的主题指纹。
    pub(super) syntect_fp: String,
    /// 最近一帧 diff 内容区的高度（翻页步长，减去上下边框）。
    page_rows: u16,
    /// 最近一帧文件列表区域（鼠标滚轮命中判定）。
    pub(super) file_list_area: Option<Rect>,
    /// 最近一帧 diff 内容区域（鼠标滚轮命中判定）。
    pub(super) content_area: Option<Rect>,
}

impl GiffView {
    /// 用本次采集到的改动构造视图，并按 `restore` 恢复上次的文件与滚动位置。
    pub fn new(
        file_changes: FileChanges,
        left_label: String,
        right_label: String,
        theme: &Theme,
        restore: &GiffRestore,
    ) -> Self {
        let mut file_names: Vec<String> = file_changes.keys().cloned().collect();
        file_names.sort();

        let current_file_idx = restore
            .file
            .as_ref()
            .and_then(|file| file_names.iter().position(|name| name == file))
            .unwrap_or(0);

        // 只恢复仍存在于本次改动里的文件的滚动位置
        let scroll_positions = restore
            .scroll_positions
            .iter()
            .filter(|(file, _)| file_names.contains(file))
            .map(|(file, offset)| (file.clone(), *offset))
            .collect();

        Self {
            file_changes,
            left_label,
            right_label,
            file_names,
            current_file_idx,
            scroll_positions,
            focused_pane: restore.focused_pane.unwrap_or(Pane::FileList),
            view_mode: restore.view_mode.unwrap_or(ViewMode::SideBySide),
            show_help: false,
            input_active: false,
            style: GiffStyle::from_theme(theme),
            syntect_theme: None,
            syntect_fp: String::new(),
            page_rows: 1,
            file_list_area: None,
            content_area: None,
        }
    }

    /// 转成跨次保留状态（按 `q` 退出时调用）。
    pub fn into_restore(self) -> GiffRestore {
        GiffRestore {
            file: self.file_names.get(self.current_file_idx).cloned(),
            scroll_positions: self.scroll_positions,
            focused_pane: Some(self.focused_pane),
            view_mode: Some(self.view_mode),
        }
    }

    /// 当前选中文件的路径；文件列表为空时返回 `None`。
    pub fn current_file(&self) -> Option<&str> {
        self.file_names
            .get(self.current_file_idx)
            .map(String::as_str)
    }

    /// 文件数（header 里的 `当前/总数`）。
    pub fn file_count(&self) -> usize {
        self.file_names.len()
    }

    /// 按绝对量移动 file 列表选择（`delta` 为 ±1 表示上下移动），越界夹回。
    pub fn move_file(&mut self, delta: isize) {
        if self.file_names.is_empty() {
            return;
        }
        let max = self.file_names.len() as isize - 1;
        self.current_file_idx = (self.current_file_idx as isize + delta).clamp(0, max) as usize;
    }

    /// 把选择移到第一个 / 最后一个文件。
    pub fn select_edge(&mut self, last: bool) {
        if self.file_names.is_empty() {
            return;
        }
        self.current_file_idx = if last { self.file_names.len() - 1 } else { 0 };
    }

    /// 滚动当前文件：`delta` 为行数（`usize::MAX` 表示滚到底，由渲染期夹回）。
    pub fn scroll_by(&mut self, delta: isize) {
        let Some(file) = self.current_file().map(str::to_string) else {
            return;
        };
        let current = self.scroll_positions.get(&file).copied().unwrap_or(0);
        let next = if delta == isize::MAX {
            usize::MAX
        } else if delta == isize::MIN {
            0
        } else {
            current.saturating_add_signed(delta)
        };
        self.scroll_positions.insert(file, next);
    }

    /// 按整屏翻页滚动当前文件，`forward` 为 `false` 时上翻。
    pub fn page(&mut self, forward: bool) {
        let step = self.page_rows.max(1) as isize;
        self.scroll_by(if forward { step } else { -step });
    }

    /// 把当前文件的滚动位置重置到顶部。
    pub fn scroll_to_top(&mut self) {
        self.scroll_by(isize::MIN);
    }

    /// 把当前文件的滚动位置置底（渲染期按内容长度夹回）。
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_by(isize::MAX);
    }

    /// 切换焦点窗格。
    pub fn toggle_focus(&mut self) {
        self.focused_pane = match self.focused_pane {
            Pane::FileList => Pane::DiffContent,
            Pane::DiffContent => Pane::FileList,
        };
    }

    /// 把焦点移到指定窗格。
    pub fn focus(&mut self, pane: Pane) {
        self.focused_pane = pane;
    }

    /// 双栏 ↔ 统一视图切换。
    pub fn toggle_view(&mut self) {
        self.view_mode = match self.view_mode {
            ViewMode::SideBySide => ViewMode::Unified,
            ViewMode::Unified => ViewMode::SideBySide,
        };
    }

    /// 开关帮助模态。
    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    /// 是否处于输入态（主页输入框显示并接收按键）。
    pub fn is_input_active(&self) -> bool {
        self.input_active
    }

    /// 进入 / 退出输入态；退出时不清空主页输入框内容。
    pub fn set_input_active(&mut self, active: bool) {
        self.input_active = active;
    }

    /// 帮助模态是否打开。
    pub fn is_help_open(&self) -> bool {
        self.show_help
    }

    /// 关闭帮助模态。
    pub fn close_help(&mut self) {
        self.show_help = false;
    }

    /// 当前焦点窗格（鼠标命中与渲染断言用）。
    pub fn focused_pane(&self) -> Pane {
        self.focused_pane
    }

    /// 最近一帧文件列表所占区域（鼠标滚轮命中判定）。
    pub fn file_list_area(&self) -> Option<Rect> {
        self.file_list_area
    }

    /// 最近一帧内容区可显示的行数（翻页步长）。
    pub fn page_rows(&self) -> usize {
        self.page_rows.max(1) as usize
    }

    /// 当前视图模式（渲染断言用）。
    pub fn view_mode(&self) -> ViewMode {
        self.view_mode
    }
}
