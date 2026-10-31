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
    utils::{
        color::Rgb,
        display::{fit_display, format_tool_duration},
        mime, paths,
    },
};
use base64::Engine as _;
use gpui::{
    AnyElement, Bounds, Context, FontStyle, FontWeight, HighlightStyle, Hsla, Image, ImageFormat,
    ImageSource, IntoElement, ParentElement, Pixels, SharedString, Styled, StyledText, div,
    prelude::*,
};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Instant, SystemTime},
};

/// 工具输出收起时显示的行数。
const COLLAPSED_OUTPUT_LINES: usize = 6;
/// 工具输出展开时显示的行数上限。
const EXPANDED_OUTPUT_LINES: usize = 200;
/// 用户气泡的最大宽度（逻辑像素）
const USER_BUBBLE_MAX_WIDTH: f32 = 560.;
/// 用户消息折叠时显示的行数（超过就截断，点击气泡展开）
const USER_COLLAPSED_LINES: usize = 6;
/// 缩略图最大边长（逻辑像素）
const IMAGE_THUMB: f32 = 360.0;

thread_local! {
    /// 缩略图图片源缓存：`(路径, (文件长度, 修改时间)) → 图片源`，免掉每帧重读缩略图文件。
    /// `ImageSource` 不是 `Send`，而渲染只在 UI 线程上，故用线程本地变量。
    static IMAGE_SOURCES: RefCell<Vec<(PathBuf, (u64, Option<SystemTime>), ImageSource)>> =
        RefCell::new(Vec::new());
}

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
    /// 目标文件路径（edit / write：卡片头部显示它，参数摘要里不再重复）
    pub path: Option<String>,
    /// edit 工具的 diff（调用阶段是预览，结果回来后换成 `details.diff`）
    pub diff: Option<String>,
    /// write 工具要写入的文件正文（调用阶段折叠预览，同 TUI）
    pub content: Option<String>,
    /// 输出文本
    pub output: String,
    /// 执行状态
    pub status: ToolStatus,
    /// 单块展开状态：`None` 跟随全局开关（Ctrl-L），点击卡片切换
    pub expanded: Option<bool>,
    /// 工具返回的图片（落成临时文件后的路径，按返回顺序）
    pub images: Vec<PathBuf>,
    /// 开始执行的时刻：仅在跑着的调用上有（用来算实时耗时），结束后为 None
    pub started_at: Option<Instant>,
    /// 执行耗时（毫秒）：来自 `tool_execution_end` / `toolResult` 的 `durationMs`，未结束或历史里没有时为 None
    pub duration_ms: Option<u64>,
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
    /// 发送时拍的图片快照（内容命名的临时文件路径）：同路径文件后来被改写，这条消息仍显示当时那张图
    pub images: Vec<PathBuf>,
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
        clear_image_sources();
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
    ///
    /// `@图片` 引用在这里按内容拍快照：同一路径的图后来被改写，已发出的这条仍显示当时那张。
    pub fn push_user(&mut self, text: String, cwd: &str) {
        let images = image_refs(&text, cwd)
            .iter()
            .filter_map(|path| snapshot_image(path))
            .collect();
        self.blocks.push(Block::User(UserMessage {
            text,
            images,
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
                tool.started_at = None;
            }
        }
        self.push_note(Note::Error("Operation aborted".to_string()));
    }

    /// 从会话历史重建（切会话后渲染既有对话）；`cwd` 用于算 edit 的预览 diff。
    pub fn from_messages(messages: &[AgentMessage], cwd: &str) -> Self {
        clear_image_sources();

        let mut transcript = Self::default();
        for message in messages {
            match message.role.as_str() {
                "user" => {
                    let text = user_text(message);
                    if !text.is_empty() {
                        transcript.push_user(text, cwd);
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
                            transcript.push_tool(id, name, arguments, cwd);
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
                        tool.diff = message
                            .details
                            .as_ref()
                            .and_then(|details| details.get("diff"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .or(tool.diff.clone());
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
                        tool.duration_ms = message.duration_ms;
                    }
                }
                _ => {}
            }
        }
        transcript.open_assistant = None;
        transcript
    }

    /// 应用一条内核 JSON 事件；`cwd` 用于算 edit 调用阶段的预览 diff。
    pub fn apply(&mut self, event: &Value, cwd: &str) {
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
                self.push_user(text, cwd);
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
                let args = event.get("args").cloned().unwrap_or(Value::Null);
                self.push_tool(&id, &name, &args, cwd);

                // 耗时从这一刻算：重建历史时不打点，缺 toolResult 的旧调用不会显示假耗时
                if let Some(Block::Tool(tool)) = self.blocks.last_mut() {
                    tool.started_at = Some(Instant::now());
                }
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
                // edit 的最终 diff 在 `result.details.diff`（优先于调用阶段的预览）
                let diff = event
                    .get("result")
                    .and_then(|r| r.get("details"))
                    .and_then(|d| d.get("diff"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(Block::Tool(tool)) = self.tool_mut(id) {
                    if !output.is_empty() {
                        tool.output = output;
                    }
                    if diff.is_some() {
                        tool.diff = diff;
                    }
                    if !images.is_empty() {
                        tool.images = images;
                    }
                    tool.status = if failed {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Ok
                    };
                    tool.duration_ms = event.get("durationMs").and_then(Value::as_u64);
                    tool.started_at = None;
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

    /// 追加一次工具调用：edit 在这里就算好预览 diff（一次文件读，不必每帧重算）。
    fn push_tool(&mut self, id: &str, name: &str, args: &Value, cwd: &str) {
        let path = args
            .get("path")
            .or_else(|| args.get("file_path"))
            .and_then(Value::as_str)
            .map(str::to_string);
        // edit / write 的参数体量很大（整段 edits / content），头部只留路径；
        // shell 类只留命令本身（不带 `command=` 前缀），卡片第二行整行折行显示
        let summary = if is_shell_tool(name) {
            args.get("command")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default()
        } else if matches!(name, "edit" | "write") && path.is_some() {
            String::new()
        } else {
            summarize_args(args)
        };
        let content = (name == "write")
            .then(|| {
                args.get("content")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten();
        let diff = (name == "edit")
            .then(|| {
                crate::core::tools::preview_edit_diff(
                    path.as_deref().unwrap_or(""),
                    args.get("edits").unwrap_or(&Value::Null),
                    cwd,
                )
            })
            .flatten();
        self.blocks.push(Block::Tool(Tool {
            name: name.to_string(),
            args: summary,
            path,
            diff,
            content,
            output: String::new(),
            status: ToolStatus::Running,
            expanded: None,
            images: Vec::new(),
            started_at: None,
            duration_ms: None,
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

/// 空助手块：只有 tool call、没正文也没思考，渲染出来高度为 0。
///
/// 它仍会被流的 `gap` 算一份，相邻块之间会凭空多出一段空白，所以渲染时跳过。
pub fn is_blank_assistant(block: &Block) -> bool {
    matches!(block, Block::Assistant(assistant)
        if assistant.text.trim().is_empty() && assistant.thinking.trim().is_empty())
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

/// 是不是 shell 类工具（命令单独占一行、用代码色）。
fn is_shell_tool(name: &str) -> bool {
    matches!(name, "bash" | "shell" | "execute" | "sh" | "powershell")
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

/// diff 的一行：gutter（前缀 + 行号）、配色与内容片段。
///
/// 片段上的 `true` 是词级差异标记（单行替换时只有变动的 token 为真）。
#[derive(Debug)]
struct DiffRow {
    /// 行号列文本（含 `-`/`+`/空格前缀；块头行直接放整行）
    gutter: String,
    /// 这一行属于哪种改动
    kind: DiffKind,
    /// 内容片段
    spans: Vec<(String, bool)>,
}

/// diff 行的改动类型，决定配色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffKind {
    /// 新增行
    Added,
    /// 删除行
    Removed,
    /// 上下文行 / 块头 / 省略号
    Context,
}

/// 把 diff 文本切成带行号与词级标记的行。
///
/// 恰好 1 删 1 增（单行替换）时做词级 diff，只有变动片段带标记；
/// 其余情况整行内容算一个片段。
fn diff_rows(diff: &str) -> Vec<DiffRow> {
    let raw: Vec<&str> = diff.split('\n').collect();
    let mut out: Vec<DiffRow> = Vec::new();
    let mut i = 0usize;
    while i < raw.len() {
        if diff_prefix(raw[i]) == Some('-') {
            let mut removed: Vec<(&str, &str)> = Vec::new();
            while i < raw.len() && diff_prefix(raw[i]) == Some('-') {
                removed.push(split_diff_line(raw[i]));
                i += 1;
            }
            let mut added: Vec<(&str, &str)> = Vec::new();
            while i < raw.len() && diff_prefix(raw[i]) == Some('+') {
                added.push(split_diff_line(raw[i]));
                i += 1;
            }
            if removed.len() == 1 && added.len() == 1 {
                let (num, content) = removed[0];
                let (anum, acontent) = added[0];
                let (old_spans, new_spans) = intra_line_spans(content, acontent);
                out.push(DiffRow {
                    gutter: format!("-{num}"),
                    kind: DiffKind::Removed,
                    spans: old_spans,
                });
                out.push(DiffRow {
                    gutter: format!("+{anum}"),
                    kind: DiffKind::Added,
                    spans: new_spans,
                });
            } else {
                for (num, content) in removed {
                    out.push(DiffRow {
                        gutter: format!("-{num}"),
                        kind: DiffKind::Removed,
                        spans: vec![(content.to_string(), false)],
                    });
                }
                for (num, content) in added {
                    out.push(DiffRow {
                        gutter: format!("+{num}"),
                        kind: DiffKind::Added,
                        spans: vec![(content.to_string(), false)],
                    });
                }
            }
            continue;
        }

        let line = raw[i];
        i += 1;
        match diff_prefix(line) {
            Some('+') => {
                let (num, content) = split_diff_line(line);
                out.push(DiffRow {
                    gutter: format!("+{num}"),
                    kind: DiffKind::Added,
                    spans: vec![(content.to_string(), false)],
                });
            }
            Some(' ') => {
                let (num, content) = split_diff_line(line);
                out.push(DiffRow {
                    gutter: num.to_string(),
                    kind: DiffKind::Context,
                    spans: vec![(content.to_string(), false)],
                });
            }
            // 块头 / 文件头 / 空行：整行原样，用上下文配色
            _ => out.push(DiffRow {
                gutter: String::new(),
                kind: DiffKind::Context,
                spans: vec![(line.to_string(), false)],
            }),
        }
    }
    out
}

/// diff 行前缀（`-` / `+` / 空格），非 diff 行返回 None。
fn diff_prefix(line: &str) -> Option<char> {
    line.chars()
        .next()
        .filter(|c| *c == ' ' || *c == '-' || *c == '+')
}

/// 解析 `前缀 + 行号(含右对齐空格) + 内容`，返回 (行号串, 内容)。
fn split_diff_line(line: &str) -> (&str, &str) {
    let rest = line.get(1..).unwrap_or("");
    let bytes = rest.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() && bytes[idx] == b' ' {
        idx += 1;
    }
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    let num = &rest[..idx];
    let mut content = &rest[idx..];
    content = content.strip_prefix(' ').unwrap_or(content);
    (num, content)
}

/// 词级 diff 的一侧：`(片段, 是否变动)` 列表。
type DiffSpans = Vec<(String, bool)>;

/// 单词级 diff：按空白/非空白切 token，`similar` 求差，变动片段带标记。
fn intra_line_spans(old: &str, new: &str) -> (DiffSpans, DiffSpans) {
    let old_toks = word_tokens(old);
    let new_toks = word_tokens(new);
    let diff = similar::TextDiff::from_slices(&old_toks, &new_toks);
    let mut removed: DiffSpans = Vec::new();
    let mut added: DiffSpans = Vec::new();
    let mut first_removed = true;
    let mut first_added = true;
    for change in diff.iter_all_changes() {
        let value: &str = change.value();
        let is_ws = value.chars().all(char::is_whitespace);
        match change.tag() {
            similar::ChangeTag::Delete => {
                let text = if first_removed {
                    first_removed = false;
                    value.trim_start_matches(char::is_whitespace)
                } else {
                    value
                };
                if !text.is_empty() {
                    removed.push((text.to_string(), !is_ws));
                }
            }
            similar::ChangeTag::Insert => {
                let text = if first_added {
                    first_added = false;
                    value.trim_start_matches(char::is_whitespace)
                } else {
                    value
                };
                if !text.is_empty() {
                    added.push((text.to_string(), !is_ws));
                }
            }
            similar::ChangeTag::Equal => {
                removed.push((value.to_string(), false));
                added.push((value.to_string(), false));
            }
        }
    }
    (removed, added)
}

/// 按空白/非空白边界切 token。
fn word_tokens(text: &str) -> Vec<&str> {
    let mut tokens: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut last_ws: Option<bool> = None;
    for (idx, ch) in text.char_indices() {
        let ws = ch.is_whitespace();
        if let Some(prev) = last_ws
            && prev != ws
        {
            tokens.push(&text[start..idx]);
            start = idx;
        }
        last_ws = Some(ws);
    }
    if start < text.len() {
        tokens.push(&text[start..]);
    }
    tokens
}

/// edit 工具的 diff 卡片：行号 gutter + 增/删/上下文配色，词级变动加底色。
fn diff_body(diff: &str, theme: &Theme, max: usize) -> (AnyElement, usize) {
    let rows = diff_rows(diff.trim_end());
    let (shown, hidden) = visible_rows(rows.len(), max);
    let mut column = div()
        .w_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .text_size(px(FONT_SMALL));
    for row in rows.into_iter().take(shown) {
        let color = match row.kind {
            DiffKind::Added => theme.success,
            DiffKind::Removed => theme.danger,
            DiffKind::Context => theme.text_secondary,
        };
        let tint = match row.kind {
            DiffKind::Added => theme.success.opacity(0.22),
            DiffKind::Removed => theme.danger.opacity(0.22),
            DiffKind::Context => theme.overlay,
        };
        let mut line = div().w_full().flex().items_start().gap(px(SPACE_SM));
        if !row.gutter.is_empty() {
            line = line.child(
                div()
                    .flex_none()
                    .text_color(color.opacity(0.7))
                    .child(SharedString::from(row.gutter)),
            );
        }
        let mut content = div()
            .min_w_0()
            .flex_1()
            .text_color(color)
            .flex()
            .flex_wrap();
        for (text, changed) in row.spans {
            let mut span = div().child(SharedString::from(text));
            if changed {
                span = span.rounded(px(2.)).bg(tint);
            }
            content = content.child(span);
        }
        column = column.child(line.child(content));
    }
    (column.into_any_element(), hidden)
}

/// 行数分配：预算内全显，超出部分算隐藏。
fn visible_rows(total: usize, max: usize) -> (usize, usize) {
    let shown = total.min(max);
    (shown, total - shown)
}

/// 工具输出正文：逐行着色并折行（单行超宽也不截断），超过 `max` 行截断（按行截断而不是
/// 裁剪像素，被裁掉的部分不计入布局高度，会盖住输入卡片）。
///
/// `paint` 决定每行的颜色：工具输出按 diff 着色，write 正文与 shell 命令统一一色。
/// 返回渲染好的正文与被截掉的行数，“还有 N 行”的提示由卡片统一出（只要一条）。
fn output_body(text: &str, paint: impl Fn(&str) -> Hsla, max: usize) -> (AnyElement, usize) {
    let mut column = div()
        .w_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .text_size(px(FONT_SMALL));
    let lines = text.trim().lines();
    let (shown, hidden) = visible_rows(lines.clone().count(), max);
    for line in lines.take(shown) {
        column = column.child(
            div()
                .w_full()
                .min_w_0()
                .text_color(paint(line))
                .child(SharedString::from(line.to_string())),
        );
    }

    (column.into_any_element(), hidden)
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

/// 缩略图元素：小图按原始尺寸显示，大图等比缩到 [`IMAGE_THUMB`] 以内，不留白框。
fn image_thumb(path: &Path) -> AnyElement {
    let (w, h) = thumb_size(path);
    div()
        .w(px(w))
        .h(px(h))
        .rounded(px(RADIUS_MD))
        .overflow_hidden()
        .child(gpui::img(image_source(path)).w(px(w)).h(px(h)))
        .into_any_element()
}

/// 丢掉缩略图图片源缓存：新会话/切会话时调，不然上一份会话的图会一直占着内存。
fn clear_image_sources() {
    IMAGE_SOURCES.with_borrow_mut(Vec::clear);
}

/// 缩略图的图片源：按文件内容建 `Image`。
///
/// `gpui::img(路径)` 走的是资源路径缓存（`Resource::Path`），文件被改写后缓存**不会失效**，
/// 同一路径换了截图仍显示第一张；按内容建 `Image` 才能让缓存键跟着内容走。
/// 读不出字节或认不出格式（如 svg）时退回按路径加载。
fn image_source(path: &Path) -> ImageSource {
    let metadata = std::fs::metadata(path).ok();
    let key = (
        metadata.as_ref().map(|m| m.len()).unwrap_or(0),
        metadata.and_then(|m| m.modified().ok()),
    );

    let cached = IMAGE_SOURCES.with_borrow(|cache| {
        cache
            .iter()
            .find(|(cached_path, ..)| cached_path == path)
            .map(|(_, cached_key, source)| (*cached_key, source.clone()))
    });

    if let Some((cached_key, source)) = cached
        && cached_key == key
    {
        return source;
    }

    let source = std::fs::read(path)
        .ok()
        .and_then(|bytes| {
            let mime = mime::detect_image_mime(&bytes)?;
            let format = ImageFormat::from_mime_type(&mime)?;
            Some(ImageSource::from(Arc::new(Image::from_bytes(
                format, bytes,
            ))))
        })
        .unwrap_or_else(|| ImageSource::from(path.to_path_buf()));

    IMAGE_SOURCES.with_borrow_mut(|cache| {
        cache.retain(|(cached_path, ..)| cached_path != path);
        cache.push((path.to_path_buf(), key, source.clone()));
    });
    source
}

/// 缩略图的显示尺寸（逻辑像素）：按图片像素等比缩放，边长不超过 [`IMAGE_THUMB`]。
///
/// 读不出尺寸（svg 等 `image` 不支持解码的格式）时退回 [`IMAGE_THUMB`] 方框。
fn thumb_size(path: &Path) -> (f32, f32) {
    let Ok((w, h)) = image::image_dimensions(path) else {
        return (IMAGE_THUMB, IMAGE_THUMB);
    };

    let (w, h) = (w.max(1) as f32, h.max(1) as f32);
    let scale = (IMAGE_THUMB / w).min(IMAGE_THUMB / h).min(1.0);
    (w * scale, h * scale)
}

/// 图片字节的 FNV-1a 哈希：只用于拼临时文件名，不要求密码学强度。
fn image_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
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
    write_image_file(&bytes, ext)
}

/// 把图片文件按内容拍成快照（内容命名的临时文件）：发送时定住这张图，
/// 同路径文件后来被改写也不会把旧消息的缩略图换掉。
fn snapshot_image(path: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(path).ok()?;
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    write_image_file(&bytes, &ext)
}

/// 把图片字节按内容哈希命名落到临时目录（同名即同内容），返回文件路径。
fn write_image_file(bytes: &[u8], ext: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("prux-gui-images");
    std::fs::create_dir_all(&dir).ok()?;

    // 名字带内容哈希：同一张图重复出现时复用同一个文件，不同内容又不会撞名（撞名会看到旧图）
    let name = format!("{:016x}.{ext}", image_hash(bytes));

    let path = dir.join(name);
    if !path.exists() {
        std::fs::write(&path, bytes).ok()?;
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
    ctx: &mut FlowCtx<'_>,
    cx: &mut Context<GuiApp>,
) -> AnyElement {
    let (expand_tools, show_thinking) = flags;
    match block {
        // 用户消息：右对齐气泡，与助手正文拉开层级
        Block::User(message) => user_block(message, theme, expand_tools, index, ctx.width, cx),
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

    // `@图片` 引用直接渲染成缩略图（用发送时的快照，不跟路径重读）
    for path in &message.images {
        bubble = bubble.child(image_thumb(path));
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

    // shell 类的命令放在卡片第二行，头部只留名字与状态
    let shell = is_shell_tool(&tool.name);

    // 头部：圆点 + 工具名 + 入参摘要（shell 例外）+ 状态
    let mut header = div()
        .w_full()
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
        );

    // 中间的弹性区：非 shell 放入参摘要，shell 用空白占位（把状态标签推到右侧）
    if !shell {
        // edit / write 的参数体量很大，头部改显文件路径（参数摘要为空）
        let header_text = if tool.args.is_empty() {
            tool.path.clone().unwrap_or_default()
        } else {
            tool.args.clone()
        };

        header = header.child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_size(px(FONT_SMALL))
                .text_color(theme.text_tertiary)
                .child(SharedString::from(header_text)),
        );
    } else {
        header = header.child(div().flex_1());
    }

    // 耗时放在状态标签左侧：进行中按当前时刻算（忙碌期间每半秒重绘一次，数字跟着走），
    // 结束后用事件里的 `durationMs` 定格
    let took = match (tool.duration_ms, tool.started_at) {
        (Some(ms), _) => Some(format!("Took {}", format_tool_duration(ms))),
        (None, Some(started)) => Some(format!(
            "Elapsed {}",
            format_tool_duration(started.elapsed().as_millis() as u64)
        )),
        (None, None) => None,
    };
    let mut status = div().flex().items_center().gap(px(SPACE_SM));
    if let Some(took) = took {
        status = status.child(
            div()
                .text_size(px(FONT_CAPTION))
                .text_color(theme.text_ghost)
                .child(SharedString::from(took)),
        );
    }
    card = card.child(
        header.child(
            status.child(
                div()
                    .text_size(px(FONT_CAPTION))
                    .text_color(color)
                    .child(SharedString::from(label)),
            ),
        ),
    );

    // 按行截断而不是靠 `max_h` 裁剪：被裁掉的那部分不计入布局高度，
    // 会盖在输入卡片上，也会让钉底算错。
    let max_lines = if expanded {
        EXPANDED_OUTPUT_LINES
    } else {
        COLLAPSED_OUTPUT_LINES
    };

    // 执行的命令：整行折行、代码色；它自己封顶在 `max_lines`，占掉的行数从输出预算里扣。
    // 命令与输出被截掉的行合并成卡片底部一条提示。
    let mut remaining = max_lines;
    let mut hidden = 0usize;
    if shell && !tool.args.is_empty() {
        let rows = tool.args.trim().lines().count();
        let (body, rest) = output_body(&tool.args, |_| theme.code_text, max_lines);
        remaining = remaining.saturating_sub(rows.min(max_lines));
        hidden += rest;
        card = card.child(body);
    }

    if let Some(diff) = &tool.diff {
        let (body, rest) = diff_body(diff, theme, remaining);
        hidden += rest;
        card = card.child(body);
    } else if let Some(content) = &tool.content {
        // write：先把要写入的正文折起来看一眼，再看工具结果
        let (body, rest) = output_body(content, |_| theme.text_secondary, remaining);
        hidden += rest;
        card = card.child(body);
    }

    if tool.diff.is_none() && !tool.output.trim().is_empty() {
        let (body, rest) = output_body(&tool.output, |line| line_color(line, theme), remaining);
        hidden += rest;
        card = card.child(body);
    }

    if let Some(hint) = output_hint(hidden, expanded) {
        card = card.child(
            div()
                .text_size(px(FONT_CAPTION))
                .text_color(theme.text_ghost)
                .child(SharedString::from(hint)),
        );
    }

    for image in &tool.images {
        card = card.child(image_thumb(image));
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

/// Markdown 文本 → 元素（覆盖层面板用：只要排版与配色，不登记选区/行位置）。
pub fn markdown_body(text: &str, theme: &Theme) -> AnyElement {
    let rows = parse_rows(text, theme);
    let mut lines = TextLines {
        next: 0,
        selection: None,
        bounds: Rc::new(RefCell::new(HashMap::new())),
        text_bounds: Rc::new(RefCell::new(HashMap::new())),
        rows: Rc::new(RefCell::new(Vec::new())),
    };
    render_rows(&rows, theme, &mut lines)
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
    let (text, highlights) = inline_runs(marker, spans, theme);
    let highlights = match selection {
        Some(range) => merge_selection(
            highlights,
            char_range_to_bytes(&text, range),
            HighlightStyle {
                background_color: Some(theme.selection),
                ..Default::default()
            },
        ),
        None => highlights,
    };
    StyledText::new(text).with_highlights(highlights)
}

/// 把选区底色并进已有高亮，切成互不重叠的段。
///
/// `StyledText` 要求高亮区间首尾相接、互不重叠：直接把选区当成一段追加进去，
/// 与行内代码/链接重叠的那部分底色就会画不出来（选中后看着像漏掉了几个词）。
fn merge_selection(
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    selection: Range<usize>,
    background: HighlightStyle,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if selection.is_empty() {
        return highlights;
    }

    let mut bounds = vec![selection.start, selection.end];
    for (range, _) in &highlights {
        bounds.push(range.start);
        bounds.push(range.end);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut out = Vec::new();
    for pair in bounds.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let mut style = highlights
            .iter()
            .find(|(range, _)| range.start <= start && end <= range.end)
            .map(|(_, style)| *style)
            .unwrap_or_default();
        if selection.start <= start && end <= selection.end {
            style = style.highlight(background);
        }
        if style != HighlightStyle::default() {
            out.push((start..end, style));
        }
    }
    out
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
        transcript.push_tool("t1", "bash", &serde_json::json!({ "command": "ls" }), ".");
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

    /// 工具耗时：跑着时有起点，结束后改成事件里的 durationMs 并停掉计时。
    #[test]
    fn tool_duration_comes_from_end_event() {
        let mut transcript = Transcript::default();
        transcript.apply(
            &serde_json::json!({
                "type": "tool_execution_start",
                "toolCallId": "t1",
                "toolName": "bash",
                "args": { "command": "ls" },
            }),
            ".",
        );
        let Block::Tool(tool) = &transcript.blocks()[0] else {
            panic!("第一块应是工具块");
        };
        assert!(tool.started_at.is_some());
        assert_eq!(tool.duration_ms, None);

        transcript.apply(
            &serde_json::json!({
                "type": "tool_execution_end",
                "toolCallId": "t1",
                "isError": false,
                "durationMs": 1500,
            }),
            ".",
        );
        let Block::Tool(tool) = &transcript.blocks()[0] else {
            panic!("第一块应是工具块");
        };
        assert_eq!(tool.status, ToolStatus::Ok);
        assert_eq!(tool.duration_ms, Some(1500));
        assert!(tool.started_at.is_none());
    }

    /// 选区底色与行内样式高亮合并后首尾相接、互不重叠（重叠段画不出底色）。
    #[test]
    fn selection_highlight_splits_inline_runs() {
        let bold = HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        };
        // 复用任意非默认样式当“选区底色”，本函数不关心具体是哪个字段
        let bg = HighlightStyle {
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        };

        // 选区盖住整段行内代码：三段首尾相接、互不重叠
        let merged = merge_selection(vec![(5..9, bold)], 0..12, bg);
        assert_eq!(
            merged,
            vec![
                (0..5, bg),
                (
                    5..9,
                    HighlightStyle {
                        font_weight: Some(FontWeight::BOLD),
                        font_style: Some(FontStyle::Italic),
                        ..Default::default()
                    }
                ),
                (9..12, bg),
            ]
        );

        // 选区只盖住行内代码的前半段：尾部保留原样式且不带底色
        let merged = merge_selection(vec![(5..9, bold)], 0..7, bg);
        assert_eq!(
            merged,
            vec![
                (0..5, bg),
                (
                    5..7,
                    HighlightStyle {
                        font_weight: Some(FontWeight::BOLD),
                        font_style: Some(FontStyle::Italic),
                        ..Default::default()
                    }
                ),
                (7..9, bold),
            ]
        );
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

    /// 空助手块（只有 tool call）不该占位；带正文或思考的都算有内容。
    #[test]
    fn blank_assistant_blocks_are_skipped() {
        let blank = Block::Assistant(Assistant::default());
        assert!(is_blank_assistant(&blank));

        let with_text = Block::Assistant(Assistant {
            text: "hi".to_string(),
            ..Assistant::default()
        });
        assert!(!is_blank_assistant(&with_text));

        let with_thinking = Block::Assistant(Assistant {
            thinking: "hmm".to_string(),
            ..Assistant::default()
        });
        assert!(!is_blank_assistant(&with_thinking));
    }

    /// diff 解析：单行替换做词级标记、多行删除/新增整行着色、块头归上下文。
    #[test]
    fn diff_rows_mark_word_level_changes() {
        let diff = "-1 old value here\n+1 new value here\n 2 context line";
        let rows = diff_rows(diff);
        assert_eq!(rows.len(), 3, "1 删 + 1 增 + 1 上下文");
        assert_eq!(rows[0].gutter, "-1");
        assert_eq!(rows[1].gutter, "+1");
        // 只有一个 token 变了：其余内容不标记
        let changed: String = rows[0]
            .spans
            .iter()
            .filter(|(_, changed)| *changed)
            .map(|(text, _)| text.as_str())
            .collect();
        assert_eq!(changed, "old");
        let joined: String = rows[1].spans.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "new value here");
        assert_eq!(rows[2].gutter, "2");
    }

    /// write 调用：正文留给卡片折叠预览，头部只留路径（参数体量太大）。
    #[test]
    fn write_tool_keeps_its_content_for_the_card() {
        let mut transcript = Transcript::default();
        transcript.push_tool(
            "w1",
            "write",
            &serde_json::json!({ "path": "/tmp/x", "content": "a\nb" }),
            ".",
        );
        let Block::Tool(tool) = &transcript.blocks()[0] else {
            panic!("第一块应是工具块");
        };
        assert_eq!(tool.path.as_deref(), Some("/tmp/x"));
        assert_eq!(tool.content.as_deref(), Some("a\nb"));
        assert!(tool.args.is_empty(), "头部只留路径");
        assert!(tool.diff.is_none(), "write 不是 diff");
    }

    /// bash 调用：命令整条留在 `args` 里——不带 `command=` 前缀，也不按宽度截断（卡片第二行折行）。
    #[test]
    fn shell_tool_keeps_the_whole_command() {
        let mut transcript = Transcript::default();
        let command = format!("rg '{}' src", "x".repeat(200));
        transcript.push_tool(
            "b1",
            "bash",
            &serde_json::json!({ "command": command }),
            ".",
        );
        let Block::Tool(tool) = &transcript.blocks()[0] else {
            panic!("第一块应是工具块");
        };
        assert_eq!(tool.args, command);
    }

    /// 行数分配：预算内全显，超出部分算隐藏（不多不少）。
    #[test]
    fn visible_rows_split_by_budget() {
        assert_eq!(visible_rows(3, 6), (3, 0));
        assert_eq!(visible_rows(0, 6), (0, 0));
        assert_eq!(visible_rows(8, 6), (6, 2));
        assert_eq!(visible_rows(3, 0), (0, 3));
    }

    /// 多行删除/新增不做词级 diff（行数不等时逐行整行着色）。
    #[test]
    fn diff_rows_keep_bulk_changes_whole() {
        let diff = "-1 a\n-2 b\n+1 c";
        let rows = diff_rows(diff);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.spans.len() == 1));
        assert_eq!(rows[0].kind, DiffKind::Removed);
        assert_eq!(rows[2].kind, DiffKind::Added);
    }

    /// 缩略图尺寸：小图保持原尺寸，大图等比缩到 [`IMAGE_THUMB`] 以内。
    #[test]
    fn thumb_size_keeps_small_images_and_caps_large_ones() {
        let dir = std::env::temp_dir().join(format!("prux-gui-thumb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let small = dir.join("small.png");
        image::RgbaImage::new(1, 1).save(&small).expect("small png");
        let large = dir.join("large.png");
        image::RgbaImage::new(800, 400)
            .save(&large)
            .expect("large png");
        assert_eq!(thumb_size(&small), (1.0, 1.0));
        assert_eq!(thumb_size(&large), (IMAGE_THUMB, IMAGE_THUMB / 2.0));
        // 读不出尺寸的路径退回方框，不给零尺寸
        assert_eq!(
            thumb_size(&dir.join("missing.png")),
            (IMAGE_THUMB, IMAGE_THUMB)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        // 同长度、同首字节的不同图片不能撞名（撞名会一直显示旧图）
        let other = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let other_path = save_tool_image(other, "image/png").expect("png 应该落盘");
        assert_ne!(path, other_path);
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

    /// 同一路径的图换过内容后，先前那条消息仍显示发送时那张（各自一份内容快照）。
    #[test]
    fn user_images_snapshot_at_send_time() {
        let dir = std::env::temp_dir().join(format!("prux-gui-img-snap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("shot.png");
        let png = |side: u32| {
            let mut buf = Vec::new();
            image::RgbaImage::new(side, side)
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .expect("png");
            buf
        };
        let cwd = dir.to_str().expect("utf8 dir");
        let text = format!("看 @{}", path.display());

        std::fs::write(&path, png(2)).expect("write");
        let mut transcript = Transcript::default();
        transcript.push_user(text.clone(), cwd);

        std::fs::write(&path, png(3)).expect("write");
        transcript.push_user(text, cwd);

        let snapshots: Vec<&PathBuf> = transcript
            .blocks()
            .iter()
            .filter_map(|block| match block {
                Block::User(message) => message.images.first(),
                _ => None,
            })
            .collect();
        assert_eq!(snapshots.len(), 2);
        assert_ne!(snapshots[0], snapshots[1], "两条消息不能共用一张快照");
        assert_eq!(std::fs::read(snapshots[0]).unwrap(), png(2));
        assert_eq!(std::fs::read(snapshots[1]).unwrap(), png(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 清空/重建会话时把图片源缓存一并丢掉（否则旧会话的图一直占着内存）。
    #[test]
    fn clearing_transcript_drops_image_sources() {
        let dir = std::env::temp_dir().join(format!("prux-gui-img-clear-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("shot.png");
        let mut buf = Vec::new();
        image::RgbaImage::new(2, 2)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("png");
        std::fs::write(&path, &buf).expect("write");

        _ = image_source(&path);
        assert!(IMAGE_SOURCES.with_borrow(|cache| !cache.is_empty()));

        Transcript::default().clear();
        assert!(IMAGE_SOURCES.with_borrow(|cache| cache.is_empty()));

        _ = image_source(&path);
        _ = Transcript::from_messages(&[], "/");
        assert!(IMAGE_SOURCES.with_borrow(|cache| cache.is_empty()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同路径换图后图片源要跟着换（gpui 的路径缓存不会失效），内容不变时复用缓存。
    #[test]
    fn image_source_follows_file_changes() {
        let dir = std::env::temp_dir().join(format!("prux-gui-img-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("shot.png");
        let png = |side: u32| {
            let mut buf = Vec::new();
            image::RgbaImage::new(side, side)
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .expect("png");
            buf
        };
        let write = |bytes: &[u8]| std::fs::write(&path, bytes).expect("write");
        let image_id = || match image_source(&path) {
            ImageSource::Image(image) => image.id(),
            _ => panic!("png 应按内容生成 Image"),
        };

        write(&png(2));
        let first = image_id();
        write(&png(3));
        assert_ne!(first, image_id(), "改图后应换成新图片源");
        write(&png(2));
        assert_eq!(first, image_id(), "改回原内容应命中内容缓存");

        write(b"not-really-a-png");
        assert!(matches!(image_source(&path), ImageSource::Resource(_)));
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
}
