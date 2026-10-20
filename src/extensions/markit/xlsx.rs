//! xlsx / xls / xlsb / xlsm / ods → Markdown：每个工作表一段表格。
//!
//! 用 `calamine` 读（它按内容而非扩展名识别容器），因此 `.xlsm` 与 `.xlsx` 共用同一条路径。
//! 首行作表头，行尾全空的行与列会被裁掉；单元格里的 `|` 与换行会被转义。
//! 公式读的是**缓存值**（calamine 的 `worksheet_range` 语义），不是公式文本。

use super::{collapse_blank_lines, escape_cell};
use crate::utils::display::format_float;
use calamine::{Data, Range, Reader, Sheets, open_workbook_auto_from_rs};
use std::io::Cursor;

/// 单个工作表最多渲染的行数（含表头）。
const MAX_ROWS_PER_SHEET: usize = 200;

/// 单个工作表最多渲染的列数；超出部分丢弃。
const MAX_COLS_PER_SHEET: usize = 32;

/// 把电子表格字节解析成 Markdown。
pub(super) fn convert(bytes: &[u8]) -> Result<String, String> {
    let mut workbook: Sheets<Cursor<&[u8]>> = open_workbook_auto_from_rs(Cursor::new(bytes))
        .map_err(|e| format!("cannot parse spreadsheet: {e}"))?;

    let names = workbook.sheet_names();
    let mut sections = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let range = match workbook.worksheet_range_at(index) {
            Some(Ok(range)) => range,
            // 图表页 / 空页读不出范围：跳过，不影响其它工作表
            Some(Err(_)) | None => continue,
        };
        if let Some(section) = render_sheet(name, &range) {
            sections.push(section);
        }
    }
    Ok(collapse_blank_lines(&sections.join("\n\n")))
}

/// 把一张工作表渲染成 `## Sheet: <名>` + Markdown 表格；整张表空时返回 `None`。
fn render_sheet(name: &str, range: &Range<Data>) -> Option<String> {
    let mut rows: Vec<Vec<String>> = range
        .rows()
        .map(|row| row.iter().map(cell_text).collect())
        .collect();

    // 只裁行尾的整行空行：用户可能在表头前留空行，那些保留
    while rows
        .last()
        .is_some_and(|row| row.iter().all(|cell| cell.trim().is_empty()))
    {
        rows.pop();
    }
    if rows.is_empty() {
        return None;
    }

    let cols = rows
        .iter()
        .map(|row| {
            row.iter()
                .rposition(|cell| !cell.trim().is_empty())
                .map_or(0, |last| last + 1)
        })
        .max()
        .unwrap_or(0)
        .min(MAX_COLS_PER_SHEET);
    if cols == 0 {
        return None;
    }

    let mut out = format!("## Sheet: {name}\n\n");
    for (index, row) in rows.iter().take(MAX_ROWS_PER_SHEET).enumerate() {
        out.push('|');
        for column in 0..cols {
            let cell = row.get(column).map(String::as_str).unwrap_or("");
            out.push(' ');
            out.push_str(&escape_cell(cell));
            out.push_str(" |");
        }
        out.push('\n');
        if index == 0 {
            out.push('|');
            for _ in 0..cols {
                out.push_str(" --- |");
            }
            out.push('\n');
        }
    }
    if rows.len() > MAX_ROWS_PER_SHEET {
        out.push_str(&format!(
            "\n(sheet truncated: {} more rows omitted)\n",
            rows.len() - MAX_ROWS_PER_SHEET
        ));
    }

    Some(out.trim_end().to_string())
}

/// 单元格取值：空单元格给空串，日期按「有则带时间」的格式，浮点整数不带 `.0`。
fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(text) => text.clone(),
        Data::Int(value) => value.to_string(),
        Data::Float(value) => format_float(*value),
        Data::Bool(value) => value.to_string(),
        Data::DateTime(datetime) => datetime
            .as_datetime()
            .map(|value| {
                let midnight = value.time() == chrono::NaiveTime::MIN;
                if midnight {
                    value.format("%Y-%m-%d").to_string()
                } else {
                    value.format("%Y-%m-%d %H:%M:%S").to_string()
                }
            })
            .unwrap_or_else(|| datetime.to_string()),
        Data::DateTimeIso(text) | Data::DurationIso(text) => text.clone(),
        Data::Error(error) => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use calamine::Cell;

    #[test]
    fn cells_render_without_escaping_regressions() {
        assert_eq!(cell_text(&Data::Int(7)), "7");
        assert_eq!(cell_text(&Data::String("a|b".to_string())), "a|b");
        assert_eq!(cell_text(&Data::Bool(true)), "true");
        assert_eq!(cell_text(&Data::Empty), "");
        assert_eq!(
            cell_text(&Data::Error(calamine::CellErrorType::Div0)),
            "#DIV/0!"
        );
    }

    #[test]
    fn sheet_renders_header_separator_and_escapes_pipes() {
        let range = Range::from_sparse(vec![
            Cell::new((0, 0), Data::String("name".to_string())),
            Cell::new((0, 1), Data::String("qty".to_string())),
            Cell::new((1, 0), Data::String("a|b".to_string())),
            Cell::new((1, 1), Data::Float(2.0)),
        ]);

        let text = render_sheet("Sheet1", &range).expect("应有内容");
        assert!(text.starts_with("## Sheet: Sheet1"), "{text}");
        assert!(text.contains("| name | qty |"), "{text}");
        assert!(text.contains("| --- | --- |"), "{text}");
        assert!(text.contains("| a\\|b | 2 |"), "{text}");
    }

    #[test]
    fn empty_sheet_is_skipped() {
        let range = Range::new((0, 0), (2, 2));
        assert_eq!(render_sheet("Empty", &range), None);
    }

    #[test]
    fn trailing_empty_rows_are_trimmed() {
        let range = Range::from_sparse(vec![
            Cell::new((0, 0), Data::Int(1)),
            Cell::new((5, 0), Data::Int(9)),
        ]);
        let text = render_sheet("Rows", &range).expect("应有内容");
        // 中间空行原样保留（用空单元格），最后一行是 9，没有多余的空表行
        assert!(text.lines().last().unwrap().contains(" 9 |"), "{text}");
    }
}
