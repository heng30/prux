//! EPUB → Markdown：按 OPF spine 的顺序逐章转正文。
//!
//! 章节 HTML 交给 `html2text`（与 `web-access` 的 `readable` 同一套渲染），
//! 因此标题 / 列表 / 链接 / 代码块都会变成 Markdown。封面图、样式表、字体不读。
//! spine 读不出来时退化为「按成员名排序的所有 HTML 文件」，保证坏书也能出内容。

use super::{collapse_blank_lines, ooxml};
use quick_xml::events::Event;
use std::{collections::HashMap, io::Cursor};
use zip::ZipArchive;

/// `html2text` 的折行宽度：给足宽度，避免正文被折成碎行。
const HTML_WIDTH: usize = 1_000;

/// 容器清单所在的固定路径。
const CONTAINER: &str = "META-INF/container.xml";

/// 把 epub 字节解析成 Markdown。
pub(super) fn convert(bytes: &[u8]) -> Result<String, String> {
    let mut zip = ooxml::open(bytes)?;
    let chapters = chapter_entries(&mut zip)?;
    if chapters.is_empty() {
        return Err("epub has no readable chapters".to_string());
    }

    let mut parts = Vec::new();
    for name in chapters {
        let Some(data) = ooxml::read_entry(&mut zip, &name)? else {
            continue;
        };
        let text = html2text::from_read(&data[..], HTML_WIDTH).unwrap_or_default();
        let text = collapse_blank_lines(text.trim());
        if !text.is_empty() {
            parts.push(text);
        }
    }
    Ok(collapse_blank_lines(&parts.join("\n\n")))
}

/// 按 spine 顺序列出章节在 ZIP 里的成员路径；读不出 spine 时退回按名排序的 HTML 列表。
fn chapter_entries(zip: &mut ZipArchive<Cursor<&[u8]>>) -> Result<Vec<String>, String> {
    if let Some(opf_path) = package_path(zip)?
        && let Some(opf) = ooxml::read_xml(zip, &opf_path)?
    {
        let chapters = parse_package(&opf, parent_dir(&opf_path))?;
        if !chapters.is_empty() {
            return Ok(chapters);
        }
    }
    Ok(fallback_entries(zip))
}

/// 从 `META-INF/container.xml` 里取 OPF 的成员路径。
fn package_path(zip: &mut ZipArchive<Cursor<&[u8]>>) -> Result<Option<String>, String> {
    let Some(container) = ooxml::read_xml(zip, CONTAINER)? else {
        return Ok(None);
    };

    let mut found = None;
    ooxml::walk(&container, |event| {
        if let Event::Start(e) | Event::Empty(e) = event
            && e.name().local_name().as_ref() == "rootfile"
            && found.is_none()
        {
            found = ooxml::attr(e, "full-path").filter(|path| !path.is_empty());
        }
        Ok(())
    })?;
    Ok(found)
}

/// 解析 OPF：manifest 的 `id → href` 与 spine 的顺序拼成章节目录。
fn parse_package(opf: &str, base: &str) -> Result<Vec<String>, String> {
    let mut hrefs: HashMap<String, String> = HashMap::new();
    let mut spine: Vec<String> = Vec::new();

    ooxml::walk(opf, |event| {
        let (Event::Start(e) | Event::Empty(e)) = event else {
            return Ok(());
        };
        match e.name().local_name().as_ref() {
            "item" => {
                let (Some(id), Some(href)) = (ooxml::attr(e, "id"), ooxml::attr(e, "href")) else {
                    return Ok(());
                };
                let media = ooxml::attr(e, "media-type").unwrap_or_default();
                if is_html(&href, &media) {
                    hrefs.insert(id, resolve_href(base, &href));
                }
            }
            "itemref" => {
                if let Some(idref) = ooxml::attr(e, "idref") {
                    spine.push(idref);
                }
            }
            _ => {}
        }
        Ok(())
    })?;

    Ok(spine
        .into_iter()
        .filter_map(|idref| hrefs.get(&idref).cloned())
        .collect())
}

/// spine 不可用时的退路：所有 HTML/XHTML 成员，按成员名排序。
fn fallback_entries(zip: &mut ZipArchive<Cursor<&[u8]>>) -> Vec<String> {
    let mut names: Vec<String> = zip
        .file_names()
        .flatten()
        .filter(|name| is_html(name, ""))
        .map(|name| name.into_owned())
        .collect();
    names.sort();
    names
}

/// 判断一个 manifest 成员 / 成员名是不是可渲染的 HTML。
fn is_html(href: &str, media_type: &str) -> bool {
    if media_type.contains("html") {
        return true;
    }
    let path = href.split(['#', '?']).next().unwrap_or(href);
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".html") || lower.ends_with(".xhtml") || lower.ends_with(".htm")
}

/// 取一个成员路径的父目录（没有目录时给空串）。
fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) => &path[..index],
        None => "",
    }
}

/// 把 OPF 里的相对 `href` 解析成 ZIP 内的成员路径：处理百分号编码、查询串与 `..`。
fn resolve_href(base: &str, href: &str) -> String {
    let path = href.split(['#', '?']).next().unwrap_or(href);
    let decoded = percent_decode(path);
    let combined = if decoded.starts_with('/') {
        decoded.trim_start_matches('/').to_string()
    } else if base.is_empty() {
        decoded
    } else {
        format!("{base}/{decoded}")
    };

    let mut segments: Vec<&str> = Vec::new();
    for segment in combined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// 极简百分号解码（`%20` → 空格），非法序列原样保留。
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(value) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hrefs_resolve_relative_to_the_package_directory() {
        assert_eq!(resolve_href("OEBPS", "ch1.xhtml"), "OEBPS/ch1.xhtml");
        assert_eq!(
            resolve_href("OEBPS/text", "../ch1.xhtml"),
            "OEBPS/ch1.xhtml"
        );
        assert_eq!(resolve_href("", "ch1.xhtml"), "ch1.xhtml");
        assert_eq!(resolve_href("OEBPS", "/ch1.xhtml"), "ch1.xhtml");
        assert_eq!(
            resolve_href("OEBPS", "ch%201.xhtml#top"),
            "OEBPS/ch 1.xhtml"
        );
    }

    #[test]
    fn html_members_are_recognized_by_media_type_or_suffix() {
        assert!(is_html("ch1.xhtml", ""));
        assert!(is_html("ch1.HTML", ""));
        assert!(is_html("text01", "application/xhtml+xml"));
        assert!(is_html("ch1.xhtml?x=1", ""));
        assert!(!is_html("style.css", "text/css"));
        assert!(!is_html("cover.jpg", "image/jpeg"));
    }

    #[test]
    fn parent_dir_strips_the_file_name() {
        assert_eq!(parent_dir("OEBPS/content.opf"), "OEBPS");
        assert_eq!(parent_dir("content.opf"), "");
    }
}
