//! Markdown → PDF（`export_pdf` 工具）。
//!
//! 把 Markdown 翻成 Typst 标记，写进临时 `.typ`，再交给系统里已安装的 `typst` 命令
//! （<https://github.com/typst/typst>）编译。本扩展**不内嵌排版引擎**：没装 typst 时
//! 工具直接把下载地址回给模型，由它转告用户，而不是反复重试。
//!
//! 字体：Typst 不做隐式字体回退，中文必须显式列进 `#set text(font: (..))` 的字体族列表。
//! 正文含 CJK 时先跑一次 `typst fonts` 问系统装了哪些字体族，按候选表挑一个 CJK 族拼进前言；
//! 一个都挑不到就照常渲染，并在结果里说明「中文会是无字空框」。
//!
//! shortcut: 只排一份自包含文档，typst 进程的 cwd 是临时目录，所以 `#include`、图片、
//! 外部包都不可用；需要多文件工程时直接把 `typst compile` 交给用户。

use crate::{
    core::{
        extensions::ToolExecCtx,
        tools::{ToolError, ToolResult},
    },
    extensions::util::{heading_level, truncate_chars},
    utils::{
        paths::resolve_path,
        text::{code_fence, escape_special, has_cjk, pick_font_family, related_font_families},
    },
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde_json::Value;
use std::{path::Path, process::Stdio, sync::OnceLock, time::Duration};

/// 未指定 `path` 时的默认输出文件名（相对会话 cwd）。
const DEFAULT_OUTPUT: &str = "output.pdf";

/// 编译等待上限：Typst 排一本手册也是秒级，超过这个数说明卡住了。
const COMPILE_TIMEOUT: Duration = Duration::from_secs(120);

/// 探测 `typst --version` / `typst fonts` 的等待上限（系统字体扫描在慢机上要几百毫秒）。
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// 回给模型的 typst 下载地址。
pub(super) const TYPST_DOWNLOAD_URL: &str = "https://github.com/typst/typst";

/// 出错时回给模型的标准错误文本上限（字符），避免把整段 stderr 灌进上下文。
const MAX_ERROR_CHARS: usize = 2000;

/// 前言里的西文正文字体族：Typst 内置，任何安装都有。
const SERIF_FAMILY: &str = "Libertinus Serif";

/// 前言里的代码字体族：同上，Typst 内置。
const MONO_FAMILY: &str = "DejaVu Sans Mono";

/// Typst 标记里的特殊字符：正文出现时一律加反斜杠按字面输出。
const MARKUP_SPECIALS: &[char] = &[
    '\\', '#', '$', '*', '_', '`', '[', ']', '~', '^', '<', '>', '@',
];

/// Typst 字符串字面量里要转义的字符（用于 `#link("...")` 的目标与字体族名）。
const STRING_SPECIALS: &[char] = &['"', '\\'];

/// 系统 CJK 字体族候选（按优先级）：先命中先得，`typst fonts` 的结果里没有就跳过。
const CJK_CANDIDATES: &[&str] = &[
    "Noto Sans CJK SC",
    "Noto Serif CJK SC",
    "Noto Sans SC",
    "Noto Serif SC",
    "Source Han Sans SC",
    "Source Han Serif SC",
    "Sarasa Gothic SC",
    "WenQuanYi Zen Hei",
    "WenQuanYi Micro Hei",
    "Microsoft YaHei",
    "SimSun",
    "SimHei",
    "PingFang SC",
    "Heiti SC",
    "Hiragino Sans GB",
    "Songti SC",
    "Noto Sans CJK JP",
    "Noto Sans CJK TC",
];

/// 候选表全落空时的兜底匹配关键字：家族名里带这些片段就当作能写 CJK。
const CJK_NAME_HINTS: &[&str] = &[
    "CJK",
    "Han Sans",
    "Han Serif",
    "YaHei",
    "PingFang",
    "Heiti",
    "SimSun",
    "SimHei",
    "Songti",
    "KaiTi",
    "Zen Hei",
    "Sarasa",
];

/// `typst fonts` 的结果缓存：系统字体在一次会话里基本不变，扫一次就够。
static FONT_FAMILIES: OnceLock<Vec<String>> = OnceLock::new();

/// Typst 渲染不出来的可变字体家族缓存（小写）：
/// 选中它们时中文字形全部变空框，且**不会**回退到列表里的下一个家族，所以必须在挑选前排除。
static VARIABLE_FAMILIES: OnceLock<Vec<String>> = OnceLock::new();

/// `export_pdf` 工具的执行体：校验参数 → 拼 Typst 源 → 写临时 `.typ` → 调 `typst compile`。
///
/// typst 不在 `$PATH` 里时返回带下载地址的 [`ToolError`]；编译失败时返回 typst 自己的
/// stderr（裁剪到 [`MAX_ERROR_CHARS`]）。
pub async fn run(args: &Value, ctx: &ToolExecCtx) -> Result<ToolResult, ToolError> {
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError("missing required parameter: content".to_string()))?;
    if content.trim().is_empty() {
        return Err(ToolError("`content` must not be empty.".to_string()));
    }

    let version = probe_typst()
        .await
        .map_err(|e| ToolError(missing_typst_message(&e)))?;

    let cjk = if has_cjk(content) {
        pick_font_family(
            &usable_families(font_families().await).await,
            CJK_CANDIDATES,
            CJK_NAME_HINTS,
        )
    } else {
        None
    };
    let source = format!("{}{}", preamble(cjk.as_deref()), to_typst(content));

    let output = match args.get("path").and_then(Value::as_str) {
        Some(raw) if !raw.trim().is_empty() => resolve_path(raw, &ctx.cwd),
        _ => Path::new(&ctx.cwd).join(DEFAULT_OUTPUT),
    };
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| ToolError(format!("cannot create {}: {e}", parent.display())))?;
    }

    let dir = tempfile::Builder::new()
        .prefix("prux-pdf")
        .tempdir()
        .map_err(|e| ToolError(format!("cannot create a temp directory: {e}")))?;
    let input = dir.path().join("document.typ");
    std::fs::write(&input, &source)
        .map_err(|e| ToolError(format!("cannot write {}: {e}", input.display())))?;

    let warnings = compile_typst(&input, &output).await?;

    let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
    let mut message = format!(
        "Wrote {} ({size} bytes) with typst {version}.",
        output.display()
    );
    match &cjk {
        Some(family) => message.push_str(&format!("\nBody font: {SERIF_FAMILY} + {family} (CJK).")),
        None if has_cjk(content) => message.push_str(
            "\nNo CJK font was found in `typst fonts`, so Chinese text will render as empty boxes. \
             Install one (e.g. Noto Sans CJK SC) or run typst with `--font-path`.",
        ),
        None => {}
    }
    let warnings = warnings.trim();
    if !warnings.is_empty() {
        message.push_str(&format!("\ntypst warnings:\n{warnings}"));
    }
    Ok(ToolResult::text(message))
}

/// typst 缺失/探测失败时回给模型的文本：显式给出下载地址，别让它反复重试。
fn missing_typst_message(error: &str) -> String {
    format!(
        "`typst` is not available ({error}). Install it from {TYPST_DOWNLOAD_URL} (or put it on $PATH), then retry."
    )
}

/// Typst 前言：页面、正文字号与字体族列表（含 CJK 回退）都写在这里。
///
/// `cjk` 为 `Some(家族名)` 时，正文字体族列表追加该家族并把 `lang` 设为中文，
/// 代码块的字体族也追加它——Typst 按字形逐个回退，所以列表顺序就是优先级。
fn preamble(cjk: Option<&str>) -> String {
    let mut body = vec![SERIF_FAMILY];
    let mut raw = vec![MONO_FAMILY];
    if let Some(family) = cjk {
        body.push(family);
        raw.push(family);
    }
    let mut source = String::from("#set page(paper: \"a4\", margin: 2.5cm, numbering: \"1\")\n");
    if cjk.is_some() {
        source.push_str("#set text(lang: \"zh\")\n");
    }
    source.push_str(&format!(
        "#set text(font: {}, size: 10pt)\n",
        family_list(&body)
    ));
    source.push_str(&format!(
        "#show raw: set text(font: {})\n",
        family_list(&raw)
    ));
    source.push_str("#set par(leading: 0.7em)\n");
    source.push_str("#show heading.where(level: 1): set align(center)\n");
    source.push_str("#show heading.where(level: 1): set text(size: 18pt, weight: \"bold\")\n\n");
    source
}

/// 把字体族名列表拼成 Typst 的数组字面量（`("a", "b")`），名字里的引号会转义。
fn family_list(families: &[&str]) -> String {
    let quoted: Vec<String> = families
        .iter()
        .map(|f| format!("\"{}\"", escape_special(f, STRING_SPECIALS)))
        .collect();
    format!("({})", quoted.join(", "))
}

/// 把 Markdown 转成 Typst 标记（不含前言，前言由 [`preamble`] 拼）。
fn to_typst(md: &str) -> String {
    let mut conv = Converter::default();
    let mut cells: Vec<String> = Vec::new();
    let mut code: Option<(String, String)> = None;

    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_MATH;
    for event in Parser::new_ext(md, options) {
        if let Some((_, body)) = code.as_mut() {
            match event {
                Event::Text(text) => body.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    let (lang, body) = code.take().unwrap_or_default();
                    let body = body.trim_end_matches('\n');
                    let ticks = code_fence(body);
                    conv.push(&format!("{ticks}{lang}\n{body}\n{ticks}"));
                    conv.blank_line();
                }
                _ => {}
            }
            continue;
        }

        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                conv.blank_line();
                conv.title_pending = false;
                conv.push(&"=".repeat(heading_level(level)));
                conv.push(" ");
            }
            Event::End(TagEnd::Heading(level)) => {
                conv.blank_line();
                conv.title_pending = heading_level(level) == 1;
            }
            Event::Start(Tag::Paragraph) => {
                if conv.title_pending {
                    conv.subtitle = Some(String::new());
                }
            }
            Event::End(TagEnd::Paragraph) => {
                if conv.cell.is_some() {
                    conv.push(" ");
                } else if !conv.finish_subtitle() {
                    conv.blank_line();
                }
                conv.title_pending = false;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                conv.blank_line();
                conv.title_pending = false;
                conv.push("#block(fill: luma(243), inset: 9pt, radius: 3pt, width: 100%, text(size: 9.8pt)[\n");
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                conv.trim_newlines();
                conv.push("\n])");
                conv.blank_line();
            }
            Event::Start(Tag::List(start)) => {
                conv.title_pending = false;
                conv.lists.push(start.is_some())
            }
            Event::End(TagEnd::List(_)) => {
                conv.lists.pop();
                if conv.lists.is_empty() {
                    conv.blank_line();
                }
            }
            Event::Start(Tag::Item) => {
                let ordered = conv.lists.last().copied().unwrap_or(false);
                let depth = conv.lists.len().saturating_sub(1);
                conv.newline();
                conv.push(&"  ".repeat(depth));
                conv.push(if ordered { "+ " } else { "- " });
            }
            Event::End(TagEnd::Item) => conv.newline(),
            Event::Start(Tag::CodeBlock(kind)) => {
                conv.blank_line();
                conv.title_pending = false;
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.trim().to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((lang, String::new()));
            }
            Event::Start(Tag::Emphasis) => conv.push("_"),
            Event::End(TagEnd::Emphasis) => conv.push("_"),
            Event::Start(Tag::Strong) => conv.push("*"),
            Event::End(TagEnd::Strong) => conv.push("*"),
            Event::Start(Tag::Strikethrough) => conv.push("#strike["),
            Event::End(TagEnd::Strikethrough) => conv.push("]"),
            Event::Start(Tag::Link { dest_url, .. }) => {
                conv.push(&format!(
                    "#link(\"{}\")[",
                    escape_special(&dest_url, STRING_SPECIALS)
                ));
            }
            Event::End(TagEnd::Link) => conv.push("]"),
            Event::Start(Tag::Image { .. }) => conv.push("["),
            Event::End(TagEnd::Image) => conv.push("]"),
            Event::Start(Tag::Table(aligns)) => {
                conv.blank_line();
                conv.title_pending = false;
                conv.push(&format!(
                    "#table(columns: {}, stroke: 0.5pt, inset: 6pt,\n",
                    aligns.len()
                ));
            }
            Event::End(TagEnd::Table) => {
                conv.push(")");
                conv.blank_line();
            }
            Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                for cell in cells.drain(..) {
                    conv.push(&format!("  [{cell}],\n"));
                }
            }
            Event::Start(Tag::TableCell) => conv.cell = Some(String::new()),
            Event::End(TagEnd::TableCell) => cells.push(conv.cell.take().unwrap_or_default()),
            Event::Text(text) => conv.push_text(&text),
            Event::Code(text) => {
                let ticks = code_fence(&text);
                conv.push(&format!("{ticks}{text}{ticks}"));
            }
            Event::InlineMath(math) => conv.push(&format!("${math}$")),
            Event::DisplayMath(math) => conv.push(&format!("$ {math} $")),
            Event::SoftBreak => conv.push(" "),
            Event::HardBreak => conv.push(" \\\n"),
            Event::Rule => {
                conv.blank_line();
                conv.push("#line(length: 100%)");
                conv.blank_line();
            }
            Event::TaskListMarker(done) => conv.push(if done { "☑ " } else { "☐ " }),
            _ => {}
        }
    }

    conv.finish()
}

/// 整段被单个 `_..._` 包裹时返回内部文本：Markdown 里紧跟标题的 `*副标题*` 行。
///
/// 段内不允许再出现 `_`——否则是「强调开头、正文结尾」的普通段落，不能整段当副标题。
fn subtitle_text(paragraph: &str) -> Option<&str> {
    let trimmed = paragraph.trim();
    let inner = trimmed.strip_prefix('_')?.strip_suffix('_')?;
    (!inner.is_empty() && !inner.contains('_')).then_some(inner.trim())
}

/// 转换过程中的状态：输出缓冲、表格单元与副标题的临时缓冲、列表嵌套栈。
///
/// 表格单元的内容先写进 `cell`，到行尾才拼成 `[...],`——Typst 的表格是一行参数列表，
/// 不能边写边换行。
#[derive(Default)]
struct Converter {
    /// 主线输出（Typst 标记）。
    out: String,
    /// `Some` 表示正在写表格单元，此时所有写入都落到这里。
    cell: Option<String>,
    /// `Some` 表示正在缓冲一级标题后的第一段（副标题候选），到段尾才决定怎么排。
    subtitle: Option<String>,
    /// 上一块是一级标题：下一段若是斜体段则排成灰色居中副标题。
    title_pending: bool,
    /// 列表嵌套栈：每个元素表示那一层是不是有序列表。
    lists: Vec<bool>,
}

impl Converter {
    /// 原样写入片段（调用方负责转义）。
    fn push(&mut self, text: &str) {
        match (&mut self.cell, &mut self.subtitle) {
            (Some(cell), _) => cell.push_str(text),
            (None, Some(subtitle)) => subtitle.push_str(text),
            (None, None) => self.out.push_str(text),
        }
    }

    /// 写用户文本：转义 Typst 标记字符，并给行首的结构字符补反斜杠。
    fn push_text(&mut self, raw: &str) {
        let escaped = escape_special(raw, MARKUP_SPECIALS);
        let at_line_start = match (&self.cell, &self.subtitle) {
            (Some(cell), _) => cell.is_empty() || cell.ends_with('\n'),
            (None, Some(subtitle)) => subtitle.is_empty() || subtitle.ends_with('\n'),
            (None, None) => self.out.is_empty() || self.out.ends_with('\n'),
        };
        if at_line_start && escaped.starts_with(['-', '+', '=', '/']) {
            self.push("\\");
        }
        self.push(&escaped);
    }

    /// 段尾收下副标题候选：整段被单个 `_..._` 包裹就排成灰色居中副标题，否则按普通段落放回主线。
    ///
    /// 返回是否当成了副标题（此时输出已写完）。
    fn finish_subtitle(&mut self) -> bool {
        let Some(buffered) = self.subtitle.take() else {
            return false;
        };
        match subtitle_text(&buffered) {
            Some(text) => {
                self.blank_line();
                self.push(&format!(
                    "#align(center)[#text(size: 9pt, fill: luma(110))[{text}]]"
                ));
                self.blank_line();
                true
            }
            None => {
                self.push(&buffered);
                false
            }
        }
    }

    /// 收尾当前行（不产生空行）；表格单元与副标题缓冲里不需要换行。
    fn newline(&mut self) {
        if self.cell.is_none()
            && self.subtitle.is_none()
            && !self.out.is_empty()
            && !self.out.ends_with('\n')
        {
            self.out.push('\n');
        }
    }

    /// 结束当前块：抹掉行尾换行后统一补一个空行；表格单元与副标题缓冲里什么都不做。
    fn blank_line(&mut self) {
        if self.cell.is_some() || self.subtitle.is_some() {
            return;
        }
        while self.out.ends_with('\n') {
            self.out.pop();
        }
        if !self.out.is_empty() {
            self.out.push_str("\n\n");
        }
    }

    /// 抹掉尾部换行（引用块闭合 `]` 前用）。
    fn trim_newlines(&mut self) {
        if self.cell.is_some() {
            return;
        }
        while self.out.ends_with('\n') {
            self.out.pop();
        }
    }

    /// 收尾：去掉多余空行，保证以单个换行结束。
    fn finish(mut self) -> String {
        while self.out.ends_with('\n') {
            self.out.pop();
        }
        self.out.push('\n');
        self.out
    }
}

/// 跑 `typst compile <input> <output>`，成功返回 stderr（其中的 warning 值得回给模型）。
///
/// 退出码非 0 时把 stderr（为空则 stdout）作为错误返回；超时直接杀进程。
async fn compile_typst(input: &Path, output: &Path) -> Result<String, ToolError> {
    let mut cmd = tokio::process::Command::new("typst");
    cmd.arg("compile")
        .arg(input)
        .arg(output)
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let out = match tokio::time::timeout(COMPILE_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(ToolError(missing_typst_message(&e.to_string()))),
        Err(_) => {
            return Err(ToolError(format!(
                "`typst compile` timed out after {}s",
                COMPILE_TIMEOUT.as_secs()
            )));
        }
    };

    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        let detail = if detail.is_empty() {
            format!("`typst compile` failed ({})", out.status)
        } else {
            detail
        };
        return Err(ToolError(truncate_chars(&detail, MAX_ERROR_CHARS)));
    }
    Ok(stderr)
}

/// 跑一次 typst 并取回 stdout；起不来或非成功退出都返回 `Err`（优先 stderr 文本）。
async fn run_typst(args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("typst");
    cmd.args(args).stdin(Stdio::null()).kill_on_drop(true);

    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("`typst {}` could not run: {e}", args.join(" "))),
        Err(_) => {
            return Err(format!(
                "`typst {}` timed out after {}s",
                args.join(" "),
                timeout.as_secs()
            ));
        }
    };

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("`typst {}` failed ({})", args.join(" "), out.status)
        } else {
            truncate_chars(&stderr, MAX_ERROR_CHARS)
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 确认 `typst` 能用并取回版本号（`typst --version` 的末尾字段）。
async fn probe_typst() -> Result<String, String> {
    let stdout = run_typst(&["--version"], PROBE_TIMEOUT).await?;
    let line = stdout.lines().next().unwrap_or_default().trim();
    Ok(line
        .rsplit_once(' ')
        .map(|(_, version)| version.to_string())
        .unwrap_or_else(|| line.to_string()))
}

/// 问一次 `typst fonts`，结果进 [`FONT_FAMILIES`] 缓存；探测失败时缓存空表。
async fn font_families() -> Vec<String> {
    if let Some(cached) = FONT_FAMILIES.get() {
        return cached.clone();
    }

    let families = run_typst(&["fonts"], PROBE_TIMEOUT)
        .await
        .map(|out| {
            out.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    _ = FONT_FAMILIES.set(families.clone());
    families
}

/// 去掉 Typst 渲染不出来的可变字体家族，剩下的才能进 `#set text(font: ..)`。
async fn usable_families(families: Vec<String>) -> Vec<String> {
    let unusable = variable_families(&families).await;
    families
        .into_iter()
        .filter(|family| !unusable.iter().any(|bad| bad.eq_ignore_ascii_case(family)))
        .collect()
}

/// 探出可变字体家族（小写），结果进 [`VARIABLE_FAMILIES`] 缓存；探测失败时缓存空表。
async fn variable_families(families: &[String]) -> Vec<String> {
    if let Some(cached) = VARIABLE_FAMILIES.get() {
        return cached.clone();
    }

    let suspects = related_font_families(families, CJK_CANDIDATES, CJK_NAME_HINTS);
    let found = if suspects.is_empty() {
        Vec::new()
    } else {
        probe_variable_fonts(&suspects).await.unwrap_or_default()
    };
    _ = VARIABLE_FAMILIES.set(found.clone());
    found
}

/// 把可疑家族各写一段中文交给 typst 编译，从 warning 里取回可变字体家族名。
///
/// Typst 遇到可变字体只发 warning（`hint: try installing a static version of "x" instead`），
/// 一次编译可以问出全部不可用的家族；编译本身失败时返回 `None`，调用方当作没有可变字体。
async fn probe_variable_fonts(suspects: &[String]) -> Option<Vec<String>> {
    let dir = tempfile::Builder::new()
        .prefix("prux-font")
        .tempdir()
        .ok()?;

    let mut source = String::from("#set text(size: 12pt)\n");
    for family in suspects {
        source.push_str(&format!(
            "#text(font: {})[中文]\n\n",
            family_list(&[family.as_str()])
        ));
    }

    let input = dir.path().join("probe.typ");
    std::fs::write(&input, source).ok()?;
    let stderr = compile_typst(&input, &dir.path().join("probe.pdf"))
        .await
        .ok()?;
    Some(parse_variable_families(&stderr))
}

/// 从 typst stderr 的当行取可变字体家族名（警告里的 hint 会给出小写家族名）。
///
/// 认的是 `hint: try installing a static version of "x" instead` 这一行，
/// 警告首行与代码片段行里的家族名不会误采。
fn parse_variable_families(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once("try installing a static version of \"")?;
            let (family, _) = rest.split_once('"')?;
            Some(family.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 标题、强调、行内代码与段落间距。
    #[test]
    fn converts_inline_markup() {
        let out = to_typst("# Title\n\nSome **bold**, *italic* and `code`.\n\n## Sub\n");
        assert!(out.starts_with("= Title\n\n"), "{out}");
        assert!(
            out.contains("Some *bold*, _italic_ and ```code```."),
            "{out}"
        );
        assert!(out.contains("\n\n== Sub\n"), "{out}");
    }

    /// 列表嵌套按缩进表达，有序列表用 `+`；引用包一层灰底块。
    #[test]
    fn converts_lists_and_quotes() {
        let out = to_typst("- a\n  - b\n1. c\n\n> quoted\n");
        assert!(out.contains("- a\n  - b\n"), "{out}");
        assert!(out.contains("+ c\n"), "{out}");
        assert!(out.contains("\nquoted\n]"), "{out}");
    }

    /// 表格每格一行 `[...],`，列数取对齐表长度。
    #[test]
    fn converts_tables() {
        let out = to_typst("| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(out.contains("#table(columns: 2"), "{out}");
        assert!(out.contains("  [a],\n"), "{out}");
        assert!(out.contains("  [1],\n"), "{out}");
    }

    /// 代码块保留原文，围栏长度随内部反引号加长，语言标记跟在围栏后。
    #[test]
    fn code_block_fence_grows() {
        let out = to_typst("````\n```\n````\n");
        assert!(out.contains("````\n```\n````"), "{out}");

        let plain = to_typst("```js\nconst a = 1;\n```\n");
        assert!(plain.contains("```js\nconst a = 1;\n```"), "{plain}");
    }

    /// 用户文本里的 Typst 标记字符必须被转义，否则会被当成排版指令。
    #[test]
    fn escapes_markup_in_text() {
        let out = to_typst("price is $5 and #1 @user\n");
        assert!(out.contains("price is \\$5 and \\#1 \\@user"), "{out}");
    }

    /// 行首的 `-` 会被当成列表标记，转义后才是普通文字。
    #[test]
    fn escapes_line_leading_marker() {
        let out = to_typst("-not a list\n");
        assert!(out.starts_with("\\-not a list"), "{out}");
    }

    /// 数学公式透传成 Typst 的 `$...$`。
    #[test]
    fn passes_math_through() {
        let out = to_typst("inline $a^2$ here\n");
        assert!(out.contains("$a^2$"), "{out}");
    }

    /// 字体族列表按 Typst 数组字面量拼，名字里的引号要转义。
    #[test]
    fn family_list_quotes_names() {
        assert_eq!(
            family_list(&["Libertinus Serif", "Noto Sans SC"]),
            "(\"Libertinus Serif\", \"Noto Sans SC\")"
        );
        assert_eq!(family_list(&["a\"b"]), "(\"a\\\"b\")");
    }

    /// 可变字体的家族名只在 hint 行里，警告首行与代码片段行不能误采。
    #[test]
    fn parses_variable_family_from_hint() {
        let stderr = "warning: variable fonts are not currently supported and may render incorrectly\n  \u{250c}\u{2500} p.typ:2:12\n  \u{2502}\n2 \u{2502} #text(font: \"Noto Sans CJK SC\")[\u{4e2d}\u{6587}]\n  \u{2502}\n  = hint: try installing a static version of \"noto sans cjk sc\" instead\n";
        assert_eq!(
            parse_variable_families(stderr),
            vec!["noto sans cjk sc".to_string()]
        );
        assert!(parse_variable_families("").is_empty());
    }

    /// 一级标题后的斜体段排成灰色居中副标题，普通段落不受影响。
    #[test]
    fn renders_italic_paragraph_after_title_as_subtitle() {
        let out = to_typst("# 电路基础自测问卷\n*学习前摸底 · 单选 3 题*\n\n正文首句。\n");
        assert!(
            out.contains(
                "#align(center)[#text(size: 9pt, fill: luma(110))[学习前摸底 · 单选 3 题]]"
            ),
            "{out}"
        );
        assert!(out.contains("正文首句。"), "{out}");
        assert!(!out.contains("luma(110))[正文首句。]"), "{out}");
    }

    /// 标题后第一段不是整段斜体时就是普通段落，不能被当成副标题。
    #[test]
    fn keeps_plain_paragraph_after_title() {
        let out = to_typst("# 项目说明\n这是正文首句，不是副标题。\n");
        assert!(!out.contains("luma(110)"), "{out}");
        assert!(out.contains("这是正文首句，不是副标题。"), "{out}");
    }

    /// 引用块排成灰底圆角块（模版里的提示框）。
    #[test]
    fn renders_blockquote_as_tinted_box() {
        let out = to_typst("> 用法：先遮住答案\n");
        assert!(
            out.contains(
                "#block(fill: luma(243), inset: 9pt, radius: 3pt, width: 100%, text(size: 9.8pt)["
            ),
            "{out}"
        );
    }

    /// 前言按是否带 CJK 家族切换：带则追加到正文字体族列表并把 lang 设为中文。
    #[test]
    fn preamble_lists_fonts_in_priority_order() {
        let plain = preamble(None);
        assert!(plain.contains("#set text(font: (\"Libertinus Serif\"), size: 10pt)"));
        assert!(!plain.contains("lang:"));

        let cjk = preamble(Some("Noto Sans SC"));
        assert!(
            cjk.contains("#set text(font: (\"Libertinus Serif\", \"Noto Sans SC\"), size: 10pt)")
        );
        assert!(
            cjk.contains("#show raw: set text(font: (\"DejaVu Sans Mono\", \"Noto Sans SC\"))")
        );
        assert!(cjk.contains("#set text(lang: \"zh\")"));
    }

    /// 一级标题（文档标题）居中，其余层级保持左对齐。
    #[test]
    fn preamble_centers_the_title() {
        assert!(preamble(None).contains("#show heading.where(level: 1): set align(center)"));
    }

    /// 缺 typst 的提示必须带下载地址，否则模型只会盲目重试。
    #[test]
    fn missing_typst_message_carries_download_url() {
        let message = missing_typst_message("No such file or directory");
        assert!(message.contains(TYPST_DOWNLOAD_URL));
    }

    /// 真机渲染：机器上没装 typst 时跳过（CI 通常没有）。
    ///
    /// 除了产出 `%PDF`，还把 PDF 文本层抽回来验证——只有真渲染出来才能抽到中文，
    /// 这样 CJK 字体族拼进前言的那一步也被覆盖了。
    #[tokio::test]
    async fn renders_pdf_when_typst_is_installed() {
        if probe_typst().await.is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("prux-markit-pdf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = super::super::tests::ctx(dir.to_str().unwrap());
        let args = serde_json::json!({
            "content": "# 报告\n\n- 一\n- 二\n\n```rust\nfn main() {}\n```\n",
            "path": "out.pdf"
        });

        let result = run(&args, &ctx).await.expect("render should succeed");
        assert!(result.text.contains("out.pdf"), "{}", result.text);

        let bytes = std::fs::read(dir.join("out.pdf")).unwrap();
        assert!(bytes.starts_with(b"%PDF"), "not a PDF");

        let has_cjk_font = pick_font_family(
            &usable_families(font_families().await).await,
            CJK_CANDIDATES,
            CJK_NAME_HINTS,
        )
        .is_some();
        assert_eq!(
            result.text.contains("Chinese text will render"),
            !has_cjk_font,
            "{}",
            result.text
        );
        if has_cjk_font {
            let text = super::super::pdf::extract(&bytes).unwrap();
            assert!(text.contains("报告"), "extracted: {text}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
