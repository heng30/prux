//! 本地资料检索：把 `datasheets/` 下的手册按块切分做词法检索。
//!
//! 这是 `hardware_search` 工具的实现本体，与探针通道（[`super::probe`]）互不相干：
//! 探针看的是**板子上真实的寄存器**，这里看的是**手册里写的寄存器**。

use crate::{
    core::{
        extensions::{ExtensionTool, ToolAnnotations, ToolExposure},
        tools::{ToolError, ToolResult},
    },
    extensions::util::{opt_i64, req_str},
    utils::{text, truncate::truncate_chars_within},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// 工具名。
pub(super) const TOOL_SEARCH: &str = "hardware_search";

/// 资料目录相对 agent 目录的路径（`/hardware` 提示与文档里都用它）。
pub(super) const DATASHEET_REL: &str = "extensions/hardware/datasheets";

/// 单次检索返回的默认块数。
const DEFAULT_LIMIT: usize = 8;

/// 单次检索返回的最大块数。
const MAX_LIMIT: usize = 20;

/// 一个不超过这个字节数的文件才读：手册转出来的 Markdown 通常远小于此。
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// 单块的字符上限：超过就在空行处切，避免一句寄存器描述占满上下文。
const MAX_CHUNK_CHARS: usize = 1200;

/// 返回给模型的每块正文上限。
const EXCERPT_CHARS: usize = 800;

/// 查询最多取用的词元个数。
const MAX_QUERY_TERMS: usize = 12;

/// 收为资料的文件扩展名（其余忽略：二进制手册、图片都索引不了）。
const TEXT_EXTENSIONS: &[&str] = &["md", "markdown", "txt", "csv", "tsv", "json", "rst", "adoc"];

/// 一块可检索的片段。
#[derive(Debug, Clone)]
struct Chunk {
    /// 相对资料目录的路径（`/` 分隔），用于展示 `file:line`。
    source: String,
    /// 起始行号（1 基）。
    line: usize,
    /// 原文正文。
    text: String,
}

/// 一条命中：块 + 分数。
#[derive(Debug, Clone)]
struct Ranked {
    /// 命中的块。
    chunk: Chunk,
    /// 词元命中分（路径命中额外加权）。
    score: f64,
}

/// 递归收集资料文件：跳过隐藏目录与非常规文件。
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            collect_files(&path, out);
        } else if kind.is_file() && is_text_source(&path) {
            out.push(path);
        }
    }
}

/// 该路径的扩展名是否在 [`TEXT_EXTENSIONS`] 里（大小写不敏感）。
fn is_text_source(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .is_some_and(|e| TEXT_EXTENSIONS.contains(&e.as_str()))
}

/// 把一份文档按空行 / 字符上限切成块。
fn chunk_file(relative: &str, text: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut buffer = String::new();
    let mut start = 1;

    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        if buffer.is_empty() {
            start = line_no;
        } else {
            buffer.push('\n');
        }
        buffer.push_str(line);

        let boundary = line.trim().is_empty() || buffer.chars().count() >= MAX_CHUNK_CHARS;
        if boundary {
            push_chunk(&mut chunks, relative, start, &mut buffer);
        }
    }
    push_chunk(&mut chunks, relative, start, &mut buffer);

    chunks
}

/// 把缓冲区收成一块（空块直接丢弃）。
fn push_chunk(chunks: &mut Vec<Chunk>, relative: &str, line: usize, buffer: &mut String) {
    if buffer.trim().is_empty() {
        buffer.clear();
        return;
    }
    chunks.push(Chunk {
        source: relative.to_string(),
        line,
        text: std::mem::take(buffer),
    });
}

/// 扫资料目录并切块；目录不存在时返回空表。
fn collect_chunks(root: &Path) -> Vec<Chunk> {
    let mut files = Vec::new();
    collect_files(root, &mut files);
    files.sort();

    let mut chunks = Vec::new();
    for path in files {
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        chunks.extend(chunk_file(&relative, &text));
    }
    chunks
}

/// 一条查询的检索：`filters` 是路径前缀（可选）。
fn rank(chunks: &[Chunk], query: &str, filters: &[String], limit: usize) -> Vec<Ranked> {
    let terms = text::terms(query, MAX_QUERY_TERMS);
    if terms.is_empty() {
        return Vec::new();
    }

    let mut ranked: Vec<Ranked> = Vec::new();
    for chunk in chunks {
        if !filters.is_empty()
            && !filters
                .iter()
                .any(|f| chunk.source.to_lowercase().starts_with(&f.to_lowercase()))
        {
            continue;
        }

        let lowered = chunk.text.to_lowercase();
        let source = chunk.source.to_lowercase();
        let mut score = 0.0;
        for term in &terms {
            let hits = lowered.matches(term.as_str()).count();
            if hits > 0 {
                // 词频开方：一段里喊 50 次的词不比喊 5 次的更相关 10 倍
                score += (hits as f64).sqrt();
            }
            if source.contains(term.as_str()) {
                // 路径命中说明这块就属于这颗芯片 / 这个外设
                score += 2.0;
            }
        }
        if score <= 0.0 {
            continue;
        }
        ranked.push(Ranked {
            chunk: chunk.clone(),
            score,
        });
    }

    // 确定性排序：分数降序 → 路径升序 → 行号升序（同样的资料与查询必得同样的输出）
    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.chunk.source.cmp(&b.chunk.source))
            .then_with(|| a.chunk.line.cmp(&b.chunk.line))
    });
    ranked.truncate(limit);
    ranked
}

/// 把命中渲染成给模型看的报告。
fn render(query: &str, root: &Path, chunks_total: usize, ranked: &[Ranked]) -> String {
    if ranked.is_empty() {
        return format!(
            "No matches for `{query}` in {} ({chunks_total} chunks indexed).\n\
             Put datasheets or pinout tables as .md / .txt / .csv files under {}.",
            root.display(),
            root.display()
        );
    }

    let mut out = format!(
        "{} match(es) for `{query}` in {}:\n",
        ranked.len(),
        root.display()
    );
    for item in ranked {
        out.push_str(&format!(
            "\n<doc source=\"{}\" line=\"{}\">\n",
            item.chunk.source, item.chunk.line
        ));
        out.push_str(&truncate_chars_within(
            item.chunk.text.trim(),
            EXCERPT_CHARS,
            "…",
        ));
        out.push_str("\n</doc>\n");
    }
    out
}

/// 执行 `hardware_search`。
pub(super) fn run_search(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let query = req_str(args, "query")?;
    if text::terms(&query, MAX_QUERY_TERMS).is_empty() {
        return Err(ToolError(
            "`query` must contain at least one searchable term.".to_string(),
        ));
    }

    let limit = match opt_i64(args, "limit") {
        Some(n) if n > 0 => (n as usize).min(MAX_LIMIT),
        Some(_) => return Err(ToolError("`limit` must be greater than 0.".to_string())),
        None => DEFAULT_LIMIT,
    };

    let filters: Vec<String> = args
        .get("path")
        .and_then(Value::as_str)
        .map(|p| {
            p.split(',')
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let root = super::datasheet_dir();
    let chunks = collect_chunks(&root);
    let ranked = rank(&chunks, &query, &filters, limit);
    Ok(ToolResult::text(render(
        &query,
        &root,
        chunks.len(),
        &ranked,
    )))
}

/// 资料目录的索引统计：文件数 + 块数（`/hardware` 面板用）。
pub(super) fn stats(root: &Path) -> (usize, usize) {
    let chunks = collect_chunks(root);
    let mut files: Vec<&str> = chunks.iter().map(|c| c.source.as_str()).collect();
    files.sort_unstable();
    files.dedup();
    (files.len(), chunks.len())
}

/// 支持的文件扩展名（`/hardware` 面板提示里列出）。
pub(super) fn text_extensions() -> &'static [&'static str] {
    TEXT_EXTENSIONS
}

/// 构造 `hardware_search` 工具定义（延迟工具：模型先用 `tool_search` 发现它）。
pub(super) fn search_tool() -> ExtensionTool {
    let mut tool = ExtensionTool::simple(
        TOOL_SEARCH,
        &format!(
            "Search local chip datasheets and pinout tables (datasheets converted to Markdown). \
             Returns `file:line` plus the matching excerpt. Files live under {DATASHEET_REL} \
             and may be grouped in per-chip subdirectories; a path hit weights that chip up."
        ),
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Register names, pin names, peripheral names or errata keywords (e.g. `USART1_TX`, `AF7`, `overrun`)."
                },
                "path": {
                    "type": "string",
                    "description": "Optional comma-separated path prefixes relative to the datasheets directory (e.g. `stm32f4/,nrf52.md`)."
                },
                "limit": {
                    "type": "integer",
                    "description": format!("Maximum chunks to return (default {DEFAULT_LIMIT}, max {MAX_LIMIT}).")
                }
            },
            "required": ["query"]
        }),
        "Search local chip datasheets and pinout tables",
    )
    .with_exposure(ToolExposure::Deferred)
    .with_annotations(ToolAnnotations {
        read_only: true,
        destructive: false,
        idempotent: true,
    });
    tool.label = Some("Hardware Search".to_string());
    tool
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{AUTH_TEST_LOCK, AgentDirGuard};
    use std::sync::MutexGuard;

    /// 串行锁 + 临时 agent_dir：资料目录落在沙箱里。
    fn guard() -> (MutexGuard<'static, ()>, AgentDirGuard) {
        let lock = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        (lock, AgentDirGuard::temp())
    }

    /// 在资料目录里写一份文档。
    fn write_doc(relative: &str, body: &str) {
        let path = super::super::datasheet_dir().join(relative);
        fs::create_dir_all(path.parent().expect("应有父目录")).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn chunks_split_on_blank_lines_and_keep_line_numbers() {
        let text = "line one\nline two\n\nline four\n";
        let chunks = chunk_file("m.md", text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].line, 1);
        assert_eq!(chunks[1].line, 4);
        assert!(chunks[0].text.contains("line two"));
    }

    #[test]
    fn long_paragraphs_are_capped() {
        let text = "x".repeat(MAX_CHUNK_CHARS + 10);
        let chunks = chunk_file("m.md", &text);
        assert_eq!(chunks.len(), 1, "没有空行时整段算一块");
        assert!(chunks[0].text.chars().count() > MAX_CHUNK_CHARS);
    }

    #[test]
    fn pinout_csv_is_searchable_by_pin_name() {
        let (_g, _ad) = guard();
        write_doc(
            "stm32f4/pins.csv",
            "PA9,USART1_TX,TIM1_CH2\nPA10,USART1_RX\n",
        );

        let chunks = collect_chunks(&super::super::datasheet_dir());
        assert_eq!(chunks.len(), 1, "csv 没有空行，整份算一块");

        let ranked = rank(&chunks, "USART1_TX", &[], 5);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].chunk.source, "stm32f4/pins.csv");
        assert!(ranked[0].chunk.text.contains("PA9"));
    }

    #[test]
    fn path_filter_limits_the_hits() {
        let (_g, _ad) = guard();
        write_doc("stm32f4/pins.csv", "PA9,USART1_TX\n");
        write_doc("nrf52/pins.csv", "PA9,USART1_TX\n");

        let chunks = collect_chunks(&super::super::datasheet_dir());
        let ranked = rank(&chunks, "USART1_TX", &["nrf52/".to_string()], 5);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].chunk.source, "nrf52/pins.csv");
    }

    #[test]
    fn a_path_hit_outranks_a_body_hit() {
        let chunks = vec![
            Chunk {
                source: "other/notes.md".to_string(),
                line: 1,
                text: "usart timing notes".to_string(),
            },
            Chunk {
                source: "usart/chip.md".to_string(),
                line: 1,
                text: "timing notes".to_string(),
            },
        ];
        let ranked = rank(&chunks, "usart", &[], 5);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].chunk.source, "usart/chip.md");
    }

    #[test]
    fn search_reports_an_empty_directory_instead_of_failing() {
        let (_g, _ad) = guard();
        let out = run_search(&json!({ "query": "usart" })).unwrap();
        assert!(out.text.contains("No matches"), "{}", out.text);
        assert!(out.text.contains("Put datasheets"), "应提示怎么放资料");
    }

    #[test]
    fn search_rejects_a_stop_word_only_query() {
        let (_g, _ad) = guard();
        assert!(run_search(&json!({ "query": "the" })).is_err());
        assert!(run_search(&json!({ "query": "usart", "limit": 0 })).is_err());
    }

    #[test]
    fn only_text_extensions_are_indexed() {
        let (_g, _ad) = guard();
        write_doc("guide.md", "usart register map\n");
        write_doc("raw.pdf", "usart register map\n");
        let chunks = collect_chunks(&super::super::datasheet_dir());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].source, "guide.md");
    }
}
