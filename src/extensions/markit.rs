//! `markit` 扩展：文档读写——把文档读成 Markdown，也把 Markdown 排成 PDF。
//!
//! `read_doc` 工具按扩展名分派给对应解析器，产出**纯 Markdown 文本**交给模型：
//!
//! | 扩展名 | 解析方式 | 产出 |
//! |---|---|---|
//! | `.pdf` | `pdf-extract` 抽文本层 | 纯文本 |
//! | `.docx` | 自己读 `word/document.xml` | 标题 / 列表 / 表格 → Markdown |
//! | `.xlsx` / `.xls` / `.xlsb` / `.xlsm` / `.ods` | `calamine` | 每个工作表一段 Markdown 表格 |
//! | `.pptx` | 自己读 `ppt/slides/slideN.xml` | 每张幻灯片一节，标题占位符作小节标题 |
//! | `.epub` | 读 OPF spine 顺序，逐章 `html2text` | 各章正文按序拼接 |
//!
//! 截断口径与内置 `read` 一致（2000 行 / 50KB，[`truncate_head`]），超过
//! [`MAX_FILE_BYTES`] 的文件在解析前就拒绝。**任何格式都没有文本可抽**时明确说明原因
//! （扫描件 / 空文档），而不是静默返回空。
//!
//! 解析器对畸形文件可能 panic（尤其 PDF），[`parse`] 用 `catch_unwind` 兜底——
//! 注意 release 构建是 `panic = "abort"`，该兜底只在 debug / 测试构建下生效。
//!
//! 反方向是 `export_pdf`：Markdown → Typst 标记 → 调系统里的 `typst` 命令编译成 PDF
//! （不内嵌排版引擎，没装就把下载地址回给模型），见 [`export`]。
//!
//! 默认**不启用**（`Dev` / `Creator`）：文档解析依赖较重，按需在 `/extension` 面板开启。

mod docx;
mod epub;
mod export;
mod ooxml;
mod pdf;
mod pptx;
mod xlsx;

use crate::{
    core::{
        extensions::{
            Extension, ExtensionMode, ExtensionTool, ForkProjectInfo, ToolAnnotations, ToolExecCtx,
            ToolExposure,
        },
        tools::{ToolError, ToolResult},
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_MARKIT, tool_search},
    utils::{
        paths::resolve_path,
        truncate::{format_size, truncate_head},
    },
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    str::FromStr,
    sync::Arc,
};
use strum_macros::{Display, EnumString, IntoStaticStr};

/// 扩展名（也是 `/extension` 面板中的标识）。
const EXT: &str = "markit";

/// 工具名（模型可见）：读文档成 Markdown。
const TOOL_READ_DOC: &str = "read_doc";

/// 工具名（模型可见）：把 Markdown 排成 PDF。
const TOOL_EXPORT_PDF: &str = "export_pdf";

/// 解析前允许的最大文件字节数（32 MiB）；超过直接拒绝。
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// 支持格式的扩展名清单，拼进错误信息与工具描述（唯一真源）。
const SUPPORTED: &str = "pdf, docx, xlsx, xls, xlsb, xlsm, ods, pptx, epub";

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static MARKIT_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_MARKIT,
    make: || -> Arc<dyn Extension> { Arc::new(Markit) },
};

/// `markit` 扩展主体。
pub struct Markit;

impl Extension for Markit {
    /// 返回固定扩展名 `markit`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Read documents (PDF, Word, Excel, PowerPoint, EPUB) as Markdown (read_doc) and render Markdown to PDF through the external `typst` command (export_pdf)."
    }

    /// 声明本扩展移植自 oh-my-pi，供 `/extension` 面板展示来源链接。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "oh-my-pi".to_string(),
            plugin_version: "18.12.2".to_string(),
            url: "https://github.com/can1357/oh-my-pi".to_string(),
        })
    }

    /// 前置工具：`read_doc` / `export_pdf` 都是延迟工具，模型得先用 `tool_search` 才能发现它们（`/extension` 详情页展示）。
    fn prerequisite_tools(&self) -> Vec<String> {
        vec![tool_search::TOOL_NAME.to_string()]
    }

    /// 默认关闭：文档解析依赖较重，由用户在 `/extension` 面板开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 提供 `read_doc` 与 `export_pdf` 工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![read_doc_tool(), export_pdf_tool()]
    }

    /// 按工具名分发；未知工具名返回 `ToolError`。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        ctx: ToolExecCtx,
    ) -> BoxFuture<'static, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            match name.as_str() {
                TOOL_READ_DOC => run_read_doc(&args, &ctx).await,
                TOOL_EXPORT_PDF => export::run(&args, &ctx).await,
                other => Err(ToolError(format!("extension {EXT}: unknown tool: {other}"))),
            }
        })
    }
}

/// 支持解析的文档格式，与文件扩展名一一对应（`strum` 负责字符串互转）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr, Display)]
#[strum(serialize_all = "lowercase")]
enum DocumentFormat {
    /// PDF：只抽文本层，不渲染版面。
    Pdf,
    /// Word（`.docx`，OOXML）。
    Docx,
    /// Excel（`.xlsx`）。
    Xlsx,
    /// Excel 97-2003（`.xls`）。
    Xls,
    /// Excel 二进制工作簿（`.xlsb`）。
    Xlsb,
    /// Excel 启用宏的工作簿（`.xlsm`），内容结构同 `.xlsx`。
    Xlsm,
    /// OpenDocument 表格（`.ods`）。
    Ods,
    /// PowerPoint（`.pptx`，OOXML）。
    Pptx,
    /// EPUB 电子书。
    Epub,
}

impl DocumentFormat {
    /// 按文件扩展名识别格式；不支持的扩展名返回 `None`。
    fn of(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Self::from_str(&ext).ok()
    }

    /// 供输出抬头使用的全大写格式名（如 `DOCX`）。
    fn label(self) -> String {
        let name: &'static str = self.into();
        name.to_ascii_uppercase()
    }

    /// 是否由电子表格解析器处理（PDF / 文档 / 幻灯片之外的一类）。
    fn is_sheet(self) -> bool {
        matches!(
            self,
            Self::Xlsx | Self::Xls | Self::Xlsb | Self::Xlsm | Self::Ods
        )
    }
}

/// 构造 `read_doc` 工具定义（描述、参数 JSON Schema 与 snippet）。
fn read_doc_tool() -> ExtensionTool {
    ExtensionTool {
        default_active: true,
        exposure: ToolExposure::Deferred,
        namespace: None,
        annotations: ToolAnnotations {
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        output_schema: None,
        name: TOOL_READ_DOC.to_string(),
        description: format!(
            "Read a document and return it as Markdown text. Supported extensions: {SUPPORTED}. \
             PDFs yield their text layer only (no layout, no OCR); Word/PowerPoint keep headings, lists and tables; \
             spreadsheets become one Markdown table per sheet; EPUB chapters are concatenated in spine order. \
             Long documents are truncated like `read`."
        ),
        label: Some("Read document".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": format!(
                        "Document path; relative paths resolve against the working directory. \
                         Supported extensions: {SUPPORTED}."
                    )
                }
            },
            "required": ["path"]
        }),
        snippet: "Use to read text out of a PDF, Word document, spreadsheet, presentation or EPUB.".to_string(),
        prompt_guidelines: vec![
            "`read_doc` extracts text only: scanned PDFs and image-only slides have no text layer, so ask the user for a text version instead of retrying.".to_string(),
            "`read_doc` output is truncated like `read`; for a huge workbook or deck, ask for a specific sheet/section instead of re-reading.".to_string(),
        ],
        constrained_sampling: false,
        render_shell: None,
        execution_mode: None,
        prepare_arguments: None,
        grammar_sampling: None,
    }
}

/// 构造 `export_pdf` 工具定义（描述、参数 JSON Schema 与 snippet）。
fn export_pdf_tool() -> ExtensionTool {
    ExtensionTool {
        default_active: true,
        exposure: ToolExposure::Deferred,
        namespace: None,
        annotations: ToolAnnotations {
            read_only: false,
            destructive: true,
            idempotent: true,
        },
        output_schema: None,
        name: TOOL_EXPORT_PDF.to_string(),
        description: format!(
            "Render Markdown into a PDF file through the external `typst` command. \
             Requires typst to be installed ({}); when it is missing the tool returns that download \
             URL instead of rendering, so relay it to the user rather than retrying.",
            export::TYPST_DOWNLOAD_URL
        ),
        label: Some("Export PDF".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "Markdown source to typeset: headings, paragraphs, emphasis, lists, code blocks, tables, links and $math$ are supported."
                },
                "path": {
                    "type": "string",
                    "description": "Output PDF path; relative paths resolve against the working directory. Defaults to `output.pdf`; an existing file is overwritten."
                }
            },
            "required": ["content"]
        }),
        snippet: "Use to render Markdown into a PDF file; needs the external `typst` command installed.".to_string(),
        prompt_guidelines: vec![
            format!(
                "`export_pdf` needs the external `typst` binary; if the result says typst is missing, \
                 give the user {} instead of retrying.",
                export::TYPST_DOWNLOAD_URL
            ),
            "`export_pdf` typesets one self-contained document: no includes, no images, no Typst packages."
                .to_string(),
        ],
        constrained_sampling: false,
        render_shell: None,
        execution_mode: None,
        prepare_arguments: None,
        grammar_sampling: None,
    }
}

/// `read_doc` 工具的执行体：识别格式 → 解析 → 按 `read` 的口径截断。
async fn run_read_doc(args: &Value, ctx: &ToolExecCtx) -> Result<ToolResult, ToolError> {
    let raw = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError("missing required parameter: path".to_string()))?;
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ToolError("`path` must not be empty.".to_string()));
    }

    let path = resolve_path(raw, &ctx.cwd);
    let (format, text) = read_document(&path).map_err(ToolError)?;
    let label = format.label();

    if text.trim().is_empty() {
        return Ok(ToolResult::text(format!(
            "Read {label} {} [no extractable text: the document is empty or contains only images]",
            path.display()
        )));
    }

    let truncation = truncate_head(&text, (None, None));
    let mut output = format!(
        "Read {label} {} ({} characters)",
        path.display(),
        text.chars().count()
    );
    output.push('\n');
    output.push_str(&truncation.content);
    if truncation.truncated {
        output.push_str(&format!(
            "\n\n[Showing {} of {} lines ({} limit). Read only the part you need, or ask the user for a smaller file.]",
            truncation.output_lines,
            truncation.total_lines,
            format_size(truncation.total_bytes)
        ));
    }

    Ok(ToolResult::text(output))
}

/// 读文件 → 识别格式 → 解析成 Markdown。
///
/// 文件读不到 / 不是普通文件 / 超过 [`MAX_FILE_BYTES`] / 扩展名不支持 / 解析失败都返回 `Err`
/// （文案直接回给模型）。
fn read_document(path: &Path) -> Result<(DocumentFormat, String), String> {
    let format = DocumentFormat::of(path).ok_or_else(|| {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_else(|| "no extension".to_string());
        format!("unsupported document format: {ext} (supported: {SUPPORTED})")
    })?;

    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("not a file: {}", path.display()));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "document is too large ({}; limit {})",
            format_size(meta.len() as usize),
            format_size(MAX_FILE_BYTES as usize)
        ));
    }

    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse(format, &bytes).map(|text| (format, text))
}

/// 按格式分派解析器，并用 `catch_unwind` 兜住解析器的 panic。
///
/// 兜底只在 debug / 测试构建生效（release 是 `panic = "abort"`），目的是让畸形文件变成
/// 一条错误而不是让进程消失。
fn parse(format: DocumentFormat, bytes: &[u8]) -> Result<String, String> {
    let parsed = catch_unwind(AssertUnwindSafe(|| match format {
        DocumentFormat::Pdf => pdf::extract(bytes),
        DocumentFormat::Docx => docx::convert(bytes),
        DocumentFormat::Pptx => pptx::convert(bytes),
        DocumentFormat::Epub => epub::convert(bytes),
        other if other.is_sheet() => xlsx::convert(bytes),
        other => Err(format!("unsupported document format: {other}")),
    }));

    match parsed {
        Ok(result) => result,
        Err(_) => Err(format!(
            "{format} parsing panicked; the file may be corrupt"
        )),
    }
}

/// 把连续空行压成一个空行，并去掉首尾空白——解析产出的 Markdown 靠它收口。
fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_run > 0 {
                out.push('\n');
            }
        }
        blank_run = 0;
        out.push_str(line);
    }
    out
}

/// 把一段文本里的连续空白压成单个空格（表格单元格、段落文本都靠它归一）。
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 把 `|` 与换行转义掉，使其能安全放进 Markdown 表格单元格。
fn escape_cell(text: &str) -> String {
    squash(text).replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::ToolExecCtx;
    use std::io::Write;

    /// 只填 cwd 的测试上下文：`read_doc` 不碰子代理与嵌套工具，其余字段给不可用占位。
    pub(super) fn ctx(cwd: &str) -> ToolExecCtx {
        ToolExecCtx {
            cwd: cwd.to_string(),
            make_sub_agent: Arc::new(|_spec| Err("subagents unavailable in tests".to_string())),
            parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            script_tools: Default::default(),
            session_branch_entries: Default::default(),
            script_call: false,
        }
    }

    /// 建一个临时目录（用例内自清理）。
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("prux-markit-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 用给定成员拼一个内存 ZIP（测试夹具共用）。
    fn zip_bytes(files: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, content) in files {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(content.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn fork_project_reports_oh_my_pi_source() {
        let info = Markit.fork_project().expect("markit 应声明 fork 来源");
        assert_eq!(info.plugin_name, "oh-my-pi");
        assert_eq!(info.plugin_version, "18.12.2");
        assert_eq!(info.url, "https://github.com/can1357/oh-my-pi");
    }

    #[test]
    fn tool_declares_required_path_parameter() {
        let tool = read_doc_tool();
        assert_eq!(tool.name, TOOL_READ_DOC);
        assert_eq!(tool.parameters["required"], json!(["path"]));
        assert!(tool.annotations.read_only);
        assert!(matches!(tool.exposure, ToolExposure::Deferred));
    }

    #[test]
    fn formats_are_recognized_by_extension() {
        for (name, want) in [
            ("a.pdf", DocumentFormat::Pdf),
            ("a.DOCX", DocumentFormat::Docx),
            ("a.xlsx", DocumentFormat::Xlsx),
            ("a.xls", DocumentFormat::Xls),
            ("a.xlsb", DocumentFormat::Xlsb),
            ("a.xlsm", DocumentFormat::Xlsm),
            ("a.ods", DocumentFormat::Ods),
            ("a.pptx", DocumentFormat::Pptx),
            ("a.epub", DocumentFormat::Epub),
        ] {
            assert_eq!(DocumentFormat::of(Path::new(name)), Some(want), "{name}");
        }
        assert_eq!(DocumentFormat::of(Path::new("a.txt")), None);
        assert_eq!(DocumentFormat::of(Path::new("noext")), None);
    }

    #[test]
    fn unsupported_extension_reports_supported_list() {
        let dir = temp_dir("unsupported");
        let file = dir.join("notes.txt");
        std::fs::write(&file, "plain text").unwrap();

        let err = read_document(&file).unwrap_err();
        assert!(err.contains("unsupported document format: .txt"), "{err}");
        assert!(err.contains("docx"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_reports_read_error() {
        let err = read_document(Path::new("/definitely/not/here.pdf")).unwrap_err();
        assert!(err.contains("cannot read"), "{err}");
    }

    #[test]
    fn directory_is_rejected() {
        let dir = temp_dir("dir");
        let path = dir.join("folder.pdf");
        std::fs::create_dir_all(&path).unwrap();
        let err = read_document(&path).unwrap_err();
        assert!(err.contains("not a file"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_file_is_rejected_before_parsing() {
        let dir = temp_dir("big");
        let file = dir.join("big.pdf");
        let f = std::fs::File::create(&file).unwrap();
        f.set_len(MAX_FILE_BYTES + 1).unwrap();
        drop(f);

        let err = read_document(&file).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn garbage_bytes_report_error_instead_of_panicking() {
        let dir = temp_dir("garbage");
        let file = dir.join("garbage.docx");
        std::fs::write(&file, b"not a docx at all").unwrap();

        let err = read_document(&file).unwrap_err();
        assert!(!err.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 构造一份带正确 xref 的最小单页 PDF，页面内容为 `Hello PDF`。
    fn minimal_pdf() -> Vec<u8> {
        let content = b"BT /F1 24 Tf 20 100 Td (Hello PDF) Tj ET";
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            [
                b"<< /Length ".to_vec(),
                content.len().to_string().into_bytes(),
                b" >>\nstream\n".to_vec(),
                content.to_vec(),
                b"\nendstream".to_vec(),
            ]
            .concat(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ];

        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, obj) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(obj);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref_pos = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    /// 一份含标题、列表、正文与表格的最小 docx。
    fn minimal_docx() -> Vec<u8> {
        let document = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Report Title</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/></w:numPr></w:pPr><w:r><w:t>First bullet</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr/></w:pPr><w:r><w:t>Second bullet</w:t></w:r></w:p>
<w:p><w:r><w:t>Plain </w:t></w:r><w:r><w:t>paragraph</w:t></w:r></w:p>
<w:tbl>
<w:tr><w:tc><w:p><w:r><w:t>Name</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Qty</w:t></w:r></w:p></w:tc></w:tr>
<w:tr><w:tc><w:p><w:r><w:t>Widget</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>3</w:t></w:r></w:p></w:tc></w:tr>
</w:tbl>
</w:body></w:document>"#;
        zip_bytes(&[("word/document.xml", document)])
    }

    #[test]
    fn docx_keeps_headings_lists_and_tables() {
        let dir = temp_dir("docx");
        std::fs::write(dir.join("report.docx"), minimal_docx()).unwrap();

        let (format, text) = read_document(&dir.join("report.docx")).unwrap();
        assert_eq!(format, DocumentFormat::Docx);
        assert!(text.contains("# Report Title"), "{text}");
        assert!(text.contains("- First bullet"), "{text}");
        assert!(text.contains("- Second bullet"), "{text}");
        assert!(text.contains("Plain paragraph"), "{text}");
        assert!(text.contains("| Name | Qty |"), "{text}");
        assert!(text.contains("| Widget | 3 |"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn docx_without_text_reports_empty() {
        let dir = temp_dir("docx-empty");
        let document = r#"<w:document xmlns:w="x"><w:body><w:p/></w:body></w:document>"#;
        std::fs::write(
            dir.join("empty.docx"),
            zip_bytes(&[("word/document.xml", document)]),
        )
        .unwrap();

        let (_, text) = read_document(&dir.join("empty.docx")).unwrap();
        assert!(text.trim().is_empty(), "{text:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn docx_without_document_xml_reports_error() {
        let dir = temp_dir("docx-broken");
        std::fs::write(dir.join("broken.docx"), zip_bytes(&[("other.xml", "<x/>")])).unwrap();

        let err = read_document(&dir.join("broken.docx")).unwrap_err();
        assert!(err.contains("word/document.xml"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 一份含标题占位符与两级列表的最小 pptx。
    fn minimal_pptx() -> Vec<u8> {
        let slide = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree>
<p:sp><p:nvSpPr><p:cNvPr id="1" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
<p:txBody><a:p><a:r><a:t>Quarterly Review</a:t></a:r></a:p></p:txBody></p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Body"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
<p:txBody>
<a:p><a:r><a:t>Revenue up</a:t></a:r></a:p>
<a:p><a:pPr lvl="1"/><a:r><a:t>By 12%</a:t></a:r></a:p>
</p:txBody></p:sp>
</p:spTree></p:cSld></p:sld>"#;
        zip_bytes(&[("ppt/slides/slide1.xml", slide)])
    }

    #[test]
    fn pptx_orders_slides_numerically_and_keeps_levels() {
        let dir = temp_dir("pptx");
        std::fs::write(dir.join("deck.pptx"), minimal_pptx()).unwrap();
        let (format, text) = read_document(&dir.join("deck.pptx")).unwrap();
        assert_eq!(format, DocumentFormat::Pptx);
        assert!(text.contains("## Slide 1: Quarterly Review"), "{text}");
        assert!(text.contains("- Revenue up"), "{text}");
        assert!(text.contains("  - By 12%"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 幻灯片号按**数值**排序：`slide10` 必须排在 `slide2` 之后。
    #[test]
    fn pptx_sorts_slide_files_by_number() {
        let dir = temp_dir("pptx-order");
        let page = |title: &str| {
            format!(
                r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree><p:sp><p:txBody>
<a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
            )
        };
        let ten = page("Ten");
        let two = page("Two");
        std::fs::write(
            dir.join("deck.pptx"),
            zip_bytes(&[
                ("ppt/slides/slide10.xml", ten.as_str()),
                ("ppt/slides/slide2.xml", two.as_str()),
            ]),
        )
        .unwrap();

        let (_, text) = read_document(&dir.join("deck.pptx")).unwrap();
        let two_at = text.find("Two").expect("应包含第二页");
        let ten_at = text.find("Ten").expect("应包含第十页");
        assert!(two_at < ten_at, "{text}");
        assert!(text.contains("## Slide 2"), "{text}");
        assert!(text.contains("## Slide 10"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 一份含 container / OPF 的最小 epub，spine 顺序故意与成员名顺序相反。
    fn minimal_epub() -> Vec<u8> {
        let container = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles>
<rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
</rootfiles></container>"#;
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0"><manifest>
<item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
<item id="c2" href="ch2.xhtml" media-type="application/xhtml+xml"/>
</manifest><spine><itemref idref="c2"/><itemref idref="c1"/></spine></package>"#;
        let ch1 =
            r#"<html><body><h1>First chapter</h1><p>Body of the first chapter.</p></body></html>"#;
        let ch2 = r#"<html><body><h1>Second chapter</h1><p>Body of the second chapter.</p></body></html>"#;
        zip_bytes(&[
            ("META-INF/container.xml", container),
            ("OEBPS/content.opf", opf),
            ("OEBPS/ch1.xhtml", ch1),
            ("OEBPS/ch2.xhtml", ch2),
        ])
    }

    #[test]
    fn epub_follows_spine_order() {
        let dir = temp_dir("epub");
        std::fs::write(dir.join("book.epub"), minimal_epub()).unwrap();

        let (format, text) = read_document(&dir.join("book.epub")).unwrap();
        assert_eq!(format, DocumentFormat::Epub);
        let second = text.find("Second chapter").expect("应含第二章");
        let first = text.find("First chapter").expect("应含第一章");
        assert!(second < first, "spine 顺序应生效：{text}");
        assert!(text.contains("Body of the first chapter."), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 一份最小 xlsx：一个名为 `Data` 的工作表，两行两列。
    fn minimal_xlsx() -> Vec<u8> {
        let content_types = r#"<?xml version="1.0"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;
        let root_rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
        let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let workbook_rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="1"><c r="A1" t="inlineStr"><is><t>name</t></is></c><c r="B1" t="inlineStr"><is><t>qty</t></is></c></row>
<row r="2"><c r="A2" t="inlineStr"><is><t>Widget</t></is></c><c r="B2"><v>3</v></c></row>
</sheetData></worksheet>"#;
        zip_bytes(&[
            ("[Content_Types].xml", content_types),
            ("_rels/.rels", root_rels),
            ("xl/workbook.xml", workbook),
            ("xl/_rels/workbook.xml.rels", workbook_rels),
            ("xl/worksheets/sheet1.xml", sheet),
        ])
    }

    #[test]
    fn xlsx_renders_one_table_per_sheet() {
        let dir = temp_dir("xlsx");
        std::fs::write(dir.join("book.xlsx"), minimal_xlsx()).unwrap();

        let (format, text) = read_document(&dir.join("book.xlsx")).unwrap();
        assert_eq!(format, DocumentFormat::Xlsx);
        assert!(text.contains("## Sheet: Data"), "{text}");
        assert!(text.contains("| name | qty |"), "{text}");
        assert!(text.contains("| --- | --- |"), "{text}");
        assert!(text.contains("| Widget | 3 |"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_read_doc_reports_format_and_path() {
        let dir = temp_dir("run");
        std::fs::write(dir.join("hello.pdf"), minimal_pdf()).unwrap();
        std::fs::write(dir.join("report.docx"), minimal_docx()).unwrap();

        let res = run_read_doc(
            &json!({ "path": "hello.pdf" }),
            &ctx(&dir.to_string_lossy()),
        )
        .await
        .unwrap();
        assert!(res.text.contains("Read PDF"), "{}", res.text);
        assert!(res.text.contains("Hello PDF"), "{}", res.text);

        let res = run_read_doc(
            &json!({ "path": "report.docx" }),
            &ctx(&dir.to_string_lossy()),
        )
        .await
        .unwrap();
        assert!(res.text.contains("Read DOCX"), "{}", res.text);
        assert!(res.text.contains("# Report Title"), "{}", res.text);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn missing_parameter_is_an_error() {
        let err = run_read_doc(&json!({}), &ctx("/tmp")).await.unwrap_err();
        assert!(err.0.contains("missing required parameter"), "{}", err.0);
    }

    #[tokio::test]
    async fn empty_path_is_an_error() {
        let err = run_read_doc(&json!({ "path": "  " }), &ctx("/tmp"))
            .await
            .unwrap_err();
        assert!(err.0.contains("must not be empty"), "{}", err.0);
    }

    #[tokio::test]
    async fn unknown_tool_name_is_rejected() {
        let err = Markit
            .execute_tool_async("nope".to_string(), json!({}), ctx("/tmp"))
            .await
            .unwrap_err();
        assert!(err.0.contains("unknown tool: nope"), "{}", err.0);
    }
}
