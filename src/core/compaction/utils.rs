//! 压缩工具函数：对话序列化与文件操作跟踪

use crate::core::{
    self,
    provider::{AgentMessage, ContentBlock},
};
use serde_json::Value;
use std::collections::HashSet;

/// 压缩序列化时工具结果保留的最大字符数，超出部分截断省略。
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// 拼接内容块里所有文本块的文本（非文本块忽略，块间不加分隔）。
fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// 把工具结果文本截到 [`TOOL_RESULT_MAX_CHARS`] 以内（回退到合法 UTF-8 边界），
/// 并在末尾追加「[... N more characters truncated]」标记；未超限则原样返回。
fn truncate_for_summary(content: &str) -> String {
    if content.len() > TOOL_RESULT_MAX_CHARS {
        // 回退到 <= 上限的合法 UTF-8 字符边界，避免在字符中间切片崩溃
        let mut end = TOOL_RESULT_MAX_CHARS;
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
        let kept = &content[..end];

        format!(
            "{}\n\n[... {} more characters truncated]",
            kept,
            content.len() - end
        )
    } else {
        content.to_string()
    }
}

/// 对话序列化
pub fn serialize_conversation(messages: &[AgentMessage]) -> String {
    let mut parts: Vec<String> = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "user" => {
                let content = content_text(&msg.content);
                if !content.is_empty() {
                    parts.push(format!("[User]: {}", content));
                }
            }
            "assistant" => {
                let mut thinking_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<String> = Vec::new();
                let mut has_text = false;

                for block in &msg.content {
                    match block {
                        ContentBlock::Thinking { thinking, .. } => {
                            thinking_parts.push(thinking.clone())
                        }
                        ContentBlock::ToolCall {
                            name, arguments, ..
                        } => {
                            let args_str = match arguments {
                                Value::Object(map) => map
                                    .iter()
                                    .map(|(k, v)| {
                                        format!(
                                            "{}={}",
                                            k,
                                            serde_json::to_string(v).unwrap_or_default()
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                other => serde_json::to_string(other).unwrap_or_default(),
                            };
                            tool_calls.push(format!("{}({})", name, args_str));
                        }
                        ContentBlock::Text { .. } => has_text = true,
                        _ => {}
                    }
                }

                if !thinking_parts.is_empty() {
                    parts.push(format!(
                        "[Assistant thinking]: {}",
                        thinking_parts.join("\n")
                    ));
                }
                if has_text {
                    parts.push(format!("[Assistant]: {}", content_text(&msg.content)));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            "toolResult" => {
                let content = content_text(&msg.content);
                if !content.is_empty() {
                    parts.push(format!("[Tool result]: {}", truncate_for_summary(&content)));
                }
            }
            "compactionSummary" | "branchSummary" => {
                let prefix = if msg.role == "compactionSummary" {
                    core::provider::COMPACTION_SUMMARY_PREFIX
                } else {
                    core::provider::BRANCH_SUMMARY_PREFIX
                };
                let suffix = if msg.role == "compactionSummary" {
                    core::provider::COMPACTION_SUMMARY_SUFFIX
                } else {
                    core::provider::BRANCH_SUMMARY_SUFFIX
                };
                let text = content_text(&msg.content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {}{}{}", prefix, text, suffix));
                }
            }
            _ => {}
        }
    }

    parts.join("\n\n")
}

// 收集工具调用中的文件路径
/// 从对话中汇总出的文件操作集合：读过、写过与编辑过的路径。
#[derive(Debug, Default, Clone)]
pub struct FileOperations {
    /// read 工具读过的路径；被写/编辑过的会被排除出「只读清单」
    pub read: HashSet<String>,
    /// write 工具新建或整体覆盖的路径
    pub written: HashSet<String>,
    /// edit 工具局部改动过的路径
    pub edited: HashSet<String>,
}

/// 仅统计 assistant 的 read/write/edit 工具调用
pub fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOperations) {
    if message.role != "assistant" {
        return;
    }
    for block in &message.content {
        let ContentBlock::ToolCall {
            name, arguments, ..
        } = block
        else {
            continue;
        };

        let Some(path) = arguments.get("path").and_then(|v| v.as_str()) else {
            continue;
        };

        match name.as_str() {
            "read" => {
                file_ops.read.insert(path.to_string());
            }
            "write" => {
                file_ops.written.insert(path.to_string());
            }
            "edit" => {
                file_ops.edited.insert(path.to_string());
            }
            _ => {}
        }
    }
}

/// readFiles 只包含没有修改过的 read；modifiedFiles = edited + written，排序
pub fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: HashSet<String> = file_ops.edited.union(&file_ops.written).cloned().collect();
    let mut read_files: Vec<String> = file_ops
        .read
        .iter()
        .filter(|f| !modified.contains(*f))
        .cloned()
        .collect();
    let mut modified_files: Vec<String> = modified.into_iter().collect();
    read_files.sort();
    modified_files.sort();
    (read_files, modified_files)
}

/// 把读/改文件列表格式化为 `<read-files>` 与 `<modified-files>` 两段，整体前置两个换行；
/// 两段都为空时返回空串（调用方可据此直接拼接）。
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}
