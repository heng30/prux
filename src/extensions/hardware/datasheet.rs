//! 资料导入：把一块芯片 / 板子的 PDF、图片与文本收进同一个目录，并生成总览。
//!
//! 目录布局（`agent_dir()/extensions/hardware/datasheets/<board>/`）：
//!
//! - `overview.md` —— 总览，每次导入重建。文本文件都按章节 / 文件名列在这里，
//!   `hardware_search` 先命中的就是它；
//! - `<stem>-chNN-<slug>.md` —— PDF 按书签（目录）拆出来的章节正文，检索走这些小块；
//! - 原件（`.pdf` / 图片）—— 原样留在同一目录，图片交给视觉模型看，PDF 可用 `read_doc`。
//!
//! 只抽 PDF 的**文本层**（`pdf-extract`）：扫描件抽出来是空的，此时明确报错，
//! 让用户直接用 `read_doc` 或看图，而不是在资料目录里留一堆空文件。

use super::search::TEXT_EXTENSIONS;
use crate::{
    core::{
        extensions::{ExtensionTool, ToolAnnotations, ToolExposure},
        tools::{ToolError, ToolResult},
    },
    extensions::util::{opt_string, req_str},
    utils::process::run_stdout,
};
use serde_json::{Value, json};
use std::{
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
};

/// 工具名。
pub(super) const TOOL_IMPORT: &str = "hardware_datasheet_import";

/// 总览文件名（每次导入重建）。
pub(super) const OVERVIEW_FILE: &str = "overview.md";

/// 单个源文件的大小上限：跟 `read_doc` 同一个量级，再大就不是能一次读完的手册。
const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;

/// 章节文件名的 slug 长度上限。
const MAX_SLUG_CHARS: usize = 40;

/// 总览里最多列多少个非文本资料。
const MAX_ASSETS_IN_OVERVIEW: usize = 40;

/// 能收的图片扩展名。
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "gif"];

/// 源文件是什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 有文本层的 PDF：抽章节正文。
    Pdf,
    /// 图片：只存原件，交给视觉模型。
    Image,
    /// 文本：原样存，直接被检索。
    Text,
}

/// 源文件的类别；扩展名不在白名单里返回 `None`。
fn classify(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if ext == "pdf" {
        Some(Kind::Pdf)
    } else if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        Some(Kind::Image)
    } else if TEXT_EXTENSIONS.contains(&ext.as_str()) {
        Some(Kind::Text)
    } else {
        None
    }
}

/// 目录名 / 文件名里允许的字符：字母数字与 `.` `_` `-`。
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// 文件主名（不含扩展名）。
fn stem_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "datasheet".to_string())
}

/// 章节标题 → 文件名片段：小写、非 ASCII 字母数字压成 `-`，空了就用 `ch<NN>`。
fn slug(text: &str, number: u32) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
        if out.chars().count() >= MAX_SLUG_CHARS {
            break;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        format!("ch{number:02}")
    } else {
        trimmed.to_string()
    }
}

/// 水印、页码这类反复出现在每页的噪声行。
fn is_noise(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed == "GoBack" {
        return true;
    }
    // 水印被拆成一片片 1~2 个字母（`PR` / `EL` / `IM` / `IN` / `A` / `RY`）
    trimmed.chars().count() <= 2 && trimmed.chars().all(|c| c.is_ascii_uppercase())
}

/// 一个章节：标题、页码区间与正文。
#[derive(Debug)]
struct Chapter {
    /// 章节序号（书签在目录里的顺序，1 基）。
    number: u32,
    /// 章节标题（取自 PDF 书签）。
    title: String,
    /// 起始页（1 基）。
    first_page: usize,
    /// 结束页（1 基，含）。
    last_page: usize,
    /// 正文（已去掉噪声行，页与页之间空一行）。
    body: String,
}

impl Chapter {
    /// 新建一章。
    fn new(number: u32, title: String, page: usize) -> Self {
        Chapter {
            number,
            title,
            first_page: page,
            last_page: page,
            body: String::new(),
        }
    }

    /// 追加一页的正文（先清掉水印碎片）。
    fn push_page(&mut self, page: usize, text: &str) {
        self.last_page = page;
        if !self.body.is_empty() {
            self.body.push_str("\n\n");
        }
        self.body.push_str(&clean_page(text));
    }
}

/// 一页正文：去掉水印碎片这类噪声行（只清正文，不参与章节判定）。
fn clean_page(page: &str) -> String {
    page.lines()
        .filter(|line| !is_noise(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// PDF 书签（目录）里的一条。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Bookmark {
    /// 书签标题，也就是章节名（`6 IO MUX and GPIO Matrix (GPIO, IO MUX)`）。
    title: String,
    /// 起始页（1 基）。
    page: usize,
}

/// 封面、目录这类不是正文章节的书签名。
fn is_front_matter(title: &str) -> bool {
    let lowered = title.trim().to_lowercase().replace(' ', "");
    matches!(
        lowered.as_str(),
        "contents" | "tableofcontents" | "目录" | "cover" | "aboutthisdocument"
    )
}

/// 读 PDF 书签：只取顶层条目，按页码排序，同一页只留第一条。
///
/// 读不出来（文档没有目录、lopdf 解析失败或 panic）时返回空表，调用方把整本当一章。
fn read_bookmarks(path: &Path) -> Vec<Bookmark> {
    let Ok(Ok(doc)) = catch_unwind(AssertUnwindSafe(|| lopdf::Document::load(path))) else {
        return Vec::new();
    };
    let Ok(Ok(toc)) = catch_unwind(AssertUnwindSafe(|| doc.get_toc())) else {
        return Vec::new();
    };
    let mut bookmarks: Vec<Bookmark> = toc
        .toc
        .into_iter()
        .filter(|entry| entry.level == 1)
        .filter(|entry| !is_front_matter(&entry.title))
        .map(|entry| Bookmark {
            title: entry.title.trim().to_string(),
            page: entry.page,
        })
        .filter(|bookmark| !bookmark.title.is_empty() && bookmark.page > 0)
        .collect();
    bookmarks.sort_by_key(|bookmark| bookmark.page);
    bookmarks.dedup_by_key(|bookmark| bookmark.page);
    bookmarks
}

/// 按书签把逐页文本切成章节；没有书签时整本作为一章。
///
/// `pages` 是 PDF 的逐页文本（1 基页码 = 下标 + 1），`bookmarks` 是同一份 PDF 的顶层
/// 书签。第一个书签之前的封面与目录不保留：它们对寄存器 / 引脚问题没有价值，
/// 章节清单会列在 `overview.md` 里。
fn split_chapters(pages: &[String], bookmarks: &[Bookmark]) -> Vec<Chapter> {
    let mut chapters: Vec<Chapter> = Vec::new();
    for (index, bookmark) in bookmarks.iter().enumerate() {
        let start = bookmark.page - 1;
        if start >= pages.len() {
            continue;
        }
        let end = bookmarks
            .get(index + 1)
            .map_or(pages.len(), |next| (next.page - 1).min(pages.len()));
        if end <= start {
            continue;
        }
        let mut chapter = Chapter::new(index as u32 + 1, bookmark.title.clone(), start + 1);
        for (offset, page) in pages[start..end].iter().enumerate() {
            chapter.push_page(start + offset + 1, page);
        }
        chapter.body = chapter.body.trim().to_string();
        if !chapter.body.is_empty() {
            chapters.push(chapter);
        }
    }

    if chapters.is_empty() {
        // 没有书签（或书签全都落在正文之外）：整本一份，检索照样能找到词。
        let body = pages
            .iter()
            .map(|page| clean_page(page))
            .collect::<Vec<_>>()
            .join("\n\n");
        let body = body.trim().to_string();
        if !body.is_empty() {
            chapters.push(Chapter {
                number: 1,
                title: "full text".to_string(),
                first_page: 1,
                last_page: pages.len(),
                body,
            });
        }
    }
    chapters
}

/// 按页抽 PDF 的文本层（不渲染版面、不识别图片）。
///
/// 主路径是内置的 `pdf-extract`；它对某些字体（如 ESP32-C6 的技术参考手册）会 panic，
/// 这时回落到 poppler 的 `pdftotext`。
fn extract_pages(path: &Path) -> Result<Vec<String>, String> {
    match extract_pages_mem(path) {
        Ok(pages) => Ok(pages),
        Err(mem_err) => extract_pages_poppler(path).map_err(|poppler_err| {
            format!("{mem_err} (poppler `pdftotext` failed too: {poppler_err})")
        }),
    }
}

/// 用 poppler 的 `pdftotext` 抽逐页文本（`pdf-extract` 解析失败时的兜底）。
///
/// 页与页之间 poppler 用 `\u{c}`（换页符）分隔，末尾那个空段丢掉。
fn extract_pages_poppler(path: &Path) -> Result<Vec<String>, String> {
    let mut cmd = std::process::Command::new("pdftotext");
    cmd.args(["-q", "-enc", "UTF-8"]).arg(path).arg("-");
    let out = run_stdout(cmd, "pdftotext")?;
    let text = String::from_utf8_lossy(&out).to_string();
    let mut pages: Vec<String> = text.split('\u{c}').map(str::to_string).collect();
    while pages.last().is_some_and(|page| page.trim().is_empty()) {
        pages.pop();
    }
    if pages.is_empty() {
        return Err(format!("`{}` produced no text", path.display()));
    }
    Ok(pages)
}

/// 内置解析器 `pdf-extract` 的逐页文本；解析器 panic 时在这里接住，不让它带走进程。
fn extract_pages_mem(path: &Path) -> Result<Vec<String>, String> {
    let bytes = fs::read(path).map_err(|e| format!("could not read `{}`: {e}", path.display()))?;
    match catch_unwind(AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem_by_pages(&bytes)
    })) {
        Ok(Ok(pages)) => Ok(pages),
        Ok(Err(err)) => Err(format!("failed to extract PDF text: {err}")),
        Err(_) => Err("failed to extract PDF text: the parser panicked".to_string()),
    }
}

/// 解析 `source` 参数：相对路径按工具调用时的 cwd 解析。
fn source_path(args: &Value, cwd: &Path) -> Result<PathBuf, ToolError> {
    let raw = req_str(args, "source")?;
    let path = cwd.join(&raw);
    if !path.is_file() {
        return Err(ToolError(format!("`{raw}` is not a file")));
    }
    if classify(&path).is_none() {
        return Err(ToolError(format!(
            "`{raw}` is not a supported datasheet: expected .pdf, an image ({}) or text ({})",
            IMAGE_EXTENSIONS.join(" / "),
            TEXT_EXTENSIONS.join(" / ")
        )));
    }
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if size > MAX_SOURCE_BYTES {
        return Err(ToolError(format!(
            "`{raw}` is {} bytes, larger than the {} byte limit",
            size, MAX_SOURCE_BYTES
        )));
    }
    Ok(path)
}

/// 目标目录名：`board` 参数优先，否则用文件名主名。
fn board_name(args: &Value, source: &Path) -> Result<String, ToolError> {
    let name = opt_string(args, "board").unwrap_or_else(|| stem_of(source));
    if !is_plain_name(&name) {
        return Err(ToolError(format!(
            "`{name}` is not a usable directory name: use letters, digits, `.`, `_` or `-`"
        )));
    }
    Ok(name)
}

/// 把原件拷进目录（已经在里面就不动）。
fn copy_original(source: &Path, dir: &Path) -> Result<PathBuf, String> {
    let name = source
        .file_name()
        .ok_or_else(|| format!("`{}` has no file name", source.display()))?;
    let target = dir.join(name);
    if target != source {
        fs::copy(source, &target)
            .map_err(|e| format!("could not copy `{}`: {e}", source.display()))?;
    }
    Ok(target)
}

/// 抽章节正文并写成 `<stem>-chNN-<slug>.md`，返回写出的文件。
fn write_chapters(source: &Path, dir: &Path) -> Result<Vec<PathBuf>, String> {
    let pages = extract_pages(source)?;
    if pages.iter().all(|page| page.trim().is_empty()) {
        return Err(format!(
            "`{}` has no text layer (a scanned PDF?); read it with `read_doc` or keep the images instead",
            source.display()
        ));
    }
    let stem = stem_of(source);
    let bookmarks = read_bookmarks(source);
    let mut written = Vec::new();
    for chapter in split_chapters(&pages, &bookmarks) {
        let name = format!(
            "{stem}-ch{:02}-{}.md",
            chapter.number,
            slug(&chapter.title, chapter.number)
        );
        let body = format!(
            "# {}\n\n<!-- from {} · pages {}-{} -->\n\n{}\n",
            chapter.title,
            source
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            chapter.first_page,
            chapter.last_page,
            chapter.body
        );
        let path = dir.join(name);
        fs::write(&path, body).map_err(|e| format!("could not write `{}`: {e}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

/// 目录里的资料按类别汇总（`overview.md` 与 `/hardware` 面板共用）。
fn scan_assets(dir: &Path) -> Vec<(String, Kind, u64)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(kind) = classify(&path) else {
            continue;
        };
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if name == OVERVIEW_FILE {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.push((name, kind, size));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 文本文件的首个非空行（去掉 `#`），总览里当一句简介。
fn first_heading(path: &Path) -> String {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines()
                .map(|line| line.trim().trim_start_matches('#').trim())
                .find(|line| !line.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// 重建目录的总览（每次导入都盖掉，手工笔记留在 `notes.md`）。
fn write_overview(dir: &Path, board: &str) -> Result<PathBuf, String> {
    let assets = scan_assets(dir);
    let mut text = format!(
        "# {board} — datasheet overview\n\n\
         <!-- Generated by `hardware_datasheet_import`; re-importing overwrites it. \
         Keep hand-written notes in notes.md. -->\n\n\
         ## Text (what `hardware_search` reads first)\n\n"
    );
    let mut texts: Vec<&(String, Kind, u64)> =
        assets.iter().filter(|a| a.1 == Kind::Text).collect();
    if texts.is_empty() {
        text.push_str(
            "- No text material yet: import a PDF with `hardware_datasheet_import`, \
             or drop a .md / .csv file in here.\n",
        );
    }
    texts.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, _, size) in &texts {
        let heading = first_heading(&dir.join(name));
        let suffix = if heading.is_empty() {
            String::new()
        } else {
            format!(" — {heading}")
        };
        text.push_str(&format!("- `{name}` ({} KB){suffix}\n", size / 1024));
    }

    let pdfs: Vec<&(String, Kind, u64)> = assets.iter().filter(|a| a.1 == Kind::Pdf).collect();
    if !pdfs.is_empty() {
        text.push_str("\n## PDF originals (kept for lookups; read with `read_doc`)\n\n");
        for (name, _, size) in pdfs.iter().take(MAX_ASSETS_IN_OVERVIEW) {
            text.push_str(&format!("- `{name}` ({} KB)\n", size / 1024));
        }
    }

    let images: Vec<&(String, Kind, u64)> = assets.iter().filter(|a| a.1 == Kind::Image).collect();
    if !images.is_empty() {
        text.push_str(
            "\n## Images (open with `read`; the vision model reads pin silkscreen / wiring)\n\n",
        );
        for (name, _, size) in images.iter().take(MAX_ASSETS_IN_OVERVIEW) {
            text.push_str(&format!("- `{name}` ({} KB)\n", size / 1024));
        }
    }

    let path = dir.join(OVERVIEW_FILE);
    fs::write(&path, text).map_err(|e| format!("could not write `{}`: {e}", path.display()))?;
    Ok(path)
}

/// 执行导入：原件进目录，PDF 展开成章节文本，最后重建总览。
pub(super) fn run_import(args: &Value, root: &Path, cwd: &Path) -> Result<ToolResult, ToolError> {
    let source = source_path(args, cwd)?;
    let board = board_name(args, &source)?;
    let dir = root.join(&board);
    fs::create_dir_all(&dir)
        .map_err(|e| ToolError(format!("could not create `{}`: {e}", dir.display())))?;

    let original = copy_original(&source, &dir).map_err(ToolError)?;
    let Some(kind) = classify(&original) else {
        // `source_path` 已经校验过扩展名，走到这里说明拷贝后文件名变了。
        return Err(ToolError(format!(
            "`{}` is not a supported datasheet format",
            original.display()
        )));
    };

    let mut chapters = Vec::new();
    if kind == Kind::Pdf {
        chapters = write_chapters(&original, &dir).map_err(ToolError)?;
    }
    let overview = write_overview(&dir, &board).map_err(ToolError)?;

    let mut out = format!(
        "**{board}** — imported `{}` into `{}`\n\n- original: `{}`\n- overview: `{}`",
        original
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        dir.display(),
        original.display(),
        overview.display()
    );
    if !chapters.is_empty() {
        out.push_str(&format!("\n- {} chapter file(s):", chapters.len()));
        for path in &chapters {
            out.push_str(&format!(
                "\n  - `{}`",
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            ));
        }
    }
    match kind {
        Kind::Pdf => out.push_str("\n\nText description is in the chapter files; use `read_doc` for the original layout."),
        Kind::Image => out.push_str("\n\nThis is an image: view it with `read` (vision model) when pin silkscreen or wiring matters."),
        Kind::Text => out.push_str("\n\nIt is a text file, so `hardware_search` picks it up as-is."),
    }
    Ok(ToolResult::text(out))
}

/// 构造 `hardware_datasheet_import` 的工具定义。
pub(super) fn import_tool() -> ExtensionTool {
    let mut tool = ExtensionTool::simple(
        TOOL_IMPORT,
        "File one board's or chip's material into its own datasheets directory: PDFs are split \
         into per-chapter Markdown by their running headers, images and the originals are kept \
         next to them, and overview.md is rebuilt. `hardware_search` reads the Markdown first, \
         then points at the PDFs / images that still need `read_doc` or `read`.",
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Path to the .pdf, image (.jpg/.png/...) or text file (absolute, or relative to the working directory)."
                },
                "board": {
                    "type": "string",
                    "description": "Directory name under the datasheets directory (e.g. `esp32c6`). Defaults to the file name without its extension. Use one directory per chip or board so its PDFs, images and extracted chapters stay together."
                }
            },
            "required": ["source"]
        }),
        "Import a datasheet or pinout into the hardware datasheets directory",
    )
    .with_exposure(ToolExposure::Deferred)
    .with_annotations(ToolAnnotations {
        read_only: false,
        destructive: false,
        idempotent: false,
    });
    tool.label = Some(super::tool_labels());
    tool
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一页手册正文（水印碎片夹在正文里）。
    fn page(body: &str) -> String {
        format!("GoBack\nPR\n\n{body}\n")
    }

    /// 一条书签。
    fn bookmark(title: &str, page: usize) -> Bookmark {
        Bookmark {
            title: title.to_string(),
            page,
        }
    }

    #[test]
    fn bookmarks_split_the_manual_into_chapters() {
        let pages = vec![
            page("cover"),
            page("core stuff"),
            page("more core"),
            page("pin stuff"),
        ];
        let bookmarks = vec![
            bookmark("1 High-Performance CPU", 2),
            bookmark("2 GPIO Matrix", 4),
        ];
        let chapters = split_chapters(&pages, &bookmarks);
        assert_eq!(chapters.len(), 2, "{chapters:?}");
        assert_eq!(chapters[0].number, 1);
        assert_eq!(chapters[0].title, "1 High-Performance CPU");
        assert_eq!((chapters[0].first_page, chapters[0].last_page), (2, 3));
        assert!(chapters[0].body.contains("core stuff"));
        assert!(chapters[0].body.contains("more core"));
        assert_eq!(chapters[1].title, "2 GPIO Matrix");
        assert_eq!(chapters[1].last_page, 4);
        assert!(!chapters[0].body.contains("cover"), "封面不进章节");
    }

    #[test]
    fn without_bookmarks_the_whole_document_is_one_chapter() {
        let pages = vec![page("no bookmarks"), page("at all")];
        let chapters = split_chapters(&pages, &[]);
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].title, "full text");
        assert_eq!((chapters[0].first_page, chapters[0].last_page), (1, 2));
        assert!(chapters[0].body.contains("no bookmarks"));
        assert!(chapters[0].body.contains("at all"));
    }

    #[test]
    fn cover_and_contents_bookmarks_are_not_chapters() {
        for title in ["Contents", "Table of Contents", "目录", "Cover"] {
            assert!(is_front_matter(title), "{title} 不算章节");
        }
        assert!(!is_front_matter("6 IO MUX and GPIO Matrix"));
    }

    #[test]
    fn watermarks_are_stripped_from_the_chapter_text() {
        let pages = vec![page("tick tock")];
        let chapters = split_chapters(&pages, &[bookmark("1 Timers", 1)]);
        assert_eq!(chapters.len(), 1);
        assert!(!chapters[0].body.contains("GoBack"), "{}", chapters[0].body);
        assert!(!chapters[0].body.contains("PR"), "{}", chapters[0].body);
        assert!(chapters[0].body.contains("tick tock"));
    }

    #[test]
    fn a_bookmark_pointing_past_the_last_page_is_ignored() {
        let pages = vec![page("only page")];
        let chapters = split_chapters(&pages, &[bookmark("1 Ghost", 9)]);
        assert_eq!(chapters.len(), 1, "越界书签不该产生空章节");
        assert_eq!(chapters[0].title, "full text");
    }

    #[test]
    fn slugs_are_filesystem_safe() {
        assert_eq!(
            slug("GPIO Matrix (GPIO, IO MUX)", 7),
            "gpio-matrix-gpio-io-mux"
        );
        assert_eq!(slug("系统和存储器", 4), "ch04");
        assert!(slug(&"x".repeat(200), 1).chars().count() <= MAX_SLUG_CHARS);
    }

    #[test]
    fn directory_names_cannot_escape_the_datasheets_root() {
        assert!(is_plain_name("esp32c6"));
        assert!(is_plain_name("nano-esp32c6_v1"));
        for bad in ["../etc", "a/b", "", ".hidden", "with space"] {
            assert!(!is_plain_name(bad), "{bad} 不该被接受");
        }
    }

    #[test]
    fn overview_lists_text_pdfs_and_images() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("ch01-gpio.md"), "# 1 GPIO\n\nbody\n").unwrap();
        fs::write(root.join("chip.pdf"), b"%PDF-1.4").unwrap();
        fs::write(root.join("board.jpg"), b"jpg").unwrap();
        let path = write_overview(root, "esp32c6").unwrap();
        let text = fs::read_to_string(path).unwrap();
        assert!(text.contains("ch01-gpio.md"), "{text}");
        assert!(text.contains("chip.pdf"), "{text}");
        assert!(text.contains("board.jpg"), "{text}");
        assert!(text.contains("`read`"), "{text}");
    }

    #[test]
    fn an_existing_original_is_not_copied_onto_itself() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("chip.pdf");
        fs::write(&source, b"%PDF-1.4").unwrap();
        let target = copy_original(&source, dir.path()).unwrap();
        assert_eq!(target, source);
        assert!(source.is_file(), "原件应原地保留");
    }
}
