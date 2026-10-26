//! 右侧面板：文件列表与工作区变更（`git status` / `git diff`）。
//!
//! 渲染期间不碰文件系统：内容由 [`RightPanel::reload`] 在切换/刷新时一次性读入。

use super::app::GuiApp;
use super::theme::Theme;
use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};
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

/// 右侧面板状态。
pub struct RightPanel {
    /// 是否可见
    pub visible: bool,
    /// 当前标签页
    pub tab: PanelTab,
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
    /// 重读面板内容（切换标签/刷新时调用）。
    pub fn reload(&mut self, cwd: &str) {
        self.error = None;
        match self.tab {
            PanelTab::Files => {
                self.files = list_files(Path::new(cwd));
            }
            PanelTab::Changes => {
                match git_changes(cwd) {
                    Ok(changes) => self.changes = changes,
                    Err(e) => {
                        self.changes.clear();
                        self.error = Some(e);
                    }
                }
                self.selected = 0;
                self.load_diff(cwd);
            }
        }
    }

    /// 读当前选中变更的 diff。
    pub fn load_diff(&mut self, cwd: &str) {
        let Some(row) = self.changes.get(self.selected) else {
            self.diff = None;
            return;
        };
        let path = row.path.clone();
        let text = git_diff(cwd, &path).unwrap_or_else(|e| format!("无法读取 diff：{e}"));
        self.diff = Some((path, text));
    }
}

/// 列出工作区文件（尊重 gitignore，跳过隐藏目录与常见重目录）。
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
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| format!("无法运行 git：{e}"))?;
    if !output.status.success() {
        return Err("当前目录不是 git 工作区，或 git 执行失败".to_string());
    }
    Ok(parse_status(&String::from_utf8_lossy(&output.stdout)))
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
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["diff", "--no-color", "--", path])
        .output()
        .map_err(|e| format!("无法运行 git：{e}"))?;
    if !output.status.success() {
        return Err("git diff 执行失败".to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    if text.trim().is_empty() {
        return Ok("（没有已暂存的差异；新文件需先 git add）".to_string());
    }
    Ok(text)
}

/// 渲染右侧面板。
pub fn render(
    theme: &Theme,
    cwd: &str,
    panel: &RightPanel,
    cx: &mut gpui::Context<GuiApp>,
) -> AnyElement {
    let mut column = div()
        .w(px(320.))
        .h_full()
        .flex_none()
        .flex()
        .flex_col()
        .bg(theme.sidebar)
        .border_l_1()
        .border_color(theme.border);

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
    column = column.child(
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(8.))
            .h(px(38.))
            .border_b_1()
            .border_color(theme.border)
            .child(tab(
                &format!("变更 {}", panel.changes.len()),
                panel.tab == PanelTab::Changes,
            ))
            .child(tab(
                &format!("文件 {}", panel.files.len()),
                panel.tab == PanelTab::Files,
            )),
    );

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
                let cwd_click = cwd.to_string();
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
                body = body.child(entry.id(("change", index)).cursor_pointer().on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.panel.selected = index;
                        this.panel.load_diff(&cwd_click);
                        cx.notify();
                    }),
                ));
            }
            if panel.changes.is_empty() && panel.error.is_none() {
                body = body.child(
                    div()
                        .px(px(8.))
                        .text_size(px(11.5))
                        .text_color(theme.text_ghost)
                        .child(SharedString::from("工作区干净")),
                );
            }
            column = column.child(body);

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
                column = column.child(diff_box);
            }
        }
        PanelTab::Files => {
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
        }
    }

    column.into_any_element()
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
