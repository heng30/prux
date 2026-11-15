//! 资料导入：把一块芯片 / 板子的 PDF、图片与文本收进同一个目录，并生成总览。
//!
//! 目录布局（`agent_dir()/extensions/hardware/datasheets/<board>/`）：
//!
//! - `overview.md` —— 总览，每次导入重建。文本文件都按章节 / 文件名列在这里，
//!   `hardware_search` 先命中的就是它；
//! - `<stem>-chNN-<slug>.md` —— PDF 按书签（目录）拆出来的章节正文，检索走这些小块；
//!   抽取保留版式，页眉页脚页码都在，所以由 `hardware_datasheet_tidy` 交给模型复核一遍；
//! - 原件（`.pdf` / 图片）—— 原样留在同一目录，图片交给视觉模型看，PDF 可用 `read_doc`。
//!
//! 只抽 PDF 的**文本层**（`pdf-extract`）：扫描件抽出来是空的，此时明确报错，
//! 让用户直接用 `read_doc` 或看图，而不是在资料目录里留一堆空文件。

use super::search::TEXT_EXTENSIONS;
use crate::{
    core::{
        auth,
        extensions::{ExtensionTool, ToolAnnotations, ToolExposure},
        model_resolver,
        provider::{self, ModelConfig},
        tools::{ToolError, ToolResult},
    },
    extensions::util::{opt_string, req_str, split_model},
    utils::process::run_stdout,
};
use serde_json::{Value, json};
use std::{
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
};

/// 工具名：导入一份资料。
pub(super) const TOOL_IMPORT: &str = "hardware_datasheet_import";

/// 工具名：把生成的章节交给模型复核，删掉版式噪声。
pub(super) const TOOL_TIDY: &str = "hardware_datasheet_tidy";

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

/// 复核后正文至少留这么多（占原字符数的百分数），低于它说明模型在丢内容而不是在删噪声。
const KEEP_PERCENT: usize = 50;

/// 复核正文时每块的字符上限：手册章节动辄几十 KB，按块送才不会撑爆上下文。
const TIDY_CHUNK_CHARS: usize = 6000;

/// 拉丁文本页里空格占比低于此百分比，说明解析器把词粘成了一串。
const MIN_SPACE_PERCENT: usize = 4;

/// 生成文件里的来源注释开头：手写文件没有它，复核时一律不动。
const ORIGIN_MARKER: &str = "<!-- from ";

/// 复核用的系统提示词：只删版式噪声，不动技术内容。
const TIDY_SYSTEM: &str = "\
You clean text that was extracted from a chip manual PDF. The extractor kept the page layout, so
the text is littered with things that are not content: running headers, footers, page numbers,
watermark fragments, the chapter title repeated on every page, and blank-line runs.

Remove exactly those. Keep everything else verbatim: tables, register names and offsets, bit
fields, addresses, reset values, pin names and numbers, electrical values with their units,
footnotes, figure captions and every sentence of substance. Never summarise, never reorder, never
translate, never merge lines that were separate rows of a table.

Reply with the cleaned text only: no preamble, no explanation, no code fences.";

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

/// 这一行足够像正文吗：至少 4 个词，或至少 8 个汉字。
fn is_prose_line(line: &str) -> bool {
    line.split_whitespace().count() >= 4 || line.chars().filter(|c| is_cjk(*c)).count() >= 8
}

/// 一个字符是否属于中日韩统一表意文字。
fn is_cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
}

/// 文本层里有没有能当正文读的东西。
///
/// 原理图、PCB 这类 CAD 导出的 PDF 有文本层，但抽出来全是 `R10` / `COC1` / `VDD`
/// 这种孤立标签（正文行占比接近 0）：再拆成章节只会用噪声挤掉检索结果里的真内容。
fn has_prose(pages: &[String]) -> bool {
    let mut total = 0usize;
    let mut prose = 0usize;
    for line in pages.iter().flat_map(|page| page.lines()) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        total += 1;
        if is_prose_line(line) {
            prose += 1;
        }
    }
    total == 0 || prose * 50 >= total
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
/// 这时回落到 poppler 的 `pdftotext`。它不报错但把表格页的词粘成一串时也改用 poppler，
/// 否则整张引脚总表会变成一行没有空格的文本，检索与阅读都用不了。
fn extract_pages(path: &Path) -> Result<Vec<String>, String> {
    match extract_pages_mem(path) {
        Ok(pages) if glued_words(&pages) => Ok(extract_pages_poppler(path).unwrap_or(pages)),
        Ok(pages) => Ok(pages),
        Err(mem_err) => extract_pages_poppler(path).map_err(|poppler_err| {
            format!("{mem_err} (poppler `pdftotext` failed too: {poppler_err})")
        }),
    }
}

/// 抽出的文本里是否有整页被粘成了一串词（页内字符数够多、以拉丁字母数字为主、几乎没有空格）。
///
/// 中文之类本来就不分词的语言不算：它们空格本来就少。
fn glued_words(pages: &[String]) -> bool {
    pages.iter().any(|page| {
        let chars = page.chars().count();
        let latin = page.chars().filter(|c| c.is_ascii_alphanumeric()).count();
        let spaces = page.chars().filter(|c| *c == ' ').count();
        chars >= 400 && latin * 100 >= chars * 50 && spaces * 100 < chars * MIN_SPACE_PERCENT
    })
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
    if !has_prose(&pages) {
        // 图纸而不是手册：原件留在目录里给 `read_doc`，不产章节文件。
        return Ok(Vec::new());
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
        text.push_str(&format!(
            "- `{name}` ({} KB){suffix}\n",
            size.div_ceil(1024)
        ));
    }

    let pdfs: Vec<&(String, Kind, u64)> = assets.iter().filter(|a| a.1 == Kind::Pdf).collect();
    if !pdfs.is_empty() {
        text.push_str("\n## PDF originals (kept for lookups; read with `read_doc`)\n\n");
        for (name, _, size) in pdfs.iter().take(MAX_ASSETS_IN_OVERVIEW) {
            text.push_str(&format!("- `{name}` ({} KB)\n", size.div_ceil(1024)));
        }
    }

    let images: Vec<&(String, Kind, u64)> = assets.iter().filter(|a| a.1 == Kind::Image).collect();
    if !images.is_empty() {
        text.push_str(
            "\n## Images (open with `read`; the vision model reads pin silkscreen / wiring)\n\n",
        );
        for (name, _, size) in images.iter().take(MAX_ASSETS_IN_OVERVIEW) {
            text.push_str(&format!("- `{name}` ({} KB)\n", size.div_ceil(1024)));
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
        Kind::Pdf if chapters.is_empty() => out.push_str(
            "\n\nNo chapter files: the text layer is only labels (a schematic or a drawing). \
             Open the original with `read_doc`.",
        ),
        Kind::Pdf => out.push_str("\n\nText description is in the chapter files; use `read_doc` for the original layout."),
        Kind::Image => out.push_str("\n\nThis is an image: view it with `read` (vision model) when pin silkscreen or wiring matters."),
        Kind::Text => out.push_str("\n\nIt is a text file, so `hardware_search` picks it up as-is."),
    }
    Ok(ToolResult::text(out))
}

/// 把生成的章节文件拆成「头（标题与来源注释）」与「正文」；不是生成的文件返回 `None`。
fn split_origin(text: &str) -> Option<(String, String)> {
    let mut header: Vec<&str> = Vec::new();
    let mut body: Vec<&str> = Vec::new();
    let mut in_body = false;
    for line in text.lines() {
        if in_body {
            body.push(line);
        } else {
            header.push(line);
            if line.trim_start().starts_with(ORIGIN_MARKER) {
                in_body = true;
            }
        }
    }
    in_body.then(|| (header.join("\n"), body.join("\n")))
}

/// 按行把正文切成不超过 [`TIDY_CHUNK_CHARS`] 的块（一行本身超限时独占一块）。
fn chunk_body(body: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in body.lines() {
        if !current.is_empty()
            && current.chars().count() + line.chars().count() + 1 > TIDY_CHUNK_CHARS
        {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    chunks
}

/// 资料目录名（也就是板卡名）。
fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "datasheets".to_string())
}

/// 待复核的目录：`board` 给了就只那个，否则 `datasheets/` 下每个目录。
fn tidy_targets(args: &Value, root: &Path) -> Result<Vec<PathBuf>, ToolError> {
    let Some(board) = opt_string(args, "board") else {
        let Ok(entries) = fs::read_dir(root) else {
            return Ok(Vec::new());
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        return Ok(dirs);
    };
    if !is_plain_name(&board) {
        return Err(ToolError(format!(
            "`{board}` is not a usable directory name: use letters, digits, `.`, `_` or `-`"
        )));
    }
    let dir = root.join(&board);
    if !dir.is_dir() {
        return Err(ToolError(format!(
            "`{}` is not a datasheet directory",
            dir.display()
        )));
    }
    Ok(vec![dir])
}

/// 把一批章节文件交给模型复核，原地改写。
///
/// 返回（复核的文件数，少掉的字符数，跳过的文件与原因）。
async fn review_dir(
    model: &ModelConfig,
    dir: &Path,
) -> Result<(usize, usize, Vec<String>), String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Err(format!("could not read `{}`", dir.display()));
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| classify(path) == Some(Kind::Text))
        .collect();
    files.sort();

    let mut reviewed = 0usize;
    let mut saved = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    for path in files {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Some((header, body)) = split_origin(&text) else {
            continue;
        };
        let total = body.chars().count();
        let mut cleaned: Vec<String> = Vec::new();
        let mut failure: Option<String> = None;
        for chunk in chunk_body(&body) {
            let max_tokens = model.max_tokens.filter(|m| *m > 0).unwrap_or(8192);
            match provider::simple_completion(model, TIDY_SYSTEM, &chunk, Some(max_tokens)).await {
                Ok((text, _usage)) => cleaned.push(text.trim().to_string()),
                Err(err) => {
                    failure = Some(format!("`{}`: {err}", file_label(&path)));
                    break;
                }
            }
        }
        // 某一块失败就不写回：半份清洗过的文件比脏的更糟。
        if let Some(note) = failure {
            skipped.push(note);
            continue;
        }

        let after_body = cleaned.join("\n\n");
        let kept = after_body.chars().count();
        // 内容掉了大半只能是模型截断或乱删：这一份保持原样，别把手册洗没了。
        if total > 0 && kept * 100 < total * KEEP_PERCENT {
            skipped.push(format!(
                "`{}`: the model dropped {}% of the text, left the original alone",
                file_label(&path),
                100 - kept * 100 / total
            ));
            continue;
        }
        saved += total.saturating_sub(kept);
        reviewed += 1;
        fs::write(&path, format!("{header}\n{after_body}\n"))
            .map_err(|err| format!("could not write `{}`: {err}", path.display()))?;
    }
    write_overview(dir, &dir_name(dir))?;
    Ok((reviewed, saved, skipped))
}

/// 文件在错误信息里的名字（只给文件名，目录太长）。
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// 执行复核：用当前会话的模型逐章重写生成的 Markdown。
pub(super) async fn run_tidy(
    args: &Value,
    root: &Path,
    parent_model: Option<&str>,
    session_id: Option<String>,
) -> Result<ToolResult, ToolError> {
    let dirs = tidy_targets(args, root)?;
    if dirs.is_empty() {
        return Ok(ToolResult::text(format!(
            "No datasheet directories under `{}` yet.",
            root.display()
        )));
    }
    let Some((provider, model_id)) = parent_model.and_then(split_model) else {
        return Err(ToolError(
            "no model to review with: pick one first with `/model`".to_string(),
        ));
    };
    auth::ensure_oauth_valid(&provider)
        .await
        .map_err(|err| ToolError(format!("{err}")))?;
    // 会话 id 要带给 provider：opencode 这类缺 `x-opencode-session` 会直接 400。
    let model = model_resolver::configured_model(&provider, &model_id, None, session_id)
        .map_err(|err| ToolError(format!("{err}")))?;

    let mut out = String::new();
    for dir in dirs {
        let board = dir_name(&dir);
        let (reviewed, saved, skipped) = review_dir(&model, &dir).await.map_err(ToolError)?;
        match reviewed {
            0 => out.push_str(&format!("\n- **{board}**: nothing to review")),
            _ => out.push_str(&format!(
                "\n- **{board}**: {reviewed} chapter file(s) cleaned, {saved} character(s) of page \
                 furniture removed"
            )),
        }
        for note in skipped {
            out.push_str(&format!("\n  - skipped {note}"));
        }
    }
    Ok(ToolResult::text(format!(
        "Datasheets reviewed by `{provider}/{model_id}`: running headers, footers, page numbers and \
         watermark litter removed, technical content kept as-is.{out}"
    )))
}

/// 构造 `hardware_datasheet_tidy` 的工具定义。
pub(super) fn tidy_tool() -> ExtensionTool {
    let mut tool = ExtensionTool::simple(
        TOOL_TIDY,
        "Have the current model clean up the chapter Markdown this extension generated: it reads \
         every generated file in the directory (or one `board`), strips running headers, footers, \
         page numbers and watermark litter, keeps tables and technical values as-is, writes the \
         result back and rebuilds overview.md. Use it after importing a manual whose chapters came \
         out noisy. Hand-written files are left alone.",
        json!({
            "type": "object",
            "properties": {
                "board": {
                    "type": "string",
                    "description": "Directory under the datasheets directory to tidy (e.g. `esp32c6`). Omit to tidy every directory."
                }
            }
        }),
        "Clean up generated datasheet chapters with the model",
    )
    .with_exposure(ToolExposure::Deferred)
    .with_annotations(ToolAnnotations {
        read_only: false,
        destructive: false,
        idempotent: true,
    });
    tool.label = Some(super::tool_labels());
    tool
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
    fn a_schematic_full_of_labels_is_not_text() {
        let labels = "RGB LED\nR10\n22R\nCOC1\nU3\nAMS1117-3.3V\nGND\nGPIO9\nVDD\nDOUT\n";
        assert!(!has_prose(&[page(labels)]), "孤立标签不是正文");
        let manual = "The GPIO matrix routes any peripheral signal to any pin.\n\n";
        assert!(has_prose(&[page(manual)]), "成句的手册正文要留下");
        let chinese = "第 4 章 系统和存储器\n芯片内部的存储器和外设都映射到同一份地址空间。\n";
        assert!(has_prose(&[page(chinese)]), "中文正文也要留下");
    }

    #[test]
    fn a_glued_table_page_is_detected() {
        let glued = format!(
            "Table7-1.QFN40PinOverview{}AntAnalog",
            "PinPinPin".repeat(40)
        );
        assert!(glued_words(&[page(&glued)]), "粘成一串的表格页要认出来");
        let prose = "The GPIO matrix routes any peripheral signal to any pin. ".repeat(20);
        assert!(!glued_words(&[page(&prose)]), "正常正文不算粘连");
        let chinese =
            "第 4 章 系统和存储器\n芯片内部的存储器和外设都映射到同一份地址空间。".repeat(20);
        assert!(!glued_words(&[page(&chinese)]), "中文本来就不分词");
        assert!(!glued_words(&[page("short")]), "短页不做判断");
    }

    #[test]
    fn origin_marker_separates_generated_from_hand_written() {
        let generated = "# 2 Pins\n\n<!-- from chip.pdf · pages 3-5 -->\n\nbody\n";
        let (header, body) = split_origin(generated).expect("生成文件应带来源注释");
        assert!(header.ends_with("pages 3-5 -->"), "{header}");
        assert_eq!(body.trim(), "body");
        assert!(split_origin("# notes\n\nhand written\n").is_none());
        assert!(split_origin("").is_none(), "空文件没有来源注释");
    }

    #[test]
    fn chunks_respect_the_size_cap_and_keep_every_line() {
        let lines = (0..2000).map(|n| format!("line {n}")).collect::<Vec<_>>();
        let body = lines.join("\n");
        let chunks = chunk_body(&body);
        assert!(chunks.len() > 1, "大正文要切块: {}", chunks.len());
        for chunk in &chunks {
            assert!(
                chunk.chars().count() <= TIDY_CHUNK_CHARS,
                "块超限: {}",
                chunk.chars().count()
            );
        }
        let rejoined = chunks.join("\n");
        assert_eq!(rejoined, body, "切块不能丢行或改行");
    }

    #[test]
    fn a_single_huge_line_still_gets_sent() {
        let line = "x".repeat(TIDY_CHUNK_CHARS + 100);
        let chunks = chunk_body(&line);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], line);
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
