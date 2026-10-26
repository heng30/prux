//! 输入行（composer）：把 [`Editor`] 状态渲染成多行文本，带光标与选区。

use super::text_edit::Editor;
use super::theme::Theme;
use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};

/// 输入行最多显示的行数（超出部分靠滚动，避免占满整屏）。
const MAX_VISIBLE_LINES: usize = 8;

/// 渲染输入框内容。
pub fn render(editor: &Editor, theme: &Theme) -> AnyElement {
    let text_color = theme.text;
    let mut column = div()
        .w_full()
        .flex()
        .flex_col()
        .max_h(px(20. * MAX_VISIBLE_LINES as f32))
        .text_size(px(13.5))
        .overflow_hidden();

    for row in 0..editor.lines().len() {
        let line = &editor.lines()[row];
        let selection = editor.selection_row(row);
        let cursor_col = (editor.cursor().row == row).then_some(editor.cursor().col);

        let mut line_row = div().w_full().flex().items_center();
        for (text, kind) in runs(line, selection, cursor_col) {
            let mut element = div().child(SharedString::from(text));
            match kind {
                RunKind::Normal => {}
                RunKind::Selected => {
                    element = element.bg(theme.selection);
                }
                RunKind::Cursor => {
                    element = element.bg(theme.accent).text_color(theme.on_inverse);
                }
            }
            line_row = line_row.child(element);
        }
        column = column.child(line_row);
    }
    column.text_color(text_color).into_any_element()
}

/// 一段同类的字符区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    /// 普通文本
    Normal,
    /// 选中
    Selected,
    /// 光标所在字符
    Cursor,
}

/// 把一行拆成「普通 / 选中 / 光标」区间；行尾光标用零宽占位符表示。
fn runs(
    line: &str,
    selection: Option<(usize, usize)>,
    cursor: Option<usize>,
) -> Vec<(String, RunKind)> {
    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<(String, RunKind)> = Vec::new();

    for (index, ch) in chars.iter().enumerate() {
        let kind = if cursor == Some(index) {
            RunKind::Cursor
        } else if selection.is_some_and(|(start, end)| index >= start && index < end) {
            RunKind::Selected
        } else {
            RunKind::Normal
        };
        push_run(&mut out, ch.to_string(), kind);
    }

    // 光标在行尾（或空行）：补一个可见方块
    if let Some(col) = cursor
        && col >= chars.len()
    {
        push_run(&mut out, " ".to_string(), RunKind::Cursor);
    }
    if out.is_empty() {
        push_run(&mut out, String::new(), RunKind::Normal);
    }
    out
}

/// 追加一段；与上一段同类时直接拼接。
fn push_run(out: &mut Vec<(String, RunKind)>, text: String, kind: RunKind) {
    if let Some((last, last_kind)) = out.last_mut()
        && *last_kind == kind
    {
        last.push_str(&text);
        return;
    }
    out.push((text, kind));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 普通文本整行一段，光标位置切成独立一段。
    #[test]
    fn splits_cursor_run() {
        let runs = runs("abc", None, Some(1));
        assert_eq!(
            runs,
            vec![
                ("a".to_string(), RunKind::Normal),
                ("b".to_string(), RunKind::Cursor),
                ("c".to_string(), RunKind::Normal),
            ]
        );
    }

    /// 行尾光标补一个占位方块；选区连续字符合成一段。
    #[test]
    fn handles_tail_cursor_and_selection() {
        let runs = runs("abcd", Some((1, 3)), Some(4));
        assert_eq!(
            runs,
            vec![
                ("a".to_string(), RunKind::Normal),
                ("bc".to_string(), RunKind::Selected),
                ("d".to_string(), RunKind::Normal),
                (" ".to_string(), RunKind::Cursor),
            ]
        );
    }
}
