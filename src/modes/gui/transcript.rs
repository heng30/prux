//! transcript：对话块模型 + 内核事件 → 块的映射 + 渲染。
//!
//! 事件形状与 TUI 消费的完全一致（`message_start` / `message_update` /
//! `tool_execution_start|update|end` / `agent_settled` …），这里只做 GUI 需要的投影。

use super::components::dot;
use super::markdown;
use super::theme::Theme;
use crate::core::provider::{AgentMessage, ContentBlock};
use gpui::{AnyElement, FontWeight, IntoElement, ParentElement, SharedString, Styled, div, px};
use serde_json::Value;
use std::collections::HashMap;

/// 工具调用的执行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// 正在执行
    Running,
    /// 成功结束
    Ok,
    /// 失败结束
    Failed,
}

/// 一段助手回复。
#[derive(Debug, Default)]
pub struct Assistant {
    /// 正文（流式追加）
    pub text: String,
    /// 思考内容（流式追加，默认折叠显示首行）
    pub thinking: String,
}

/// 一次工具调用。
#[derive(Debug)]
pub struct Tool {
    /// 工具名
    pub name: String,
    /// 入参摘要（单行 JSON）
    pub args: String,
    /// 输出文本
    pub output: String,
    /// 执行状态
    pub status: ToolStatus,
}

/// 非对话提示。
#[derive(Debug)]
pub enum Note {
    /// 一般信息（压缩、重试、状态）
    Info(String),
    /// 错误
    Error(String),
}

/// transcript 里的一块。
#[derive(Debug)]
pub enum Block {
    /// 用户消息
    User(String),
    /// 助手回复
    Assistant(Assistant),
    /// 工具调用
    Tool(Tool),
    /// 提示/错误
    Note(Note),
}

/// 对话流状态机。
#[derive(Default)]
pub struct Transcript {
    /// 已产生的块
    blocks: Vec<Block>,
    /// 正在追加的助手块下标
    open_assistant: Option<usize>,
    /// 工具调用 id → 块下标
    tools: HashMap<String, usize>,
}

impl Transcript {
    /// 清空（切会话/新会话）。
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.open_assistant = None;
        self.tools.clear();
    }

    /// 当前块列表。
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// 追加一条用户消息（本地回显，agent 自己也会发 user 消息但不重复渲染）。
    pub fn push_user(&mut self, text: String) {
        self.blocks.push(Block::User(text));
        self.open_assistant = None;
    }

    /// 追加一条提示。
    pub fn push_note(&mut self, note: Note) {
        self.blocks.push(Block::Note(note));
    }

    /// 从会话历史重建（切会话后渲染既有对话）。
    pub fn from_messages(messages: &[AgentMessage]) -> Self {
        let mut transcript = Self::default();
        for message in messages {
            match message.role.as_str() {
                "user" => {
                    let text = message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        transcript.push_user(text);
                    }
                }
                "assistant" => {
                    let mut assistant = Assistant::default();
                    for block in &message.content {
                        match block {
                            ContentBlock::Text { text, .. } => assistant.text.push_str(text),
                            ContentBlock::Thinking { thinking, .. } => {
                                assistant.thinking.push_str(thinking)
                            }
                            _ => {}
                        }
                    }
                    transcript.blocks.push(Block::Assistant(assistant));
                    transcript.open_assistant = Some(transcript.blocks.len() - 1);
                    for block in &message.content {
                        if let ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } = block
                        {
                            transcript.push_tool(id, name, &summarize_args(arguments));
                        }
                    }
                }
                "toolResult" => {
                    let output = tool_result_text(&message.content);
                    if let Some(index) = message
                        .tool_call_id
                        .as_ref()
                        .and_then(|id| transcript.tools.get(id).copied())
                        && let Some(Block::Tool(tool)) = transcript.blocks.get_mut(index)
                    {
                        tool.output = output;
                        tool.status = if message.is_error {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Ok
                        };
                    }
                }
                _ => {}
            }
        }
        transcript.open_assistant = None;
        transcript
    }

    /// 应用一条内核 JSON 事件。
    pub fn apply(&mut self, event: &Value) {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "message_start" => {
                let role = event
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if role == "assistant" {
                    self.blocks.push(Block::Assistant(Assistant::default()));
                    self.open_assistant = Some(self.blocks.len() - 1);
                }
            }
            "message_update" => {
                let inner = event.get("assistantMessageEvent");
                let inner_type = inner
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let delta = inner
                    .and_then(|e| e.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let Some(index) = self.open_assistant else {
                    return;
                };
                let Some(Block::Assistant(assistant)) = self.blocks.get_mut(index) else {
                    return;
                };
                match inner_type {
                    "text_delta" => assistant.text.push_str(delta),
                    "thinking_delta" => assistant.thinking.push_str(delta),
                    _ => {}
                }
            }
            "tool_execution_start" => {
                let id = event
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = event
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let args = event.get("args").map(summarize_args).unwrap_or_default();
                self.push_tool(&id, &name, &args);
                self.open_assistant = None;
            }
            "tool_execution_update" => {
                let id = event
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let chunk = event
                    .get("partialResult")
                    .and_then(|r| r.get("content"))
                    .and_then(Value::as_array)
                    .map(|content| {
                        content
                            .iter()
                            .filter_map(|c| c.get("text").and_then(Value::as_str))
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                if let Some(Block::Tool(tool)) = self.tool_mut(id) {
                    tool.output.push_str(&chunk);
                }
            }
            "tool_execution_end" => {
                let id = event
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let failed = event
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let output = event
                    .get("result")
                    .and_then(|r| r.get("content"))
                    .and_then(Value::as_array)
                    .map(|content| {
                        content
                            .iter()
                            .filter_map(|c| c.get("text").and_then(Value::as_str))
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                if let Some(Block::Tool(tool)) = self.tool_mut(id) {
                    if !output.is_empty() {
                        tool.output = output;
                    }
                    tool.status = if failed {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Ok
                    };
                }
            }
            "compaction_start" => self.push_note(Note::Info("正在压缩上下文…".to_string())),
            "compaction_end" => self.push_note(Note::Info("上下文已压缩".to_string())),
            "auto_retry_start" => {
                let attempt = event
                    .get("attempt")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                self.push_note(Note::Info(format!("请求失败，第 {attempt} 次重试…")));
            }
            "error" => {
                let message = event
                    .get("message")
                    .or_else(|| event.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误");
                self.push_note(Note::Error(message.to_string()));
            }
            _ => {}
        }
    }

    /// 追加一次工具调用。
    fn push_tool(&mut self, id: &str, name: &str, args: &str) {
        self.blocks.push(Block::Tool(Tool {
            name: name.to_string(),
            args: args.to_string(),
            output: String::new(),
            status: ToolStatus::Running,
        }));
        let index = self.blocks.len() - 1;
        if !id.is_empty() {
            self.tools.insert(id.to_string(), index);
        }
    }

    /// 按 id 找工具块。
    fn tool_mut(&mut self, id: &str) -> Option<&mut Block> {
        let index = *self.tools.get(id)?;
        self.blocks.get_mut(index)
    }
}

/// 入参压成一行摘要（过长截断）。
fn summarize_args(args: &Value) -> String {
    let text = match args {
        Value::Object(map) if !map.is_empty() => map
            .iter()
            .map(|(k, v)| {
                let value = match v {
                    Value::String(s) => s.replace('\n', " "),
                    other => other.to_string(),
                };
                format!("{k}={value}")
            })
            .collect::<Vec<_>>()
            .join(" "),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    truncate(&text, 120)
}

/// 工具结果内容 → 纯文本。
fn tool_result_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 按字符截断，超出补省略号。
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// 单个块的渲染。
pub fn render_block(block: &Block, theme: &Theme, expand_tools: bool) -> AnyElement {
    match block {
        // 用户消息：右对齐气泡，与助手正文拉开层级
        Block::User(text) => div()
            .w_full()
            .flex()
            .justify_end()
            .pt(px(6.))
            .child(
                div()
                    .max_w(px(560.))
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(10.))
                    .bg(theme.raised)
                    .text_size(px(13.5))
                    .text_color(theme.text)
                    .child(SharedString::from(text.clone())),
            )
            .into_any_element(),
        // 助手回复：不加卡片，直接正文；思考内容单独一段细边框提示
        Block::Assistant(assistant) => {
            let mut column = div().w_full().flex().flex_col().gap(px(8.)).pt(px(2.));
            if !assistant.thinking.trim().is_empty() {
                column = column.child(
                    div()
                        .pl(px(10.))
                        .border_l_2()
                        .border_color(theme.border_strong)
                        .text_size(px(12.))
                        .text_color(theme.text_tertiary)
                        .child(SharedString::from(truncate(assistant.thinking.trim(), 300))),
                );
            }
            column
                .child(render_markdown(&assistant.text, theme))
                .into_any_element()
        }
        // 工具调用：带描边的小卡片，状态用圆点表示
        Block::Tool(tool) => {
            let (color, label) = match tool.status {
                ToolStatus::Running => (theme.warning, "运行中"),
                ToolStatus::Ok => (theme.success, "完成"),
                ToolStatus::Failed => (theme.danger, "失败"),
            };
            let mut card = div()
                .w_full()
                .rounded(px(8.))
                .border_1()
                .border_color(theme.border)
                .bg(theme.inset)
                .flex()
                .flex_col()
                .gap(px(6.))
                .px(px(12.))
                .py(px(8.));
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(dot(color, 6.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from(tool.name.clone())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(theme.text_tertiary)
                            .child(SharedString::from(tool.args.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(color)
                            .child(SharedString::from(label)),
                    ),
            );
            if !tool.output.trim().is_empty() {
                let (max_h, limit) = if expand_tools {
                    (1200., 20000)
                } else {
                    (150., 900)
                };
                card = card.child(
                    div()
                        .max_h(px(max_h))
                        .overflow_hidden()
                        .text_size(px(11.5))
                        .text_color(theme.text_secondary)
                        .child(SharedString::from(truncate(tool.output.trim(), limit))),
                );
            }
            card.into_any_element()
        }
        Block::Note(note) => {
            let (text, color, mark) = match note {
                Note::Info(text) => (text, theme.text_tertiary, "·"),
                Note::Error(text) => (text, theme.danger, "!"),
            };
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(2.))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(color)
                        .child(SharedString::from(mark)),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(color)
                        .child(SharedString::from(text.clone())),
                )
                .into_any_element()
        }
    }
}

/// Markdown 文本 → 元素（行内样式、代码块高亮、标题字号）。
pub fn render_markdown(text: &str, theme: &Theme) -> AnyElement {
    let mut column = div().w_full().flex().flex_col().gap(px(4.));
    for block in markdown::parse(text) {
        let element: AnyElement = match block {
            markdown::Block::Line(line) => {
                let mut row = div().w_full().text_size(px(13.5)).text_color(theme.text);
                match line.kind {
                    markdown::LineKind::Heading(level) => {
                        let size = match level {
                            1 => 19.,
                            2 => 17.,
                            3 => 15.5,
                            _ => 14.,
                        };
                        row = row
                            .mt(px(6.))
                            .text_size(px(size))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.text);
                    }
                    markdown::LineKind::Bullet(depth) => {
                        row = row.pl(px(14. * (depth as f32 + 1.))).child(
                            div()
                                .pr(px(6.))
                                .text_color(theme.text_tertiary)
                                .child(SharedString::from("•")),
                        );
                    }
                    markdown::LineKind::Numbered(index) => {
                        row = row.pl(px(14.)).child(
                            div()
                                .pr(px(6.))
                                .text_color(theme.text_tertiary)
                                .child(SharedString::from(format!("{index}."))),
                        );
                    }
                    markdown::LineKind::Quote => {
                        row = row
                            .pl(px(10.))
                            .border_l_2()
                            .border_color(theme.border_strong)
                            .text_color(theme.text_secondary);
                    }
                    markdown::LineKind::Text => {}
                }
                for span in line.spans {
                    row = row.child(render_span(&span, theme));
                }
                row.into_any_element()
            }
            markdown::Block::Code { lang, code } => {
                let mut card = div()
                    .w_full()
                    .my(px(4.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.inset)
                    .overflow_hidden();
                card = card.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(10.))
                        .py(px(5.))
                        .border_b_1()
                        .border_color(theme.border)
                        .text_size(px(10.5))
                        .text_color(theme.text_ghost)
                        .child(SharedString::from(
                            lang.clone().unwrap_or_else(|| "text".into()),
                        )),
                );
                let mut body = div()
                    .flex()
                    .flex_col()
                    .px(px(10.))
                    .py(px(8.))
                    .font_family("monospace");
                let syntect_theme = theme.syntect();
                for line in super::code::highlight(&code, lang.as_deref(), &syntect_theme) {
                    let mut row = div()
                        .w_full()
                        .flex()
                        .text_size(px(11.5))
                        .text_color(theme.text);
                    for (color, text) in line {
                        row = row.child(match color {
                            Some(rgb) => div()
                                .text_color(theme.rgb(rgb))
                                .child(SharedString::from(text)),
                            None => div().child(SharedString::from(text)),
                        });
                    }
                    body = body.child(row);
                }
                card.child(body).into_any_element()
            }
            markdown::Block::Rule => div()
                .w_full()
                .h(px(1.))
                .my(px(8.))
                .bg(theme.border)
                .into_any_element(),
        };
        column = column.child(element);
    }
    column.into_any_element()
}

/// 行内片段 → 元素。
fn render_span(span: &markdown::Span, theme: &Theme) -> AnyElement {
    let mut element = div().child(SharedString::from(span.text.clone()));
    if span.style.bold {
        element = element.font_weight(FontWeight::BOLD);
    }
    if span.style.italic {
        element = element.italic();
    }
    if span.style.code {
        element = element
            .px(px(4.))
            .rounded(px(4.))
            .bg(theme.code_wash)
            .text_color(theme.code_text);
    } else if span.style.link {
        element = element.text_color(theme.link);
    }
    element.into_any_element()
}
