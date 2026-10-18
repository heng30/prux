//! `/giff` 视图的单测：行对齐、统一视图、路径截断、状态恢复与整帧渲染断言。

use super::{
    GiffRestore, GiffView, Pane, ViewMode,
    diff::parse,
    render::{align_lines, build_unified_lines, render, truncate_path, unified_line_count},
};
use crate::modes::interactive::theme::Theme;
use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use std::collections::HashMap;

/// 两文件样本：`a.rs` 一处替换，`b.rs` 全新增。
const SAMPLE: &str = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,3 +1,3 @@
 fn main() {
-    old();
+    new();
 }
diff --git a/b.rs b/b.rs
--- /dev/null
+++ b/b.rs
@@ -0,0 +1,2 @@
+one
+two
";

/// 构造视图（默认无历史状态）。
fn view(diff: &str) -> GiffView {
    GiffView::new(
        parse(diff),
        "HEAD".to_string(),
        "Working Tree".to_string(),
        &Theme::default(),
        &GiffRestore::default(),
    )
}

/// 渲染一帧并返回终端，供逐行断言。
fn draw(view: &mut GiffView, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let theme = Theme::default();
    terminal
        .draw(|frame| render(frame, Rect::new(0, 0, width, height), view, &theme))
        .unwrap();
    terminal
}

/// 取某一行可见文本（右侧空格已裁剪）。
fn row(terminal: &Terminal<TestBackend>, y: u16, width: u16) -> String {
    let buf = terminal.backend().buffer();
    (0..width)
        .map(|x| buf.cell((x, y)).unwrap().symbol())
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// 改动块按行配对：删除与新增各 1 行时对齐成 2 行，上下文行两侧各一份。
#[test]
fn align_pairs_change_blocks() {
    let (base, head) = &parse(SAMPLE)["a.rs"];
    let (aligned_base, aligned_head) = align_lines(base, head);

    assert_eq!(aligned_base.len(), aligned_head.len());
    assert_eq!(aligned_base.len(), 3, "上下文 1 + 改动块 1 + 上下文 1");
    assert!(aligned_base[1].1.starts_with('-'));
    assert!(aligned_head[1].1.starts_with('+'));
    assert_eq!(aligned_base[0], head[0], "上下文行两侧一致");
}

/// 改动块里删除多于新增时，短的一侧补 `(0, "")` 占位行。
#[test]
fn align_pads_shorter_side() {
    let base = vec![(1, "-a".to_string()), (2, "-b".to_string())];
    let head = vec![(1, "+a".to_string())];
    let (aligned_base, aligned_head) = align_lines(&base, &head);

    assert_eq!(aligned_base.len(), 2);
    assert_eq!(aligned_head[1], (0, String::new()));
}

/// 统一视图：同一改动块先列删除再列新增，上下文行只出现一次。
#[test]
fn unified_lists_removals_before_additions() {
    let (base, head) = &parse(SAMPLE)["a.rs"];
    let unified = build_unified_lines(base, head);

    assert_eq!(unified.len(), 4, "上下文 1 + 删除 1 + 新增 1 + 上下文 1");
    assert_eq!(unified[1].1, "-    old();");
    assert_eq!(unified[2].1, "+    new();");
    assert_eq!(unified_line_count(base, head), unified.len());
}

/// 路径从左侧截断，保留文件名尾部。
#[test]
fn truncate_path_keeps_tail() {
    assert_eq!(truncate_path("src/main.rs", 20), "src/main.rs");
    assert_eq!(truncate_path("abcdefghij", 6), "…fghij");
    assert_eq!(truncate_path("あいうえお", 5), "…えお", "全角按 2 列计");
    assert_eq!(truncate_path("abcdef", 1), "…");
}

/// 整帧：header 显示两侧标签与计数，左侧列出文件，底部是键位行。
#[test]
fn render_shows_header_files_and_keybar() {
    let mut view = view(SAMPLE);
    let terminal = draw(&mut view, 100, 20);

    let header = row(&terminal, 0, 100);
    assert!(header.contains("HEAD → Working Tree"), "header: {header}");
    assert!(header.contains("Side-by-Side"), "header: {header}");
    assert!(header.contains("1/2"), "header: {header}");

    let files = row(&terminal, 2, 100);
    assert!(files.contains("a.rs"), "文件列表: {files}");

    let keybar = row(&terminal, 19, 100);
    assert!(keybar.contains("Quit"), "键位行: {keybar}");
    assert!(keybar.contains("Input"), "键位行: {keybar}");
}

/// `u` 切换统一视图后 header 与内容标题随之变化。
#[test]
fn toggle_view_updates_header() {
    let mut view = view(SAMPLE);
    view.toggle_view();
    assert_eq!(view.view_mode(), ViewMode::Unified);

    let terminal = draw(&mut view, 100, 20);
    assert!(row(&terminal, 0, 100).contains("Unified"));
}

/// 输入态切换底部键位行文案（输入框本身由核心渲染，不在此视图内）。
#[test]
fn input_mode_switches_keybar() {
    let mut view = view(SAMPLE);
    view.set_input_active(true);

    let terminal = draw(&mut view, 100, 20);
    let keybar = row(&terminal, 19, 100);
    assert!(keybar.contains("Send"), "输入态键位行: {keybar}");
    assert!(keybar.contains("Cancel input"), "输入态键位行: {keybar}");
}

/// 状态恢复：上次选中的文件与滚动位置在下次进入时生效。
#[test]
fn restore_reselects_last_file_and_scroll() {
    let restore = GiffRestore {
        file: Some("b.rs".to_string()),
        scroll_positions: HashMap::from([("b.rs".to_string(), 3)]),
        ..GiffRestore::default()
    };

    let view = GiffView::new(
        parse(SAMPLE),
        "HEAD".to_string(),
        "Working Tree".to_string(),
        &Theme::default(),
        &restore,
    );

    assert_eq!(view.current_file(), Some("b.rs"));
    assert_eq!(view.scroll_positions.get("b.rs"), Some(&3));
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

    assert_eq!(view.current_file(), Some("a.rs"));
    assert!(
        view.scroll_positions.is_empty(),
        "已消失文件的滚动位置被丢弃"
    );
    assert_eq!(view.view_mode(), ViewMode::Unified, "视图模式仍恢复");
}

/// 退出时保存的状态带回「当前文件 / 滚动位置 / 焦点 / 视图模式」。
#[test]
fn into_restore_captures_state() {
    let mut view = view(SAMPLE);
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

    assert_eq!(reopened.current_file(), Some("b.rs"));
    assert_eq!(reopened.scroll_positions.get("b.rs"), Some(&5));
    assert_eq!(reopened.focused_pane(), Pane::DiffContent);
    assert_eq!(reopened.view_mode(), ViewMode::Unified);
}

/// 文件选择夹在两端，不环绕。
#[test]
fn file_navigation_clamps() {
    let mut view = view(SAMPLE);
    view.move_file(-1);
    assert_eq!(view.current_file(), Some("a.rs"));

    view.move_file(10);
    assert_eq!(view.current_file(), Some("b.rs"));

    view.select_edge(false);
    assert_eq!(view.current_file(), Some("a.rs"));
}

/// 滚动位置在渲染时按内容长度夹回（内容比视口短 → 归零）。
#[test]
fn scroll_is_clamped_to_content() {
    let mut view = view(SAMPLE);
    view.scroll_to_bottom();
    let _ = draw(&mut view, 100, 20);

    assert_eq!(
        view.scroll_positions.get("a.rs"),
        Some(&0),
        "内容不足一屏时归零"
    );
}

/// 帮助模态开关。
#[test]
fn help_modal_toggles() {
    let mut view = view(SAMPLE);
    assert!(!view.is_help_open());
    view.toggle_help();
    assert!(view.is_help_open());

    let terminal = draw(&mut view, 100, 30);
    assert!(
        (0..30).any(|y| row(&terminal, y, 100).contains("Keybindings")),
        "模态框标题应可见"
    );

    view.close_help();
    assert!(!view.is_help_open());
}
