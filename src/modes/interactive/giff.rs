//! `/giff` 审阅视图：浏览工作树未提交改动（文件列表 + 双栏 / 统一 diff）。
//!
//! **只读**——本视图不修改任何文件，也不执行 rebase 或提交。
//!
//! 本模块持有视图的**数据与状态**：`git diff` 的采集与解析（[`collect_uncommitted`] /
//! [`parse`]）、基础枚举（[`Pane`] / [`ViewMode`]）、配色快照（[`GiffStyle`]）、跨次保留
//! 状态（[`GiffRestore`]）与状态机本体（[`GiffView`]）。渲染在
//! [`crate::modes::interactive::render::giff`]，按键与鼠标在
//! [`crate::modes::interactive::handlers::giff`]。
//!
//! 生命周期由 TUI 侧掌握：`/giff` 采集数据后把 [`GiffView`] 装进 `App`，按 `q` 退出时
//! 视图转成 [`GiffRestore`] 留在 `App` 上，下次 `/giff` 重新采集数据但恢复上次看的文件
//! 与滚动位置。

use crate::{
    error::{Error, Result},
    modes::interactive::theme::{Theme, color},
    utils::wheel::KeyRepeatAccelerator,
};
use ratatui::{layout::Rect, style::Color, text::Line};
use regex::Regex;
use std::{collections::HashMap, process::Command, sync::LazyLock};

/// 一行改动：(源文件里的行号, 行文本)；行号为 `0` 表示左右对齐时补的占位空行。
pub type LineChange = (usize, String);

/// 一个文件的改动：左侧 `base`（`-` 行与上下文行）、右侧 `head`（`+` 行与上下文行）。
pub type FileChanges = HashMap<String, (Vec<LineChange>, Vec<LineChange>)>;

/// `diff --git a/<旧路径> b/<新路径>` 的文件头；取第二个捕获组（`b/` 侧路径）作为文件名。
static DIFF_FILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^diff --git a/(.+) b/(.+)$").unwrap());

/// hunk 头 `@@ -<base 起始行>[,行数] +<head 起始行>[,行数] @@`，用于恢复两侧行号。
static HUNK_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@@ -(\d+),?\d* \+(\d+),?\d* @@").unwrap());

/// ANSI 颜色转义：已加 `--no-color`，但用户 `diff.color` 配置可能仍强制上色，这里兜底剥掉。
static ANSI_ESCAPE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[.*?m").unwrap());

/// 采集未提交改动（等价 `git diff`：HEAD ↔ 工作树）。
///
/// 返回 `(改动, 左侧标签, 右侧标签)`；标签用于界面 header 与两栏标题。
pub fn collect_uncommitted() -> Result<(FileChanges, String, String)> {
    let output = git_diff()?;
    Ok((
        parse(&output),
        "HEAD".to_string(),
        "Working Tree".to_string(),
    ))
}

/// 执行 `git diff --no-color` 并返回标准输出文本。
///
/// `git` 无法启动（未安装）或命令失败（非 git 仓库）时返回可展示的错误，
/// 错误里带 `git` 自己写的 stderr。
fn git_diff() -> Result<String> {
    let output = Command::new("git")
        .args(["diff", "--no-color"])
        .output()
        .map_err(|e| Error::Message(format!("failed to run git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Message(format!(
            "git diff failed: {}",
            stderr.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 解析 `git diff` 输出。
///
/// 逐行走：文件头切分文件、hunk 头恢复两侧起始行号、`-`/`+` 分别推进左右、其余行
/// 作为上下文同时进两侧。`\ No newline at end of file` 与各类元数据行丢弃（不是内容）。
pub fn parse(diff_output: &str) -> FileChanges {
    let mut file_changes = FileChanges::new();
    let mut current_file = String::new();
    let mut base_lines: Vec<LineChange> = Vec::new();
    let mut head_lines: Vec<LineChange> = Vec::new();
    let mut base_line_number = 1usize;
    let mut head_line_number = 1usize;

    for raw_line in diff_output.lines() {
        let trimmed = raw_line.trim_end();

        // "no newline" 标记不是内容，既不渲染也不占行号
        if trimmed.starts_with("\\ ") {
            continue;
        }

        let line = ANSI_ESCAPE_RE.replace_all(trimmed, "");

        if let Some(caps) = DIFF_FILE_RE.captures(line.as_ref()) {
            if !current_file.is_empty() {
                file_changes.insert(
                    std::mem::take(&mut current_file),
                    (
                        std::mem::take(&mut base_lines),
                        std::mem::take(&mut head_lines),
                    ),
                );
            }
            current_file = match caps.get(2) {
                Some(m) => m.as_str().to_string(),
                None => continue,
            };
            base_line_number = 1;
            head_line_number = 1;
            continue;
        }

        if let Some(caps) = HUNK_HEADER_RE.captures(line.as_ref()) {
            base_line_number = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<usize>().ok())
                .unwrap_or(1);
            head_line_number = caps
                .get(2)
                .and_then(|m| m.as_str().parse::<usize>().ok())
                .unwrap_or(1);
            continue;
        }

        if is_metadata(line.as_ref()) {
            continue;
        }

        if let Some(rest) = line.strip_prefix('-') {
            base_lines.push((base_line_number, format!("-{rest}")));
            base_line_number += 1;
        } else if let Some(rest) = line.strip_prefix('+') {
            head_lines.push((head_line_number, format!("+{rest}")));
            head_line_number += 1;
        } else {
            base_lines.push((base_line_number, line.to_string()));
            head_lines.push((head_line_number, line.to_string()));
            base_line_number += 1;
            head_line_number += 1;
        }
    }

    if !current_file.is_empty() {
        file_changes.insert(current_file, (base_lines, head_lines));
    }

    file_changes
}

/// 是否为 diff 的元数据行（文件模式、重命名、二进制标记等，不属于任何一侧的内容）。
fn is_metadata(line: &str) -> bool {
    [
        "index",
        "---",
        "+++",
        "@@",
        "new file mode",
        "new mode",
        "old mode",
        "deleted file mode",
        "rename from",
        "rename to",
        "copy from",
        "copy to",
        "similarity index",
        "dissimilarity index",
        "Binary files",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

/// 当前获得键盘焦点的窗格（`Tab` / `h` / `l` 切换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// 左侧文件列表：`j`/`k` 移动选中文件。
    FileList,
    /// 右侧 diff 内容：`j`/`k` 滚动当前文件。
    DiffContent,
}

/// diff 内容的排布方式（`u` 切换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// 左右两栏对照：左 `-` 与上下文、右 `+` 与上下文，按改动块对齐。
    SideBySide,
    /// 单栏统一视图：同一改动块里先列删除行、再列新增行。
    Unified,
}

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

/// 当前文件的渲染产物缓存：语法高亮后的左右栏行与内容行数。
///
/// 语法高亮（syntect）对整份 diff 是逐行解码，大文件下重跑一次要几百毫秒；滚动与窗口
/// 尺寸变化都会重渲染，没有缓存就会变成「按一次 j 卡一下」。命中条件 = 文件 + 视图模式 +
/// 主题指纹（含是否启用语法高亮），三者不变时滚动直接复用。
pub(super) struct GiffContentCache {
    /// 缓存对应的文件路径（切换文件即失效）。
    pub(super) file: String,
    /// 缓存对应的双栏 / 统一视图（切换视图即失效）。
    pub(super) view_mode: ViewMode,
    /// 缓存对应的主题指纹（切换主题 / 开关语法高亮即失效）。
    pub(super) theme_fp: String,
    /// 左栏高亮行（双栏模式的 `base` 侧）；统一模式下为空。
    pub(super) left: Vec<Line<'static>>,
    /// 右栏高亮行（双栏模式的 `head` 侧）或统一模式的单栏行。
    pub(super) right: Vec<Line<'static>>,
    /// 当前视图下的内容行数（滚动夹取的上界）。
    pub(super) content_len: usize,
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
///
/// 字段可见度为 `pub(super)`（= `crate::modes::interactive` 内）：渲染在
/// [`crate::modes::interactive::render::giff`]、按键与鼠标在
/// [`crate::modes::interactive::handlers::giff`]，两处都要直接读写这些状态。
pub struct GiffView {
    /// 各文件的左右行序列（每次 `/giff` 重新采集）。
    pub(super) file_changes: FileChanges,
    /// 左侧标签（如 `HEAD`）。
    pub(super) left_label: String,
    /// 右侧标签（如 `Working Tree`）。
    pub(super) right_label: String,
    /// 按路径排序后的文件列表。
    pub(super) file_names: Vec<String>,
    /// 当前选中文件在 [`Self::file_names`] 中的下标；列表为空时为 0。
    pub(super) current_file_idx: usize,
    /// 文件路径 → 滚动行偏移。
    pub(super) scroll_positions: HashMap<String, usize>,
    /// 当前焦点窗格。
    pub(super) focused_pane: Pane,
    /// 双栏 / 统一视图。
    pub(super) view_mode: ViewMode,
    /// 帮助模态是否打开（打开期间只接受关闭键）。
    pub(super) show_help: bool,
    /// 是否处于输入态：`true` 时主页输入框显示并接收按键，`Esc` 回到导航。
    pub input_active: bool,
    /// 颜色快照（每帧由当前主题重建）。
    pub(super) style: GiffStyle,
    /// 缓存的 syntect 主题（语法高亮用），随主题指纹失效。
    pub(super) syntect_theme: Option<syntect::highlighting::Theme>,
    /// 生成 [`Self::syntect_theme`] 时的主题指纹。
    pub(super) syntect_fp: String,
    /// 最近一帧 diff 内容区的高度（翻页步长，减去上下边框）。
    pub(super) page_rows: u16,
    /// 最近一帧文件列表区域（鼠标滚轮命中判定）。
    pub(super) file_list_area: Option<Rect>,
    /// 最近一帧 diff 内容区域（鼠标滚轮命中判定）。
    pub(super) content_area: Option<Rect>,
    /// 当前文件的高亮缓存（见 [`GiffContentCache`]）；`None` = 无当前文件。
    pub(super) content_cache: Option<GiffContentCache>,
    /// `j`/`k`、`↓`/`↑` 的连按加速状态（按住不放时步长递增）。
    pub(super) key_repeat: KeyRepeatAccelerator,
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
            content_cache: None,
            key_repeat: KeyRepeatAccelerator::new(),
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

    /// 连续按同一导航键时的移动量：孤立一次按返回 ±1，按住不放按间隔加速
    /// （最多 10 行 / 10 个文件）。`direction` 为 `1`（`j`/`↓`）或 `-1`（`k`/`↑`），
    /// `now_ms` 需单调递增（取 `App::accel_epoch` 的 elapsed 毫秒）。
    pub fn nav_step(&mut self, direction: i32, now_ms: u64) -> isize {
        self.key_repeat.next(direction, now_ms) as isize
    }

    /// 复位连按加速：按下与导航无关的键时调用，避免把杂键并进同一个连按手势。
    pub fn reset_nav_repeat(&mut self) {
        self.key_repeat.reset();
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

    /// 文件路径 → 滚动行偏移（渲染后已夹回可视范围）；渲染断言与调试用。
    pub fn scroll_positions(&self) -> &HashMap<String, usize> {
        &self.scroll_positions
    }

    /// 当前视图模式（渲染断言用）。
    pub fn view_mode(&self) -> ViewMode {
        self.view_mode
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::theme::Theme;
    use std::collections::HashMap;

    /// 两文件样本：`src/a.rs` 一处替换（hunk 前带元数据行），`src/b.rs` 全新增。
    const SAMPLE: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,4 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
     // keep
 }
diff --git a/src/b.rs b/src/b.rs
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/src/b.rs
@@ -0,0 +1,2 @@
+line one
+line two
";

    /// 用样本 diff 构造视图（默认无历史状态）。
    fn view() -> GiffView {
        GiffView::new(
            parse(SAMPLE),
            "HEAD".to_string(),
            "Working Tree".to_string(),
            &Theme::default(),
            &GiffRestore::default(),
        )
    }

    /// 文件按 `b/` 侧路径切分，上下文行同时进两侧。
    #[test]
    fn parse_splits_files_and_context() {
        let changes = parse(SAMPLE);
        assert_eq!(changes.len(), 2);

        let (base, head) = &changes["src/a.rs"];
        assert_eq!(base.len(), 4, "上下文 2 + 删除 1 + 上下文 1");
        assert_eq!(head.len(), 4);
        assert_eq!(base[0], (1, " fn main() {".to_string()));
        assert_eq!(base[1], (2, "-    println!(\"old\");".to_string()));
        assert_eq!(head[1], (2, "+    println!(\"new\");".to_string()));
        assert_eq!(base[3].0, 4, "删除行之后的行号继续推进");

        let (base, head) = &changes["src/b.rs"];
        assert!(base.is_empty(), "新增文件没有左侧内容");
        assert_eq!(head.len(), 2);
        assert_eq!(head[0], (1, "+line one".to_string()));
    }

    /// hunk 头恢复行号：第二个 hunk 从它自己的起始行继续，而不是接着上一个 hunk 数。
    #[test]
    fn hunk_header_resets_line_numbers() {
        let diff = "\
diff --git a/f.rs b/f.rs
--- a/f.rs
+++ b/f.rs
@@ -1,1 +1,1 @@
-a
+b
@@ -10,1 +10,1 @@
-c
+d
";
        let (base, head) = &parse(diff)["f.rs"];
        assert_eq!(base[0], (1, "-a".to_string()));
        assert_eq!(base[1], (10, "-c".to_string()));
        assert_eq!(head[1], (10, "+d".to_string()));
    }

    /// 元数据行与 `\ No newline` 标记不进内容。
    #[test]
    fn metadata_and_no_newline_marker_are_skipped() {
        let diff = "\
diff --git a/f.rs b/f.rs
old mode 100644
new mode 100755
rename from f.rs
rename to g.rs
Binary files a/x.bin and b/x.bin differ
diff --git a/h.rs b/h.rs
--- a/h.rs
+++ b/h.rs
@@ -1,1 +1,1 @@
-a
\\ No newline at end of file
+b
\\ No newline at end of file
";
        let changes = parse(diff);
        let (base, head) = &changes["f.rs"];
        assert!(
            base.is_empty() && head.is_empty(),
            "纯元数据的文件两侧都没有内容"
        );
        let (base, head) = &changes["h.rs"];
        assert_eq!(base.len(), 1);
        assert_eq!(head.len(), 1);
    }

    /// 非 diff 输出（空串）解析为空表，调用方据此提示「没有改动」。
    #[test]
    fn empty_input_yields_no_changes() {
        assert!(parse("").is_empty());
    }

    /// 状态恢复：上次选中的文件与滚动位置在下次进入时生效。
    #[test]
    fn restore_reselects_last_file_and_scroll() {
        let restore = GiffRestore {
            file: Some("src/b.rs".to_string()),
            scroll_positions: HashMap::from([("src/b.rs".to_string(), 3)]),
            ..GiffRestore::default()
        };

        let view = GiffView::new(
            parse(SAMPLE),
            "HEAD".to_string(),
            "Working Tree".to_string(),
            &Theme::default(),
            &restore,
        );

        assert_eq!(view.current_file(), Some("src/b.rs"));
        assert_eq!(view.scroll_positions.get("src/b.rs"), Some(&3));
    }

    /// 上次的文件已不在本次改动里时回落到第一个文件，且不恢复它的滚动位置。
    #[test]
    fn restore_falls_back_when_file_gone() {
        let restore = GiffRestore {
            file: Some("gone.rs".to_string()),
            scroll_positions: HashMap::from([("gone.rs".to_string(), 7)]),
            view_mode: Some(ViewMode::Unified),
            ..GiffRestore::default()
        };

        let view = GiffView::new(
            parse(SAMPLE),
            "HEAD".to_string(),
            "Working Tree".to_string(),
            &Theme::default(),
            &restore,
        );

        assert_eq!(view.current_file(), Some("src/a.rs"));
        assert!(
            view.scroll_positions.is_empty(),
            "已消失文件的滚动位置被丢弃"
        );
        assert_eq!(view.view_mode(), ViewMode::Unified, "视图模式仍恢复");
    }

    /// 退出时保存的状态带回「当前文件 / 滚动位置 / 焦点 / 视图模式」。
    #[test]
    fn into_restore_captures_state() {
        let mut view = view();
        view.move_file(1);
        view.scroll_by(5);
        view.toggle_focus();
        view.toggle_view();

        let restore = view.into_restore();
        let reopened = GiffView::new(
            parse(SAMPLE),
            "HEAD".to_string(),
            "Working Tree".to_string(),
            &Theme::default(),
            &restore,
        );

        assert_eq!(reopened.current_file(), Some("src/b.rs"));
        assert_eq!(reopened.scroll_positions.get("src/b.rs"), Some(&5));
        assert_eq!(reopened.focused_pane(), Pane::DiffContent);
        assert_eq!(reopened.view_mode(), ViewMode::Unified);
    }

    /// 连按加速：孤立一次按 1 行，33ms 间隔（约 30 次/秒）加速到 3 行；
    /// 复位后重新从 1 行起算。
    #[test]
    fn nav_step_accelerates_while_held() {
        let mut view = view();
        assert_eq!(view.nav_step(1, 0), 1, "孤立一次按 1 行");
        assert_eq!(view.nav_step(1, 33), 3, "按住不放加速");
        view.reset_nav_repeat();
        assert_eq!(view.nav_step(1, 40), 1, "复位后重新从 1 行起算");
    }

    /// 文件选择夹在两端，不环绕。
    #[test]
    fn file_navigation_clamps() {
        let mut view = view();
        view.move_file(-1);
        assert_eq!(view.current_file(), Some("src/a.rs"));

        view.move_file(10);
        assert_eq!(view.current_file(), Some("src/b.rs"));

        view.select_edge(false);
        assert_eq!(view.current_file(), Some("src/a.rs"));
    }
}
