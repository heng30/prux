//! 右侧面板：文件列表与工作区变更（`git status` / `git diff`）。
//!
//! 渲染期间不碰文件系统，也不在主线程读：内容由 [`RightPanel::load`] 在工作线程算完，
//! 再经 [`RightPanel::apply`] 一次性换上去。

use super::{
    app::GuiApp,
    code,
    text_edit::Editor,
    theme::{
        CONTROL_H_MD, CONTROL_H_SM, DIFF_GUTTER_W, FONT_CAPTION, FONT_SMALL, HEADER_H, ICON_SIZE,
        RADIUS_MD, RADIUS_SM, SPACE_MD, SPACE_SM, SPACE_XS, SPACE_XXS, Theme, px,
    },
};
use crate::utils::{color::Rgb, process::run_stdout};
use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, prelude::*};
use std::{path::Path, process::Command};
use syntect::highlighting::Theme as SynTheme;

/// 文件列表最多展示多少条（再多的仓库里靠搜索，而不是滚一年）。
const MAX_FILES: usize = 400;

/// 文件预览最多高亮多少行（再长的文件没必要全铺进面板）。
const MAX_PREVIEW_LINES: usize = 2000;

/// 文件预览的大小上限：超过就不读（二进制/超大文件进内存只会卡界面）。
const MAX_PREVIEW_BYTES: u64 = 2 * 1024 * 1024;

/// 文件列表的限高：列表再长也不挤掉下面的内容区。
const FILE_LIST_MAX_H: f32 = 200.;

/// 面板标签页。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PanelTab {
    /// 文件列表
    Files,
    /// 变更列表与 diff
    Changes,
}

/// 文件列表里的一行。
pub struct FileRow {
    /// 文件名
    pub name: String,
    /// 所在目录（相对 cwd；顶层为空）
    pub dir: String,
    /// 相对路径（打开 diff 用）
    pub path: String,
}

/// 变更列表里的一行。
pub struct ChangeRow {
    /// 状态字母（M/A/D/R/??）
    pub status: String,
    /// 相对路径
    pub path: String,
}

/// 打开的文件正文：路径 + 语法高亮后的逐行片段。
pub struct FileContent {
    /// 相对路径（内容区顶部显示）
    pub path: String,
    /// 逐行片段：颜色为 None 表示用默认前景色
    pub lines: Vec<Vec<(Option<Rgb>, String)>>,
}

/// 后台一次读完的面板内容（纯数据，可在工作线程里算）。
pub struct PanelData {
    /// 文件列表（两个页签都算，用于显示计数）
    files: Vec<FileRow>,
    /// 变更列表（两个页签都算，用于显示计数）
    changes: Vec<ChangeRow>,
    /// 选中变更的 diff：(路径, 文本)
    diff: Option<(String, String)>,
    /// Files 页选中文件的正文
    content: Option<FileContent>,
    /// 读取失败时的提示
    error: Option<String>,
}

/// 右侧面板状态。
pub struct RightPanel {
    /// 是否可见
    pub visible: bool,
    /// 当前标签页
    pub tab: PanelTab,
    /// 是否有一轮读盘正在进行
    pub loading: bool,
    /// 文件列表
    pub files: Vec<FileRow>,
    /// 变更列表
    pub changes: Vec<ChangeRow>,
    /// 选中的变更下标
    pub selected: usize,
    /// 选中变更的 diff：(路径, 文本)
    pub diff: Option<(String, String)>,
    /// Files 页的过滤框（按相对路径包含匹配）
    pub search: Editor,
    /// 过滤框是否在吃按键：点搜索框置位，Esc / Enter / 点别的区域清掉
    pub search_focus: bool,
    /// Files 页选中的文件（相对路径）
    pub selected_file: Option<String>,
    /// 选中文件的正文
    pub content: Option<FileContent>,
    /// 读取失败时的提示
    pub error: Option<String>,
}

impl Default for RightPanel {
    fn default() -> Self {
        Self {
            visible: false,
            tab: PanelTab::Changes,
            loading: false,
            files: Vec::new(),
            changes: Vec::new(),
            selected: 0,
            diff: None,
            search: Editor::new(),
            search_focus: false,
            selected_file: None,
            content: None,
            error: None,
        }
    }
}

impl RightPanel {
    /// 纯读盘：按 `tab` 算一份面板内容（不碰自身状态，可在工作线程跑）。
    /// `selected` 是期望展示 diff 的变更下标，`file` 是 Files 页要预览的路径，
    /// `syn` 是语法高亮主题（高亮也在工作线程做，渲染时不再碰 syntect）。
    pub fn load(
        cwd: &str,
        tab: PanelTab,
        selected: usize,
        file: Option<&str>,
        syn: &SynTheme,
    ) -> PanelData {
        // 两个页签的计数都要显示，故文件列表恒算一遍
        let files = list_files(Path::new(cwd));
        match tab {
            PanelTab::Files => PanelData {
                files,
                changes: git_changes(cwd).unwrap_or_default(),
                diff: None,
                content: file.map(|path| read_file(cwd, path, syn)),
                error: None,
            },
            PanelTab::Changes => match git_changes(cwd) {
                Ok(changes) => {
                    let diff = changes.get(selected).map(|row| {
                        let path = row.path.clone();
                        let text = git_diff(cwd, &path)
                            .unwrap_or_else(|e| format!("Cannot read diff: {e}"));
                        (path, text)
                    });
                    PanelData {
                        files,
                        changes,
                        diff,
                        content: None,
                        error: None,
                    }
                }
                Err(e) => PanelData {
                    files,
                    changes: Vec::new(),
                    diff: None,
                    content: None,
                    error: Some(e),
                },
            },
        }
    }

    /// 换上后台算好的内容（变更列表变短时把选中下标收回范围内）。
    pub fn apply(&mut self, data: PanelData) {
        self.loading = false;
        self.error = data.error;
        self.files = data.files;
        self.changes = data.changes;
        self.diff = data.diff;
        self.content = data.content;
        if !self.changes.is_empty() {
            self.selected = self.selected.min(self.changes.len() - 1);
        }
    }

    /// 过滤框命中的文件下标（空框 = 全部，按相对路径包含匹配）。
    pub fn filtered_files(&self) -> Vec<usize> {
        let query = self.search.text().trim().to_lowercase();
        self.files
            .iter()
            .enumerate()
            .filter(|(_, row)| query.is_empty() || row.path.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }
}

/// 读一个文件并语法高亮。
///
/// 非 UTF-8 / 超大 / 读失败都不报错，直接给一行提示当正文（面板里没什么可挽救的）。
fn read_file(cwd: &str, path: &str, syn: &SynTheme) -> FileContent {
    let note = |text: String| FileContent {
        path: path.to_string(),
        lines: vec![vec![(None, text)]],
    };
    let full = Path::new(cwd).join(path);
    let size = std::fs::metadata(&full).map(|m| m.len()).unwrap_or(0);
    if size > MAX_PREVIEW_BYTES {
        return note(format!("(file too large: {} KB)", size / 1024));
    }
    let Ok(bytes) = std::fs::read(&full) else {
        return note("Cannot read file".to_string());
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return note("(binary file)".to_string());
    };

    let truncated = text.lines().count() > MAX_PREVIEW_LINES;
    let body: String = text
        .lines()
        .take(MAX_PREVIEW_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let lang = full.extension().and_then(|e| e.to_str());
    let mut lines = code::highlight(&body, lang, syn);
    if truncated {
        lines.push(vec![(
            None,
            format!("⋯ truncated at {MAX_PREVIEW_LINES} lines"),
        )]);
    }
    FileContent {
        path: path.to_string(),
        lines,
    }
}

/// 列出工作区文件（尊重 gitignore，跳过隐藏目录与常见重目录）。
/// 列文件（供输入框 `@` 补全用），返回相对路径。
pub(super) fn file_paths(cwd: &str) -> Vec<String> {
    list_files(Path::new(cwd))
        .into_iter()
        .map(|row| row.path)
        .collect()
}

fn list_files(cwd: &Path) -> Vec<FileRow> {
    let mut rows = Vec::new();
    let walker = ignore::WalkBuilder::new(cwd)
        .hidden(true)
        .max_depth(Some(6))
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git" | "node_modules" | "target" | "dist" | "build" | ".venv" | "__pycache__"
            )
        })
        .build();

    for entry in walker.flatten().skip(1) {
        if rows.len() >= MAX_FILES {
            break;
        }
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(cwd) else {
            continue;
        };
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let dir = relative
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        rows.push(FileRow {
            name,
            dir,
            path: relative.to_string_lossy().to_string(),
        });
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    rows
}

/// `git status --porcelain` → 变更列表。
fn git_changes(cwd: &str) -> Result<Vec<ChangeRow>, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(cwd).args(["status", "--porcelain"]);
    let stdout = run_stdout(cmd, "git status")?;
    Ok(parse_status(&String::from_utf8_lossy(&stdout)))
}

/// `git status --porcelain` 文本 → 变更列表（重命名只保留新路径）。
fn parse_status(text: &str) -> Vec<ChangeRow> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.len() < 4 {
            continue;
        }
        let status = line[..2].trim().to_string();
        let path = line[3..].trim().to_string();
        // 重命名行形如 `old -> new`：只保留新路径
        let path = path.rsplit(" -> ").next().unwrap_or(&path).to_string();
        rows.push(ChangeRow {
            status: if status.is_empty() {
                "?".into()
            } else {
                status
            },
            path,
        });
    }
    rows
}

/// 单个文件的 diff（未跟踪文件没有 diff 内容）。
fn git_diff(cwd: &str, path: &str) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(["diff", "--no-color", "--", path]);
    let stdout = run_stdout(cmd, "git diff")?;
    let text = String::from_utf8_lossy(&stdout).to_string();
    if text.trim().is_empty() {
        return Ok("(no staged diff; new files need `git add` first)".to_string());
    }
    Ok(text)
}

/// 渲染右侧面板。
pub fn render(
    theme: &Theme,
    panel: &RightPanel,
    width: f32,
    cx: &mut gpui::Context<GuiApp>,
) -> AnyElement {
    let mut column = div()
        .w(px(width))
        .h_full()
        .flex_none()
        .flex()
        .flex_col()
        .bg(theme.sidebar)
        .border_l_1()
        .border_color(theme.border);

    column = column.child(tab_bar(theme, panel, cx));

    if let Some(error) = &panel.error {
        column = column.child(
            div()
                .px(px(SPACE_SM))
                .py(px(SPACE_SM))
                .text_size(px(FONT_SMALL))
                .text_color(theme.text_tertiary)
                .child(SharedString::from(error.clone())),
        );
    }

    match panel.tab {
        PanelTab::Changes => {
            column = column.child(changes_body(theme, panel, cx));

            if let Some((path, diff)) = &panel.diff {
                column = column.child(
                    div().px(px(SPACE_MD)).py(px(SPACE_XS)).child(
                        div()
                            .text_size(px(FONT_CAPTION))
                            .text_color(theme.text_ghost)
                            .truncate()
                            .child(SharedString::from(path.clone())),
                    ),
                );
                column = column.child(diff_view(theme, diff));
            }
        }
        PanelTab::Files => {
            column = column.child(files_body(theme, panel, cx));
        }
    }

    column.into_any_element()
}

/// 顶部标签栏：变更 / 文件两个页签 + 加载提示。
fn tab_bar(theme: &Theme, panel: &RightPanel, cx: &mut gpui::Context<GuiApp>) -> AnyElement {
    let bar = div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(SPACE_XS))
        .px(px(SPACE_SM))
        .h(px(HEADER_H))
        .border_b_1()
        .border_color(theme.border)
        .child(tab_button(
            theme,
            &format!("Changes {}", panel.changes.len()),
            panel.tab == PanelTab::Changes,
            PanelTab::Changes,
            cx,
        ))
        .child(tab_button(
            theme,
            &format!("files {}", panel.files.len()),
            panel.tab == PanelTab::Files,
            PanelTab::Files,
            cx,
        ))
        .child(div().flex_1())
        .child(
            div()
                .pr(px(SPACE_XS))
                .text_size(px(FONT_CAPTION))
                .text_color(theme.text_ghost)
                .child(SharedString::from(if panel.loading {
                    "Loading…"
                } else {
                    ""
                })),
        );
    bar.into_any_element()
}

/// 一个页签按钮：当前页高亮，点一下切过去。
fn tab_button(
    theme: &Theme,
    label: &str,
    active: bool,
    tab: PanelTab,
    cx: &mut gpui::Context<GuiApp>,
) -> AnyElement {
    let mut element = div()
        .id(("panel-tab", tab as usize))
        .h(px(CONTROL_H_MD))
        .px(px(SPACE_SM))
        .rounded(px(RADIUS_MD))
        .flex()
        .items_center()
        .cursor_pointer()
        .text_size(px(FONT_SMALL))
        .text_color(if active {
            theme.text
        } else {
            theme.text_tertiary
        })
        .hover(|style| style.bg(theme.overlay).text_color(theme.text))
        .on_click(cx.listener(move |this, _, _, cx| this.set_panel_tab(tab, cx)));
    if active {
        element = element.bg(theme.overlay);
    }
    element
        .child(SharedString::from(label.to_string()))
        .into_any_element()
}

/// `Changes` 页的文件列表：git 状态字母 + 路径。
fn changes_body(theme: &Theme, panel: &RightPanel, cx: &mut gpui::Context<GuiApp>) -> AnyElement {
    let mut body = div()
        .id("changes")
        .flex()
        .flex_col()
        .flex_none()
        .max_h(px(220.))
        .gap(px(SPACE_XXS))
        .p(px(SPACE_SM))
        .overflow_y_scroll();
    for (index, row) in panel.changes.iter().enumerate() {
        let color = status_color(theme, &row.status);
        let mut entry = div()
            .w_full()
            .h(px(CONTROL_H_MD))
            .px(px(SPACE_SM))
            .rounded(px(RADIUS_SM))
            .flex()
            .items_center()
            .gap(px(SPACE_SM))
            .text_size(px(FONT_SMALL));

        if index == panel.selected {
            entry = entry.bg(theme.overlay_strong);
        }

        entry = entry
            .child(
                div()
                    .w(px(ICON_SIZE))
                    .flex_none()
                    .text_color(color)
                    .child(SharedString::from(row.status.clone())),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(row.path.clone())),
            );
        body = body.child(
            entry
                .id(("change", index))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.select_change(index, cx))),
        );
    }

    if panel.changes.is_empty() && panel.error.is_none() {
        body = body.child(
            div()
                .px(px(SPACE_SM))
                .text_size(px(FONT_SMALL))
                .text_color(theme.text_ghost)
                .child(SharedString::from(if panel.loading {
                    "Reading changes…"
                } else {
                    "Working tree clean"
                })),
        );
    }
    body.into_any_element()
}

/// diff 正文：跳过 `diff --git` / `index` 等文件头，逐行带旧/新行号染色（新增绿 / 删除红），最多 400 行。
fn diff_view(theme: &Theme, diff: &str) -> AnyElement {
    let mut diff_box = div()
        .id("diff")
        .flex_1()
        .min_h_0()
        .mx(px(SPACE_SM))
        .mb(px(SPACE_SM))
        .p(px(SPACE_SM))
        .rounded(px(RADIUS_MD))
        .bg(theme.inset)
        .flex()
        .flex_col()
        .overflow_y_scroll();

    let (mut old_no, mut new_no) = (0, 0);
    let mut in_hunk = false;

    for line in diff.lines().take(400) {
        if line.starts_with("@@") {
            let mut fields = line.split(' ').skip(1);
            old_no = hunk_start(fields.next());
            new_no = hunk_start(fields.next());
            in_hunk = true;
            continue;
        }

        // 首个 hunk 头之前全是 `diff --git` / `index` / `---` / `+++` 之类的文件头
        if !in_hunk {
            continue;
        }

        // 标记已经并到行号里，正文剥掉 diff 前缀
        let (label, body, color) = match line.as_bytes().first() {
            Some(b'+') => {
                new_no += 1;
                (format!("+{new_no}"), &line[1..], theme.success)
            }
            Some(b'-') => {
                old_no += 1;
                (format!("-{old_no}"), &line[1..], theme.danger)
            }
            Some(b' ') => {
                old_no += 1;
                new_no += 1;
                (format!(" {new_no}"), &line[1..], theme.text_secondary)
            }
            _ => (String::new(), line, theme.text_ghost),
        };

        diff_box = diff_box.child(
            div()
                .flex()
                .gap(px(SPACE_XS))
                .text_size(px(FONT_CAPTION))
                .text_color(color)
                .child(line_no(label))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .child(SharedString::from(body.to_string())),
                ),
        );
    }
    diff_box.into_any_element()
}

/// hunk 头里的字段（`-4,22` / `+4,33`）→ 起始行号；缺失或非法时为 0。
fn hunk_start(field: Option<&str>) -> usize {
    field
        .map(|f| f.trim_start_matches(['-', '+']))
        .and_then(|f| f.split(',').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// diff 左侧行号格：`+428` / `-428` / ` 429` 这种带标记的写法，无行号时留空占位。
fn line_no(label: String) -> AnyElement {
    div()
        .flex_none()
        .min_w(px(DIFF_GUTTER_W))
        .child(SharedString::from(label))
        .into_any_element()
}

/// `Files` 页：搜索框 + 文件列表（限高可滚）+ 选中文件的内容区。
fn files_body(theme: &Theme, panel: &RightPanel, cx: &mut gpui::Context<GuiApp>) -> AnyElement {
    let mut column = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .gap(px(SPACE_XS))
        .p(px(SPACE_SM));

    column = column.child(search_box(theme, panel, cx));
    column = column.child(file_list(theme, panel, cx));

    if let Some(content) = &panel.content {
        column = column.child(
            div()
                .px(px(SPACE_XXS))
                .text_size(px(FONT_CAPTION))
                .text_color(theme.text_ghost)
                .truncate()
                .child(SharedString::from(content.path.clone())),
        );
    }
    column = column.child(content_view(theme, panel));
    column.into_any_element()
}

/// 搜索框：点一下开始输入，框内显示光标。
fn search_box(theme: &Theme, panel: &RightPanel, cx: &mut gpui::Context<GuiApp>) -> AnyElement {
    let empty = panel.search.is_empty();
    let focused = panel.search_focus;
    div()
        .id("file-search")
        .w_full()
        .h(px(CONTROL_H_MD))
        .px(px(SPACE_SM))
        .rounded(px(RADIUS_MD))
        .flex()
        .items_center()
        .bg(theme.inset)
        .border_1()
        .border_color(if focused {
            theme.border_strong
        } else {
            theme.border
        })
        .text_size(px(FONT_SMALL))
        .text_color(if empty { theme.text_ghost } else { theme.text })
        .cursor_pointer()
        .on_click(cx.listener(|this, _, _, cx| this.focus_panel_search(cx)))
        .child(
            div()
                .flex()
                .items_center()
                .child(SharedString::from(if empty {
                    "Search files…".to_string()
                } else {
                    panel.search.text()
                }))
                .child(
                    div()
                        .w(px(if focused && !empty { 1.5 } else { 0. }))
                        .h(px(FONT_SMALL))
                        .ml(px(SPACE_XXS))
                        .bg(theme.accent),
                ),
        )
        .into_any_element()
}

/// 文件列表：点一行切内容（超过 [`FILE_LIST_MAX_H`] 就滚）。
fn file_list(theme: &Theme, panel: &RightPanel, cx: &mut gpui::Context<GuiApp>) -> AnyElement {
    let mut list = div()
        .id("files")
        .flex()
        .flex_col()
        .w_full()
        .max_h(px(FILE_LIST_MAX_H))
        .gap(px(SPACE_XXS))
        .overflow_y_scroll();

    let hits = panel.filtered_files();

    for index in hits.iter().copied() {
        let row = &panel.files[index];
        let active = panel.selected_file.as_deref() == Some(row.path.as_str());
        let mut entry = div()
            .w_full()
            .h(px(CONTROL_H_SM))
            .px(px(SPACE_XS))
            .rounded(px(RADIUS_SM))
            .flex()
            .items_center()
            .gap(px(SPACE_XS))
            .cursor_pointer()
            .hover(|style| style.bg(theme.overlay));
        if active {
            entry = entry.bg(theme.overlay_strong);
        }
        list = list.child(
            entry
                .id(("file", index))
                .on_click(cx.listener(move |this, _, _, cx| this.select_file(index, cx)))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(FONT_CAPTION))
                        .text_color(if active {
                            theme.text
                        } else {
                            theme.text_tertiary
                        })
                        .child(SharedString::from(row.name.clone())),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(px(FONT_CAPTION))
                        .text_color(theme.text_ghost)
                        .child(SharedString::from(row.dir.clone())),
                ),
        );
    }

    if hits.is_empty() {
        list = list.child(
            div()
                .px(px(SPACE_XS))
                .text_size(px(FONT_CAPTION))
                .text_color(theme.text_ghost)
                .child(SharedString::from(if panel.files.is_empty() {
                    if panel.loading {
                        "Reading files…"
                    } else {
                        "No files"
                    }
                } else {
                    "No matches"
                })),
        );
    }
    list.into_any_element()
}

/// 内容区：语法高亮的文件正文（左侧带行号），可在框内滚动。
fn content_view(theme: &Theme, panel: &RightPanel) -> AnyElement {
    let mut body = div()
        .id("file-content")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .p(px(SPACE_SM))
        .rounded(px(RADIUS_MD))
        .bg(theme.inset)
        .text_size(px(FONT_CAPTION))
        .font_family("monospace")
        .overflow_y_scroll();

    match &panel.content {
        Some(content) => {
            for (index, line) in content.lines.iter().enumerate() {
                let mut row = div()
                    .w_full()
                    .flex()
                    .gap(px(SPACE_XS))
                    .text_color(theme.text)
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(DIFF_GUTTER_W))
                            .text_color(theme.text_ghost)
                            .child(SharedString::from(format!("{:>4}", index + 1))),
                    );
                for (color, text) in line {
                    row = row.child(match color {
                        Some(rgb) => div()
                            .text_color(theme.rgb(*rgb))
                            .child(SharedString::from(text.clone())),
                        // 空行留一个空格，否则整行高度塌成 0
                        None => div().child(SharedString::from(if text.is_empty() {
                            " ".to_string()
                        } else {
                            text.clone()
                        })),
                    });
                }
                body = body.child(row);
            }
        }
        None => {
            body = body.child(
                div()
                    .text_size(px(FONT_SMALL))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from("Select a file to preview")),
            );
        }
    }
    body.into_any_element()
}

/// git 状态字母 → 颜色。
fn status_color(theme: &Theme, status: &str) -> gpui::Hsla {
    if status.contains('D') {
        theme.danger
    } else if status.contains('A') || status == "??" {
        theme.success
    } else if status.contains('M') {
        theme.warning
    } else {
        theme.text_tertiary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// porcelain 文本解析：状态字母、重命名取新路径、跳过空行。
    #[test]
    fn parses_git_status() {
        let rows = parse_status(" M src/main.rs\n?? new.txt\nR  old.rs -> new.rs\n\n");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].status, "M");
        assert_eq!(rows[0].path, "src/main.rs");
        assert_eq!(rows[1].status, "??");
        assert_eq!(rows[2].status, "R");
        assert_eq!(rows[2].path, "new.rs");
    }

    /// hunk 头字段解析：取逗号前的起始行号，缺字段时给 0。
    #[test]
    fn parses_hunk_start() {
        assert_eq!(hunk_start(Some("-4,22")), 4);
        assert_eq!(hunk_start(Some("+12")), 12);
        assert_eq!(hunk_start(None), 0);
    }

    /// 文件列表只收文件、排好序、跳过隐藏目录。
    #[test]
    fn lists_files_skipping_hidden() {
        let dir = tempfile::tempdir().expect("临时目录");
        std::fs::create_dir_all(dir.path().join("src")).expect("建目录");
        std::fs::create_dir_all(dir.path().join(".git")).expect("建目录");
        std::fs::write(dir.path().join("src/a.rs"), "fn main() {}").expect("写文件");
        std::fs::write(dir.path().join("README.md"), "# x").expect("写文件");

        let rows = list_files(dir.path());
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["README.md", "src/a.rs"]);
        assert_eq!(rows[1].name, "a.rs");
        assert_eq!(rows[1].dir, "src");
    }

    /// 搜索框按相对路径包含匹配（大小写不敏感），空框给全部。
    #[test]
    fn search_filters_files_by_path() {
        let mut panel = RightPanel {
            files: ["Cargo.toml", "src/main.rs", "src/app.rs"]
                .into_iter()
                .map(|path| FileRow {
                    name: path.rsplit('/').next().unwrap_or(path).to_string(),
                    dir: path
                        .rsplit_once('/')
                        .map(|(dir, _)| dir.to_string())
                        .unwrap_or_default(),
                    path: path.to_string(),
                })
                .collect(),
            ..RightPanel::default()
        };
        assert_eq!(panel.filtered_files(), vec![0, 1, 2]);

        panel.search.insert("APP");
        assert_eq!(panel.filtered_files(), vec![2]);

        panel.search.insert("crate");
        assert!(panel.filtered_files().is_empty());
    }

    /// 缺失的文件不会 panic：正文退化成一行提示。
    #[test]
    fn missing_file_becomes_a_note() {
        let dir = tempfile::tempdir().expect("临时目录");
        let syn = Theme::dark().syntect();
        let content = read_file(&dir.path().to_string_lossy(), "nope.rs", &syn);
        assert_eq!(content.path, "nope.rs");
        assert_eq!(
            content.lines,
            vec![vec![(None, "Cannot read file".to_string())]]
        );
    }
}
