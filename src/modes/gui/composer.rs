//! 输入行（composer）：把 [`Editor`] 状态渲染成多行文本，带光标与选区。

use super::{
    text_edit::Editor,
    theme::{FONT_BODY, RADIUS_MD, SPACE_SM, SPACE_XXS, Theme, UI_FONT_FAMILY, px},
};
use gpui::{
    AnyElement, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div,
};
use std::{cell::Cell, collections::HashMap, rc::Rc};

/// 输入行最多显示的行数（超出部分靠滚动，避免占满整屏）。
const MAX_VISIBLE_LINES: usize = 8;

/// 输入框字号：与对话流正文同一档（[`FONT_BODY`]）。
const FONT_SIZE: f32 = FONT_BODY;

/// 输入框行高倍数：gpui 默认是 φ（1.618em），选中/光标背景会比字形高出一截、
/// 多行时相邻行的背景连成一片，文字也不在背景里垂直居中。
///
/// 下限受字体几何约束：gpui 逐行把字形裁在自己的行盒里，行盒矮于字体的
/// ascent + descent（思源黑体 1.16em + 0.288em = 1.448em）就会削掉行底的字尾
/// （Windows 上落笔位置更低，「g」的下伸部直接被切平）。
const LINE_HEIGHT: f32 = 1.5;

/// 单行高度（逻辑像素），`max_h` 与逐行布局共用。
const ROW_HEIGHT: f32 = FONT_SIZE * LINE_HEIGHT;

/// 行间空隙：多行选中时相邻行的背景不至于连成一整块。
const LINE_GAP: f32 = SPACE_XXS;

/// 表单输入框（[`field`]）的自然高度：上下描边 + 上下内边距 + 单行行高。
///
/// 行高取 [`ROW_HEIGHT`]（未按物理像素取整的 19.5），差别不到 1px，
/// 搜索框只需要一个稳定的基准高度。
const FIELD_H: f32 = 2. + SPACE_SM * 2. + ROW_HEIGHT;

/// 搜索框高度相对表单输入框自然高度的比例：侧栏会话搜索与面板搜索比对话框输入框矮一档。
const SEARCH_SCALE: f32 = 0.8;

/// 搜索框（[`field_search`]）的固定高度：表单输入框自然高度的 0.8 倍。
const SEARCH_FIELD_H: f32 = FIELD_H * SEARCH_SCALE;

/// 光标 / 选中块的高度：字形墨迹，不取行盒高度。思源黑体的汉字在字身框里上下各留
/// 0.12em，墨迹正好 1em；块比墨迹高就从字底伸出去一截，看着像“光标过长”。
const BLOCK_H: f32 = FONT_SIZE;

/// 块相对行盒顶的下移量。行盒按 ascent（1.16em）/ descent（0.288em）居中，墨迹的重心
/// 因此落在行盒中心下方约 0.06em（与 [`super::theme::TEXT_RAISE_EM`] 说的是同一个不对称）：
/// 块跟行盒一样居中就会“光标比字高”，所以再往下带这一段。带上之后块顶与字顶齐平，块底也正好落在字底。
const BLOCK_TOP: f32 = (ROW_HEIGHT - BLOCK_H) / 2. + FONT_SIZE * 0.06;

/// 鼠标点选回调：参数是（事件、行号、行内字符下标）。
pub type PickHandler =
    Rc<dyn Fn(&gpui::MouseDownEvent, usize, usize, &mut gpui::Window, &mut gpui::App)>;

/// 鼠标拖动（已按下左键）回调：参数同 [`PickHandler`]。
pub type DragHandler =
    Rc<dyn Fn(&gpui::MouseMoveEvent, usize, usize, &mut gpui::Window, &mut gpui::App)>;

/// 渲染输入框内容（只读：对话框、补全过滤框等场景）。
pub fn render(editor: &Editor, theme: &Theme) -> AnyElement {
    render_inner(editor, theme, None, None, None, true)
}

/// 搜索框一类单行输入框的渲染（侧栏会话搜索、面板文件搜索、列表面板搜索）。
///
/// 字形排版与光标就是 [`render`] 那一套：块状光标贴在字形上，位置自然对得上。
/// 空且未聚焦时只显示占位文案——不聚焦的框里没有光标，也不需要画字形。
pub fn render_field(
    editor: &Editor,
    theme: &Theme,
    placeholder: impl Into<SharedString>,
    focused: bool,
) -> AnyElement {
    if !focused && editor.is_empty() {
        return div()
            .text_size(px(FONT_SIZE))
            .text_color(theme.text_ghost)
            .child(placeholder.into())
            .into_any_element();
    }
    render_inner(editor, theme, None, None, None, focused)
}

/// 表单输入框的外框：与对话框（设置 API key、重命名会话）里的输入框同一套底色、
/// 描边与内边距，高度由字形与内边距决定，不写死。
pub fn field(theme: &Theme, focused: bool) -> gpui::Div {
    div()
        .w_full()
        .rounded(px(RADIUS_MD))
        .bg(theme.canvas)
        .border_1()
        .border_color(if focused {
            theme.border_strong
        } else {
            theme.border
        })
        .px(px(SPACE_SM))
        .py(px(SPACE_SM))
}

/// 搜索框（侧栏会话搜索、右侧文件面板搜索）的外框：底色、描边、左右内边距与 [`field`] 同一套，
/// 高度固定为 [`SEARCH_FIELD_H`]（表单输入框的 0.8 倍），内容在框内垂直居中。
pub fn field_search(theme: &Theme, focused: bool) -> gpui::Div {
    field(theme, focused)
        .h(px(SEARCH_FIELD_H))
        .py_0()
        .flex()
        .items_center()
}

/// 渲染可点选的输入框，并按 `wrap_width` 折行（主输入框用）。
///
/// 每个字符单独挂命中区域，点击/拖动直接落到 (行, 列)，省掉「像素 → 字符」换算。
///
/// 不靠 flex 自动换行：逐字符的 flex 项在换行容器里会把宽度算成「最宽那个字符」，
/// 整块内容会跑出输入卡片；视觉行由 [`wrap_chunks`] 按排版器量出的真实字宽切。
/// 超出 [`MAX_VISIBLE_LINES`] 的部分靠滚动看，光标移动时会自动滚进视口。
// 参数都是渲染期上下文，各调用点传的取值都不同，包结构体反而绕。
#[allow(clippy::too_many_arguments)]
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
    render_inner(editor, theme, Some(pick), Some(drag), Some(wrap), true)
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
/// `wrap_width` 与 `width_of` 必须用**同一个单位**（[`render_inner`] 两边都给设备像素）；
/// 空行返回一个空区间，保证仍然渲染出一行（光标可见）。
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

/// 按行切视觉行，给行尾光标留一格：光标在行尾而最后一段已排满时，
/// 再补一个空段把光标排到下一视觉行。
///
/// 不补的话行尾那个光标方块（一个空格宽）会顶出卡片右边——看着就是“多出一个字符”。
fn wrap_chunks_for_line(
    line: &str,
    wrap_width: f32,
    cursor_at_end: bool,
    mut width_of: impl FnMut(char) -> f32,
) -> Vec<std::ops::Range<usize>> {
    let mut chunks = wrap_chunks(line, wrap_width, &mut width_of);
    if cursor_at_end && wrap_width > 0. {
        let used: f32 = chunks
            .last()
            .map(|chunk| {
                line.chars()
                    .skip(chunk.start)
                    .take(chunk.end - chunk.start)
                    .map(&mut width_of)
                    .sum()
            })
            .unwrap_or(0.);
        if used + width_of(' ') > wrap_width {
            let end = line.chars().count();
            chunks.push(end..end);
        }
    }
    chunks
}

/// [`render`] 与 [`render_wrapped`] 的公共实现。
fn render_inner(
    editor: &Editor,
    theme: &Theme,
    pick: Option<PickHandler>,
    drag: Option<DragHandler>,
    wrap: Option<Wrap<'_>>,
    show_cursor: bool,
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
        let cursor_col = (show_cursor && editor.cursor().row == row).then_some(editor.cursor().col);
        // 逐字符的显示类别：普通 / 选中 / 光标所在字符
        let mut kinds: Vec<RunKind> = Vec::new();
        for (text, kind) in runs(line, selection, cursor_col) {
            kinds.extend(std::iter::repeat_n(kind, text.chars().count()));
        }
        let chars: Vec<char> = line.chars().collect();
        let chunks = match wrap.as_ref() {
            Some(wrap) => {
                let font_size = px(FONT_SIZE);
                // 折行预算用设备像素（整数）：布局引擎就是在这个平面上给每个字符元素定宽的。
                // 容器宽向下取整、字符宽向上取整，两个方向都偏保守——只会少排一个字，不会溢出。
                let width = (f32::from(wrap.width) * wrap.window.scale_factor()).floor();
                let at_end = cursor_col.is_some_and(|col| col >= chars.len());
                wrap_chunks_for_line(line, width, at_end, |ch| {
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
                    .relative();
                // 块先画、字形后画：绝对定位的块不参与布局，行盒与字形位置都不动
                match kind {
                    RunKind::Normal => {}
                    RunKind::Selected => {
                        element = element.child(block(theme.selection));
                    }
                    RunKind::Cursor => {
                        element = element
                            .text_color(theme.on_inverse)
                            .child(block(theme.accent));
                    }
                }
                element = element.child(SharedString::from(
                    chars.get(at).map(|ch| ch.to_string()).unwrap_or_default(),
                ));
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
            // 光标在行尾（或空行）：补一个可见方块，否则整行没有光标。
            // 只看最后一个视觉行：行排满时前面还有一个正常段的 `end` 也是行尾。
            if chunk_index + 1 == chunks.len() && cursor_col.is_some_and(|col| col >= chars.len()) {
                line_row = line_row.child(
                    div()
                        .id(gpui::ElementId::Name(format!("c{row}:end").into()))
                        .relative()
                        .text_color(theme.on_inverse)
                        .child(block(theme.accent))
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

/// 光标 / 选中块：绝对定位在字符格里，不占布局、也不改字形位置。
fn block(color: gpui::Hsla) -> gpui::Div {
    div()
        .absolute()
        .left_0()
        .right_0()
        .top(px(BLOCK_TOP))
        .h(px(BLOCK_H))
        .bg(color)
}

/// 单个字符占的宽度，单位是**设备像素**（整数）：与布局引擎给这个字符元素定宽的算式
/// 逐步对齐，折行预算才不会与实际排布错开。
///
/// 两步取整缺一不可：文本元素量宽时把字形宽向上取整到整数逻辑像素
/// （`elements/text.rs` 的 `size.width.ceil()`），布局引擎再把每个实测尺寸向上取整到
/// 整数设备像素（`taffy.rs` 的 `ceil_to_device_pixel`）。只取第一道时预算比实际占位
/// 小，逐字累加的差额把排满的行顶出盒子（行尾字符被裁），且缩放级别一变就换一批字
/// 触发、看起来时好时坏。
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
    let raw = f32::from(
        window
            .text_system()
            .layout_line(text, font_size, &runs, None)
            .width,
    );
    let width = device_width(raw, window.scale_factor());
    cache.insert(ch, width);
    width
}

/// 把排版器量出的逻辑宽度换算成布局引擎实际占的**设备像素**（整数）：
/// 先按文本元素量宽那样向上取整到整数逻辑像素，再向上取整到整数设备像素。
fn device_width(logical: f32, scale: f32) -> f32 {
    (logical.ceil() * scale).ceil()
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

    /// 折行预算必须跟布局引擎的双重向上取整对齐：先到整数逻辑像素，再到整数设备像素。
    ///
    /// 只取一道的话预算比实际占位小，一行排满时累计差额就把行尾字符顶出盒子；
    /// 差额随缩放级别变（哪批字跨过像素边界会换），所以“某个缩放下好、换一个就坏”。
    #[test]
    fn char_width_matches_layout_rounding() {
        // 文本元素量与布局量的两道向上取整
        assert_eq!(device_width(16.926, 1.25), 22.); // ceil(16.926)=17 → ceil(21.25)=22
        assert_eq!(device_width(27.3, 1.25), 35.); // ceil(27.3)=28 → ceil(35.0)=35
        assert_eq!(device_width(7.215, 1.25), 10.); // ceil=8 → ceil(10.0)=10
        assert_eq!(device_width(13., 1.), 13.);
        // 离整数最近的字符也会把小数部分抹掉：预算永远 ≥ 真实逻辑宽度
        for raw in [7.01_f32, 7.5, 7.99, 16.926, 27.3] {
            for scale in [1., 1.25, 1.5, 2.625] {
                assert!(device_width(raw, scale) >= raw * scale - 1e-3);
            }
        }
    }

    /// 行尾光标也占一格：排满时另起一视觉行，还有空位就留在行尾。
    #[test]
    fn tail_cursor_gets_its_own_row_when_line_is_full() {
        assert_eq!(
            wrap_chunks_for_line("abcdefgh", 40., true, |_| 10.),
            vec![0..4, 4..8, 8..8]
        );
        // 光标不在行尾时不多出一行
        assert_eq!(
            wrap_chunks_for_line("abcdefgh", 40., false, |_| 10.),
            vec![0..4, 4..8]
        );
        assert_eq!(wrap_chunks_for_line("abc", 40., true, |_| 10.), vec![0..3]);
    }

    /// 光标块只盖字形墨迹：块高一个字身，块顶、块底都与字的墨迹上下沿齐平。
    #[test]
    fn cursor_block_covers_the_glyph_ink_only() {
        // 思源黑体几何：半行距 + ascent 得基线，汉字墨迹在 0.88em…-0.12em
        let baseline = (LINE_HEIGHT - 1.448) / 2. + 1.16;
        let ink_top = (baseline - 0.88) * FONT_SIZE;
        let ink_bottom = (baseline + 0.12) * FONT_SIZE;
        assert!((BLOCK_H - FONT_SIZE).abs() < 1e-3, "块高应为一字墨迹");
        assert!((BLOCK_TOP - ink_top).abs() < 0.1, "块顶应与墨迹顶齐平");
        assert!(
            (BLOCK_TOP + BLOCK_H - ink_bottom).abs() < 0.1,
            "块底应与墨迹底齐平"
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
