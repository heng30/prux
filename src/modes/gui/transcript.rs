//! transcript：对话块模型 + 内核事件 → 块的映射 + 渲染。
//!
//! 事件形状与 TUI 消费的完全一致（`message_start` / `message_update` /
//! `tool_execution_start|update|end` / `agent_settled` …），这里只做 GUI 需要的投影。

use super::{
    app::GuiApp,
    code,
    components::dot,
    markdown,
    theme::{
        DOT_SIZE, FONT_BODY, FONT_CAPTION, FONT_SMALL, RADIUS_MD, SPACE_LG, SPACE_MD, SPACE_SM,
        SPACE_XS, SPACE_XXS, Theme, px,
    },
};
use crate::{
    core::provider::{AgentMessage, ContentBlock},
    utils::{color::Rgb, display::fit_display, mime, paths},
};
use base64::Engine as _;
use gpui::{
    AnyElement, Bounds, Context, FontStyle, FontWeight, HighlightStyle, Hsla, IntoElement,
    ParentElement, Pixels, SharedString, Styled, StyledText, div, prelude::*,
};
use serde_json::Value;
use std::{cell::RefCell, collections::HashMap, ops::Range, path::PathBuf, rc::Rc};

/// 工具输出收起时显示的行数。
const COLLAPSED_OUTPUT_LINES: usize = 6;
/// 工具输出展开时显示的行数上限。
const EXPANDED_OUTPUT_LINES: usize = 200;
/// 工具输出每一行最多保留的显示宽度（CJK 字符按 2 算）。
const OUTPUT_LINE_CHARS: usize = 160;
/// 用户气泡的最大宽度（逻辑像素）
const USER_BUBBLE_MAX_WIDTH: f32 = 560.;
/// 用户消息折叠时显示的行数（超过就截断，点击气泡展开）
const USER_COLLAPSED_LINES: usize = 6;
/// 缩略图最大边长（逻辑像素）
const IMAGE_THUMB: f32 = 180.0;

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
        lines: Vec<Vec<(Option<Rgb>, String)>>,
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
    /// 工具返回的图片（落成临时文件后的路径，按返回顺序）
    pub images: Vec<PathBuf>,
}

/// 非对话提示。
#[derive(Debug)]
pub enum Note {
    /// 一般信息（压缩、重试、状态）
    Info(String),
    /// 错误
    Error(String),
}

/// 对话流里的文本选区：两端都是（行序号, 行内字符下标）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextSel {
    /// 起点（按下时落在哪一行哪一列）
    pub anchor: (usize, usize),
    /// 终点（拖动后落在哪一行哪一列）
    pub focus: (usize, usize),
}

impl TextSel {
    /// 起止按文档顺序排好（起点在前）。
    pub fn ordered(self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    /// 这一行被选中的字符区间（`len` 是这一行的字符数）；`None` = 整行都没选中。
    pub fn range_for(self, line: usize, len: usize) -> Option<Range<usize>> {
        let ((first, first_col), (last, last_col)) = self.ordered();
        if line < first || line > last {
            return None;
        }
        let start = if line == first { first_col.min(len) } else { 0 };
        let end = if line == last { last_col.min(len) } else { len };
        (start < end).then_some(start..end)
    }
}

/// 一行可选文本的登记信息。
#[derive(Clone, Debug)]
pub struct LineInfo {
    /// 行序号（渲染顺序，从 0 开始）
    pub index: usize,
    /// 行内纯文本（含列表/引用标记）
    pub text: String,
    /// 该行字号（逻辑像素），点选定位时按它重新排版
    pub font_size: f32,
}

/// 选区覆盖的文本：按行拼接，行间插换行符。
///
/// `lines` 是渲染时收集的（行序号, 行文本），顺序即文档顺序。
pub fn selected_text(sel: TextSel, lines: &[LineInfo]) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        let len = line.text.chars().count();
        if let Some(range) = sel.range_for(line.index, len) {
            out.push(
                line.text
                    .chars()
                    .skip(range.start)
                    .take(range.end - range.start)
                    .collect(),
            );
        }
    }
    out.join("\n")
}

/// 渲染对话流的版式上下文：内容可用宽度 + 选区/行登记。
pub struct FlowCtx<'a> {
    /// 内容可用内宽（逻辑像素）：带 padding 的盒子按它算内部换行宽度
    pub width: Pixels,
    /// 选区高亮与行登记
    pub lines: &'a mut TextLines,
}

/// 渲染时收集的可选文本上下文：行序号、选区高亮、每行的边界与文本。
pub struct TextLines {
    /// 下一行的序号
    pub next: usize,
    /// 当前选区（画成背景高亮）
    pub selection: Option<TextSel>,
    /// 行序号 → 该行元素在窗口里的边界（点选时按坐标反查行）
    pub bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    /// 行序号 → 该行文字元素在窗口里的边界（列定位用：外层行盒含缩进/padding，
    /// 拿它算列号会整体偏移）
    pub text_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    /// 逐行的文本与字号（复制取文本、点选时按字号重新排版定位）
    pub rows: Rc<RefCell<Vec<LineInfo>>>,
}

/// 用户消息块：正文 + 单块展开态（`None` 跟随全局 Ctrl-L，点击气泡切换）。
#[derive(Debug)]
pub struct UserMessage {
    /// 消息正文
    pub text: String,
    /// 单块展开态：`None` 跟随全局开关
    pub expanded: Option<bool>,
}

/// transcript 里的一块。
#[derive(Debug)]
pub enum Block {
    /// 用户消息
    User(UserMessage),
    /// 助手回复
    Assistant(Assistant),
    /// 工具调用
    Tool(Tool),
    /// 提示/错误
    Note(Note),
    /// 系统消息（role=system：内核注入的指令、扩展提示等）
    System(String),
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
        self.blocks.push(Block::User(UserMessage {
            text,
            expanded: None,
        }));
        self.open_assistant = None;
    }

    /// 追加一条提示。
    pub fn push_note(&mut self, note: Note) {
        self.blocks.push(Block::Note(note));
    }

    /// 硬中止（Esc）后的收尾：在跑的工具块改成失败，补一条中断提示。
    ///
    /// 回合 future 被 drop 后不会再有 `tool_execution_end`，不收尾的话工具块会
    /// 永远显示 running。
    pub fn abort(&mut self) {
        self.open_assistant = None;
        for block in &mut self.blocks {
            if let Block::Tool(tool) = block
                && tool.status == ToolStatus::Running
            {
                tool.status = ToolStatus::Failed;
            }
        }
        self.push_note(Note::Error("Operation aborted".to_string()));
    }

    /// 从会话历史重建（切会话后渲染既有对话）。
    pub fn from_messages(messages: &[AgentMessage]) -> Self {
        let mut transcript = Self::default();
        for message in messages {
            match message.role.as_str() {
                "user" => {
                    let text = user_text(message);
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
                "system" => {
                    let text = message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.trim().is_empty() {
                        transcript.blocks.push(Block::System(text));
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
                        tool.images = message
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Image { data, mime_type } => {
                                    save_tool_image(data, mime_type)
                                }
                                _ => None,
                            })
                            .collect();
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
                } else if role == "system" {
                    // 系统消息不流式：开头就把正文取出来
                    let text = event
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(Value::as_array)
                        .map(|content| {
                            content
                                .iter()
                                .filter_map(|c| c.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    if !text.trim().is_empty() {
                        self.blocks.push(Block::System(text));
                    }
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
            "message_end" => {
                let Some(message) = event.get("message") else {
                    return;
                };
                let Ok(message) = serde_json::from_value::<AgentMessage>(message.clone()) else {
                    return;
                };
                if message.role != "user" {
                    return;
                }
                // 排队（steer / follow-up）的消息不本地回显，由 agent 回合开始时的
                // message_end 推入；本地已推过同文本就不重复渲染
                let text = user_text(&message);
                if text.is_empty()
                    || matches!(self.blocks.last(), Some(Block::User(prev)) if prev.text == text)
                {
                    return;
                }
                self.push_user(text);
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
                // 结果里的图片单独落盘，随后按缩略图渲染
                let images: Vec<std::path::PathBuf> = event
                    .get("result")
                    .and_then(|r| r.get("content"))
                    .and_then(Value::as_array)
                    .map(|content| {
                        content
                            .iter()
                            .filter(|c| c.get("type").and_then(Value::as_str) == Some("image"))
                            .filter_map(|c| {
                                let data = c.get("data").and_then(Value::as_str)?;
                                let mime = c
                                    .get("mimeType")
                                    .and_then(Value::as_str)
                                    .unwrap_or("image/png");
                                save_tool_image(data, mime)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(Block::Tool(tool)) = self.tool_mut(id) {
                    if !output.is_empty() {
                        tool.output = output;
                    }
                    if !images.is_empty() {
                        tool.images = images;
                    }
                    tool.status = if failed {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Ok
                    };
                }
            }
            "compaction_start" => self.push_note(Note::Info("Compacting context…".to_string())),
            "compaction_end" => self.push_note(Note::Info("Context compacted".to_string())),
            "auto_retry_start" => {
                let attempt = event
                    .get("attempt")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                self.push_note(Note::Info(format!("Request failed, retry #{attempt}…")));
            }
            "error" => {
                let message = event
                    .get("message")
                    .or_else(|| event.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
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
            images: Vec::new(),
        }));
        let index = self.blocks.len() - 1;
        if !id.is_empty() {
            self.tools.insert(id.to_string(), index);
        }
    }

    /// 最后一条助手回复的正文（`/copy` 用）。
    pub fn last_assistant_text(&self) -> String {
        self.blocks
            .iter()
            .rev()
            .find_map(|block| match block {
                Block::Assistant(assistant) => Some(assistant.text.clone()),
                _ => None,
            })
            .unwrap_or_default()
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
    fit_display(&text, 120)
}

/// 用户消息的正文：只取 text 块，按行拼接（图片块不回显）。
fn user_text(message: &AgentMessage) -> String {
    tool_result_text(&message.content)
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
/// 被裁掉的部分不计入布局高度，会盖住输入卡片），剩下的行数用与用户气泡同样式的提示标出。
fn output_body(text: &str, theme: &Theme, max: usize, expanded: bool) -> AnyElement {
    let mut column = div()
        .w_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .text_size(px(FONT_SMALL));
    let lines = text.trim().lines();
    let total = lines.clone().count();
    for line in lines.take(max) {
        let shown = crate::utils::display::fit_display(line, OUTPUT_LINE_CHARS);
        column = column.child(
            div()
                .w_full()
                .truncate()
                .text_color(line_color(line, theme))
                .child(SharedString::from(shown)),
        );
    }

    if let Some(hint) = output_hint(total.saturating_sub(max), expanded) {
        column = column.child(
            div()
                .text_size(px(FONT_CAPTION))
                .text_color(theme.text_ghost)
                .child(SharedString::from(hint)),
        );
    }
    column.into_any_element()
}

/// 被截掉的输出行提示：`hidden` 为没显示出来的行数，与用户消息气泡的折叠提示一致。
///
/// 没有剩余时不提示；已展开时只说还剩多少行（展开已经到底，再点只会收起）。
fn output_hint(hidden: usize, expanded: bool) -> Option<String> {
    (hidden > 0).then(|| {
        if expanded {
            format!("⋯ {hidden} more line(s)")
        } else {
            format!("⋯ {hidden} more line(s) · click to expand")
        }
    })
}

/// 把 base64 图片写成临时文件，供 `gpui::img` 读取；解码失败返回 None。
fn save_tool_image(data: &str, mime_type: &str) -> Option<PathBuf> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()?;
    let ext = match mime_type.rsplit('/').next()? {
        "jpeg" | "jpg" => "jpg",
        "png" => "png",
        "gif" => "gif",
        "webp" => "webp",
        "bmp" => "bmp",
        _ => return None,
    };

    let dir = std::env::temp_dir().join("prux-gui-images");
    std::fs::create_dir_all(&dir).ok()?;

    // 名字用内容长度 + 首字节凑合：同一张图重复出现时复用同一个文件
    let name = format!(
        "{}-{:x}.{ext}",
        bytes.len(),
        bytes.first().copied().unwrap_or(0)
    );

    let path = dir.join(name);
    if !path.exists() {
        std::fs::write(&path, &bytes).ok()?;
    }

    Some(path)
}

/// 从消息里挑出可显示的图片引用：`@路径` 且扩展名是图片、文件确实存在。
fn image_refs(text: &str, cwd: &str) -> Vec<PathBuf> {
    text.split_whitespace()
        .filter_map(|token| token.strip_prefix('@'))
        .map(|raw| {
            let raw = raw.trim_matches(|c: char| c == '，' || c == ',');
            paths::resolve_path(raw, cwd)
        })
        .filter(|path| path.is_file() && mime::is_image_path(path))
        .collect()
}

/// 用户消息该显示哪些行：收起时只留前 `max` 行，展开时给全部行。
///
/// 返回 (要显示的行, 被折起来的行数)；`hidden` 只用来决定提示文案，与展开态无关。
fn user_preview(text: &str, expanded: bool, max: usize) -> (Vec<&str>, usize) {
    let lines: Vec<&str> = text.split('\n').collect();
    let hidden = lines.len().saturating_sub(max);
    let shown = if expanded {
        lines
    } else if hidden > 0 {
        lines[..max].to_vec()
    } else {
        lines
    };
    (shown, hidden)
}

/// 单个块的渲染；`index` 是块下标（点击工具卡片切换展开时要回查），
/// `flags` 里的 `(expand_tools, show_thinking)` 对应全局 Ctrl-O / Ctrl-T 开关。
pub fn render_block(
    block: &mut Block,
    theme: &Theme,
    flags: (bool, bool),
    index: usize,
    cwd: &str,
    ctx: &mut FlowCtx<'_>,
    cx: &mut Context<GuiApp>,
) -> AnyElement {
    let (expand_tools, show_thinking) = flags;
    match block {
        // 用户消息：右对齐气泡，与助手正文拉开层级
        Block::User(message) => user_block(message, theme, expand_tools, index, cwd, ctx.width, cx),
        // 助手回复：不加卡片，直接正文；思考内容单独一段细边框提示（Ctrl-T 收起）
        Block::Assistant(assistant) => assistant_block(assistant, theme, show_thinking, ctx),
        // 工具调用：带描边的小卡片，状态用圆点表示
        Block::Tool(tool) => tool_block(tool, theme, expand_tools, index, cx),
        // 系统消息：灰底细边框 + 标签，和用户/助手区分开
        Block::System(text) => system_block(text, theme, ctx.width),
        // 提示 / 错误：一行小字 + 记号
        Block::Note(note) => note_block(note, theme),
    }
}

/// 用户消息气泡：右对齐，长消息可点开，`@图片` 引用渲染成缩略图。
fn user_block(
    message: &UserMessage,
    theme: &Theme,
    expand_tools: bool,
    index: usize,
    cwd: &str,
    width: Pixels,
    cx: &mut Context<GuiApp>,
) -> AnyElement {
    let text = &message.text;
    let expanded = message.expanded.unwrap_or(expand_tools);
    let (shown, hidden) = user_preview(text, expanded, USER_COLLAPSED_LINES);
    // 宽度显式给出：带 padding 的盒子里，文本实测用的宽度和绘制用的不一致，
    // 会少算换行高度、底部被截
    let bubble_w = width.min(px(USER_BUBBLE_MAX_WIDTH));
    let inner_w = bubble_w - px(SPACE_MD * 2.);
    let mut bubble = div()
        .id(("user", index))
        .w(bubble_w)
        .min_w_0()
        .px(px(SPACE_MD))
        .py(px(SPACE_SM))
        .rounded(px(RADIUS_MD))
        .bg(theme.raised)
        .flex()
        .flex_col()
        .gap(px(SPACE_SM))
        .text_size(px(FONT_BODY))
        .text_color(theme.text)
        .cursor_pointer()
        .hover(|style| style.bg(theme.overlay_strong))
        // 点气泡展开/收起长消息
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_user(index, cx)))
        .child(
            div()
                .w(inner_w)
                .min_w_0()
                .child(SharedString::from(shown.join("\n"))),
        );

    if hidden > 0 {
        bubble = bubble.child(
            div()
                .text_size(px(FONT_CAPTION))
                .text_color(theme.text_ghost)
                .child(SharedString::from(if expanded {
                    "click to collapse".to_string()
                } else {
                    format!("⋯ {hidden} more line(s) · click to expand")
                })),
        );
    }

    // `@图片` 引用直接渲染成缩略图
    let images = image_refs(text, cwd);
    for path in images {
        bubble = bubble.child(
            div()
                .max_w(px(IMAGE_THUMB))
                .max_h(px(IMAGE_THUMB))
                .rounded(px(RADIUS_MD))
                .overflow_hidden()
                .child(
                    gpui::img(path)
                        .max_w(px(IMAGE_THUMB))
                        .max_h(px(IMAGE_THUMB)),
                ),
        );
    }

    div()
        .w_full()
        .flex()
        .justify_end()
        .pt(px(SPACE_XS))
        .child(bubble)
        .into_any_element()
}

/// 助手回复：正文 + 折叠的思考段（`show_thinking` 为假时整段不渲染，同 TUI 的 Ctrl-T）。
fn assistant_block(
    assistant: &mut Assistant,
    theme: &Theme,
    show_thinking: bool,
    ctx: &mut FlowCtx<'_>,
) -> AnyElement {
    let mut column = div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(SPACE_SM))
        .pt(px(SPACE_XXS));

    if show_thinking && !assistant.thinking.trim().is_empty() {
        column = column.child(
            div()
                .pl(px(SPACE_SM))
                .border_l_2()
                .border_color(theme.border_strong)
                .text_size(px(FONT_SMALL))
                .text_color(theme.text_tertiary)
                .child(SharedString::from(crate::utils::display::fit_display(
                    assistant.thinking.trim(),
                    300,
                ))),
        );
    }

    let rows = assistant.body.rows(&assistant.text, theme);
    column
        .child(render_rows(rows, theme, ctx.lines))
        .into_any_element()
}

/// 工具调用卡片：状态圆点 + 入参 + 可展开的输出与图片。
fn tool_block(
    tool: &Tool,
    theme: &Theme,
    expand_tools: bool,
    index: usize,
    cx: &mut Context<GuiApp>,
) -> AnyElement {
    let (color, label) = match tool.status {
        ToolStatus::Running => (theme.warning, "running"),
        ToolStatus::Ok => (theme.success, "done"),
        ToolStatus::Failed => (theme.danger, "failed"),
    };

    let expanded = tool.expanded.unwrap_or(expand_tools);
    let mut card = div()
        .id(("tool", index))
        .w_full()
        .rounded(px(RADIUS_MD))
        .border_1()
        .border_color(theme.border)
        .bg(theme.inset)
        .flex()
        .flex_col()
        .gap(px(SPACE_XS))
        .px(px(SPACE_MD))
        .py(px(SPACE_SM))
        .cursor_pointer()
        .hover(|style| style.border_color(theme.border_strong))
        // 点卡片展开/收起这一块的输出
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_tool(index, cx)));

    card = card.child(
        div()
            .flex()
            .items_center()
            .gap(px(SPACE_SM))
            .child(dot(color, DOT_SIZE))
            .child(
                div()
                    .text_size(px(FONT_SMALL))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(tool.name.clone())),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(px(FONT_SMALL))
                    // shell 类调用用代码色，跟普通工具区分开
                    .text_color(tool_arg_color(&tool.name, theme))
                    .child(SharedString::from(tool.args.clone())),
            )
            .child(
                div()
                    .text_size(px(FONT_CAPTION))
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
        card = card.child(output_body(&tool.output, theme, lines, expanded));
    }

    for image in &tool.images {
        card = card.child(
            div()
                .max_w(px(IMAGE_THUMB))
                .max_h(px(IMAGE_THUMB))
                .rounded(px(RADIUS_MD))
                .overflow_hidden()
                .child(
                    gpui::img(image.clone())
                        .max_w(px(IMAGE_THUMB))
                        .max_h(px(IMAGE_THUMB)),
                ),
        );
    }
    card.into_any_element()
}

/// 系统消息：灰底细边框 + `system` 标签，逐行渲染。
fn system_block(text: &str, theme: &Theme, width: Pixels) -> AnyElement {
    div()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .rounded(px(RADIUS_MD))
        .border_1()
        .border_color(theme.border)
        .bg(theme.inset)
        .px(px(SPACE_MD))
        .py(px(SPACE_SM))
        .flex()
        .flex_col()
        .gap(px(SPACE_XS))
        .child(
            div()
                .text_size(px(FONT_CAPTION))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_ghost)
                .child(SharedString::from("system")),
        )
        .child(
            div()
                .w(width - px(24.))
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(SPACE_XXS))
                .text_size(px(FONT_SMALL))
                .text_color(theme.text_tertiary)
                .children(text.split('\n').map(|line| {
                    div()
                        .w_full()
                        .min_w_0()
                        .child(SharedString::from(line.to_string()))
                })),
        )
        .into_any_element()
}

/// 提示 / 错误行：`·` 或 `!` 记号 + 逐行正文。
fn note_block(note: &Note, theme: &Theme) -> AnyElement {
    let (text, color, mark) = match note {
        Note::Info(text) => (text, theme.text_tertiary, "·"),
        Note::Error(text) => (text, theme.danger, "!"),
    };

    // 系统提示常是多行（命令输出、会话树）：逐行成子元素，不能当一个长行挤
    div()
        .w_full()
        .flex()
        .items_start()
        .gap(px(SPACE_SM))
        .px(px(SPACE_XXS))
        .child(
            div()
                .text_size(px(FONT_SMALL))
                .text_color(color)
                .child(SharedString::from(mark)),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .text_size(px(FONT_SMALL))
                .text_color(color)
                .children(text.split('\n').map(|line| {
                    div()
                        .w_full()
                        .min_w_0()
                        .child(SharedString::from(line.to_string()))
                })),
        )
        .into_any_element()
}

/// Markdown 文本 → 预解析行（代码块的高亮只在解析时做一次）。
fn parse_rows(text: &str, theme: &Theme) -> Vec<Row> {
    let syntect_theme = theme.syntect();
    markdown::parse(text)
        .into_iter()
        .map(|block| match block {
            markdown::Block::Line(line) => Row::Line(line),
            markdown::Block::Code { lang, code } => Row::Code {
                lines: code::highlight(&code, lang.as_deref(), &syntect_theme),
                lang,
            },
            markdown::Block::Rule => Row::Rule,
        })
        .collect()
}

/// 预解析行 → 元素（行内样式、标题字号、代码块的逐行着色）。
fn render_rows(rows: &[Row], theme: &Theme, lines: &mut TextLines) -> AnyElement {
    let mut column = div().w_full().flex().flex_col().gap(px(SPACE_XS));
    // 子元素下标 → 行序号（规则线之类不参与选区的行记 None）
    let mut child_lines: Vec<Option<usize>> = Vec::new();
    for row in rows {
        let element: AnyElement = match row {
            Row::Line(line) => {
                let mut font_size = 13.5_f32;
                let mut element = div()
                    .w_full()
                    .text_size(px(font_size))
                    .text_color(theme.text);
                let marker = match line.kind {
                    markdown::LineKind::Heading(level) => {
                        let size = match level {
                            1 => 19.,
                            2 => 17.,
                            3 => 15.5,
                            _ => 14.,
                        };
                        font_size = size;
                        element = element
                            .mt(px(SPACE_XS))
                            .text_size(px(size))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.text);
                        None
                    }
                    markdown::LineKind::Bullet(depth) => {
                        element = element.pl(px(SPACE_LG * (depth as f32 + 1.)));
                        Some("• ".to_string())
                    }
                    markdown::LineKind::Numbered(index) => {
                        element = element.pl(px(SPACE_MD));
                        Some(format!("{index}. "))
                    }
                    markdown::LineKind::Quote => {
                        element = element
                            .pl(px(SPACE_SM))
                            .border_l_2()
                            .border_color(theme.border_strong)
                            .text_color(theme.text_secondary);
                        None
                    }
                    markdown::LineKind::Text => None,
                };
                // 这一行参与选区：登记序号/文本，并把选区画成背景高亮
                let index = lines.next;
                lines.next += 1;
                let (plain, _) = inline_runs(marker.as_deref(), &line.spans, theme);
                let len = plain.chars().count();
                let selected = lines
                    .selection
                    .and_then(|selection| selection.range_for(index, len));
                lines.rows.borrow_mut().push(LineInfo {
                    index,
                    text: plain,
                    font_size,
                });
                child_lines.push(Some(index));
                let text_bounds = lines.text_bounds.clone();
                element
                    .child(inline_text(marker.as_deref(), &line.spans, theme, selected))
                    // 文字元素的实测边界：列定位按它算，外层行盒的 padding/缩进不进列号
                    .on_children_prepainted(move |children: Vec<Bounds<Pixels>>, _, _| {
                        if let Some(child) = children.first() {
                            text_bounds.borrow_mut().insert(index, *child);
                        }
                    })
                    .into_any_element()
            }
            Row::Code { lang, lines: code } => {
                let mut card = div()
                    .w_full()
                    .my(px(SPACE_XS))
                    .rounded(px(RADIUS_MD))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.inset)
                    .overflow_hidden();
                card = card.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(SPACE_SM))
                        .py(px(SPACE_XS))
                        .border_b_1()
                        .border_color(theme.border)
                        .text_size(px(FONT_CAPTION))
                        .text_color(theme.text_ghost)
                        .child(SharedString::from(
                            lang.clone().unwrap_or_else(|| "text".into()),
                        )),
                );

                let mut body = div()
                    .flex()
                    .flex_col()
                    .px(px(SPACE_SM))
                    .py(px(SPACE_SM))
                    .font_family("monospace");

                for line in code {
                    let mut element = div()
                        .w_full()
                        .flex()
                        .text_size(px(FONT_SMALL))
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

                child_lines.push(None);
                card.child(body).into_any_element()
            }
            Row::Rule => {
                child_lines.push(None);
                div()
                    .w_full()
                    .h(px(1.))
                    .my(px(SPACE_SM))
                    .bg(theme.border)
                    .into_any_element()
            }
        };
        column = column.child(element);
    }

    let bounds = lines.bounds.clone();
    column = column.on_children_prepainted(move |children, _, _| {
        let mut map = bounds.borrow_mut();
        for (child, line) in children.iter().zip(child_lines.iter()) {
            if let Some(line) = line {
                map.insert(*line, *child);
            }
        }
    });
    column.into_any_element()
}

/// 一行的行内片段 → 单个文本元素。
///
/// 一个片段一个 `div` 是不行的：每个 `div` 都是独立的块，行内代码会被排成
/// 单独一行并撑满宽度，正文也就断在半句上。行内样式只能靠 `StyledText` 的
/// 分段高亮表达，标记（`•` / `1.`）也拼进同一段文本里才能跟着首行缩进。
fn inline_text(
    marker: Option<&str>,
    spans: &[markdown::Span],
    theme: &Theme,
    selection: Option<Range<usize>>,
) -> StyledText {
    let (text, mut highlights) = inline_runs(marker, spans, theme);
    if let Some(range) = selection {
        let byte = char_range_to_bytes(&text, range);
        if !byte.is_empty() {
            highlights.push((
                byte,
                HighlightStyle {
                    background_color: Some(theme.selection),
                    ..Default::default()
                },
            ));
        }
    }
    StyledText::new(text).with_highlights(highlights)
}

/// 行内片段 → (纯文本, 分段高亮)；纯文本用来复制与按字符定位。
fn inline_runs(
    marker: Option<&str>,
    spans: &[markdown::Span],
    theme: &Theme,
) -> (String, Vec<(Range<usize>, HighlightStyle)>) {
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
    (text, highlights)
}

/// 字符下标区间 → 字节区间（越界自动夹住）。
fn char_range_to_bytes(text: &str, range: Range<usize>) -> Range<usize> {
    let start = text
        .char_indices()
        .nth(range.start)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len());
    let end = text
        .char_indices()
        .nth(range.end)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len());
    start..end.max(start)
}

/// 行内片段样式 → 文本高亮；默认前景色留给外层容器的文字样式。
///
/// 行内代码只上色不画底色：gpui 的高亮底色铺满整个行盒（行高 1.32em），
/// 比字形高一截、文字看着偏下，折行时相邻行的底色还会连成一片。
fn span_highlight(style: &markdown::SpanStyle, theme: &Theme) -> HighlightStyle {
    let color = if style.code {
        Some(theme.code_text)
    } else if style.link {
        Some(theme.link)
    } else {
        None
    };

    HighlightStyle {
        color,
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    /// 工具输出截断提示：没隐藏行就不提，折叠时提示可点开，展开后只说还剩多少行。
    #[test]
    fn output_hint_counts_hidden_lines() {
        assert_eq!(output_hint(0, false), None);
        assert_eq!(output_hint(0, true), None);
        assert_eq!(
            output_hint(3, false),
            Some("⋯ 3 more line(s) · click to expand".to_string())
        );
        assert_eq!(output_hint(3, true), Some("⋯ 3 more line(s)".to_string()));
    }

    /// Esc 硬中止：跑着的工具要收尾成失败，并补一条中断提示（否则永远显示 running）。
    #[test]
    fn abort_fails_running_tools_and_notes_interruption() {
        let mut transcript = Transcript::default();
        transcript.push_tool("t1", "bash", "ls");
        transcript.abort();
        let Block::Tool(tool) = &transcript.blocks()[0] else {
            panic!("第一块应是工具块");
        };
        assert_eq!(tool.status, ToolStatus::Failed);
        assert!(matches!(
            transcript.blocks().last(),
            Some(Block::Note(Note::Error(text))) if text == "Operation aborted"
        ));
    }

    /// 选区按行切字符区间：跨行、反向拖动、越界都夹住。
    #[test]
    fn selection_ranges_slice_lines() {
        let lines = vec![
            LineInfo {
                index: 0,
                text: "abcdef".to_string(),
                font_size: 13.5,
            },
            LineInfo {
                index: 1,
                text: "ghij".to_string(),
                font_size: 13.5,
            },
            LineInfo {
                index: 2,
                text: "klm".to_string(),
                font_size: 13.5,
            },
        ];
        let forward = TextSel {
            anchor: (0, 2),
            focus: (2, 1),
        };
        assert_eq!(forward.range_for(0, 6), Some(2..6));
        assert_eq!(forward.range_for(1, 4), Some(0..4));
        assert_eq!(forward.range_for(2, 3), Some(0..1));
        assert_eq!(forward.range_for(3, 3), None);
        assert_eq!(selected_text(forward, &lines), "cdef\nghij\nk");

        // 反向拖动与正向等价
        let backward = TextSel {
            anchor: (2, 1),
            focus: (0, 2),
        };
        assert_eq!(backward.ordered(), ((0, 2), (2, 1)));
        assert_eq!(selected_text(backward, &lines), "cdef\nghij\nk");

        // 单击不产生文本
        let click = TextSel {
            anchor: (1, 2),
            focus: (1, 2),
        };
        assert_eq!(selected_text(click, &lines), "");
    }

    /// 字符区间转字节区间：中文按字符算，越界夹到行尾。
    #[test]
    fn char_range_becomes_bytes() {
        let text = "中文abcd";
        assert_eq!(char_range_to_bytes(text, 0..2), 0..6);
        assert_eq!(char_range_to_bytes(text, 2..6), 6..10);
        assert_eq!(char_range_to_bytes(text, 4..99), 8..10);
    }

    /// 展开时必须给全部行，收起只留前几行。
    #[test]
    fn user_preview_expands_to_every_line() {
        let text = "1\n2\n3\n4\n5\n6\n7\n8";
        let (shown, hidden) = user_preview(text, false, 6);
        assert_eq!(shown.len(), 6);
        assert_eq!(hidden, 2);

        let (shown, hidden) = user_preview(text, true, 6);
        assert_eq!(shown.len(), 8);
        assert_eq!(shown.last(), Some(&"8"));
        assert_eq!(hidden, 2);

        // 短消息不收也不折
        let (shown, hidden) = user_preview("a\nb", false, 6);
        assert_eq!(shown.len(), 2);
        assert_eq!(hidden, 0);
    }

    use super::*;

    /// 工具返回的图片：base64 解不开或 MIME 不认识时不落盘。
    #[test]
    fn tool_images_skip_undecodable_data() {
        assert!(save_tool_image("not-base64!!", "image/png").is_none());
        assert!(save_tool_image("AAAA", "application/pdf").is_none());
        // 1x1 PNG 的 base64（真实可解码）
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let path = save_tool_image(png, "image/png").expect("png 应该落盘");
        assert!(path.is_file());
        assert_eq!(path.extension().unwrap(), "png");
    }

    /// 只把存在的图片 `@引用` 挑成缩略图：非图片、找不到的路径都跳过。
    #[test]
    fn image_refs_only_take_existing_images() {
        let dir = std::env::temp_dir().join(format!("prux-gui-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let image = dir.join("a.png");
        std::fs::write(&image, b"not-really-a-png").expect("write");
        let text = format!(
            "看这个 @{} 和 @{}/missing.png",
            image.display(),
            dir.display()
        );
        let refs = image_refs(&text, "/");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0], image);
        // 扩展名不是图片的引用不渲染
        let rs = dir.join("a.rs");
        std::fs::write(&rs, b"").expect("write");
        assert!(image_refs(&format!("@{}", rs.display()), "/").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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
