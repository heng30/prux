//! `pdf` 扩展：读取 PDF 的**文本层**。
//!
//! 提供 `read_pdf` 工具：把 PDF 里能抽出的文本交给模型（不渲染版面、不识别图片）。
//! 截断口径与内置 `read` 一致（[`crate::utils::truncate::truncate_head`] 的
//! 2000 行 / 50KB），超过 [`MAX_FILE_BYTES`] 的文件在解析前就拒绝，避免把内存吃穿。
//!
//! 局限：只读文本层——扫描件（图片型 PDF）抽不出内容，此时明确提示而不是静默返回空。
//! 解析器对畸形文件可能 panic，[`extract_pdf`] 用 `catch_unwind` 兜底（注意 release
//! 构建是 `panic = "abort"`，该兜底只在 debug / 测试构建下生效）。
//!
//! 默认**不启用**（`Dev` / `Creator`）：PDF 解析是重依赖能力，按需开启。

use crate::{
    core::{
        extensions::{
            Extension, ExtensionMode, ExtensionTool, ForkProjectInfo, ToolAnnotations, ToolExecCtx,
            ToolExposure,
        },
        tools::{ToolError, ToolResult},
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_PDF, tool_search},
    utils::{
        paths::resolve_path,
        truncate::{format_size, truncate_head},
    },
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{panic::AssertUnwindSafe, path::Path, sync::Arc};

/// 扩展名（也是 `/extension` 面板中的标识）。
const EXT: &str = "pdf";

/// 工具名（模型可见）。
const TOOL_READ_PDF: &str = "read_pdf";

/// 解析前允许的最大文件字节数（32 MiB）；超过直接拒绝。
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static PDF_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_PDF,
    make: || -> Arc<dyn Extension> { Arc::new(Pdf) },
};

/// `pdf` 扩展主体。
pub struct Pdf;

impl Extension for Pdf {
    /// 返回固定扩展名 `pdf`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Read the text layer of PDF files (read_pdf)."
    }

    /// 声明本扩展移植自 jcode 0.94.0，供 `/extension` 面板展示来源链接。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "jcode".to_string(),
            plugin_version: "0.94.0".to_string(),
            url: "https://github.com/1jehuang/jcode".to_string(),
        })
    }

    /// 前置工具：`read_pdf` 是延迟工具，模型得先用 `tool_search` 才能发现它（`/extension` 详情页展示）。
    fn prerequisite_tools(&self) -> Vec<String> {
        vec![tool_search::TOOL_NAME.to_string()]
    }

    /// 默认关闭：PDF 解析依赖较重，由用户在 `/extension` 面板开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 提供 `read_pdf` 工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![read_pdf_tool()]
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
                TOOL_READ_PDF => run_read_pdf(&args, &ctx).await,
                other => Err(ToolError(format!("extension {EXT}: unknown tool: {other}"))),
            }
        })
    }
}

/// 构造 `read_pdf` 工具定义（描述、参数 JSON Schema 与 snippet）。
fn read_pdf_tool() -> ExtensionTool {
    ExtensionTool {
        exposure: ToolExposure::Deferred,
        namespace: None,
        annotations: ToolAnnotations {
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        output_schema: None,
        name: TOOL_READ_PDF.to_string(),
        description: "Extract the text layer of a PDF file. Returns plain text, not a rendered layout; scanned (image-only) PDFs have no text layer and report that instead of content. Long documents are truncated like `read`.".to_string(),
        label: Some("Read PDF".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "PDF file path; relative paths resolve against the working directory."
                }
            },
            "required": ["path"]
        }),
        snippet: "Use to read text out of a PDF; scanned PDFs without a text layer return nothing useful.".to_string(),
        prompt_guidelines: vec![
            "`read_pdf` only extracts the PDF text layer; for scanned documents ask the user for a text version instead of retrying.".to_string(),
        ],
        constrained_sampling: false,
        render_shell: None,
        execution_mode: None,
        prepare_arguments: None,
        grammar_sampling: None,
    }
}

/// `read_pdf` 工具的执行体：解析路径 → 抽取文本 → 按 `read` 的口径截断。
async fn run_read_pdf(args: &Value, ctx: &ToolExecCtx) -> Result<ToolResult, ToolError> {
    let raw = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError("missing required parameter: path".to_string()))?;
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ToolError("`path` must not be empty.".to_string()));
    }

    let path = resolve_path(raw, &ctx.cwd);
    let text = extract_pdf(&path).map_err(ToolError)?;

    if text.trim().is_empty() {
        return Ok(ToolResult::text(format!(
            "Read PDF {} [text layer is empty: this looks like a scanned document with no extractable text]",
            path.display()
        )));
    }

    let truncation = truncate_head(&text, (None, None));
    let mut output = format!(
        "Read PDF {} ({} characters)",
        path.display(),
        text.chars().count()
    );
    output.push('\n');
    output.push_str(&truncation.content);
    if truncation.truncated {
        output.push_str(&format!(
            "\n\n[Showing {} of {} lines ({} limit). Convert the PDF to text to read the rest.]",
            truncation.output_lines,
            truncation.total_lines,
            format_size(truncation.total_bytes)
        ));
    }

    Ok(ToolResult::text(output))
}

/// 读取并抽取 PDF 文本：先按字节上限拦，再读文件、抽文本。
///
/// 文件读不到 / 不是普通文件 / 超过 [`MAX_FILE_BYTES`] / 解析失败都返回 `Err`（文本直接回给模型）。
fn extract_pdf(path: &Path) -> Result<String, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("not a file: {}", path.display()));
    }

    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "PDF is too large ({}; limit {})",
            format_size(meta.len() as usize),
            format_size(MAX_FILE_BYTES as usize)
        ));
    }

    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;

    // 解析器对畸形文件可能 panic；release 构建是 panic=abort，这层兜底只在 debug / 测试生效
    match std::panic::catch_unwind(AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(&bytes)
    })) {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => Err(format!("failed to extract PDF text: {e}")),
        Err(_) => Err("PDF parsing panicked; the file may be corrupt".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::ToolExecCtx;

    /// 只填 cwd 的测试上下文：`read_pdf` 不碰子代理与嵌套工具，其余字段给不可用占位。
    fn ctx(cwd: &str) -> ToolExecCtx {
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
        let dir = std::env::temp_dir().join(format!("prux-pdf-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fork_project_reports_jcode_source() {
        let info = Pdf.fork_project().expect("pdf 应声明 fork 来源");
        assert_eq!(info.plugin_name, "jcode");
        assert_eq!(info.plugin_version, "0.94.0");
        assert_eq!(info.url, "https://github.com/1jehuang/jcode");
    }

    #[test]
    fn tool_declares_required_path_parameter() {
        let tool = read_pdf_tool();
        assert_eq!(tool.name, TOOL_READ_PDF);
        assert_eq!(tool.parameters["required"], json!(["path"]));
        assert!(tool.annotations.read_only);
    }

    #[test]
    fn missing_file_reports_read_error() {
        let err = extract_pdf(Path::new("/definitely/not/here.pdf")).unwrap_err();
        assert!(err.contains("cannot read"), "{err}");
    }

    #[test]
    fn directory_is_rejected() {
        let dir = temp_dir("dir");
        let err = extract_pdf(&dir).unwrap_err();
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

        let err = extract_pdf(&file).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn garbage_bytes_report_error_instead_of_panicking() {
        let dir = temp_dir("garbage");
        let file = dir.join("garbage.pdf");
        std::fs::write(&file, b"not a pdf at all").unwrap();

        let err = extract_pdf(&file).unwrap_err();
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

    #[test]
    fn extracts_text_from_minimal_pdf() {
        let dir = temp_dir("ok");
        let file = dir.join("hello.pdf");
        std::fs::write(&file, minimal_pdf()).unwrap();

        let text = extract_pdf(&file).unwrap();
        assert!(text.contains("Hello PDF"), "{text:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_read_pdf_reports_text_and_path() {
        let dir = temp_dir("run");
        std::fs::write(dir.join("hello.pdf"), minimal_pdf()).unwrap();

        let res = run_read_pdf(
            &json!({ "path": "hello.pdf" }),
            &ctx(&dir.to_string_lossy()),
        )
        .await
        .unwrap();
        assert!(res.text.contains("Read PDF"), "{}", res.text);
        assert!(res.text.contains("Hello PDF"), "{}", res.text);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn missing_parameter_is_an_error() {
        let err = run_read_pdf(&json!({}), &ctx("/tmp")).await.unwrap_err();
        assert!(err.0.contains("missing required parameter"), "{}", err.0);
    }

    #[tokio::test]
    async fn empty_path_is_an_error() {
        let err = run_read_pdf(&json!({ "path": "  " }), &ctx("/tmp"))
            .await
            .unwrap_err();
        assert!(err.0.contains("must not be empty"), "{}", err.0);
    }

    #[tokio::test]
    async fn unknown_tool_name_is_rejected() {
        let err = Pdf
            .execute_tool_async("nope".to_string(), json!({}), ctx("/tmp"))
            .await
            .unwrap_err();
        assert!(err.0.contains("unknown tool: nope"), "{}", err.0);
    }
}
