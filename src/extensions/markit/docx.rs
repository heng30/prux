//! docx（WordprocessingML）→ Markdown。
//!
//! 只读 `word/document.xml`：段落按 `w:pStyle` 变标题、按 `w:numPr` 变列表项，
//! `w:tbl` 变 Markdown 表格。图表、脚注、批注、图片与页眉页脚都不读
//! （它们要么是别的 part，要么没有文本价值）。

use super::{collapse_blank_lines, escape_cell, ooxml, squash};
use quick_xml::events::{BytesStart, Event};

/// 正文所在的 ZIP 成员。
const DOCUMENT: &str = "word/document.xml";

/// 单张表格最多渲染的行数（含表头），超出部分只给一行省略说明。
const MAX_TABLE_ROWS: usize = 200;

/// 单张表格最多渲染的列数；超出部分丢弃，避免一行长到无法阅读。
const MAX_TABLE_COLS: usize = 32;

/// 把 docx 字节解析成 Markdown。
pub(super) fn convert(bytes: &[u8]) -> Result<String, String> {
    let mut zip = ooxml::open(bytes)?;
    let xml = ooxml::read_xml(&mut zip, DOCUMENT)?
        .ok_or_else(|| "docx has no word/document.xml; the file may be damaged".to_string())?;

    let mut renderer = Renderer::default();
    ooxml::walk(&xml, |event| {
        renderer.event(event);
        Ok(())
    })?;
    renderer.finish();
    Ok(renderer.into_text())
}

/// 解析过程中的状态机：把段落、列表项与表格收口成一串 Markdown 块。
#[derive(Default)]
struct Renderer {
    /// 已收口的块（段落 / 标题 / 列表项 / 整张表格），最终按顺序拼接。
    blocks: Vec<String>,
    /// 当前段落正在累积的文本。
    para: String,
    /// 当前段落的标题级别（来自 `w:pStyle`，已夹到 1..=6）。
    heading: Option<u8>,
    /// 当前段落是否列表项（出现 `w:numPr`）。
    list_item: bool,
    /// 是否正在表格内（决定段落文本落到单元格而不是块列表）。
    in_table: bool,
    /// 当前表格已完成的各行（每行是一串单元格文本）。
    rows: Vec<Vec<String>>,
    /// 当前行已完成的单元格。
    row: Vec<String>,
    /// 当前单元格正在累积的文本。
    cell: String,
}

impl Renderer {
    /// 处理一个 XML 事件。
    fn event(&mut self, event: &Event<'_>) {
        match event {
            Event::Start(e) | Event::Empty(e) => self.start(e),
            Event::End(e) => self.end(e.name().local_name().as_ref()),
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
                ooxml::push_text(event, self.sink())
            }
            _ => {}
        }
    }

    /// 文本往哪里追加：表格里进单元格，其它进当前段落。
    fn sink(&mut self) -> &mut String {
        if self.in_table {
            &mut self.cell
        } else {
            &mut self.para
        }
    }

    /// 处理开始标签（自闭合标签走同一分支）。
    fn start(&mut self, e: &BytesStart<'_>) {
        match e.name().local_name().as_ref() {
            "p" => {
                self.para.clear();
                self.heading = None;
                self.list_item = false;
            }
            "pStyle" => {
                self.heading = ooxml::attr(e, "val").as_deref().and_then(heading_level);
            }
            "numPr" => self.list_item = true,
            // 软换行 / 制表符在 markdown 里没有对应物，统一压成一个空格（`squash` 会再归一）
            "br" | "cr" | "tab" => self.sink().push(' '),
            "tbl" => {
                self.flush_para();
                self.in_table = true;
                self.rows.clear();
            }
            "tr" => self.row.clear(),
            "tc" => self.cell.clear(),
            _ => {}
        }
    }

    /// 处理结束标签。
    fn end(&mut self, name: &str) {
        match name {
            "p" => self.flush_para(),
            "tc" => self.flush_cell(),
            "tr" => self.flush_row(),
            "tbl" => {
                self.flush_row();
                self.flush_table();
                self.in_table = false;
            }
            _ => {}
        }
    }

    /// 收口当前段落：表格内的进当前单元格，其它按标题 / 列表项 / 普通段落落块。
    fn flush_para(&mut self) {
        let text = squash(&std::mem::take(&mut self.para));
        let heading = self.heading.take();
        let list_item = std::mem::take(&mut self.list_item);
        if text.is_empty() {
            return;
        }

        if self.in_table {
            if !self.cell.is_empty() {
                self.cell.push(' ');
            }
            self.cell.push_str(&text);
            return;
        }

        match heading {
            Some(level) => {
                let block = format!("{} {text}", "#".repeat(level as usize));
                self.push_block(block);
            }
            None if list_item => self.push_block(format!("- {text}")),
            None => self.push_block(text),
        }
    }

    /// 收口当前单元格。
    fn flush_cell(&mut self) {
        let cell = std::mem::take(&mut self.cell);
        self.row.push(squash(&cell));
    }

    /// 收口当前行（空行留到渲染时统一丢弃）。
    fn flush_row(&mut self) {
        if !self.row.is_empty() {
            self.rows.push(std::mem::take(&mut self.row));
        }
    }

    /// 把当前表格渲染成 Markdown 表格块（首行当表头）。
    fn flush_table(&mut self) {
        let rows: Vec<Vec<String>> = std::mem::take(&mut self.rows)
            .into_iter()
            .filter(|row| row.iter().any(|cell| !cell.trim().is_empty()))
            .collect();
        if rows.is_empty() {
            return;
        }

        let cols = rows
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .min(MAX_TABLE_COLS);
        if cols == 0 {
            return;
        }

        let mut out = String::new();
        for (index, row) in rows.iter().take(MAX_TABLE_ROWS).enumerate() {
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
        if rows.len() > MAX_TABLE_ROWS {
            out.push_str(&format!(
                "(table truncated: {} more rows omitted)\n",
                rows.len() - MAX_TABLE_ROWS
            ));
        }

        self.push_block(out.trim_end().to_string());
    }

    /// 落一个块；相邻列表项之间只隔一个换行，保持列表连续。
    fn push_block(&mut self, block: String) {
        match self.blocks.last_mut() {
            Some(prev) if prev.starts_with("- ") && block.starts_with("- ") => {
                prev.push('\n');
                prev.push_str(&block);
            }
            _ => self.blocks.push(block),
        }
    }

    /// 文档结束：把残留的段落 / 行 / 表格都收口。
    fn finish(&mut self) {
        self.flush_para();
        self.flush_row();
        self.flush_table();
    }

    /// 拼出最终 Markdown（连续空行压成一个）。
    fn into_text(self) -> String {
        collapse_blank_lines(&self.blocks.join("\n\n"))
    }
}

/// 把 Word 的段落样式名映射成标题级别；`None` 表示不是标题。
///
/// 只认 Word 默认样式名（`Title` / `Heading1`…`Heading9`），自定义样式不进标题层级——
/// 猜错层级比不给层级更糟。
fn heading_level(style: &str) -> Option<u8> {
    if style == "Title" {
        return Some(1);
    }
    if style == "Subtitle" {
        return Some(2);
    }
    let digits = style.strip_prefix("Heading")?;
    let level: u8 = digits.parse().ok()?;
    Some(level.clamp(1, 6))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_styles_map_to_levels() {
        assert_eq!(heading_level("Title"), Some(1));
        assert_eq!(heading_level("Subtitle"), Some(2));
        assert_eq!(heading_level("Heading1"), Some(1));
        assert_eq!(heading_level("Heading3"), Some(3));
        assert_eq!(heading_level("Heading9"), Some(6));
        assert_eq!(heading_level("HeadingX"), None);
        assert_eq!(heading_level("Quote"), None);
        assert_eq!(heading_level("My Heading 2"), None);
    }
}
