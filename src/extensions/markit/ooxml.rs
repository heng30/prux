//! docx / pptx / epub 共用的 ZIP 与 XML 读取工具。

use quick_xml::{
    Reader, XmlVersion,
    escape::resolve_predefined_entity,
    events::{BytesStart, Event},
};
use std::io::{Cursor, Read};
use zip::ZipArchive;

/// 单个 ZIP 成员解压后的字节上限（64 MiB），防解压炸弹把内存吃穿。
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;

/// 按**本地名**取属性值（忽略 `w:` / `a:` 之类前缀）；不存在或取值失败返回 `None`。
pub(super) fn attr(e: &BytesStart<'_>, want: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == want)
        .and_then(|a| {
            a.normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// 打开内存里的 ZIP；不是合法 ZIP 时返回给用户看的错误。
pub(super) fn open(bytes: &[u8]) -> Result<ZipArchive<Cursor<&[u8]>>, String> {
    ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not a valid zip container: {e}"))
}

/// 读一个 ZIP 成员的完整内容。
///
/// 成员不存在（或本身是目录）返回 `Ok(None)`；解压失败、成员超过 [`MAX_ENTRY_BYTES`] 返回 `Err`。
pub(super) fn read_entry(
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Result<Option<Vec<u8>>, String> {
    let file = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(format!("cannot read {name}: {e}")),
    };
    if file.is_dir() {
        return Ok(None);
    }
    if file.size() > MAX_ENTRY_BYTES {
        return Err(format!(
            "{name} is too large to parse ({} bytes; limit {MAX_ENTRY_BYTES})",
            file.size()
        ));
    }

    let mut buf = Vec::with_capacity(file.size().min(1 << 20) as usize);
    file.take(MAX_ENTRY_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot decompress {name}: {e}"))?;
    Ok(Some(buf))
}

/// 按名取一个 XML 成员的文本（解析前按 UTF-8 有损转换）；成员缺失返回 `Ok(None)`。
pub(super) fn read_xml(
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Result<Option<String>, String> {
    Ok(read_entry(zip, name)?.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

/// 把 `Text` / `CData` / `GeneralRef` 三类事件的可见字符追加到 `out`。
///
/// `quick-xml` 0.42 把实体引用拆成了独立的 `GeneralRef` 事件，因此三处都得处理，
/// 否则 `&amp;` 之类会整段消失。
pub(super) fn push_text(event: &Event<'_>, out: &mut String) {
    match event {
        Event::Text(t) => out.push_str(t),
        Event::CData(c) => out.push_str(c),
        Event::GeneralRef(r) => {
            if let Ok(Some(ch)) = r.resolve_char_ref() {
                out.push(ch);
            } else if let Some(repl) = resolve_predefined_entity(r.as_ref()) {
                out.push_str(repl);
            }
        }
        _ => {}
    }
}

/// 用给定字符串跑一遍 XML 事件循环，把每个事件交给 `f`；解析错误转成用户可读的 `Err`。
pub(super) fn walk<F>(xml: &str, mut f: F) -> Result<(), String>
where
    F: FnMut(&Event<'_>) -> Result<(), String>,
{
    let mut reader = Reader::from_str(xml);
    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("malformed XML: {e}"))?;
        if matches!(event, Event::Eof) {
            return Ok(());
        }
        f(&event)?;
    }
}
