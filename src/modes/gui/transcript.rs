//! transcript：对话块模型 + 内核事件 → 块的映射 + 渲染。
//!
//! 事件形状与 TUI 消费的完全一致（`message_start` / `message_update` /
//! `tool_execution_start|update|end` / `agent_settled` …），这里只做 GUI 需要的投影。

use super::app::GuiApp;
use super::components::dot;
use super::markdown;
use super::theme::{Theme, px};
use crate::core::provider::{AgentMessage, ContentBlock};
use gpui::{
    AnyElement, Context, FontStyle, FontWeight, HighlightStyle, Hsla, IntoElement, ParentElement,
    SharedString, Styled, StyledText, div, prelude::*,
};
use serde_json::Value;
use std::collections::HashMap;

/// 工具输出收起时显示的行数。
const COLLAPSED_OUTPUT_LINES: usize = 6;
/// 工具输出展开时显示的行数上限。
const EXPANDED_OUTPUT_LINES: usize = 200;
/// 工具输出每一行最多保留的字符数。
const OUTPUT_LINE_CHARS: usize = 160;

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
    /// 正文的解析结果缓存
    body: Body,
}

/// 助手正文的渲染缓存：解析 + 语法高亮只在正文或明暗主题变化时重做。
///
/// 每帧都把整段正文重新喂给 pulldown-cmark / syntect 会让「敲一个字」变成 O(整段对话)，
/// 这是输入卡顿的主因，故缓存成预解析的行。
#[derive(Debug, Default)]
struct Body {
    /// 解析时的正文字节长度（正文只追加，长度变了就重解析）
    len: usize,
    /// 解析时的明暗主题：换主题要按新配色重算代码高亮
    dark: bool,
    /// 预解析好的行
    rows: Vec<Row>,
}

/// 预解析的一行。
#[derive(Debug)]
enum Row {
    /// 普通 Markdown 行（段落/标题/列表/引用）
    Line(markdown::Line),
    /// 围栏代码块，已按主题高亮成 `(颜色, 文本)` 片段
    Code {
        /// 语言标签（None 显示 `text`）
        lang: Option<String>,
        /// 每行的着色片段
        lines: Vec<Vec<(Option<crate::utils::color::Rgb>, String)>>,
    },
    /// 分隔线
    Rule,
}

impl Body {
    /// 取（必要时重建）预解析结果。
    fn rows(&mut self, text: &str, theme: &Theme) -> &[Row] {
        if self.len != text.len() || self.dark != theme.is_dark {
            self.len = text.len();
            self.dark = theme.is_dark;
            self.rows = parse_rows(text, theme);
        }
        &self.rows
    }
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
    /// 单块展开状态：`None` 跟随全局开关（Ctrl-L），点击卡片切换
    pub expanded: Option<bool>,
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

    /// 当前块列表（可变：渲染时会顺手刷新每块的解析缓存）。
    pub fn blocks_mut(&mut self) -> &mut [Block] {
        &mut self.blocks
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
            expanded: None,
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

/// 工具调用的入参颜色：shell 类用代码色，其余用次要色。
fn tool_arg_color(name: &str, theme: &Theme) -> Hsla {
    match name {
        "bash" | "shell" | "execute" | "sh" => theme.code_text,
        _ => theme.text_tertiary,
    }
}

/// 工具输出的着色行：diff 的增/删/块头各有颜色，其余用次要色。
fn line_color(line: &str, theme: &Theme) -> Hsla {
    if line.starts_with("@@") || line.starts_with("diff ") || line.starts_with("index ") {
        theme.link
    } else if line.starts_with('+') && !line.starts_with("+++") {
        theme.success
    } else if line.starts_with('-') && !line.starts_with("---") {
        theme.danger
    } else {
        theme.text_secondary
    }
}

/// 工具输出正文：逐行着色，超过 `max` 行截断（按行截断而不是裁剪像素，
/// 被裁掉的部分不计入布局高度，会盖住输入卡片）。
fn output_body(text: &str, theme: &Theme, max: usize) -> AnyElement {
    let mut column = div()
        .w_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .text_size(px(11.5));
    let mut lines = text.trim().lines();
    for line in lines.by_ref().take(max) {
        let shown = truncate(line, OUTPUT_LINE_CHARS);
        column = column.child(
            div()
                .w_full()
                .truncate()
                .text_color(line_color(line, theme))
                .child(SharedString::from(shown)),
        );
    }
    if lines.next().is_some() {
        column = column.child(
            div()
                .text_color(theme.text_ghost)
                .child(SharedString::from("…")),
        );
    }
    column.into_any_element()
}

/// 单个块的渲染；`index` 是块下标（点击工具卡片切换展开时要回查）。
pub fn render_block(
    block: &mut Block,
    theme: &Theme,
    expand_tools: bool,
    index: usize,
    cx: &mut Context<GuiApp>,
) -> AnyElement {
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
            let rows = assistant.body.rows(&assistant.text, theme);
            column.child(render_rows(rows, theme)).into_any_element()
        }
        // 工具调用：带描边的小卡片，状态用圆点表示
        Block::Tool(tool) => {
            let (color, label) = match tool.status {
                ToolStatus::Running => (theme.warning, "运行中"),
                ToolStatus::Ok => (theme.success, "完成"),
                ToolStatus::Failed => (theme.danger, "失败"),
            };
            let expanded = tool.expanded.unwrap_or(expand_tools);
            let mut card = div()
                .id(("tool", index))
                .w_full()
                .rounded(px(8.))
                .border_1()
                .border_color(theme.border)
                .bg(theme.inset)
                .flex()
                .flex_col()
                .gap(px(6.))
                .px(px(12.))
                .py(px(8.))
                .cursor_pointer()
                .hover(|style| style.border_color(theme.border_strong))
                // 点卡片展开/收起这一块的输出
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_tool(index, cx)));
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
                            // shell 类调用用代码色，跟普通工具区分开
                            .text_color(tool_arg_color(&tool.name, theme))
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
                // 按行截断而不是靠 `max_h` 裁剪：被裁掉的那部分不计入布局高度，
                // 会盖在输入卡片上，也会让钉底算错。
                let lines = if expanded {
                    EXPANDED_OUTPUT_LINES
                } else {
                    COLLAPSED_OUTPUT_LINES
                };
                card = card.child(output_body(&tool.output, theme, lines));
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

/// Markdown 文本 → 预解析行（代码块的高亮只在解析时做一次）。
fn parse_rows(text: &str, theme: &Theme) -> Vec<Row> {
    let syntect_theme = theme.syntect();
    markdown::parse(text)
        .into_iter()
        .map(|block| match block {
            markdown::Block::Line(line) => Row::Line(line),
            markdown::Block::Code { lang, code } => Row::Code {
                lines: super::code::highlight(&code, lang.as_deref(), &syntect_theme),
                lang,
            },
            markdown::Block::Rule => Row::Rule,
        })
        .collect()
}

/// 预解析行 → 元素（行内样式、标题字号、代码块的逐行着色）。
fn render_rows(rows: &[Row], theme: &Theme) -> AnyElement {
    let mut column = div().w_full().flex().flex_col().gap(px(4.));
    for row in rows {
        let element: AnyElement = match row {
            Row::Line(line) => {
                let mut element = div().w_full().text_size(px(13.5)).text_color(theme.text);
                let marker = match line.kind {
                    markdown::LineKind::Heading(level) => {
                        let size = match level {
                            1 => 19.,
                            2 => 17.,
                            3 => 15.5,
                            _ => 14.,
                        };
                        element = element
                            .mt(px(6.))
                            .text_size(px(size))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.text);
                        None
                    }
                    markdown::LineKind::Bullet(depth) => {
                        element = element.pl(px(14. * (depth as f32 + 1.)));
                        Some("• ".to_string())
                    }
                    markdown::LineKind::Numbered(index) => {
                        element = element.pl(px(14.));
                        Some(format!("{index}. "))
                    }
                    markdown::LineKind::Quote => {
                        element = element
                            .pl(px(10.))
                            .border_l_2()
                            .border_color(theme.border_strong)
                            .text_color(theme.text_secondary);
                        None
                    }
                    markdown::LineKind::Text => None,
                };
                element
                    .child(inline_text(marker.as_deref(), &line.spans, theme))
                    .into_any_element()
            }
            Row::Code { lang, lines } => {
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
                for line in lines {
                    let mut element = div()
                        .w_full()
                        .flex()
                        .text_size(px(11.5))
                        .text_color(theme.text);
                    for (color, text) in line {
                        element = element.child(match color {
                            Some(rgb) => div()
                                .text_color(theme.rgb(*rgb))
                                .child(SharedString::from(text.clone())),
                            None => div().child(SharedString::from(text.clone())),
                        });
                    }
                    body = body.child(element);
                }
                card.child(body).into_any_element()
            }
            Row::Rule => div()
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

/// 一行的行内片段 → 单个文本元素。
///
/// 一个片段一个 `div` 是不行的：每个 `div` 都是独立的块，行内代码会被排成
/// 单独一行并撑满宽度，正文也就断在半句上。行内样式只能靠 `StyledText` 的
/// 分段高亮表达，标记（`•` / `1.`）也拼进同一段文本里才能跟着首行缩进。
fn inline_text(marker: Option<&str>, spans: &[markdown::Span], theme: &Theme) -> StyledText {
    let mut text = String::new();
    let mut highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = Vec::new();
    let mut push = |text: &mut String, body: &str, style: HighlightStyle| {
        let start = text.len();
        text.push_str(body);
        if style != HighlightStyle::default() {
            highlights.push((start..text.len(), style));
        }
    };
    if let Some(marker) = marker {
        push(
            &mut text,
            marker,
            HighlightStyle {
                color: Some(theme.text_tertiary),
                ..Default::default()
            },
        );
    }
    for span in spans {
        push(&mut text, &span.text, span_highlight(&span.style, theme));
    }
    StyledText::new(text).with_highlights(highlights)
}

/// 行内片段样式 → 文本高亮；默认前景色留给外层容器的文字样式。
fn span_highlight(style: &markdown::SpanStyle, theme: &Theme) -> HighlightStyle {
    let (color, background_color) = if style.code {
        (Some(theme.code_text), Some(theme.code_wash))
    } else if style.link {
        (Some(theme.link), None)
    } else {
        (None, None)
    };
    HighlightStyle {
        color,
        background_color,
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单行过长时按字符截断并补省略号。
    #[test]
    fn truncate_caps_long_lines() {
        let long = truncate(&"x".repeat(OUTPUT_LINE_CHARS + 5), OUTPUT_LINE_CHARS);
        assert_eq!(long.chars().count(), OUTPUT_LINE_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    /// diff 行按增/删/块头着色，普通行用次要色。
    #[test]
    fn diff_lines_get_their_own_colors() {
        let theme = Theme::dark();
        assert_eq!(line_color("+新增", &theme), theme.success);
        assert_eq!(line_color("-删除", &theme), theme.danger);
        assert_eq!(line_color("@@ -1,2 +1,3 @@", &theme), theme.link);
        assert_eq!(line_color(" 上下文", &theme), theme.text_secondary);
        // `+++` / `---` 是文件头，不算增删行
        assert_eq!(line_color("+++ b/a.rs", &theme), theme.text_secondary);
        assert_eq!(line_color("--- a/a.rs", &theme), theme.text_secondary);
    }

    /// shell 类工具的入参用代码色高亮。
    #[test]
    fn shell_calls_use_code_color() {
        let theme = Theme::dark();
        assert_eq!(tool_arg_color("bash", &theme), theme.code_text);
        assert_eq!(tool_arg_color("edit", &theme), theme.text_tertiary);
    }
}
