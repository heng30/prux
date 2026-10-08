//! Markdown 表格的**列宽布局**（类型无关）。
//!
//! 仓里有两套 Markdown 渲染器，产出类型不同：
//! - `modes/interactive/render/markdown.rs` → `ratatui::text::Line<Span<Style>>`（聊天区）；
//! - `core/extensions/markdown.rs` → `DockLine`（扩展覆盖层）。
//!
//! 依赖方向单向向上，`core` 不能依赖 `modes::interactive`，两边的**输出类型**也互不相干，
//! 所以渲染成各自类型的那部分必须各留一份。但"算列宽"是只依赖单元格**文本**的纯算法，
//! 放在这里共享：两边各自把单元格文本喂进来，拿回每列宽度后各自渲染。

use crate::utils::display::grapheme_width;

/// 边框与竖线占用的宽度：`│ ` + 列间 ` │ ` + 末尾 ` │`。
fn border_overhead(num_cols: usize) -> usize {
    3 * num_cols + 1
}

/// 扣掉边框后**留给所有列**的总宽度。
pub fn column_avail(width: usize, num_cols: usize) -> usize {
    width.saturating_sub(border_overhead(num_cols))
}

/// 最长单词显示宽度（列的可断点下限）。CJK 无空格时整段视作一个词。
fn longest_word_width(text: &str) -> usize {
    text.split_whitespace()
        .map(grapheme_width)
        .max()
        .unwrap_or(0)
}

/// 算每列宽度。
///
/// `header` / `rows` 是单元格（`C` 由调用方决定，`cell_text` 负责取纯文本）；
/// `avail` 是 [`column_avail`] 的结果。
///
/// 宽度一律按 `grapheme_width`（与 ratatui 落格一致）计量：emoji/ZWJ 序列按整体宽度算，
/// 不用 `display_width` 按 char 累加（会把 emoji 高估成多列、列宽虚高、表格撑破可用宽度）。
///
/// 规则：自然宽放得下就用自然宽；否则先压到最小宽（最长单词，clamp `1..=30`），
/// 再削最宽列（下限 1），硬保证 `sum(cols) <= avail`（当 `avail >= 列数` 时）。
/// 调用方应先处理"连一列都放不下"的退化情形（`avail < num_cols`）。
pub fn compute_column_widths<C>(
    header: &[C],
    rows: &[Vec<C>],
    cell_text: impl Fn(&C) -> String,
    avail: usize,
) -> Vec<usize> {
    let mut natural: Vec<usize> = header
        .iter()
        .map(|c| grapheme_width(&cell_text(c)))
        .collect();
    let mut min_w: Vec<usize> = natural.clone();

    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i >= natural.len() {
                continue;
            }
            let t = cell_text(cell);
            natural[i] = natural[i].max(grapheme_width(&t));
            min_w[i] = min_w[i].max(longest_word_width(&t).clamp(1, 30));
        }
    }

    let mut cols = natural.clone();
    let total_natural: usize = natural.iter().sum();
    if total_natural > avail {
        let mut overflow = total_natural - avail;

        while overflow > 0 {
            let mut reduced = false;

            for i in 0..cols.len() {
                if cols[i] > min_w[i] && overflow > 0 {
                    let cut = (cols[i] - min_w[i]).min(overflow);
                    cols[i] -= cut;
                    overflow -= cut;
                    reduced = true;
                }
            }

            if !reduced {
                break;
            }
        }
        // 不可断长词让最小宽之和超过 avail：继续削最宽列（下限 1）
        while overflow > 0 {
            let Some(i) = (0..cols.len())
                .filter(|&i| cols[i] > 1)
                .max_by_key(|&i| cols[i])
            else {
                break;
            };

            cols[i] -= 1;
            overflow -= 1;
        }
    } else {
        for (i, c) in cols.iter_mut().enumerate() {
            *c = (*c).max(min_w[i]);
        }
    }

    cols
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells<S: AsRef<str>>(row: &[S]) -> Vec<String> {
        row.iter().map(|s| s.as_ref().to_string()).collect()
    }

    #[test]
    fn natural_widths_when_they_fit() {
        let header = cells(&["Name", "Value"]);
        let rows = vec![cells(&["foo", "1"]), cells(&["bar", "22"])];
        let cols = compute_column_widths(&header, &rows, |c: &String| c.clone(), 40);
        assert_eq!(cols, vec![4, 5]);
    }

    #[test]
    fn shrinks_to_fit_avail() {
        let header = cells(&["Name", "Value"]);
        let rows = vec![cells(&["hello world foo bar", "1"])];
        let cols = compute_column_widths(&header, &rows, |c: &String| c.clone(), 13);
        assert_eq!(cols.iter().sum::<usize>(), 13);
        // 不可断长词下限 5 不会被压破
        assert_eq!(cols[1], 5);
    }

    #[test]
    fn border_overhead_matches_row_layout() {
        // 每行 = "│ " + Σ列宽 + 列间 " │ " + 末尾 " │"
        for n in 1..6 {
            assert_eq!(2 + 3 * (n - 1) + 2, border_overhead(n));
        }
    }
}
