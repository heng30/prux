//! 右侧面板：文件列表与工作区变更（`git status` / `git diff`）。
//!
//! 渲染期间不碰文件系统，也不在主线程读：内容由 [`RightPanel::load`] 在工作线程算完，
//! 再经 [`RightPanel::apply`] 一次性换上去。

use super::app::GuiApp;
use super::theme::Theme;
use super::theme::px;
use crate::utils::process::run_stdout;
use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, prelude::*};
use std::path::Path;
use std::process::Command;

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

/// 后台一次读完的面板内容（纯数据，可在工作线程里算）。
pub struct PanelData {
    /// 文件列表（仅 [`PanelTab::Files`] 有内容）
    files: Vec<FileRow>,
    /// 变更列表（仅 [`PanelTab::Changes`] 有内容）
    changes: Vec<ChangeRow>,
    /// 选中变更的 diff：(路径, 文本)
    diff: Option<(String, String)>,
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
            error: None,
        }
    }
}

/// 文件列表最多展示多少条（再多的仓库里靠搜索，而不是滚一年）。
const MAX_FILES: usize = 400;

impl RightPanel {
    /// 纯读盘：按 `tab` 算一份面板内容（不碰自身状态，可在工作线程跑）。
    /// `selected` 是期望展示 diff 的变更下标，越界时不给 diff。
    pub fn load(cwd: &str, tab: PanelTab, selected: usize) -> PanelData {
        match tab {
            PanelTab::Files => PanelData {
                files: list_files(Path::new(cwd)),
                changes: Vec::new(),
                diff: None,
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
                        files: Vec::new(),
                        changes,
                        diff,
                        error: None,
                    }
                }
                Err(e) => PanelData {
                    files: Vec::new(),
                    changes: Vec::new(),
                    diff: None,
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
        if !self.changes.is_empty() {
            self.selected = self.selected.min(self.changes.len() - 1);
        }
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

    column = column.child(tab_bar(theme, panel));

    if let Some(error) = &panel.error {
        column = column.child(
            div()
                .px(px(10.))
                .py(px(8.))
                .text_size(px(11.5))
                .text_color(theme.text_tertiary)
                .child(SharedString::from(error.clone())),
        );
    }

    match panel.tab {
        PanelTab::Changes => {
            column = column.child(changes_body(theme, panel, cx));

            if let Some((path, diff)) = &panel.diff {
                column = column.child(
                    div().px(px(12.)).py(px(4.)).child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.text_ghost)
                            .truncate()
                            .child(SharedString::from(path.clone())),
                    ),
                );
                column = column.child(diff_view(theme, diff));
            }
        }
        PanelTab::Files => {
            column = column.child(files_body(theme, panel));
        }
    }

    column.into_any_element()
}

/// 顶部标签栏：变更 / 文件两个页签 + 加载提示。
fn tab_bar(theme: &Theme, panel: &RightPanel) -> AnyElement {
    // 标签栏
    let tab = |label: &str, active: bool| -> AnyElement {
        let mut element = div()
            .h(px(26.))
            .px(px(10.))
            .rounded(px(6.))
            .flex()
            .items_center()
            .text_size(px(11.5))
            .text_color(if active {
                theme.text
            } else {
                theme.text_tertiary
            });
        if active {
            element = element.bg(theme.overlay);
        }
        element
            .child(SharedString::from(label.to_string()))
            .into_any_element()
    };
    let bar = div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(4.))
        .px(px(8.))
        .h(px(38.))
        .border_b_1()
        .border_color(theme.border)
        .child(tab(
            &format!("Changes {}", panel.changes.len()),
            panel.tab == PanelTab::Changes,
        ))
        .child(tab(
            &format!("files {}", panel.files.len()),
            panel.tab == PanelTab::Files,
        ))
        .child(div().flex_1())
        .child(
            div()
                .pr(px(6.))
                .text_size(px(10.))
                .text_color(theme.text_ghost)
                .child(SharedString::from(if panel.loading {
                    "Loading…"
                } else {
                    ""
                })),
        );
    bar.into_any_element()
}

/// `Changes` 页的文件列表：git 状态字母 + 路径。
fn changes_body(theme: &Theme, panel: &RightPanel, cx: &mut gpui::Context<GuiApp>) -> AnyElement {
    let mut body = div()
        .id("changes")
        .flex()
        .flex_col()
        .flex_none()
        .max_h(px(220.))
        .gap(px(2.))
        .p(px(8.))
        .overflow_y_scroll();
    for (index, row) in panel.changes.iter().enumerate() {
        let color = status_color(theme, &row.status);
        let mut entry = div()
            .w_full()
            .h(px(24.))
            .px(px(8.))
            .rounded(px(5.))
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(11.5));
        if index == panel.selected {
            entry = entry.bg(theme.overlay_strong);
        }
        entry = entry
            .child(
                div()
                    .w(px(16.))
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
                .px(px(8.))
                .text_size(px(11.5))
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

/// diff 正文：逐行染色（新增绿 / 删除红），最多 400 行。
fn diff_view(theme: &Theme, diff: &str) -> AnyElement {
    let mut diff_box = div()
        .id("diff")
        .flex_1()
        .min_h_0()
        .mx(px(8.))
        .mb(px(8.))
        .p(px(8.))
        .rounded(px(8.))
        .bg(theme.inset)
        .flex()
        .flex_col()
        .overflow_y_scroll();
    for line in diff.lines().take(400) {
        let color = if line.starts_with('+') && !line.starts_with("+++") {
            theme.success
        } else if line.starts_with('-') && !line.starts_with("---") {
            theme.danger
        } else {
            theme.text_secondary
        };
        diff_box = diff_box.child(
            div()
                .text_size(px(10.5))
                .text_color(color)
                .child(SharedString::from(line.to_string())),
        );
    }
    diff_box.into_any_element()
}

/// `Files` 页的目录列表。
fn files_body(theme: &Theme, panel: &RightPanel) -> AnyElement {
    let mut body = div()
        .id("files")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .gap(px(1.))
        .p(px(8.))
        .overflow_y_scroll();
    for row in &panel.files {
        body = body.child(
            div()
                .w_full()
                .h(px(22.))
                .px(px(6.))
                .rounded(px(4.))
                .flex()
                .items_center()
                .gap(px(6.))
                .hover(|style| style.bg(theme.overlay))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(theme.text_tertiary)
                        .child(SharedString::from(row.name.clone())),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(px(10.))
                        .text_color(theme.text_ghost)
                        .child(SharedString::from(row.dir.clone())),
                ),
        );
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
}
