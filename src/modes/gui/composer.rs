//! 输入行（composer）：把 [`Editor`] 状态渲染成多行文本，带光标与选区。

use super::text_edit::Editor;
use super::theme::{FONT_BODY, SPACE_XXS, Theme, UI_FONT_FAMILY, px};
use gpui::{
    AnyElement, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div,
};
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

/// 输入行最多显示的行数（超出部分靠滚动，避免占满整屏）。
const MAX_VISIBLE_LINES: usize = 8;

/// 输入框字号：与对话流正文同一档（[`FONT_BODY`]）。
const FONT_SIZE: f32 = FONT_BODY;

/// 输入框行高倍数：gpui 默认是 φ（1.618em），选中/光标背景会比字形高出一截、
/// 多行时相邻行的背景连成一片，文字也不在背景里垂直居中。
const LINE_HEIGHT: f32 = 1.3;

/// 单行高度（逻辑像素），`max_h` 与逐行布局共用。
const ROW_HEIGHT: f32 = FONT_SIZE * LINE_HEIGHT;

/// 行间空隙：多行选中时相邻行的背景不至于连成一整块。
const LINE_GAP: f32 = SPACE_XXS;

/// 鼠标点选回调：参数是（事件、行号、行内字符下标）。
pub type PickHandler =
    Rc<dyn Fn(&gpui::MouseDownEvent, usize, usize, &mut gpui::Window, &mut gpui::App)>;

/// 鼠标拖动（已按下左键）回调：参数同 [`PickHandler`]。
pub type DragHandler =
    Rc<dyn Fn(&gpui::MouseMoveEvent, usize, usize, &mut gpui::Window, &mut gpui::App)>;

/// 渲染输入框内容（只读：对话框、补全过滤框等场景）。
pub fn render(editor: &Editor, theme: &Theme) -> AnyElement {
    render_inner(editor, theme, None, None, None)
}

/// 渲染可点选的输入框，并按 `wrap_width` 折行（主输入框用）。
///
/// 每个字符单独挂命中区域，点击/拖动直接落到 (行, 列)，省掉「像素 → 字符」换算。
///
/// 不靠 flex 自动换行：逐字符的 flex 项在换行容器里会把宽度算成「最宽那个字符」，
/// 整块内容会跑出输入卡片；视觉行由 [`wrap_chunks`] 按排版器量出的真实字宽切。
/// 超出 [`MAX_VISIBLE_LINES`] 的部分靠滚动看，光标移动时会自动滚进视口。
pub fn render_wrapped(
    editor: &Editor,
    theme: &Theme,
    pick: PickHandler,
    drag: DragHandler,
    wrap_width: gpui::Pixels,
    scroll: &gpui::ScrollHandle,
    cursor_mark: &Cell<Option<(usize, usize)>>,
    window: &gpui::Window,
) -> AnyElement {
    let wrap = Wrap {
        width: wrap_width,
        scroll,
        cursor_mark,
        window,
    };
    render_inner(editor, theme, Some(pick), Some(drag), Some(wrap))
}

/// 折行上下文：可用宽度、滚动句柄、上次让光标滚进视口的位置，以及量字宽用的窗口。
struct Wrap<'a> {
    /// 可用内宽（逻辑像素）
    width: gpui::Pixels,
    /// 输入框的滚动句柄
    scroll: &'a gpui::ScrollHandle,
    /// 上次触发过「滚到光标」的 (行, 列)：光标没动就不抢用户的滚动
    cursor_mark: &'a Cell<Option<(usize, usize)>>,
    /// 排版器入口，`char_width` 要用
    window: &'a gpui::Window,
}

/// 按可用宽度把一行切成若干视觉行（每段是该行的字符区间）。
///
/// `width_of` 给出单个字符的排印宽度；空行返回一个空区间，保证仍然渲染出一行（光标可见）。
pub fn wrap_chunks(
    line: &str,
    wrap_width: f32,
    mut width_of: impl FnMut(char) -> f32,
) -> Vec<std::ops::Range<usize>> {
    let chars = line.chars().count();
    if wrap_width <= 0. {
        return std::iter::once(0..chars).collect();
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut used = 0.0_f32;
    for (index, ch) in line.chars().enumerate() {
        let width = width_of(ch);
        if used + width > wrap_width && index > start {
            chunks.push(start..index);
            start = index;
            used = 0.;
        }
        used += width;
    }
    chunks.push(start..chars);
    chunks
}

/// [`render`] 与 [`render_wrapped`] 的公共实现。
fn render_inner(
    editor: &Editor,
    theme: &Theme,
    pick: Option<PickHandler>,
    drag: Option<DragHandler>,
    wrap: Option<Wrap<'_>>,
) -> AnyElement {
    let text_color = theme.text;
    let mut column = div()
        .id("composer")
        .w_full()
        .flex()
        .flex_col()
        .max_h(px((ROW_HEIGHT + LINE_GAP) * MAX_VISIBLE_LINES as f32))
        .gap(px(LINE_GAP))
        .text_size(px(FONT_SIZE))
        .line_height(gpui::relative(LINE_HEIGHT));
    match wrap.as_ref() {
        Some(wrap) => column = column.overflow_y_scroll().track_scroll(wrap.scroll),
        // 只读场景（对话框、补全过滤框）不滚，超出部分直接裁掉
        None => column = column.overflow_hidden(),
    }
    // 字宽按字符缓存：同一帧里重复字符不重复排版
    let mut widths: HashMap<char, f32> = HashMap::new();
    // 视觉行号（滚动容器的子项下标）与光标所在的视觉行
    let mut visual_row = 0usize;
    let mut cursor_visual = 0usize;

    for row in 0..editor.lines().len() {
        let line = &editor.lines()[row];
        let selection = editor.selection_row(row);
        let cursor_col = (editor.cursor().row == row).then_some(editor.cursor().col);
        // 逐字符的显示类别：普通 / 选中 / 光标所在字符
        let mut kinds: Vec<RunKind> = Vec::new();
        for (text, kind) in runs(line, selection, cursor_col) {
            kinds.extend(std::iter::repeat_n(kind, text.chars().count()));
        }
        let chars: Vec<char> = line.chars().collect();
        let chunks = match wrap.as_ref() {
            Some(wrap) => {
                let font_size = px(FONT_SIZE);
                let width = f32::from(wrap.width);
                wrap_chunks(line, width, |ch| {
                    char_width(ch, font_size, wrap.window, &mut widths)
                })
            }
            None => std::iter::once(0..chars.len()).collect(),
        };
        // 光标落在第几个视觉行（列超出最后一个分段时算在该行最后一行）
        let cursor_chunk = cursor_col.map(|col| {
            chunks
                .iter()
                .position(|chunk| col < chunk.end)
                .unwrap_or(chunks.len().saturating_sub(1))
        });
        for (chunk_index, chunk) in chunks.iter().enumerate() {
            if cursor_chunk == Some(chunk_index) {
                cursor_visual = visual_row;
            }
            visual_row += 1;
            let chunk = chunk.clone();
            let mut line_row = div()
                .w_full()
                .flex()
                .flex_shrink_0()
                .items_center()
                .h(px(ROW_HEIGHT));
            // 点空白处（行尾之后）落到行尾
            if let Some(pick) = pick.clone() {
                let end = line.chars().count();
                line_row =
                    line_row.on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                        pick(event, row, end.saturating_add(1), window, cx);
                        cx.stop_propagation();
                    });
            }
            for at in chunk.clone() {
                let kind = kinds.get(at).copied().unwrap_or(RunKind::Normal);
                let mut element = div()
                    .id(gpui::ElementId::Name(format!("c{row}:{at}").into()))
                    .child(SharedString::from(
                        chars.get(at).map(|ch| ch.to_string()).unwrap_or_default(),
                    ));
                match kind {
                    RunKind::Normal => {}
                    RunKind::Selected => {
                        element = element.bg(theme.selection);
                    }
                    RunKind::Cursor => {
                        element = element.bg(theme.accent).text_color(theme.on_inverse);
                    }
                }
                if let Some(pick) = pick.clone() {
                    element =
                        element.on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                            pick(event, row, at, window, cx);
                            cx.stop_propagation();
                        });
                }
                if let Some(drag) = drag.clone() {
                    element = element.on_mouse_move(move |event, window, cx| {
                        if event.pressed_button == Some(gpui::MouseButton::Left) {
                            drag(event, row, at, window, cx);
                        }
                    });
                }
                line_row = line_row.child(element);
            }
            // 光标在行尾（或空行）：补一个可见方块，否则整行没有光标
            if chunk.end == chars.len() && cursor_col.is_some_and(|col| col >= chars.len()) {
                line_row = line_row.child(
                    div()
                        .id(gpui::ElementId::Name(format!("c{row}:end").into()))
                        .bg(theme.accent)
                        .text_color(theme.on_inverse)
                        .child(SharedString::from(" ")),
                );
            }
            column = column.child(line_row);
        }
    }
    // 光标动了才把它滚进视口：手动滚轮滚远了不会被每帧拽回来
    if let Some(wrap) = wrap.as_ref() {
        let cursor = (editor.cursor().row, editor.cursor().col);
        if wrap.cursor_mark.get() != Some(cursor) {
            wrap.cursor_mark.set(Some(cursor));
            wrap.scroll.scroll_to_item(cursor_visual);
        }
    }
    column.text_color(text_color).into_any_element()
}

/// 单个字符的排印宽度：排版器量真字形，不做等宽估算（CJK、回退字体都按实际算）。
fn char_width(
    ch: char,
    font_size: gpui::Pixels,
    window: &gpui::Window,
    cache: &mut HashMap<char, f32>,
) -> f32 {
    if let Some(width) = cache.get(&ch) {
        return *width;
    }
    let style = gpui::TextStyle {
        font_family: UI_FONT_FAMILY.into(),
        ..Default::default()
    };
    let mut buffer = [0u8; 4];
    let text = ch.encode_utf8(&mut buffer);
    let runs = [style.to_run(text.len())];
    let width = f32::from(
        window
            .text_system()
            .layout_line(text, font_size, &runs, None)
            .width,
    );
    cache.insert(ch, width);
    width
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

    /// 折行：按每个字符给定的宽度累加，超宽就切一个新视觉行。
    #[test]
    fn wrap_chunks_split_by_width() {
        // 10 个 ASCII，每字符 10 宽，行宽 40 → 每行 4 个
        assert_eq!(
            wrap_chunks("abcdefghij", 40., |_| 10.),
            vec![0..4, 4..8, 8..10]
        );
        // 全角占两格：行宽 40 时一行只放得下 2 个汉字
        assert_eq!(
            wrap_chunks("中文测试", 40., |ch| if ch.is_ascii() {
                10.
            } else {
                20.
            }),
            vec![0..2, 2..4]
        );
        // 空行仍要有一行（否则光标没地方画）
        assert_eq!(wrap_chunks("", 40., |_| 10.), vec![0..0]);
        // 宽度未知时不折行
        assert_eq!(wrap_chunks("abcdef", 0., |_| 10.), vec![0..6]);
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
