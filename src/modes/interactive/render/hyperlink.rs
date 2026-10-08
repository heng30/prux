//! OSC8 超链接：把渲染层插在文本里的哨兵翻译成终端的超链接转义序列。
//!
//! ratatui 的 text 渲染路径（`Buffer::set_stringn`）会丢弃含控制字符的 grapheme，所以
//! `Span` 里塞 OSC8 序列走不通；但 `Cell::set_symbol` + `CellDiffOption::ForcedWidth` 是
//! ratatui 为超链接预留的正路（见 ratatui-core `buffer.rs` 的 `merge_diff_split_link` 测试）：
//! 把一个 cell 的 symbol 换成「OSC8 序列 + 该字符」并声明宽度 1，列数与视觉都不变。
//!
//! 因此渲染层只需在链接文本首尾各放一个哨兵（[`LINK_START`] / [`LINK_END`]，各占 1 列），
//! 本模块在 buffer 定型后（`render_frame` 的最后一步）把哨兵列还原成空格、并给相邻的
//! 首/末字符 cell 前后粘上起始/结束序列。折行后每行各有一组哨兵 → 每行独立开闭链接。

use super::super::app::LinkRow;
use ratatui::{
    Frame,
    buffer::{Buffer, CellDiffOption},
    layout::Rect,
    widgets::Widget,
};
use std::num::NonZeroU16;

/// 链接区间的起始哨兵：渲染层放在链接文本之前，占 1 列，定型时被换成 OSC8 起始序列。
pub(crate) const LINK_START: char = '\u{E000}';

/// 链接区间的结束哨兵：渲染层放在链接文本之后，占 1 列，定型时被换成 OSC8 结束序列。
pub(crate) const LINK_END: char = '\u{E001}';

/// OSC8 关闭序列。
const OSC8_END: &str = "\x1b]8;;\x1b\\";

/// `ForcedWidth` 用的宽度 1（一个 cell 占的列数）。
const ONE: NonZeroU16 = NonZeroU16::MIN;

/// OSC8 开启序列：`ESC ] 8 ; ; <url> ESC \\`，其后的字符都归属该链接，直到 [`OSC8_END`]。
fn osc8_start(url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\")
}

/// 在 buffer 定型后注入超链接：面板哨兵翻译 + 消息区显式区域注入。
///
/// 必须在**所有** widget 渲染完之后调用（`render_frame` 的末尾），否则后续 widget 会覆盖
/// 已经粘上序列的 cell。`enabled` 为 `false`（终端不支持 OSC8，见
/// [`hyperlinks_enabled`](crate::utils::terminal_caps::hyperlinks_enabled)）时：哨兵只被剥掉
/// （链接退化为纯文本），显式区域不做改动——点击仍由鼠标命中区负责。
pub(crate) fn apply(frame: &mut Frame, enabled: bool, url: Option<&str>, links: &[LinkRow]) {
    let url = if enabled { url } else { None };
    let links = if enabled { links } else { &[] };
    let area = frame.area();
    // `Frame` 不暴露 buffer，只能借一次 widget 渲染拿到 `&mut Buffer`。
    frame.render_widget(LinkPass { url, links }, area);
}

/// 执行替换的 widget：扫描哨兵并就地改写 cell symbol，再处理显式链接区域。
struct LinkPass<'a> {
    /// 面板哨兵对应的链接目标（登录面板一次只展示一个 URL）；`None` 表示只剥哨兵。
    url: Option<&'a str>,
    /// 消息区等给出的链接区域（每行一段，URL 各自独立）。
    links: &'a [LinkRow],
}

/// 把序列粘到 `(row, col)` 的 cell：`suffix` 为 true 时贴在该字符之后，否则之前。
/// 可见宽度不变（仍是 1 列），所以列对齐不受影响。
fn attach(buf: &mut Buffer, row: u16, col: u16, sequence: &str, suffix: bool) {
    let Some(cell) = buf.cell_mut((col, row)) else {
        return;
    };

    let text = cell.symbol().to_string();
    let symbol = if suffix {
        format!("{text}{sequence}")
    } else {
        format!("{sequence}{text}")
    };
    cell.set_symbol(&symbol);
    cell.set_diff_option(CellDiffOption::ForcedWidth(ONE));
}

impl Widget for LinkPass<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        self.render_sentinels(area, buf);
        self.render_links(area, buf);
    }
}

impl LinkPass<'_> {
    /// 消息区等显式给出的区域：直接按列区间贴序列，不需要哨兵。
    fn render_links(&self, area: Rect, buf: &mut Buffer) {
        for link in self.links {
            if link.col_end <= link.col_start
                || link.row < area.top()
                || link.row >= area.bottom()
                || link.url.is_empty()
            {
                continue;
            }

            let start = link.col_start.max(area.left());
            let end = link.col_end.min(area.right());
            if start >= end {
                continue;
            }

            attach(buf, link.row, start, &osc8_start(&link.url), false);
            attach(buf, link.row, end - 1, OSC8_END, true);
        }
    }

    /// 面板哨兵：链接行首/尾各一个，位置由渲染层插入（见 `panel::link_line`）。
    fn render_sentinels(&self, area: Rect, buf: &mut Buffer) {
        for y in area.top()..area.bottom() {
            let Some((start_x, end_x)) = sentinel_columns(buf, area, y) else {
                continue;
            };

            // 哨兵列本身还原成空格：布局与加哨兵前一致，只是链接文本左右各多 1 列空白。
            for x in [start_x, end_x] {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_symbol(" ");
                }
            }

            // 不支持超链接：哨兵剥掉即完成（文本原样可见）。
            let Some(url) = self.url else {
                continue;
            };

            // 起始序列粘在链接首字符 cell 上、结束序列粘在末字符 cell 上：
            // 可见宽度仍是 1，不会挤动后面的列。
            attach(buf, y, start_x + 1, &osc8_start(url), false);
            if end_x > 0 {
                attach(buf, y, end_x - 1, OSC8_END, true);
            }
        }
    }
}

/// 找出一行里两个哨兵所在的列，返回 `(起始哨兵列, 结束哨兵列)`。
///
/// 哨兵由 text 渲染路径逐个 grapheme 落格，因此总是单独占一个 cell（用 `starts_with` 判断即可）。
/// 没有哨兵、或结束哨兵不在起始哨兵右侧（区间为空）时返回 `None`，该行不做任何改动。
/// 一行只有一对：登录面板里一行就是一个折行段。
fn sentinel_columns(buf: &Buffer, area: Rect, y: u16) -> Option<(u16, u16)> {
    let mut start = None;
    let mut end = None;
    for x in area.left()..area.right() {
        let symbol = buf[(x, y)].symbol();
        if start.is_none() && symbol.starts_with(LINK_START) {
            start = Some(x);
        }
        if symbol.starts_with(LINK_END) {
            end = Some(x);
        }
    }

    match (start, end) {
        (Some(start), Some(end)) if end > start => Some((start, end)),
        _ => None,
    }
}

/// 去掉文本里的链接哨兵，得到纯可见文本。
///
/// 真实渲染路径里哨兵由 [`apply`] 换成转义序列或空格，不会抵达终端；
/// 供测试与调试读渲染文本时用（否则折行处的哨兵会夹在两段之间）。
#[cfg(test)]
pub(crate) fn strip(text: &str) -> String {
    text.chars()
        .filter(|c| *c != LINK_START && *c != LINK_END)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        Terminal,
        backend::TestBackend,
        text::{Line, Span},
        widgets::Paragraph,
    };

    /// 渲染一行（含哨兵）并返回该行 60 个 cell 的 symbol。
    ///
    /// `apply` 用显式 `enabled` 参数，测试不依赖所在终端的 OSC8 能力探测。
    fn line_cells_with(
        text: &str,
        enabled: bool,
        url: Option<&str>,
        links: &[LinkRow],
    ) -> Vec<String> {
        let backend = TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::raw(text.to_string()))),
                    frame.area(),
                );
                apply(frame, enabled, url, links);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..60)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect()
    }

    /// 不带显式区域时的便捷封装。
    fn line_cells(text: &str, enabled: bool, url: Option<&str>) -> Vec<String> {
        line_cells_with(text, enabled, url, &[])
    }

    #[test]
    fn sequences_attach_to_link_text_without_shifting_columns() {
        let url = "https://example.com/x";
        let cells = line_cells(&format!("  {LINK_START}{url}{LINK_END}"), true, Some(url));
        let first = 3;
        let last = first + url.len() - 1;

        // 第 0/1 列是前导空白；列 2 是起始哨兵 → 换成空格。
        assert_eq!(cells[2], " ");
        assert_eq!(cells[first], format!("\x1b]8;;{url}\x1b\\h"));
        // 中间字符原样（列号不变，没有整体位移）。
        assert_eq!(cells[first + 1], "t");
        // 末字符带关闭序列，末哨兵列换成空格。
        assert_eq!(cells[last], format!("x{OSC8_END}"));
        assert_eq!(cells[last + 1], " ");
    }

    #[test]
    fn disabled_or_missing_url_only_strips_sentinels() {
        let url = "https://example.com/x";
        for (enabled, target) in [(false, Some(url)), (true, None)] {
            let cells = line_cells(&format!(" {LINK_START}{url}{LINK_END}"), enabled, target);
            assert_eq!(cells[1], " ", "哨兵列应还原成空格");
            assert_eq!(cells[2], "h", "首字符不带序列；enabled={enabled}");
            assert_eq!(cells[3], "t");
            assert_eq!(cells[2 + url.len()], " ", "结束哨兵列应还原成空格");
            assert!(
                cells.iter().all(|cell| !cell.contains('\x1b')),
                "enabled={enabled} 时不该有转义序列"
            );
        }
    }

    #[test]
    fn line_without_sentinels_is_left_alone() {
        let cells = line_cells(" plain text", true, Some("https://example.com"));
        let text: String = cells.iter().map(String::as_str).collect();
        assert!(text.starts_with(" plain text"), "{text:?}");
    }

    #[test]
    fn explicit_regions_attach_sequences_to_first_and_last_cell() {
        let url = "https://example.com/docs";
        let links = [LinkRow {
            row: 0,
            col_start: 2,
            col_end: 6,
            url: url.to_string(),
        }];
        let cells = line_cells_with("  docs    ", true, None, &links);

        assert_eq!(
            cells[2],
            format!("\x1b]8;;{url}\x1b\\d"),
            "起始序列贴在首字符上"
        );
        assert_eq!(cells[3], "o", "中间字符原样");
        assert_eq!(cells[5], format!("s{OSC8_END}"), "结束序列贴在末字符上");
        assert_eq!(cells[6], " ", "区间之外的列不变");
        assert!(
            cells
                .iter()
                .all(|c| !c.contains(LINK_START) && !c.contains(LINK_END)),
            "显式区域不需要哨兵"
        );
    }

    #[test]
    fn disabled_explicit_regions_are_left_untouched() {
        let links = [LinkRow {
            row: 0,
            col_start: 2,
            col_end: 6,
            url: "https://example.com".to_string(),
        }];
        let cells = line_cells_with("  docs    ", false, None, &links);
        assert_eq!(cells[2], "d");
        assert!(
            cells.iter().all(|c| !c.contains('\x1b')),
            "终端不支持 OSC8 时不得注入序列"
        );
    }
}
