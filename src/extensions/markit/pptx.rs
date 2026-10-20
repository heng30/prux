//! pptx（PresentationML）→ Markdown：每张幻灯片一节。
//!
//! 只读 `ppt/slides/slideN.xml`（按**数字**排序，不是字典序）：标题占位符的段落作本节标题，
//! 其余段落按 `a:pPr lvl` 缩进成列表项。演讲者备注、动画、图表数据与图片不读。

use super::{collapse_blank_lines, ooxml, squash};
use quick_xml::events::{BytesStart, Event};
use std::io::Cursor;
use zip::ZipArchive;

/// 幻灯片正文成员的前缀与后缀（`ppt/slides/slide12.xml`）。
const SLIDE_PREFIX: &str = "ppt/slides/slide";
/// 幻灯片正文成员的扩展名后缀。
const SLIDE_SUFFIX: &str = ".xml";

/// 把 pptx 字节解析成 Markdown。
pub(super) fn convert(bytes: &[u8]) -> Result<String, String> {
    let mut zip = ooxml::open(bytes)?;
    let mut slides = slide_entries(&mut zip);
    if slides.is_empty() {
        return Err(format!(
            "pptx has no {SLIDE_PREFIX}N{SLIDE_SUFFIX} entries; the file may be damaged"
        ));
    }
    slides.sort_by_key(|(number, _)| *number);

    let mut sections = Vec::new();
    for (number, name) in slides {
        let Some(xml) = ooxml::read_xml(&mut zip, &name)? else {
            continue;
        };
        let section = render_slide(number, &xml)?;
        if !section.is_empty() {
            sections.push(section);
        }
    }
    Ok(collapse_blank_lines(&sections.join("\n\n")))
}

/// 列出所有幻灯片正文成员及其页码；不是幻灯片正文的成员（母版 / 版式）被过滤掉。
fn slide_entries(zip: &mut ZipArchive<Cursor<&[u8]>>) -> Vec<(u32, String)> {
    let mut slides = Vec::new();
    for name in zip.file_names() {
        let Ok(name) = name else {
            continue;
        };
        let Some(rest) = name.strip_prefix(SLIDE_PREFIX) else {
            continue;
        };
        let Some(digits) = rest.strip_suffix(SLIDE_SUFFIX) else {
            continue;
        };
        let Ok(number) = digits.parse::<u32>() else {
            continue;
        };
        slides.push((number, name.into_owned()));
    }
    slides
}

/// 把一张幻灯片渲染成一节 Markdown；整页没有文字时返回空串。
fn render_slide(number: u32, xml: &str) -> Result<String, String> {
    let mut slide = Slide::default();
    ooxml::walk(xml, |event| {
        slide.event(event);
        Ok(())
    })?;
    slide.finish();
    Ok(slide.into_text(number))
}

/// 单张幻灯片的解析状态机。
#[derive(Default)]
struct Slide {
    /// 当前所在形状是否是标题占位符（`p:ph type="title"` / `"ctrTitle"`）。
    title_shape: bool,
    /// 当前段落正在累积的文本。
    para: String,
    /// 当前段落的缩进级别（来自 `a:pPr` 的 `lvl`，默认 0）。
    level: usize,
    /// 幻灯片标题：取标题占位符里的第一个非空段落。
    title: Option<String>,
    /// 正文条目：(缩进级别, 文本)。
    body: Vec<(usize, String)>,
}

impl Slide {
    /// 处理一个 XML 事件。
    fn event(&mut self, event: &Event<'_>) {
        match event {
            Event::Start(e) | Event::Empty(e) => self.start(e),
            Event::End(e) => match e.name().local_name().as_ref() {
                "p" => self.flush_para(),
                "sp" => self.title_shape = false,
                _ => {}
            },
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
                ooxml::push_text(event, &mut self.para)
            }
            _ => {}
        }
    }

    /// 处理开始标签（自闭合标签走同一分支）。
    fn start(&mut self, e: &BytesStart<'_>) {
        match e.name().local_name().as_ref() {
            "sp" => self.title_shape = false,
            "ph" => {
                if matches!(
                    ooxml::attr(e, "type").as_deref(),
                    Some("title") | Some("ctrTitle")
                ) {
                    self.title_shape = true;
                }
            }
            "p" => {
                self.para.clear();
                self.level = 0;
            }
            "pPr" => {
                self.level = ooxml::attr(e, "lvl")
                    .and_then(|l| l.parse::<usize>().ok())
                    .unwrap_or(0)
                    .min(8);
            }
            "br" => self.para.push(' '),
            _ => {}
        }
    }

    /// 收口当前段落：标题占位符的第一个非空段落作标题，其余进正文。
    fn flush_para(&mut self) {
        let text = squash(&std::mem::take(&mut self.para));
        if text.is_empty() {
            return;
        }
        if self.title_shape && self.title.is_none() {
            self.title = Some(text);
        } else {
            self.body.push((self.level, text));
        }
    }

    /// 文档结束：收口最后一个段落。
    fn finish(&mut self) {
        self.flush_para();
    }

    /// 拼出这一节的 Markdown。
    fn into_text(self, number: u32) -> String {
        let mut out = match self.title {
            Some(title) => format!("## Slide {number}: {title}"),
            None => format!("## Slide {number}"),
        };
        for (level, text) in self.body {
            out.push('\n');
            out.push_str(&"  ".repeat(level));
            out.push_str("- ");
            out.push_str(&text);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slide_entries_are_filtered_by_name_shape() {
        let xml = r#"<p:sld xmlns:p="p"><p:cSld><p:spTree/></p:cSld></p:sld>"#;
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for name in [
            "ppt/slides/slide1.xml",
            "ppt/slides/slide12.xml",
            "ppt/slides/_rels/slide1.xml.rels",
            "ppt/slideLayouts/slideLayout1.xml",
            "ppt/slides/slideX.xml",
        ] {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            use std::io::Write;
            writer.write_all(xml.as_bytes()).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();

        let mut zip = ooxml::open(&bytes).unwrap();
        let mut found = slide_entries(&mut zip);
        found.sort();
        assert_eq!(
            found,
            vec![
                (1, "ppt/slides/slide1.xml".to_string()),
                (12, "ppt/slides/slide12.xml".to_string()),
            ]
        );
    }
}
