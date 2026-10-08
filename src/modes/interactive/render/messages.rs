//! 消息渲染
//!
//! - user：背景块（userMessageBg）+ markdown（userMessageText），左右 1 空格 padding
//!
//! - assistant：markdown 无背景（outputPad=1）；连续 thinking 块合并为一个
//!   markdown 段（thinkingText 斜体），折叠时显示 "Thinking..."；stopReason
//!   length/aborted/error 追加错误色提示
//!
//! - assistant 的每个 toolCall：前置空行 + 背景块（toolPendingBg 进行中 /
//!   toolErrorBg 失败 / toolSuccessBg 成功）+ 粗体工具名（toolTitle）+ 参数 +
//!   输出（toolOutput）；toolResult 消息更新对应块（不单独成行）
//!
//! - compaction/branch summary：背景块（customMessageBg）+ `[compaction]`/`[branch]`
//!   粗体标签（customMessageLabel）+ 折叠/展开摘要（customMessageText）

pub(crate) use super::markdown::LinkSpan;
use super::markdown::{collect_link_spans, render_markdown, render_markdown_styled};
use crate::{
    core::{
        extensions,
        provider::{AgentMessage, ContentBlock},
        skills::{self, SkillBlock},
    },
    modes::interactive::{
        app::{MsgLevel, SysMsg, SysSpan, sys_spans_text},
        theme::Theme,
    },
    utils::{
        display::{
            char_width, display_width, format_cost_usd, grapheme_width, sanitize_output,
            truncate_display, wrap_line,
        },
        glyphs::{DEF_DONE, DEF_ELLIPSIS, DEF_FAILED, DEF_NESTED},
        terminal_image,
        time::now_ms,
    },
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc, time::Duration};

/// 一条历史渲染段：(全局起始行, 块行, 块 pending delta)
pub type HistorySegment = (usize, Arc<Vec<Line<'static>>>, Arc<Vec<PendingTool>>);

/// 内联图片的最大宽度（列）
const IMAGE_MAX_COLS: usize = 60;

/// system 消息在时间线里的 seq 基址（与常规消息的索引 i 区分，避免缓存 key 冲突）。
/// app.rs 的 push_msg_dedup 依赖此常量失效被替换条目的渲染缓存。
pub(crate) const SYSTEM_SEQ_BASE: usize = 100_000_000;

/// 用户消息整块背景色，包裹正文与其中的 markdown 渲染结果。
const USER_MSG_BG: (&str, &str) = ("userMessageBg", "#343541");
/// 用户消息正文与 markdown 内容的默认前景色。
const USER_MSG_TEXT: (&str, &str) = ("userMessageText", "#d4d4d4");
/// assistant thinking 段落的斜体灰字前景色。
const THINKING_TEXT: (&str, &str) = ("thinkingText", "#808080");
/// compaction / branch 摘要消息背景块的底色。
const CUSTOM_MSG_BG: (&str, &str) = ("customMessageBg", "#2d2838");
/// 摘要消息正文（展开后的摘要文本）的前景色。
const CUSTOM_MSG_TEXT: (&str, &str) = ("customMessageText", "#d4d4d4");
/// 摘要消息 `[compaction]` / `[branch]` 粗体标签的前景色。
const CUSTOM_MSG_LABEL: (&str, &str) = ("customMessageLabel", "#9575cd");
/// 工具调用进行中、尚无结果时的块背景色。
const TOOL_PENDING_BG: (&str, &str) = ("toolPendingBg", "#282832");
/// 工具调用成功结束时的块背景色。
const TOOL_SUCCESS_BG: (&str, &str) = ("toolSuccessBg", "#283228");
/// 工具调用失败（isError）时的块背景色。
const TOOL_ERROR_BG: (&str, &str) = ("toolErrorBg", "#3c2828");
/// 工具块里工具名与参数行的粗体前景色。
const TOOL_TITLE: (&str, &str) = ("toolTitle", "#d4d4d4");
/// 工具执行输出文本的前景色（次要灰）。
const TOOL_OUTPUT: (&str, &str) = ("toolOutput", "#808080");
/// diff 视图中新增行的前景色（绿）。
const DIFF_ADDED: (&str, &str) = ("toolDiffAdded", "#98c379");
/// diff 视图中删除行的前景色（红）。
const DIFF_REMOVED: (&str, &str) = ("toolDiffRemoved", "#e06c75");
/// diff 视图中未改动上下文行的前景色（灰）。
const DIFF_CONTEXT: (&str, &str) = ("toolDiffContext", "#6a737d");
/// 错误提示文本的前景色（stopReason 异常、工具报错等）。
const ERROR: (&str, &str) = ("error", "#cc6666");

/// 系统消息各级别默认色：通用状态键 + 兜底色（info 用最弱的三级文本色）
const SYS_LEVEL_KEYS: [(&str, &str, bool); 4] = [
    ("dim", "#666666", false),
    ("success", "#b5bd68", false),
    ("warning", "#d8a657", true),
    ("error", "#cc6666", true),
];

/// 工具调用块预览行数：bash 显示末尾**视觉行**（宽度折行后仍只显示 5 行）；
/// 其他工具显示开头行（ls 折叠 maxLines=20）；
/// write 工具的 content 参数折叠前 10 行（带 total 提示）。
const BASH_PREVIEW_LINES: usize = 5;
/// 非 bash 工具输出折叠时预览的开头行数。
const TOOL_PREVIEW_LINES: usize = 20;
/// `mcp` 工具输出折叠时预览的开头行数
const MCP_PREVIEW_LINES: usize = 5;
/// write 工具 content 参数折叠时预览的行数。
const WRITE_PREVIEW_LINES: usize = 10;
/// 折叠态参数对的字符上限（超出截断）。
const COLLAPSED_ARGS_CHARS: usize = 100;
/// codemode 子调用列表折叠时保留的末尾条数
const SUB_CALL_PREVIEW_COUNT: usize = 8;
/// 展开态每个参数占一行的缩进前缀（键值对的续行再缩进 4 空格）。
const ARG_INDENT: &str = "  ";
const ARG_CONTINUATION_INDENT: &str = "    ";

/// 有自定义调用头的内置工具（`bash` 单独处理）。
/// 其余工具（扩展工具，如 `mcp`）没有自定义调用头，按通用形式显示参数：
/// 折叠时 `key=value` 排在同一行，展开时每个参数一行 `key: value`。
const CUSTOM_CALL_HEADER_TOOLS: &[&str] =
    &["read", "write", "edit", "ls", "grep", "find", "powershell"];

/// 消息内可折叠块的类型（决定缺省展开态：thinking 跟随 `show_thinking`，其余跟随 `expand_all`）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    /// compaction/branch 摘要（整条消息一个块）
    Summary,
    /// 用户消息里的 `<skill>` 调用区域
    Skill,
    /// assistant 的一段 thinking run（连续 thinking 块合并）
    Thinking,
    /// assistant 的一次工具调用块（输出内联在块里）
    Tool,
}

/// 可折叠块的稳定标识：时间线条目 `(ts, seq)` + 消息内块序号
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId {
    /// 该条目所属消息的时间戳（毫秒），用于定位时间线条目。
    pub ts: u64,
    /// 同一毫秒内区分条目的序号，系统提示以 100_000_000 为基数。
    pub seq: usize,
    /// 消息内可折叠块的下标，与 `collapsible_blocks` 输出顺序一致。
    pub block: usize,
}

/// 消息内一个可折叠块的行范围（相对该消息渲染结果起点，`[start, end)`）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockSpan {
    /// 块序号（与该消息 `collapsible_blocks` 的输出下标一致）
    pub index: usize,
    /// 块起始行号（相对该消息渲染结果起点，含该行）。
    pub start: usize,
    /// 块结束行号（不含，与起始构成半开区间）。
    pub end: usize,
}

/// 该消息内可折叠块的类型序列：**顺序 = 渲染顺序 = 块序号**，
/// 供调用方先把逐块展开态解析成 `&[bool]` 再交给 `render_message_blocks`。
pub fn collapsible_blocks(msg: &AgentMessage) -> Vec<BlockKind> {
    match msg.role.as_str() {
        "compactionSummary" | "branchSummary" => vec![BlockKind::Summary],
        "user" => {
            if skills::parse_skill_block(&msg.text()).is_some() {
                vec![BlockKind::Skill]
            } else {
                Vec::new()
            }
        }
        "assistant" => {
            let mut out = Vec::new();
            let content = &msg.content;
            let mut i = 0;
            while i < content.len() {
                match &content[i] {
                    ContentBlock::Thinking { .. } => {
                        // 连续 thinking 合并为一个 run（与 render_assistant 一致）；全空的 run 不渲染，也就不算一个块
                        let mut non_empty = false;
                        while i < content.len() {
                            if let ContentBlock::Thinking { thinking, .. } = &content[i] {
                                if !thinking.trim().is_empty() {
                                    non_empty = true;
                                }
                                i += 1;
                            } else {
                                break;
                            }
                        }

                        if non_empty {
                            out.push(BlockKind::Thinking);
                        }
                    }
                    ContentBlock::ToolCall { .. } => {
                        out.push(BlockKind::Tool);
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// 无逐块覆盖时该块的缺省展开态
pub fn default_block_expanded(kind: BlockKind, expand_all: bool, show_thinking: bool) -> bool {
    match kind {
        BlockKind::Thinking => show_thinking,
        BlockKind::Summary | BlockKind::Skill | BlockKind::Tool => expand_all,
    }
}

/// 解析一条消息各可折叠块的展开态：逐块覆盖优先，缺失回退全局设置。
/// 输出与 `collapsible_blocks(msg)` 一一对应。
pub fn resolve_block_expansions(
    msg: &AgentMessage,
    ts: u64,
    seq: usize,
    overrides: &HashMap<BlockId, bool>,
    expand_all: bool,
    show_thinking: bool,
) -> Vec<bool> {
    collapsible_blocks(msg)
        .into_iter()
        .enumerate()
        .map(|(i, kind)| {
            overrides
                .get(&BlockId { ts, seq, block: i })
                .copied()
                .unwrap_or(default_block_expanded(kind, expand_all, show_thinking))
        })
        .collect()
}

/// 等待结果的工具调用（toolResult 消息按 toolCallId 匹配填充）
#[derive(Debug, Clone)]
pub struct PendingTool {
    /// 工具调用 id（toolCallId），用于与 toolResult 消息匹配。
    pub id: String,
    /// 工具结果是否失败；结果尚未到达时为 None。
    pub is_error: Option<bool>,
    /// 工具返回的文本内容；结果尚未到达时为 None。
    pub result: Option<String>,
    /// 工具执行耗时（毫秒）；结果未返回或未上报时为 None。
    pub duration_ms: Option<u64>,
    /// 工具启动墙钟毫秒（tool_execution_start 事件携带）；进行中时渲染 `Elapsed X.Xs`
    pub started_at_ms: Option<u64>,
}

impl PendingTool {
    /// 由工具调用事件构造待完成工具条目：结果相关字段留空，只记录 id 与可选的启动时间。
    fn from_call(id: &str, _name: &str, _args: Value, started_at_ms: Option<u64>) -> Self {
        PendingTool {
            id: id.to_string(),
            is_error: None,
            result: None,
            duration_ms: None,
            started_at_ms,
        }
    }
}

/// 工具耗时格式化：
/// <60s → `X.Xs`；≥60s → `Xm Ys`；≥1h → `Xh Ym Zs`。
fn format_tool_duration(ms: u64) -> String {
    let seconds = ms as f64 / 1000.0;
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }

    let total = seconds as u64;
    let minutes = total / 60;
    let remainder = total % 60;

    if minutes < 60 {
        return format!("{minutes}m {remainder}s");
    }

    format!("{}h {}m {remainder}s", minutes / 60, minutes % 60)
}

/// 渲染系统消息：保留 \n 换行 + 按段配色 + 超宽自动换行（对齐 pi Text 组件）。
/// 颜色解析：级别决定默认色（info→灰、success→绿、warning→黄、error→红，后两者加粗）；
/// `fg` 为主题键或 hex 时覆盖级别默认色（如 /session 富文本的自定义分段配色），None 时用级别默认色。
pub(crate) fn render_sys_message(
    spans: &[SysSpan],
    level: MsgLevel,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if sys_spans_text(spans).trim().is_empty() {
        return Vec::new();
    }

    let out_w = width.saturating_sub(2);
    if out_w == 0 {
        return Vec::new();
    }

    let level_style = sys_level_style(level, theme);

    // 把 span 流按 \n 切成「行 → (样式, 文本)」列表，并去掉尾部纯空行
    let lines = sys_lines_from_spans(spans, level_style, theme);

    let mut out: Vec<Line<'static>> = Vec::new();
    for row in &lines {
        if row.is_empty() {
            out.push(Line::from(""));
            continue;
        }
        // 非空行做超宽包裹（多段流式拼接，超宽拆行）
        out.extend(render_sys_row(row, out_w));
    }

    // 统一出口：系统消息块尾部补纯空行（渲染层间隔；末尾已是空行则不重复）
    push_blank_gap(&mut out);
    out
}

/// 按消息级别取默认样式：info→弱灰（`dim`）、success→绿、warning→黄、error→红（后两者加粗）。
fn sys_level_style(level: MsgLevel, theme: &Theme) -> Style {
    let (key, default, bold) = match level {
        MsgLevel::Info => SYS_LEVEL_KEYS[0],
        MsgLevel::Success => SYS_LEVEL_KEYS[1],
        MsgLevel::Warning => SYS_LEVEL_KEYS[2],
        MsgLevel::Error => SYS_LEVEL_KEYS[3],
    };

    let style = theme.style(key, default);

    if bold {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// 把系统消息 span 流按 \n 切分成「行 → (样式, 文本)」列表，并去掉尾部纯空行
fn sys_lines_from_spans(
    spans: &[SysSpan],
    level_style: Style,
    theme: &Theme,
) -> Vec<Vec<(Style, String)>> {
    let mut lines: Vec<Vec<(Style, String)>> = vec![vec![]];
    for span in spans {
        let style = match &span.fg {
            Some(spec) => {
                let base = Style::default().fg(theme.resolve_color(spec));
                if span.bold {
                    base.add_modifier(Modifier::BOLD)
                } else {
                    base
                }
            }
            None => {
                if span.bold {
                    level_style.add_modifier(Modifier::BOLD)
                } else {
                    level_style
                }
            }
        };

        for (i, piece) in span.text.split('\n').enumerate() {
            if i > 0 {
                lines.push(vec![]);
            }
            if !piece.is_empty() {
                // ratatui 落格会丢弃制表符，而 prux 宽度口径按 4 列计数：
                // 先展开成空格，否则渲染宽度与实际落格漂移且 tab 内容整段消失。
                lines
                    .last_mut()
                    .unwrap()
                    .push((style, piece.replace('\t', "    ")));
            }
        }
    }

    // 消息不以空行收尾（末尾连续 \n 只视为换行，不产生额外空行）
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

/// 渲染单行系统消息：超宽自动折行（多段流式拼接，超宽拆行）
fn render_sys_row(row: &[(Style, String)], out_w: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    for (style, text) in row {
        let mut byte_start = 0usize;
        let mut w = 0usize;

        for (bi, ch) in text.char_indices() {
            let cw = char_width(ch);
            if cw == 0 {
                continue;
            }

            if cur_w + w + cw > out_w {
                if w > 0 {
                    cur.push(Span::styled(text[byte_start..bi].to_string(), *style));
                }
                if !cur.is_empty() {
                    out.push(Line::from(std::mem::take(&mut cur)));
                }
                cur_w = 0;
                byte_start = bi;
                w = 0;
            }
            w += cw;
        }

        if w > 0 {
            cur.push(Span::styled(text[byte_start..].to_string(), *style));
        }
        cur_w += w;
    }
    if !cur.is_empty() {
        out.push(Line::from(cur));
    }
    out
}

/// 渲染一条消息（默认展开态由 `expand_all` / `show_thinking` 推导，不收集块范围）。
/// `tool_results` 为预扫描的 toolResult（assistant 工具调用块显示结果用）；
/// `tool_started_at` 为进行中工具的启动墙钟毫秒（toolCallId → ms，渲染 `Elapsed` 实时计时用）；
/// `nested_calls` 为进行中工具的嵌套调用（codemode 脚本，toolCallId → 行）。
#[allow(clippy::too_many_arguments)]
pub fn render_message(
    msg: &AgentMessage,
    width: usize,
    theme: &Theme,
    expand_all: bool,
    show_thinking: bool,
    pending: &mut Vec<PendingTool>,
    tool_results: &HashMap<String, ToolResultView>,
    tool_started_at: &HashMap<String, u64>,
    nested_calls: &HashMap<String, Vec<NestedCallView>>,
    edit_map: &HashMap<String, EditRenderData>,
) -> Vec<Line<'static>> {
    let expansions: Vec<bool> = collapsible_blocks(msg)
        .into_iter()
        .map(|k| default_block_expanded(k, expand_all, show_thinking))
        .collect();
    let mut blocks = Vec::new();
    render_message_blocks(
        msg,
        width,
        theme,
        &expansions,
        &mut blocks,
        pending,
        tool_results,
        tool_started_at,
        nested_calls,
        edit_map,
        &mut Vec::new(),
        ImageLayout::disabled(),
    )
}

/// 渲染一条消息，并收集各可折叠块的行范围（供鼠标点击命中区用）。
/// `expansions` 按 `collapsible_blocks(msg)` 的顺序给出每个块是否展开/可见。
/// `images` 收集消息里预留出来的内联图片槽位（相对本条目起点）；`layout` 为图片渲染参数，
/// 关闭时图片块整块不渲染、不产生槽位。
#[allow(clippy::too_many_arguments)]
pub fn render_message_blocks(
    msg: &AgentMessage,
    width: usize,
    theme: &Theme,
    expansions: &[bool],
    blocks: &mut Vec<BlockSpan>,
    pending: &mut Vec<PendingTool>,
    tool_results: &HashMap<String, ToolResultView>,
    tool_started_at: &HashMap<String, u64>,
    nested_calls: &HashMap<String, Vec<NestedCallView>>,
    edit_map: &HashMap<String, EditRenderData>,
    images: &mut Vec<ImageSlot>,
    layout: ImageLayout,
) -> Vec<Line<'static>> {
    // 每条消息的块游标从 0 开始（与 `collapsible_blocks` 顺序一致）
    let mut out = match msg.role.as_str() {
        "user" => {
            let expand = expansions.first().copied().unwrap_or(false);
            let mut lines = render_user(&msg.text(), width, theme, expand, blocks);

            // 用户消息里的图片（粘贴/附件）：正文之后，沿用消息背景块
            let bg = theme.style_bg(USER_MSG_BG.0, USER_MSG_BG.1);
            for block in &msg.content {
                if let ContentBlock::Image { data, .. } = block {
                    push_image_block(&mut lines, data, width, theme, layout, images, Some(bg));
                }
            }
            lines
        }
        "assistant" => {
            let mut cursor = 0usize;
            render_assistant(
                msg,
                width,
                theme,
                expansions,
                &mut cursor,
                blocks,
                pending,
                tool_results,
                tool_started_at,
                nested_calls,
                edit_map,
                images,
                layout,
            )
        }
        "toolResult" => render_tool_result(msg, width, theme, false, pending),
        "compactionSummary" => {
            let expand = expansions.first().copied().unwrap_or(false);
            render_summary(&msg.text(), true, width, theme, expand, blocks)
        }
        "branchSummary" => {
            let expand = expansions.first().copied().unwrap_or(false);
            render_summary(&msg.text(), false, width, theme, expand, blocks)
        }
        _ => Vec::new(),
    };

    // 统一出口：消息块尾部补一行纯空行
    push_blank_gap(&mut out);
    out
}

/// 用户消息：背景块（上下各 1 背景行）。含 <skill> 块时折叠渲染
fn render_user(
    text: &str,
    width: usize,
    theme: &Theme,
    expand: bool,
    blocks: &mut Vec<BlockSpan>,
) -> Vec<Line<'static>> {
    // <skill> 块时折叠渲染
    if let Some(block) = skills::parse_skill_block(text) {
        let mut out = render_skill_invocation(&block, width, theme, expand, blocks);
        if let Some(user_message) = block.user_message {
            out.push(Line::from(""));
            out.extend(render_user(
                &user_message,
                width,
                theme,
                expand,
                &mut Vec::new(),
            ));
        }
        return out;
    }

    let inner = width.saturating_sub(2).max(1);
    let lines = render_markdown_styled(
        text,
        inner,
        theme,
        theme.style(USER_MSG_TEXT.0, USER_MSG_TEXT.1),
    );
    let bg = theme.style_bg(USER_MSG_BG.0, USER_MSG_BG.1);
    let mut out = Vec::new();
    out.push(Line::from(Span::styled(" ".repeat(width), bg))); // Box 上下 padding（背景行）

    if lines.is_empty() {
        out.push(bg_line(Line::from(""), width, bg, 1));
    } else {
        for line in lines {
            out.push(bg_line(line, width, bg, 1));
        }
    }

    out.push(Line::from(Span::styled(" ".repeat(width), bg)));
    out
}

/// 技能调用块：折叠显示 `[skill] name (Ctrl+O/Alt+click to expand)`，展开显示 label + **name** + Markdown body。
/// 整块登记为一个可折叠块（块序号 0，行范围相对本函数返回值）
fn render_skill_invocation(
    block: &SkillBlock,
    width: usize,
    theme: &Theme,
    expand: bool,
    blocks: &mut Vec<BlockSpan>,
) -> Vec<Line<'static>> {
    let label_style = theme
        .style(CUSTOM_MSG_LABEL.0, CUSTOM_MSG_LABEL.1)
        .add_modifier(Modifier::BOLD);
    let text_style = theme.style(CUSTOM_MSG_TEXT.0, CUSTOM_MSG_TEXT.1);
    let muted = theme.style("muted", "#808080");
    let bg = theme.style_bg(CUSTOM_MSG_BG.0, CUSTOM_MSG_BG.1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(" ".repeat(width), bg))); // Box(1,1)：上下 padding 背景行

    if expand {
        lines.push(bg_line(
            Line::from(vec![Span::styled("[skill]", label_style)]),
            width,
            bg,
            1,
        ));
        lines.push(Line::from(Span::styled(" ".repeat(width), bg))); // label 后 Spacer（背景）
        let inner = width.saturating_sub(2).max(1);
        let md = format!("**{}**\n\n{}", block.name, block.content);

        for l in render_markdown_styled(&md, inner, theme, text_style) {
            lines.push(bg_line(l, width, bg, 1));
        }
    } else {
        let line = Line::from(vec![
            Span::styled("[skill] ", label_style),
            Span::styled(block.name.clone(), text_style),
            Span::styled(" (Ctrl+O/Alt+click to expand)", muted),
        ]);
        lines.push(bg_line(line, width, bg, 1));
    }

    lines.push(Line::from(Span::styled(" ".repeat(width), bg))); // Box 下 padding
    blocks.push(BlockSpan {
        index: 0,
        start: 0,
        end: lines.len(),
    });
    lines
}

/// 请扩展渲染器画一个工具块；没注册 / 不接管时返回 `None`（调用方走内置渲染）。
///
/// 返回的已经是**铺好背景、折好行、必要时折叠**的完整内容行（不含块顶/底 padding）。
/// 折叠按渲染器给的 `preview_lines`（视觉行，宽度折行后）取开头，提示行沿用内置文案。
#[allow(clippy::too_many_arguments)]
fn render_extension_tool_block(
    name: &str,
    args: &Value,
    result: Option<(&str, bool)>,
    duration_ms: Option<u64>,
    started_at_ms: Option<u64>,
    theme: &Theme,
    bg: Style,
    width: usize,
    inner: usize,
    expand_all: bool,
) -> Option<Vec<Line<'static>>> {
    // 进行中的耗时：与内置渲染同一口径（距开始时间的差值）
    let duration_ms = duration_ms.or_else(|| {
        result
            .is_none()
            .then_some(started_at_ms)
            .flatten()
            .map(|started| now_ms().saturating_sub(started))
    });
    let ctx = extensions::ToolRenderCtx {
        tool: name,
        args,
        result,
        duration_ms,
        expanded: expand_all,
        width,
        inner_width: inner,
    };
    let rendered = extensions::render_tool_block(name, &ctx)?;

    let muted = theme.style("muted", "#808080");
    let default_style = theme.style(TOOL_OUTPUT.0, TOOL_OUTPUT.1);
    let mut visual: Vec<Line<'static>> = Vec::new();
    for rich in rendered.lines {
        let spans: Vec<Span<'static>> = rich
            .into_iter()
            .map(|s| {
                let style = match s.fg.as_deref() {
                    Some(fg) => theme.style(fg, fg),
                    None => default_style,
                };
                Span::styled(s.text, style)
            })
            .collect();
        visual.extend(wrap_spans(spans, inner));
    }

    // 折叠：只保留头部 preview_lines 行视觉行（与内置非 bash 折叠同向：看开头）
    let mut collapsed_hint: Option<String> = None;
    if !expand_all
        && let Some(preview_lines) = rendered.preview_lines
        && preview_lines > 0
        && visual.len() > preview_lines
    {
        collapsed_hint = Some(format!(
            "... ({} more lines, ctrl+o/alt+click to expand)",
            visual.len() - preview_lines
        ));
        visual.truncate(preview_lines);
    }

    let mut lines: Vec<Line<'static>> = visual
        .into_iter()
        .map(|l| bg_line(l, width, bg, 1))
        .collect();
    if let Some(hint) = collapsed_hint {
        lines.push(bg_line(Line::from(Span::styled(hint, muted)), width, bg, 1));
    }
    Some(lines)
}

/// 给一行加上左侧 padding 与右侧背景填充
fn bg_line(line: Line<'static>, width: usize, bg: Style, pad: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), bg));
    }

    let mut content_w = pad;
    for sp in line.spans {
        // 用与 ratatui 落格一致的 grapheme 宽度：display_width 按 char 累加且
        // 给零宽字符/制表符兜底计数，会高估内容宽度 → 右侧少补背景形成缺口。
        content_w += grapheme_width(&sp.content);
        spans.push(Span::styled(sp.content, sp.style.patch(bg)));
    }

    if content_w < width {
        spans.push(Span::styled(" ".repeat(width - content_w), bg));
    }

    Line::from(spans)
}

/// 判断一行是否为真正的块间间隔行：内容为空**且无背景色**。
/// 背景 padding 行（Box 上下衬）属于块本身，不能算做与下一块的间隔
pub(crate) fn is_pure_gap(line: &Line<'static>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
        && !line.spans.iter().any(|s| s.style.bg.is_some())
}

/// 末尾非纯间隔行时追加纯空行
fn push_blank_gap(out: &mut Vec<Line<'static>>) {
    let ends_blank = out.last().map(is_pure_gap).unwrap_or(true);
    if !ends_blank {
        out.push(Line::from(""));
    }
}

/// 图片块上方的一行间隔：末尾已经是空行时不再补。
///
/// 与 [`push_blank_gap`] 的区别在于只看“是否空行”、不管背景：工具块 / 用户消息块里
/// 要沿用块背景（`bg`），否则色块会被这一行截断、露出终端底色；而块内那些空白的
/// 背景行（盒顶衬、图片预留行）本身已经起到了间隔作用，不能再叠一行。
fn push_image_gap(out: &mut Vec<Line<'static>>, width: usize, bg: Option<Style>) {
    let ends_blank = out
        .last()
        .map(|line| line.spans.iter().all(|s| s.content.trim().is_empty()))
        .unwrap_or(true);
    if ends_blank {
        return;
    }

    out.push(match bg {
        Some(bg) => bg_line(Line::from(""), width, bg, 1),
        None => Line::from(""),
    });
}

/// assistant 消息：文本 + thinking + 工具调用块 + stopReason 提示
#[allow(clippy::too_many_arguments)]
fn render_assistant(
    msg: &AgentMessage,
    width: usize,
    theme: &Theme,
    expansions: &[bool],
    cursor: &mut usize,
    blocks: &mut Vec<BlockSpan>,
    pending: &mut Vec<PendingTool>,
    tool_results: &HashMap<String, ToolResultView>,
    tool_started_at: &HashMap<String, u64>,
    nested_calls: &HashMap<String, Vec<NestedCallView>>,
    edit_map: &HashMap<String, EditRenderData>,
    images: &mut Vec<ImageSlot>,
    layout: ImageLayout,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut i = 0;
    let content = &msg.content;

    while i < content.len() {
        match &content[i] {
            ContentBlock::Text { text, .. } => {
                let t = text.trim();
                if !t.is_empty() {
                    out.extend(pad_markdown(t, width, theme, 1));
                }
                i += 1;

                push_blank_gap(&mut out);
            }
            ContentBlock::Thinking { .. } => {
                // 合并连续 thinking 块（一个 run 渲染为一个 section）
                let mut run = String::new();
                while i < content.len() {
                    if let ContentBlock::Thinking { thinking, .. } = &content[i] {
                        let t = thinking.trim();
                        if !t.is_empty() {
                            if !run.is_empty() {
                                run.push_str("\n\n");
                            }
                            run.push_str(t);
                        }
                        i += 1;
                    } else {
                        break;
                    }
                }

                if run.is_empty() {
                    continue;
                }

                // 渲染 thinking run（展开为斜体 markdown，折叠时仅显示 "Thinking..."）
                let index = *cursor;
                *cursor += 1;
                let start = out.len();
                out.extend(render_thinking_run(
                    &run,
                    expansions.get(index).copied().unwrap_or(false),
                    width,
                    theme,
                ));
                blocks.push(BlockSpan {
                    index,
                    start,
                    end: out.len(),
                });
                push_blank_gap(&mut out);
            }
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                let view = tool_results.get(id.as_str());
                let result = view.map(|v| (v.text.as_str(), v.is_error, v.duration_ms));
                // 结果态优先用 `details` 里的子调用记录（含最终耗时与 `models.*` 成本）；
                // 进行中工具用实时事件累积的行
                let nested = view
                    .map(|v| sub_calls_from_details(v.details.as_ref()))
                    .filter(|calls| !calls.is_empty())
                    .or_else(|| nested_calls.get(id.as_str()).cloned())
                    .unwrap_or_default();
                let started_at = tool_started_at.get(id.as_str()).copied();
                let edit = edit_map.get(id.as_str());
                let index = *cursor;
                *cursor += 1;
                let start = out.len();
                let images_before = images.len();
                out.extend(render_tool_block_inner(
                    name,
                    arguments,
                    result.map(|(t, e, _)| (t, e)),
                    result.and_then(|(_, _, d)| d),
                    started_at,
                    edit,
                    &nested,
                    width,
                    theme,
                    expansions.get(index).copied().unwrap_or(false),
                    view.map(|v| v.images.as_slice()).unwrap_or(&[]),
                    images,
                    layout,
                ));
                // 块内登记的图片槽位是相对**块**起点的行号，这里换算成相对条目起点：
                // 否则图片会被画到块的上方（吃掉上方间隔、盖住块内文本），块底预留行空着。
                for slot in images[images_before..].iter_mut() {
                    slot.line += start;
                }

                blocks.push(BlockSpan {
                    index,
                    start,
                    end: out.len(),
                });
                pending.push(PendingTool::from_call(
                    id,
                    name,
                    arguments.clone(),
                    started_at,
                ));
                i += 1;

                push_blank_gap(&mut out);
            }
            ContentBlock::Image { data, .. } => {
                push_image_block(&mut out, data, width, theme, layout, images, None);
                i += 1;

                push_blank_gap(&mut out);
            }
        }
    }

    // stopReason 提示
    append_stop_reason_hint(msg, theme, width, &mut out);

    push_blank_gap(&mut out);
    out
}

/// 渲染一段 thinking run：展开时渲染为斜体 markdown，折叠时仅显示 "Thinking..."
fn render_thinking_run(
    run: &str,
    visible: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let style = theme
        .style(THINKING_TEXT.0, THINKING_TEXT.1)
        .add_modifier(Modifier::ITALIC);
    if visible {
        let inner = width.saturating_sub(2).max(1);
        render_markdown_styled(run, inner, theme, style)
            .into_iter()
            .map(|l| pad_line(l, 1))
            .collect()
    } else {
        vec![Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled("Thinking...", style),
        ])]
    }
}

/// 渲染图片占位行
pub(super) fn render_image_placeholder(theme: &Theme) -> Line<'static> {
    Line::from(vec![Span::styled(
        " [image]",
        theme.style("muted", "#808080"),
    )])
}

/// 内联图片的渲染参数：开关 + 单元格像素尺寸（关闭时图片块整块不渲染）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLayout {
    /// 是否内联渲染图片（settings.json `showImages`）
    pub enabled: bool,
    /// 单元格像素尺寸 (宽, 高)，用于把图片像素尺寸等比换算成占用的列数与行数
    pub font: (u16, u16),
}

impl ImageLayout {
    /// 关闭内联图片：所有图片块整块不渲染。
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            font: (1, 1),
        }
    }
}

/// 一处已预留行的内联图片：相对条目起点的行号与目标单元格尺寸。
///
/// 槽位只带内容指纹与数据，不带已编码的协议数据——编码结果按指纹缓存在
/// [`image`](crate::modes::interactive::render::image) 模块里（后台线程编码），
/// 命中消息缓存时不会重复编码。
#[derive(Debug, Clone)]
pub struct ImageSlot {
    /// 相对条目起点（该条消息渲染行数组的首行）的行号，图片左上角所在行
    pub line: usize,
    /// 目标宽度（列）
    pub cols: u16,
    /// 目标高度（行）
    pub rows: u16,
    /// 图片内容指纹（协议缓存键，见 [`crate::utils::terminal_image::image_key`]）
    pub key: u64,
    /// 图片 base64 数据（不含 data URI 前缀）；Arc 共享避免逐帧复制
    pub data: Arc<str>,
    /// 透明补边要填的底色（halfblocks 无 alpha，补边不填色就是黑的）；`None` 表示不填
    pub pad: terminal_image::PadRgb,
}

/// 按消息宽度与单元格像素尺寸算出图片占用的 (列, 行)。
///
/// 盒子上限：宽度取 `min(可用宽度, 60)` 列，高度上限取「同宽正方形」换算出的行数。
/// 宽度不够或图片尺寸异常时返回 `None`（调用方回落 `[image]` 占位）。
fn image_cell_size(img_w: u32, img_h: u32, width: usize, font: (u16, u16)) -> Option<(u16, u16)> {
    let max_cols = width.saturating_sub(2).min(IMAGE_MAX_COLS) as u16;
    let max_rows = ((max_cols as f32 * font.0 as f32 / font.1.max(1) as f32).ceil() as u16).max(1);
    let (cols, rows) = terminal_image::fit_cell_size(img_w, img_h, font, max_cols, max_rows);

    (cols > 0 && rows > 0).then_some((cols, rows))
}

/// 追加一个图片块：启用内联图片且能算出尺寸时预留 `rows` 行（+ 可能的一行间隔）并登记槽位；
/// 关闭内联图片时**整块不渲染**；开启但算不出尺寸（图片数据坏了 / 宽度不够）时退回
/// `[image]` 占位行。
///
/// 块前空一行（见 [`push_image_gap`]）——图片直接贴在正文/耗时行下面太挤。
/// 预留行之后要不要再补一行间隔，看图片盒子底部的**透明**补边（库的 `Resize::Fit` 会把
/// 不足一格的余量补成透明像素：补边不显色，但**照样占位置**）：补边已经接近一行时不再补，
/// 否则图片之间会空出两行。
/// 补行与预留行同样式——用户消息要沿用消息背景，否则色块中间会断开露出终端底色
/// （不带背景的纯空行仍由 `push_blank_gap` 在消息尾部补）。
///
/// `bg` 为所在块的背景样式（用户消息带背景块，其余为 `None`）。预留行由图片覆盖，
/// 只是为了让布局、滚动与裁剪的行数在绘制前就已确定；`bg` 的底色同时作为 halfblocks
/// 的补边填充色（见 [`terminal_image::PadRgb`]）。
fn push_image_block(
    out: &mut Vec<Line<'static>>,
    data: &str,
    width: usize,
    theme: &Theme,
    layout: ImageLayout,
    images: &mut Vec<ImageSlot>,
    bg: Option<Style>,
) {
    if !layout.enabled {
        return;
    }

    push_image_gap(out, width, bg);

    if let Some((img_w, img_h)) = terminal_image::image_dimensions(data)
        && let Some((cols, rows)) = image_cell_size(img_w, img_h, width, layout.font)
    {
        let line = out.len();
        for _ in 0..rows {
            out.push(match bg {
                Some(bg) => bg_line(Line::from(""), width, bg, 1),
                None => Line::from(Span::styled(" ".repeat(width), Style::default())),
            });
        }

        let pad = terminal_image::image_bottom_pad(img_w, img_h, cols, rows, layout.font);
        if pad * 2 < layout.font.1.max(1) as u32 {
            out.push(match bg {
                Some(bg) => bg_line(Line::from(""), width, bg, 1),
                None => Line::from(""),
            });
        }

        images.push(ImageSlot {
            line,
            cols,
            rows,
            key: terminal_image::image_key(data),
            data: Arc::from(data),
            pad: bg
                .and_then(|style| style.bg)
                .and_then(terminal_image::style_bg_rgb),
        });
        return;
    }

    out.push(render_image_placeholder(theme));
}

/// 追加 stopReason 提示（truncated/aborted/error），超宽错误文本按消息宽度折行
fn append_stop_reason_hint(
    msg: &AgentMessage,
    theme: &Theme,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let aborted = msg.stop_reason.as_deref() == Some("aborted");
    let errored = msg.stop_reason.as_deref() == Some("error");
    let length = msg.stop_reason.as_deref() == Some("length");
    let hint: Option<String> = if length {
        Some("Response was truncated before completion.".to_string())
    } else if aborted {
        Some("Operation aborted".to_string())
    } else if errored {
        msg.error_message.clone().map(|e| format!("Error: {}", e))
    } else {
        None
    };

    if let Some(h) = hint {
        if !out.is_empty() {
            push_blank_gap(out);
        }
        let style = theme.style(ERROR.0, ERROR.1);
        // 超宽错误文本（provider 错误 JSON 等无换行长串）按消息宽度折行，不得横向截断
        let inner = width.saturating_sub(2).max(1);
        for wrapped in wrap_hint_line(&h, inner) {
            out.push(Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(wrapped, style),
            ]));
        }
    }
}

/// 把 markdown 渲染到「总宽 - pad」的内宽，再给每行左侧补齐 pad 个空格。
fn pad_markdown(text: &str, width: usize, theme: &Theme, pad: usize) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(pad).max(1);
    render_markdown(text, inner, theme)
        .into_iter()
        .map(|l| pad_line(l, pad))
        .collect()
}

/// 在行首插入 pad 个空格；pad 为 0 时原样返回。
fn pad_line(line: Line<'static>, pad: usize) -> Line<'static> {
    if pad == 0 {
        return line;
    }
    let mut spans = vec![Span::raw(" ".repeat(pad))];
    spans.extend(line.spans);
    Line::from(spans)
}

/// 工具调用块
/// - 头部：bash → `$ cmd`（toolTitle bold，timeout 后缀 muted）；
///   mcp → `server/tool`（`action=call`）或 `mcp <action>`；
///   其他 → 工具名 + 路径/参数摘要（无自定义调用头的工具按 key=value 显示参数）；
///   超宽时折行显示完整命令/路径（不截断）
/// - write：content 参数渲染在块内——折叠时显示前 10 行并提示
///   `... (N more lines, M total, ctrl+o/alt+click to expand)`，展开显示全部
/// - 输出：toolOutput 色；折叠时 bash 显示末尾 5 个**视觉行**（宽度折行后计数）
///   并提示 `... (N earlier lines, ctrl+o/alt+click to expand)`，
///   `mcp` 显示开头 5 行、其他工具显示开头 20 行并提示
///   `... (N more lines, ctrl+o/alt+click to expand)`
/// - 耗时：进行中（result=None 且 started_at 存在）显示 `Elapsed X.Xs`（muted，实时刷新）；
///   结果完成后显示 `Took X.Xs`（muted）
///
/// 扩展注册了该工具的渲染器（[`core::extensions::tool_renderer_for`]）时，整块内容交给它，
/// 这里只负责铺背景与按它给的 `preview_lines` 折叠；未注册/渲染器放弃接管才走上面的内置逻辑。
#[allow(clippy::too_many_arguments)]
pub fn render_tool_block(
    name: &str,
    args: &Value,
    result: Option<(&str, bool)>,
    duration_ms: Option<u64>,
    started_at_ms: Option<u64>,
    edit: Option<&EditRenderData>,
    nested: &[NestedCallView],
    width: usize,
    theme: &Theme,
    expand_all: bool,
) -> Vec<Line<'static>> {
    render_tool_block_inner(
        name,
        args,
        result,
        duration_ms,
        started_at_ms,
        edit,
        nested,
        width,
        theme,
        expand_all,
        &[],
        &mut Vec::new(),
        ImageLayout::disabled(),
    )
}

/// [`render_tool_block`] 的实现：额外接收工具结果里的图片（`result_images`，base64 数据）
/// 与图片渲染参数，把预留的图片行登记进 `images`（相对本块起点）。
#[allow(clippy::too_many_arguments)]
fn render_tool_block_inner(
    name: &str,
    args: &Value,
    result: Option<(&str, bool)>,
    duration_ms: Option<u64>,
    started_at_ms: Option<u64>,
    edit: Option<&EditRenderData>,
    nested: &[NestedCallView],
    width: usize,
    theme: &Theme,
    expand_all: bool,
    result_images: &[Arc<str>],
    images: &mut Vec<ImageSlot>,
    layout: ImageLayout,
) -> Vec<Line<'static>> {
    let bg_key = match result {
        None => TOOL_PENDING_BG,
        Some((_, true)) => TOOL_ERROR_BG,
        Some((_, false)) => TOOL_SUCCESS_BG,
    };
    let bg = theme.style_bg(bg_key.0, bg_key.1);
    let title = theme
        .style(TOOL_TITLE.0, TOOL_TITLE.1)
        .add_modifier(Modifier::BOLD);
    let muted = theme.style("muted", "#808080");
    let inner = width.saturating_sub(2).max(1);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(" ".repeat(width), bg)));

    // 扩展渲染器优先：命中即整块（头部 + 输出）交给它，这里只铺背景与折叠。
    // edit 工具的内置 diff 不参与（宿主对它有专门渲染，扩展拿不到 diff 数据）。
    if name != "edit"
        && let Some(rendered) = render_extension_tool_block(
            name,
            args,
            result,
            duration_ms,
            started_at_ms,
            theme,
            bg,
            width,
            inner,
            expand_all,
        )
    {
        lines.extend(rendered);
        lines.push(Line::from(Span::styled(" ".repeat(width), bg))); // Box 下 padding
        return lines;
    }

    // 头部超宽折行成多行时逐行铺背景（不再截断命令/路径）
    for hl in tool_call_header(name, args, &title, &muted, inner, expand_all) {
        lines.push(bg_line(hl, width, bg, 1));
    }

    // edit 工具：头部 + 预览/结果 diff，不显示原始 PATCH 文本
    if name == "edit"
        && let Some(edit) = edit
    {
        lines.extend(render_edit_body(
            edit,
            result,
            duration_ms,
            theme,
            bg,
            width,
            inner,
        ));
    } else {
        // write 工具：content 参数渲染进调用块
        lines.extend(render_tool_write_content(
            name, args, theme, bg, width, inner, expand_all,
        ));

        // 输出预览 + 嵌套调用（codemode 脚本）+ 耗时
        lines.extend(render_tool_output(
            name,
            result,
            duration_ms,
            started_at_ms,
            nested,
            theme,
            bg,
            width,
            inner,
            expand_all,
        ));
    }

    // 工具结果里的图片（read 工具截图等）：正文之后，沿用工具块背景。
    // 不受折叠预览行数限制：图片本身就是这些结果要展示的东西。
    for data in result_images {
        push_image_block(&mut lines, data, width, theme, layout, images, Some(bg));
    }

    lines.push(Line::from(Span::styled(" ".repeat(width), bg))); // Box 下 padding
    lines
}

/// edit 工具块主体：成功后渲染 `details.diff` 或 call 阶段预览 diff；
/// 出错时显示错误文本；最后保留通用耗时行 `Took X.Xs`。
fn render_edit_body(
    edit: &EditRenderData,
    result: Option<(&str, bool)>,
    duration_ms: Option<u64>,
    theme: &Theme,
    bg: Style,
    width: usize,
    inner: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    match result {
        Some((text, true)) => {
            let err_style = theme.style(ERROR.0, ERROR.1);
            lines.push(bg_line(Line::from(""), width, bg, 1));

            for l in wrap_text_lines(text.trim(), inner) {
                lines.push(bg_line(
                    Line::from(Span::styled(l, err_style)),
                    width,
                    bg,
                    1,
                ));
            }
        }
        _ => {
            // 完成时优先 result_diff，否则用 call 阶段预览
            let diff = edit.result_diff.clone().or_else(|| edit.preview.clone());
            if let Some(d) = diff {
                lines.push(bg_line(Line::from(""), width, bg, 1));
                for l in render_diff(&d, theme, inner) {
                    lines.push(bg_line(l, width, bg, 1));
                }
            } else if let Some((text, false)) = result
                && !text.trim().is_empty()
            {
                // 成功但无 diff 元数据时兜底显示原始文本
                let output = theme.style(TOOL_OUTPUT.0, TOOL_OUTPUT.1);
                lines.push(bg_line(Line::from(""), width, bg, 1));

                for l in wrap_text_lines(text.trim(), inner) {
                    lines.push(bg_line(Line::from(Span::styled(l, output)), width, bg, 1));
                }
            }
        }
    }

    if let Some(dur) = duration_ms {
        let muted = theme.style("muted", "#808080");
        lines.push(bg_line(Line::from(""), width, bg, 1));
        let took = format!("Took {}", format_tool_duration(dur));
        lines.push(bg_line(Line::from(Span::styled(took, muted)), width, bg, 1));
    }

    lines
}

/// 把 diff 字符串渲染成带行号 gutter + 配色的行
/// - `-` 行：toolDiffRemoved；`+` 行：toolDiffAdded；上下文/省略号：toolDiffContext；
/// - 恰好 1 删 1 增（单行替换）时做词级 diff，变动片段用 REVERSED 反色。
fn render_diff(diff_text: &str, theme: &Theme, inner: usize) -> Vec<Line<'static>> {
    let added = theme.style(DIFF_ADDED.0, DIFF_ADDED.1);
    let removed = theme.style(DIFF_REMOVED.0, DIFF_REMOVED.1);
    let context = theme.style(DIFF_CONTEXT.0, DIFF_CONTEXT.1);
    let added_inv = added.add_modifier(Modifier::REVERSED);
    let removed_inv = removed.add_modifier(Modifier::REVERSED);

    let raw: Vec<&str> = diff_text.split('\n').collect();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut i = 0usize;
    while i < raw.len() {
        let prefix = diff_prefix(raw[i]);
        if prefix == Some('-') {
            let mut removed_lines: Vec<(&str, &str)> = Vec::new();
            while i < raw.len() && diff_prefix(raw[i]) == Some('-') {
                removed_lines.push(split_diff_line(raw[i]));
                i += 1;
            }

            let mut added_lines: Vec<(&str, &str)> = Vec::new();
            while i < raw.len() && diff_prefix(raw[i]) == Some('+') {
                added_lines.push(split_diff_line(raw[i]));
                i += 1;
            }

            if removed_lines.len() == 1 && added_lines.len() == 1 {
                let (num, content) = removed_lines[0];
                let (anum, acontent) = added_lines[0];
                let (rspans, aspans) = intra_line_spans(
                    content,
                    acontent,
                    &removed,
                    &added,
                    &removed_inv,
                    &added_inv,
                );

                let mut r = vec![Span::styled(format!("-{} ", num), removed)];
                r.extend(rspans);
                let mut a = vec![Span::styled(format!("+{} ", anum), added)];
                a.extend(aspans);
                out.extend(wrap_spans(r, inner));
                out.extend(wrap_spans(a, inner));
            } else {
                for (num, content) in removed_lines {
                    out.extend(wrap_styled(
                        format!("-{} {}", num, content),
                        &removed,
                        inner,
                    ));
                }

                for (num, content) in added_lines {
                    out.extend(wrap_styled(format!("+{} {}", num, content), &added, inner));
                }
            }
        } else if prefix == Some('+') {
            let (num, content) = split_diff_line(raw[i]);
            out.extend(wrap_styled(format!("+{} {}", num, content), &added, inner));
            i += 1;
        } else {
            out.extend(wrap_styled(raw[i].to_string(), &context, inner));
            i += 1;
        }
    }
    out
}

/// 取 diff 行前缀字符（`-` / `+` / 空格），非 diff 行返回 None。
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

    // 跳过左填充空格（右对齐行号列）
    while idx < bytes.len() && bytes[idx] == b' ' {
        idx += 1;
    }

    // 数字
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }

    // 含左填充空格的完整行号列
    let num = &rest[..idx];
    let mut content = &rest[idx..];
    content = content.strip_prefix(' ').unwrap_or(content);
    (num, content)
}

/// 单词级 diff：按空白/非空白切 token，`similar` 求差，变动片段反色。
/// 返回 (删除行 spans, 新增行 spans)；首 token 的前导空白不反色。
#[allow(clippy::too_many_arguments)]
fn intra_line_spans(
    old: &str,
    new: &str,
    removed: &Style,
    added: &Style,
    removed_inv: &Style,
    added_inv: &Style,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let old_toks = word_tokens(old);
    let new_toks = word_tokens(new);
    let diff = similar::TextDiff::from_slices(&old_toks, &new_toks);
    let mut rspans: Vec<Span<'static>> = Vec::new();
    let mut aspans: Vec<Span<'static>> = Vec::new();
    let mut first_removed = true;
    let mut first_added = true;
    for change in diff.iter_all_changes() {
        let val: &str = change.value();
        let is_ws = val.chars().all(char::is_whitespace);
        match change.tag() {
            similar::ChangeTag::Delete => {
                let v = if first_removed {
                    let s = val.trim_start_matches(char::is_whitespace);
                    first_removed = false;
                    s
                } else {
                    val
                };
                if !v.is_empty() {
                    rspans.push(Span::styled(
                        v.to_string(),
                        if is_ws { *removed } else { *removed_inv },
                    ));
                }
            }
            similar::ChangeTag::Insert => {
                let v = if first_added {
                    let s = val.trim_start_matches(char::is_whitespace);
                    first_added = false;
                    s
                } else {
                    val
                };

                if !v.is_empty() {
                    aspans.push(Span::styled(
                        v.to_string(),
                        if is_ws { *added } else { *added_inv },
                    ));
                }
            }

            similar::ChangeTag::Equal => {
                rspans.push(Span::styled(val.to_string(), *removed));
                aspans.push(Span::styled(val.to_string(), *added));
            }
        }
    }
    (rspans, aspans)
}

/// 把字符串按空白/非空白切分成 token
fn word_tokens(s: &str) -> Vec<&str> {
    let mut toks: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut last_ws: Option<bool> = None;

    for (idx, c) in s.char_indices() {
        let ws = c.is_whitespace();
        if let Some(lw) = last_ws
            && lw != ws
        {
            toks.push(&s[start..idx]);
            start = idx;
        }
        last_ws = Some(ws);
    }

    if start < s.len() {
        toks.push(&s[start..]);
    }
    toks
}

/// 单行纯样式文本折行
fn wrap_styled(text: String, style: &Style, width: usize) -> Vec<Line<'static>> {
    wrap_text_lines(&text, width)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, *style)))
        .collect()
}

/// 多 span 按显示宽度折行（保留各自样式；长 span 内部按字符断）
fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::from(spans)];
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    for sp in spans {
        let mut byte_start = 0usize;
        let mut w = 0usize;
        for (bi, ch) in sp.content.char_indices() {
            let cw = char_width(ch);
            if cw == 0 {
                continue;
            }

            if cur_w + w + cw > width {
                if w > 0 {
                    cur.push(Span::styled(
                        sp.content[byte_start..bi].to_string(),
                        sp.style,
                    ));
                }
                if !cur.is_empty() {
                    out.push(Line::from(std::mem::take(&mut cur)));
                }
                cur_w = 0;
                byte_start = bi;
                w = 0;
            }
            w += cw;
        }

        if w > 0 {
            cur.push(Span::styled(sp.content[byte_start..].to_string(), sp.style));
        }
        cur_w += w;
    }
    if !cur.is_empty() {
        out.push(Line::from(cur));
    }
    out
}

/// 渲染 write 工具的 content 参数（折叠时前 10 行并提示，展开显示全部）
fn render_tool_write_content(
    name: &str,
    args: &serde_json::Value,
    theme: &Theme,
    bg: Style,
    width: usize,
    inner: usize,
    expand_all: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if name == "write"
        && let Some(content) = args.get("content").and_then(|v| v.as_str())
    {
        let sanitized = sanitize_output(content);
        let content = sanitized.trim();
        if !content.is_empty() {
            let output = theme.style(TOOL_OUTPUT.0, TOOL_OUTPUT.1);
            let muted = theme.style("muted", "#808080");
            lines.push(bg_line(Line::from(""), width, bg, 1));
            let content_lines: Vec<&str> = content.lines().collect();
            let total = content_lines.len();
            let preview: Vec<&str> = if expand_all {
                content_lines.clone()
            } else {
                content_lines[..content_lines.len().min(WRITE_PREVIEW_LINES)].to_vec()
            };

            for l in preview {
                for wrapped in wrap_text_lines(l, inner) {
                    lines.push(bg_line(
                        Line::from(Span::styled(wrapped, output)),
                        width,
                        bg,
                        1,
                    ));
                }
            }

            if !expand_all && total > WRITE_PREVIEW_LINES {
                let more = total - WRITE_PREVIEW_LINES;
                let hint = format!(
                    "... ({} more lines, {} total, ctrl+o/alt+click to expand)",
                    more, total
                );
                lines.push(bg_line(Line::from(Span::styled(hint, muted)), width, bg, 1));
            }
        }
    }
    lines
}

/// 渲染工具调用结果：折叠预览（bash 按视觉行取末尾，其他工具取开头）+ 耗时。
/// 进行中（result=None）且有启动时间：显示 `Elapsed X.Xs`；完成：显示 `Took X.Xs`。
#[allow(clippy::too_many_arguments)]
fn render_tool_output(
    name: &str,
    result: Option<(&str, bool)>,
    duration_ms: Option<u64>,
    started_at_ms: Option<u64>,
    nested: &[NestedCallView],
    theme: &Theme,
    bg: Style,
    width: usize,
    inner: usize,
    expand_all: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // 进行中：显示实时耗时（每帧重渲染刷新；由 app.rs 按 1s 节流强制失效缓存）
    if result.is_none()
        && let Some(started) = started_at_ms
    {
        let muted = theme.style("muted", "#808080");
        let elapsed = now_ms().saturating_sub(started);
        lines.push(bg_line(Line::from(""), width, bg, 1));
        let took = format!("Elapsed {}", format_tool_duration(elapsed));
        lines.push(bg_line(Line::from(Span::styled(took, muted)), width, bg, 1));
        lines.extend(render_sub_calls(
            nested, expand_all, theme, bg, width, inner,
        ));
        return lines;
    }

    if let Some((out_text, is_err)) = result {
        let output = theme.style(TOOL_OUTPUT.0, TOOL_OUTPUT.1);
        let muted = theme.style("muted", "#808080");
        // 先剥离 ANSI / 展开 tab，避免转义与制表符造成宽度口径漂移
        let sanitized = sanitize_output(out_text);
        let text = sanitized.trim();

        if !text.is_empty() {
            lines.push(bg_line(Line::from(""), width, bg, 1));
            let out_lines: Vec<&str> = text.lines().collect();
            let is_bash = name == "bash";
            let style = if is_err && !expand_all {
                theme.style(ERROR.0, ERROR.1)
            } else {
                output
            };

            // 折叠预览：bash 按视觉行（宽度折行后）取末尾，其他工具取开头原始行
            // hint_before：bash 提示行在预览前；hint_after：非 bash 提示行在预览后
            let (preview, hint_before, hint_after): (Vec<String>, Option<String>, Option<String>) =
                if expand_all {
                    (
                        out_lines
                            .iter()
                            .flat_map(|l| wrap_text_lines(l, inner))
                            .collect(),
                        None,
                        None,
                    )
                } else if is_bash {
                    let visual: Vec<String> = out_lines
                        .iter()
                        .flat_map(|l| wrap_text_lines(l, inner))
                        .collect();
                    let skipped = visual.len().saturating_sub(BASH_PREVIEW_LINES);
                    if skipped > 0 {
                        (
                            visual[skipped..].to_vec(),
                            Some(format!(
                                "... ({} earlier lines, ctrl+o/alt+click to expand)",
                                skipped
                            )),
                            None,
                        )
                    } else {
                        (visual, None, None)
                    }
                } else {
                    // MCP 结果折叠到 5 行，其余工具 20 行。
                    // 与 bash 一致按**视觉行**（宽度折行后）取开头，否则一条超长逻辑行（minified JSON 等）会折出整屏。
                    let preview_lines = if name == "mcp" {
                        MCP_PREVIEW_LINES
                    } else {
                        TOOL_PREVIEW_LINES
                    };

                    let visual: Vec<String> = out_lines
                        .iter()
                        .flat_map(|l| wrap_text_lines(l, inner))
                        .collect();
                    if visual.len() > preview_lines {
                        (
                            visual[..preview_lines].to_vec(),
                            None,
                            Some(format!(
                                "... ({} more lines, ctrl+o/alt+click to expand)",
                                visual.len() - preview_lines
                            )),
                        )
                    } else {
                        (visual, None, None)
                    }
                };

            if let Some(h) = hint_before {
                lines.push(bg_line(Line::from(Span::styled(h, muted)), width, bg, 1));
            }

            for l in preview {
                lines.push(bg_line(Line::from(Span::styled(l, style)), width, bg, 1));
            }

            if let Some(h) = hint_after {
                lines.push(bg_line(Line::from(Span::styled(h, muted)), width, bg, 1));
            }
        }

        // 子调用（codemode 脚本调起的嵌套工具，以及脚本内带成本的 `models.*` 调用）
        lines.extend(render_sub_calls(
            nested, expand_all, theme, bg, width, inner,
        ));

        // 耗时
        if let Some(dur) = duration_ms {
            lines.push(bg_line(Line::from(""), width, bg, 1));
            let took = format!("Took {}", format_tool_duration(dur));
            lines.push(bg_line(Line::from(Span::styled(took, muted)), width, bg, 1));
        }
    }
    lines
}

/// 工具块里的子调用行（codemode 脚本调起的嵌套工具与脚本内 `models.*` 调用）。
///
/// 形态对齐 pi 的 codemode renderer：`<状态图标> name  args  duration  cost`，
/// 图标取 `…`（进行中）/ `✓`（成功）/ `✗`（失败），耗时取结果态上报值或按
/// [`NestedCallView::started_at_ms`] 实时计算；折叠时只显示末尾
/// [`SUB_CALL_PREVIEW_COUNT`] 条并在前面给省略提示，带成本的调用多于 1 条时
/// 追加 `Model calls: <合计>` 汇总行（合计覆盖全部调用，不只可见的那些）。
fn render_sub_calls(
    calls: &[NestedCallView],
    expanded: bool,
    theme: &Theme,
    bg: Style,
    width: usize,
    inner: usize,
) -> Vec<Line<'static>> {
    if calls.is_empty() {
        return Vec::new();
    }

    let muted = theme.style("muted", "#808080");
    let dim = theme.style("dim", "#808080");
    let running = theme.style("warning", "#e5c07b");
    let success = theme.style("success", "#98c379");
    let error = theme.style("error", "#e06c75");

    let shown = if expanded || calls.len() <= SUB_CALL_PREVIEW_COUNT {
        calls
    } else {
        &calls[calls.len() - SUB_CALL_PREVIEW_COUNT..]
    };

    let mut lines = Vec::new();
    let hidden = calls.len() - shown.len();
    if hidden > 0 {
        lines.push(bg_line(
            Line::from(Span::styled(
                format!("... ({hidden} earlier calls, ctrl+o/alt+click to expand)"),
                muted,
            )),
            width,
            bg,
            1,
        ));
    }

    for call in shown {
        let duration = match (call.duration_ms, call.started_at_ms) {
            (Some(ms), _) => Some(format_tool_duration(ms)),
            (None, Some(started)) => Some(format_tool_duration(now_ms().saturating_sub(started))),
            (None, None) => None,
        };
        let (mark, mark_style) = match call.is_error {
            None => (DEF_ELLIPSIS, running),
            Some(true) => (DEF_FAILED, error),
            Some(false) => (DEF_DONE, success),
        };

        let mut spans = vec![
            Span::styled(format!("{mark} "), mark_style),
            Span::styled(call.name.clone(), muted),
        ];
        if !call.summary.is_empty() {
            spans.push(Span::styled(format!("  {}", call.summary), muted));
        }
        if let Some(duration) = duration {
            spans.push(Span::styled(format!("  {duration}"), dim));
        }
        if let Some(cost) = call.cost.filter(|cost| *cost != 0.0) {
            spans.push(Span::styled(format!("  {}", format_cost_usd(cost)), dim));
        }

        for line in soft_wrap_spans(spans, inner) {
            lines.push(bg_line(line, width, bg, 1));
        }
    }

    // 汇总行：折叠行隐藏了更早的调用，所以合计覆盖全部调用
    let priced: Vec<f64> = calls
        .iter()
        .filter_map(|call| call.cost)
        .filter(|cost| *cost != 0.0)
        .collect();

    if priced.len() > 1 {
        let total: f64 = priced.iter().sum();
        lines.push(bg_line(
            Line::from(Span::styled(
                format!("Model calls: {}", format_cost_usd(total)),
                muted,
            )),
            width,
            bg,
            1,
        ));
    }

    lines
}

/// 工具调用块头部：超宽时折行保留完整命令/路径（不截断、不溢出），返回一行或多行。
///
/// - `bash`：整行 title 样式（`$ cmd (timeout Ns)`）；
/// - `mcp`：`server/tool` 标题 + 参数（见 [`mcp_call_header`]）；
/// - [`CUSTOM_CALL_HEADER_TOOLS`]：title + 路径/pattern + limit(muted)；
/// - 其余工具：pi 的通用形式（[`generic_call_header`]）。
fn tool_call_header(
    name: &str,
    args: &Value,
    title: &Style,
    muted: &Style,
    inner: usize,
    expanded: bool,
) -> Vec<Line<'static>> {
    if name == "bash" {
        let cmd = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("...");
        let mut text = format!("$ {}", sanitize_output(cmd).trim_end());
        if let Some(t) = args.get("timeout").and_then(|v| v.as_f64()) {
            text.push_str(&format!(" (timeout {}s)", t));
        }
        return soft_wrap_spans(vec![Span::styled(text, *title)], inner);
    }

    if name == "mcp" {
        return mcp_call_header(args, title, muted, inner, expanded);
    }

    if !CUSTOM_CALL_HEADER_TOOLS.contains(&name) {
        return generic_call_header(name, Some(args), title, muted, inner, expanded);
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(name.to_string(), *title));
    let path = args
        .get("path")
        .or_else(|| args.get("file_path"))
        .and_then(|v| v.as_str());
    let pattern = args.get("pattern").and_then(|v| v.as_str());

    if let Some(p) = path {
        spans.push(Span::raw(format!(" {}", p)));
    } else if name == "ls" {
        spans.push(Span::raw(" .")); // 空路径回退
    } else if let Some(p) = pattern {
        spans.push(Span::raw(format!(" {}", p)));
    }

    if let Some(lim) = args.get("limit").and_then(|v| v.as_i64()) {
        spans.push(Span::styled(format!(" (limit {})", lim), *muted));
    }

    soft_wrap_spans(spans, inner)
}

/// `mcp` 代理工具的调用头：`action=call` 时标题为 `server/tool` 并以该工具自身的参数
/// 作为键值对；其余动作标题为 `mcp <action>`，参数为除 `action` 之外的实参。
fn mcp_call_header(
    args: &Value,
    title: &Style,
    muted: &Style,
    inner: usize,
    expanded: bool,
) -> Vec<Line<'static>> {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");

    if action == "call" {
        let server = args.get("server").and_then(Value::as_str).unwrap_or("");
        let tool = args.get("name").and_then(Value::as_str).unwrap_or("");
        let label = if server.is_empty() || tool.is_empty() {
            "mcp".to_string()
        } else {
            format!("{server}/{tool}")
        };
        return generic_call_header(&label, args.get("arguments"), title, muted, inner, expanded);
    }

    let label = if action.is_empty() {
        "mcp".to_string()
    } else {
        format!("mcp {action}")
    };

    let rest = args.as_object().map(|map| {
        Value::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != "action")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        )
    });
    generic_call_header(&label, rest.as_ref(), title, muted, inner, expanded)
}

/// 标题 + 参数。折叠时参数是同一行内的 `key=value`（超出 [`COLLAPSED_ARGS_CHARS`] 截断），
/// 展开时每个参数一行 `  key: value`（字符串原样，其余按 JSON 缩进，续行再缩进 4 空格）。
/// `args` 为 `None` / 空对象时只有标题行。
fn generic_call_header(
    label: &str,
    args: Option<&Value>,
    title: &Style,
    muted: &Style,
    inner: usize,
    expanded: bool,
) -> Vec<Line<'static>> {
    let entries: Vec<(&str, &Value)> = match args {
        None => Vec::new(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| (key.as_str(), value))
            .collect(),
        Some(other) => vec![("args", other)],
    };
    let head = Span::styled(label.to_string(), *title);
    if entries.is_empty() {
        return soft_wrap_spans(vec![head], inner);
    }

    if !expanded {
        let pairs = entries
            .iter()
            .map(|(key, value)| format!("{key}={}", compact_json(value)))
            .collect::<Vec<_>>()
            .join(" ");
        let preview = if pairs.chars().count() > COLLAPSED_ARGS_CHARS {
            format!(
                "{}...",
                pairs
                    .chars()
                    .take(COLLAPSED_ARGS_CHARS - 3)
                    .collect::<String>()
            )
        } else {
            pairs
        };
        return soft_wrap_spans(
            vec![head, Span::raw(" "), Span::styled(preview, *muted)],
            inner,
        );
    }

    let mut lines = soft_wrap_spans(vec![head], inner);
    for (key, value) in entries {
        for (index, segment) in expanded_arg_segments(key, value).into_iter().enumerate() {
            let indent = if index == 0 {
                ARG_INDENT
            } else {
                ARG_CONTINUATION_INDENT
            };
            lines.extend(soft_wrap_spans(
                vec![Span::styled(format!("{indent}{segment}"), *muted)],
                inner,
            ));
        }
    }
    lines
}

/// `serde_json` 的紧凑序列化
pub fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// 展开态单个参数的显示片段：首段含 `key: value`，其余段为多行值的续行
fn expanded_arg_segments(key: &str, value: &Value) -> Vec<String> {
    let raw = match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };

    let normalized = raw.replace('\t', "   ").replace('\r', "");
    let mut segments: Vec<String> = normalized.split('\n').map(String::from).collect();
    if let Some(first) = segments.first_mut() {
        *first = format!("{key}: {first}");
    }
    segments
}

/// 带样式折行（词级优先）：优先在空白断点换行，单个超长 token（无空白的路径/长词）
/// 按字符硬断。与 wrap_spans 的纯字符断行不同，避免把 `(limit 5)`、`src/lib` 之类劈散。
/// 返回的每行宽度均不超过 `width`（width==0 时原样单行返回）。
fn soft_wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::from(spans)];
    }

    let mut toks: Vec<(Style, String)> = Vec::new();
    for sp in spans {
        for w in word_tokens(&sp.content) {
            toks.push((sp.style, w.to_string()));
        }
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    for (style, tok) in toks {
        let tw = display_width(&tok);
        if tw > width {
            // 单 token 超宽：先落当前行，再按字符硬断
            if !cur.is_empty() {
                out.push(Line::from(std::mem::take(&mut cur)));
                cur_w = 0;
            }
            for piece in wrap_line(&tok, width) {
                out.push(Line::from(Span::styled(piece, style)));
            }
            continue;
        }

        if cur_w + tw > width && !cur.is_empty() {
            out.push(Line::from(std::mem::take(&mut cur)));
            cur_w = 0;
        }

        cur.push(Span::styled(tok, style));
        cur_w += tw;
    }

    if !cur.is_empty() {
        out.push(Line::from(cur));
    }
    out
}

/// toolResult 消息：更新对应 PendingTool；无匹配时输出折叠行
fn render_tool_result(
    msg: &AgentMessage,
    _width: usize,
    theme: &Theme,
    _expand_all: bool,
    pending: &mut [PendingTool],
) -> Vec<Line<'static>> {
    let id = msg.tool_call_id.as_deref().unwrap_or("");
    let text = msg.text();
    let is_err = msg.is_error;

    if let Some(tool) = pending.iter_mut().find(|t| t.id == id) {
        tool.result = Some(text);
        tool.is_error = Some(is_err);
        tool.duration_ms = msg.duration_ms;
        // 结果已落地：清除进行中计时，避免后续渲染仍判为 Elapsed 而强制重建
        tool.started_at_ms = None;
        return Vec::new();
    }

    // 无匹配（历史被压缩等）：折叠显示
    let (summary, _) = truncate_display(&text, 120);
    let style = theme.style("dim", "#666666");
    vec![Line::from(vec![
        Span::styled(format!("{DEF_NESTED} "), style),
        Span::styled(summary, style),
    ])]
}

/// compaction / branch summary
fn render_summary(
    text: &str,
    is_compaction: bool,
    width: usize,
    theme: &Theme,
    expand: bool,
    blocks: &mut Vec<BlockSpan>,
) -> Vec<Line<'static>> {
    let label = if is_compaction {
        "[compaction]"
    } else {
        "[branch]"
    };
    let label_style = theme
        .style(CUSTOM_MSG_LABEL.0, CUSTOM_MSG_LABEL.1)
        .add_modifier(Modifier::BOLD);
    let text_style = theme.style(CUSTOM_MSG_TEXT.0, CUSTOM_MSG_TEXT.1);
    let bg = theme.style_bg(CUSTOM_MSG_BG.0, CUSTOM_MSG_BG.1);
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Box(1,1)：上下 padding 背景行
    lines.push(Line::from(Span::styled(" ".repeat(width), bg)));
    lines.push(bg_line(
        Line::from(vec![Span::styled(label.to_string(), label_style)]),
        width,
        bg,
        1,
    ));
    lines.push(Line::from(Span::styled(" ".repeat(width), bg))); // label 后 Spacer（背景）
    //
    if expand {
        let inner = width.saturating_sub(2).max(1);
        let md = if is_compaction {
            format!("**Compacted context**\n\n{}", text)
        } else {
            format!("**Branch Summary**\n\n{}", text)
        };

        for l in render_markdown_styled(&md, inner, theme, text_style) {
            lines.push(bg_line(l, width, bg, 1));
        }
    } else {
        let hint = if is_compaction {
            "Compacted context (Ctrl+O/Alt+click to expand)"
        } else {
            "Branch summary (Ctrl+O/Alt+click to expand)"
        };

        lines.push(bg_line(
            Line::from(vec![Span::styled(hint, text_style)]),
            width,
            bg,
            1,
        ));
    }

    lines.push(Line::from(Span::styled(" ".repeat(width), bg))); // Box 下 padding
    blocks.push(BlockSpan {
        index: 0,
        start: 0,
        end: lines.len(),
    });
    lines
}

/// 流式内容渲染
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_streaming(
    thinking: &str,
    tools: &[String],
    text: &str,
    width: usize,
    theme: &Theme,
    expand_all: bool,
    show_thinking: bool,
    pending: &mut Vec<PendingTool>,
    _tool_started_at: &HashMap<String, u64>,
    links: &mut Vec<LinkSpan>,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    // 流式 thinking：斜体
    if !thinking.trim().is_empty() {
        if show_thinking {
            let style = theme
                .style(THINKING_TEXT.0, THINKING_TEXT.1)
                .add_modifier(Modifier::ITALIC);
            let inner = width.saturating_sub(2).max(1);
            out.extend(
                render_markdown_styled(thinking, inner, theme, style)
                    .into_iter()
                    .map(|l| pad_line(l, 1)),
            );
        } else {
            let style = theme
                .style(THINKING_TEXT.0, THINKING_TEXT.1)
                .add_modifier(Modifier::ITALIC);
            out.push(Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled("Thinking...", style),
            ]));
        }

        // 流式块间也保持 Spacer 间隔（thinking → 工具块 → 文本）
        if !tools.is_empty() || !text.trim().is_empty() {
            push_blank_gap(&mut out);
        }
    }

    // 流式工具调用（toolcall_end 已带 name 与参数摘要）
    for (ti, tool) in tools.iter().enumerate() {
        let (name, args) = tool
            .split_once(' ')
            .map(|(n, a)| (n.to_string(), a.to_string()))
            .unwrap_or_else(|| (tool.clone(), String::new()));
        let args_val = serde_json::from_str(&args).unwrap_or_else(|_| serde_json::json!(&args));
        out.extend(render_tool_block(
            &name,
            &args_val,
            None,
            None,
            None,
            None,
            &[],
            width,
            theme,
            expand_all,
        ));

        pending.push(PendingTool {
            id: format!("streaming-{}", pending.len()),
            is_error: None,
            result: None,
            duration_ms: None,
            started_at_ms: None,
        });

        // 工具块后仍有流式文本时补间隔
        if ti + 1 < tools.len() || !text.trim().is_empty() {
            push_blank_gap(&mut out);
        }
    }

    // 流式文本：markdown 无背景
    if !text.trim().is_empty() {
        let inner = width.saturating_sub(2).max(1);
        out.extend(
            render_markdown(text, inner, theme)
                .into_iter()
                .map(|l| pad_line(l, 1)),
        );
    }

    push_blank_gap(&mut out);
    // 链接区域在最终行上扫描（流式段里 markdown 的缩进已应用）。
    links.extend(collect_link_spans(&out, theme));
    out
}

/// 按显示宽度折行（不跨单词，长词按字符断）
fn wrap_text_lines(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split('\n') {
        for l in wrap_line(raw, width) {
            out.push(l);
        }
    }
    out
}

/// 错误提示折行：优先在标点/空格边界断行。provider 错误 JSON 无空格，
/// 逐字符硬断会把词劈成 “Co nsole Go” 这类碎片，这里按 `, : { } [ ] " ;` 切 token 贪心合并。
fn wrap_hint_line(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split('\n') {
        if display_width(raw) <= width {
            out.push(raw.to_string());
            continue;
        }
        let mut tokens: Vec<String> = Vec::new();
        let mut cur = String::new();
        for c in raw.chars() {
            cur.push(c);
            if matches!(c, ' ' | ',' | ':' | '{' | '}' | '[' | ']' | '"' | ';') {
                tokens.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            tokens.push(cur);
        }
        let mut line = String::new();
        let mut line_w = 0usize;
        for tok in tokens {
            let tw = display_width(&tok);
            if !line.is_empty() && line_w + tw > width {
                out.push(line.trim_end().to_string());
                line = String::new();
                line_w = 0;
            }
            if line.is_empty() && tw > width {
                for piece in wrap_line(&tok, width) {
                    out.push(piece);
                }
                continue;
            }
            line.push_str(&tok);
            line_w += tw;
        }
        if !line.is_empty() {
            out.push(line.trim_end().to_string());
        }
    }
    out
}

/// 时间线上的一项引用（消息或系统提示；排序规则与 render_all_messages 一致）
#[derive(Clone, Copy)]
pub(crate) enum TimelineRef<'a> {
    Msg(&'a AgentMessage),
    Sys(&'a SysMsg),
}

/// 可折叠块在内容坐标下的命中区：全局行范围 `[start, end)`（半开）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClickTarget {
    /// 命中区起始全局行号（含该行）。
    pub start: usize,
    /// 命中区结束全局行号（不含该行）。
    pub end: usize,
    /// 被点击的可折叠块稳定标识。
    pub id: BlockId,
    /// 缺省展开态（点击切换时 `cur = override.unwrap_or(default)`）
    pub default_expanded: bool,
}

/// 构建合并时间线（消息 + 系统提示按时间戳排序；seq 保证同毫秒时保持原有顺序）
/// 返回 (timestamp, seq, 引用)；sys 的 seq 使用 100_000_000 偏移（与 render_all_messages 一致）。
pub(crate) fn timeline_items<'a>(
    messages: &'a [AgentMessage],
    system: &'a [(u64, SysMsg)],
) -> Vec<(u64, usize, TimelineRef<'a>)> {
    // 消息按列表顺序（处理顺序）渲染；时间戳仅用于把系统消息穿插到对应消息之间。
    // 不能按时间戳重排消息：steer/注入消息的时间戳是用户输入时刻（可能早于
    // 前一条 assistant 完成时刻），重排会把它们显示到上一回合输出之前。
    let mut items: Vec<(u64, usize, TimelineRef<'a>)> = Vec::new();
    let mut si = 0usize;

    for (i, m) in messages.iter().enumerate() {
        while si < system.len() && system[si].0 <= m.timestamp {
            items.push((
                system[si].0,
                SYSTEM_SEQ_BASE + si,
                TimelineRef::Sys(&system[si].1),
            ));
            si += 1;
        }
        items.push((m.timestamp, i, TimelineRef::Msg(m)));
    }

    while si < system.len() {
        items.push((
            system[si].0,
            SYSTEM_SEQ_BASE + si,
            TimelineRef::Sys(&system[si].1),
        ));
        si += 1;
    }
    items
}

/// 预扫描全部 toolResult：id → (文本, 是否错误, 耗时)（assistant 工具块渲染填充结果）
/// toolResult 的渲染视图（assistant 的工具调用块显示其结果）。
#[derive(Clone, Default)]
pub struct ToolResultView {
    /// 结果文本。
    pub text: String,
    /// 结果里的图片 base64 数据（内联图片用）；无图片时为空。
    pub images: Vec<Arc<str>>,
    /// 是否失败（toolResult 消息的 `isError`）。
    pub is_error: bool,
    /// 工具执行耗时（毫秒）；未上报时为 None。
    pub duration_ms: Option<u64>,
    /// 附加元数据（如 `details.nestedCalls`）；无附加数据时为 None。
    pub details: Option<Value>,
}

/// 工具调用块里子调用的一行：codemode 脚本调起的嵌套工具，以及脚本内的
/// `models.*` 调用（后者由 `details.modelCalls` 提供成本）。
#[derive(Clone, Default)]
pub struct NestedCallView {
    /// 调用名（嵌套工具名，或 `models.classify` 之类的脚本内调用）。
    pub name: String,
    /// 关键参数摘要（单行；超宽由渲染层折行）。
    pub summary: String,
    /// 是否失败；结果未到达时为 None（进行中）。
    pub is_error: Option<bool>,
    /// 已耗时（毫秒）；进行中为 None（用 [`Self::started_at_ms`] 算实时耗时）。
    pub duration_ms: Option<u64>,
    /// 启动墙钟毫秒；结果态为 None。
    pub started_at_ms: Option<u64>,
    /// 单次调用成本（美元）；仅脚本内 `models.*` 调用上报，None 表示无成本信息。
    pub cost: Option<f64>,
}

/// 嵌套调用行的参数摘要：优先 `command`（bash 的整条命令），否则压缩 JSON；超长按字符截断。
pub fn nested_arg_summary(args: &Value) -> String {
    let text = args
        .get("command")
        .and_then(|v| v.as_str())
        .map(|command| command.to_string())
        .unwrap_or_else(|| compact_json(args));
    let max = 80;
    if text.chars().count() <= max {
        return text;
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}...")
}

/// 从 toolResult 的 `details` 提取子调用渲染视图（结果态）：`nestedCalls`（嵌套工具）
/// 之后追加 `modelCalls`（脚本内 `models.*` 调用，带成本）。
///
/// nestedCalls 每条形如 `{toolName, args, isError, durationMs}`（见 `NestedCallLog`）；
/// modelCalls 每条形如 `{name, args, status, durationMs, cost?}`（见 `codemode::ModelCallLog`），
/// `status = running` 视作结果未到达（无耗时、无成本）。缺字段时取安全默认值。
fn sub_calls_from_details(details: Option<&Value>) -> Vec<NestedCallView> {
    let mut calls = Vec::new();
    if let Some(entries) = details
        .and_then(|d| d.get("nestedCalls"))
        .and_then(|v| v.as_array())
    {
        calls.extend(entries.iter().map(|call| {
            NestedCallView {
                name: call
                    .get("toolName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                summary: call.get("args").map(nested_arg_summary).unwrap_or_default(),
                is_error: Some(
                    call.get("isError")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                ),
                duration_ms: call.get("durationMs").and_then(|v| v.as_u64()),
                started_at_ms: None,
                cost: None,
            }
        }));
    }
    if let Some(entries) = details
        .and_then(|d| d.get("modelCalls"))
        .and_then(|v| v.as_array())
    {
        calls.extend(entries.iter().map(model_call_view));
    }
    calls
}

/// 把一条 `details.modelCalls` 记录转成渲染视图。
///
/// `status` 为 `running` 时结果未到达（返回 `is_error = None`，耗时/成本留空）；
/// `ok` / 其它值按成功处理，`error` 按失败处理。`args` 是字符串（`provider/model`），
/// 非字符串时退化为压缩 JSON。
fn model_call_view(call: &Value) -> NestedCallView {
    let status = call.get("status").and_then(|v| v.as_str()).unwrap_or("ok");
    let running = status == "running";
    NestedCallView {
        name: call
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        summary: match call.get("args") {
            Some(Value::String(text)) => text.clone(),
            Some(args) => nested_arg_summary(args),
            None => String::new(),
        },
        is_error: if running {
            None
        } else {
            Some(status == "error")
        },
        duration_ms: if running {
            None
        } else {
            call.get("durationMs").and_then(|v| v.as_u64())
        },
        started_at_ms: None,
        cost: call.get("cost").and_then(|v| v.as_f64()),
    }
}

/// toolResult 的渲染视图（assistant 的工具调用块显示其结果）。
pub fn scan_tool_results(messages: &[AgentMessage]) -> HashMap<String, ToolResultView> {
    let mut map = HashMap::new();
    for m in messages {
        if m.role == "toolResult"
            && let Some(id) = m.tool_call_id.as_deref()
        {
            map.insert(
                id.to_string(),
                ToolResultView {
                    text: m.text(),
                    images: m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Image { data, .. } => Some(Arc::from(data.as_str())),
                            _ => None,
                        })
                        .collect(),
                    is_error: m.is_error,
                    duration_ms: m.duration_ms,
                    details: m.details.clone(),
                },
            );
        }
    }
    map
}

/// edit 工具块的渲染附加数据。
/// - `preview`：call 阶段根据 args 算出的预览 diff（工具尚未返回结果时显示）；
/// - `result_diff`：toolResult 的 `details.diff`（完成后显示，优先于 preview）。
#[derive(Clone, Default)]
pub struct EditRenderData {
    /// 工具尚未返回时按调用参数算出的预览 diff。
    pub preview: Option<String>,
    /// 工具返回后从 `details.diff` 提取的最终 diff，优先显示。
    pub result_diff: Option<String>,
}

/// 从全部 toolResult 的 `details.diff` 建 edit 渲染表（不含 IO，后台快照也可用）。
/// 调用方（App）会再补上 pending 阶段的预览 diff。
pub(crate) fn edit_map_from_results(messages: &[AgentMessage]) -> HashMap<String, EditRenderData> {
    let mut map: HashMap<String, EditRenderData> = HashMap::new();
    for m in messages {
        if m.role == "toolResult"
            && let Some(id) = m.tool_call_id.as_deref()
            && let Some(diff) = m
                .details
                .as_ref()
                .and_then(|d| d.get("diff"))
                .and_then(|v| v.as_str())
        {
            map.entry(id.to_string()).or_default().result_diff = Some(diff.to_string());
        }
    }
    map
}

/// 渲染时间线上的一个条目，返回渲染行。`accum_pending` 是跨消息共享的累积
/// pending 列表（assistant 工具调用 push，toolResult 渲染时按 id 匹配并原地更新），
/// 调用方在渲染前后切片得到本消息新增的 pending delta 存入缓存。
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_timeline_item(
    iref: TimelineRef<'_>,
    width: usize,
    theme: &Theme,
    expansions: &[bool],
    blocks: &mut Vec<BlockSpan>,
    tool_results: &HashMap<String, ToolResultView>,
    tool_started_at: &HashMap<String, u64>,
    nested_calls: &HashMap<String, Vec<NestedCallView>>,
    edit_map: &HashMap<String, EditRenderData>,
    accum_pending: &mut Vec<PendingTool>,
    links: &mut Vec<LinkSpan>,
    images: &mut Vec<ImageSlot>,
    layout: ImageLayout,
) -> Vec<Line<'static>> {
    let lines = match iref {
        TimelineRef::Msg(m) => render_message_blocks(
            m,
            width,
            theme,
            expansions,
            blocks,
            accum_pending,
            tool_results,
            tool_started_at,
            nested_calls,
            edit_map,
            images,
            layout,
        ),
        TimelineRef::Sys(s) => render_sys_message(&s.spans, s.level, width, theme),
    };
    // 链接区域在**最终行**上扫描：块内缩进/折叠/裁剪都已定型，列不会再平移。
    links.extend(collect_link_spans(&lines, theme));
    lines
}

/// 消息级渲染缓存条目：单条消息（或系统提示）的渲染行、可折叠块行范围
/// 与其产生的 pending 工具状态。
/// Arc 共享：渲染帧拼接/裁剪只克隆指针，命中缓存时零重渲染。
#[derive(Clone)]
pub struct CachedMsg {
    /// 该条目渲染后的行，Arc 共享使缓存命中时零重渲染。
    pub lines: Arc<Vec<Line<'static>>>,
    pub pending: Arc<Vec<PendingTool>>, // 相当于指纹(key)，用于缓存匹配
    /// 本消息内可折叠块的行范围（相对 `lines` 起点）
    pub blocks: Arc<Vec<BlockSpan>>,
    /// 本消息内可点击链接的区域（相对 `lines` 起点；列区间按字符宽度计算）
    pub links: Arc<Vec<LinkSpan>>,
    /// 本消息内预留的内联图片槽位（相对 `lines` 起点）
    pub images: Arc<Vec<ImageSlot>>,
}

/// 一次历史渲染的结果：按块（每条消息）切分的段落 + 总行数（含块间间隔行）。
/// 每段记录其在全局时间线中的起始行号（含前置间隔），裁剪定位用。
/// `pending` 为全部历史累积的工具 pending 状态（流式段继续使用）。
pub struct HistoryRender {
    /// (全局起始行, 块行, 块 pending delta)
    pub segments: Vec<HistorySegment>,
    /// 全部历史行数（不含流式段），含块间间隔
    pub total: usize,
    /// 跨全部消息累积的 pending 工具状态（含流式段渲染需继续追加的基础）
    pub pending: Vec<PendingTool>,
    /// 流式锚点（assistant 开始位置对应的历史行号，含块后间隔）：
    /// 流式输出渲染插在此之后，避免后注入 steer 消息插到流式输出前
    pub anchor_end: Option<usize>,
    /// 可点击折叠条目的命中区（内容全局行，含块自身行、不含块后间隔）
    pub click_targets: Vec<ClickTarget>,
    /// 可点击链接的区域（内容全局行 + 终端列）；流式段插入后由渲染层平移
    pub links: Vec<LinkSpan>,
    /// 内联图片槽位（内容全局行 + 目标单元格尺寸）
    pub images: Vec<ImageSlot>,
}

/// 后台补高亮任务的输入快照（恢复启动时收集；渲染在后台线程执行，不触碰主锁）
#[derive(Clone)]
pub struct HighlightSnapshot {
    /// 快照时刻的消息列表副本。
    pub msgs: Vec<AgentMessage>,
    /// 快照时刻的系统提示（时间戳 + 消息）。
    pub sys: Vec<(u64, SysMsg)>,
    /// 渲染使用的终端宽度（列数）。
    pub width: usize,
    /// true 时 summary/skill/tool 块缺省展开。
    pub expand_all: bool,
    /// true 时 thinking 块缺省展开。
    pub show_thinking: bool,
    /// 快照时刻的内联图片渲染参数（开关 + 单元格像素尺寸）：
    /// 与主页管线一致，否则补高亮后的行数（预留的图片行）与开关状态不符。
    pub images: ImageLayout,
    /// 逐块折叠覆盖（与主页管线一致；快照必须携带，否则补高亮后逐块展开态丢失）
    pub overrides: HashMap<BlockId, bool>,
    /// 快照时刻的主题，已置 syntax_highlight 开启。
    pub theme: Theme,
}

/// 用完整语法高亮渲染快照中的全部历史，返回 (ts, seq) → 缓存条目。
/// 后台线程调用：无 App 锁、无缓存查询；主题已置 syntax_highlight=true。
/// 与 render_history_lines 的缓存条目 key 规则一致，可整批 extend 进 msg_cache。
pub(crate) fn render_full_history(snap: &HighlightSnapshot) -> HashMap<(u64, usize), CachedMsg> {
    let tool_results = scan_tool_results(&snap.msgs);
    // 后台快照只带 result_diff（无 IO 预览）：历史已完成工具按 details.diff 渲染
    let edit_map = edit_map_from_results(&snap.msgs);
    // 后台补高亮快照：不携带进行中工具的实时计时与嵌套进度
    // （历史已完成工具按 duration_ms 显示 Took，嵌套调用来自 details.nestedCalls）
    let tool_started_at: HashMap<String, u64> = HashMap::new();
    let nested_calls: HashMap<String, Vec<NestedCallView>> = HashMap::new();
    let items = timeline_items(&snap.msgs, &snap.sys);
    let mut accum_pending: Vec<PendingTool> = Vec::new();
    let mut out = HashMap::new();

    for (idx, (ts, seq, iref)) in items.into_iter().enumerate() {
        let base = accum_pending.len();
        let expansions: Vec<bool> = match iref {
            TimelineRef::Msg(m) => resolve_block_expansions(
                m,
                ts,
                seq,
                &snap.overrides,
                snap.expand_all,
                snap.show_thinking,
            ),
            TimelineRef::Sys(_) => Vec::new(),
        };
        let mut blocks: Vec<BlockSpan> = Vec::new();
        let mut links: Vec<LinkSpan> = Vec::new();
        let mut images: Vec<ImageSlot> = Vec::new();
        let lines = render_timeline_item(
            iref,
            snap.width,
            &snap.theme,
            &expansions,
            &mut blocks,
            &tool_results,
            &tool_started_at,
            &nested_calls,
            &edit_map,
            &mut accum_pending,
            &mut links,
            &mut images,
            snap.images,
        );
        let delta: Vec<PendingTool> = accum_pending[base..].to_vec();

        out.insert(
            (ts, seq),
            CachedMsg {
                lines: Arc::new(lines),
                pending: Arc::new(delta),
                blocks: Arc::new(blocks),
                links: Arc::new(links),
                images: Arc::new(images),
            },
        );

        // 后台线程让步：避免长时间独占 CPU 拖慢 UI 渲染帧
        if idx % 32 == 31 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    out
}

/// 追加式消息列表的渲染缓存（覆盖层查看器用）。
///
/// 与主页历史管线同一套底层函数（`render_message` + `CachedMsg` + pending 增量回放），
/// 但不含系统提示时间线、流式段与编辑预览 IO。缓存按**消息下标**：transcript 只追加，
/// 已出现消息不变，因此新消息只渲染增量；`toolResult` 到达会改变对应 assistant 消息的
/// 渲染（结果内联显示在 assistant 的工具块里），用 `result_fp` 检测并只失效那一条。
///
/// 渲染参数（宽度 / `expand_all` / `show_thinking` / 主题指纹）变化 → 整体失效重建。
#[derive(Default)]
pub struct MessageCache {
    /// 按消息下标的缓存条目，附带工具结果指纹用于判定单条失效。
    entries: Vec<(CachedMsg, u64)>,
    /// 上次渲染宽度，变化时整表清空。
    width: usize,
    /// 上次渲染的全局展开态，变化时整表清空。
    expand_all: bool,
    /// 上次渲染的 thinking 展开态，变化时整表清空。
    show_thinking: bool,
    /// 上次渲染的主题指纹，变化时整表清空。
    theme_fp: String,
    /// 上次渲染时生效的扩展渲染器指纹，变化时整表清空。
    renderers_fp: String,
}

impl MessageCache {
    /// 渲染整段消息列表（含块间空行），逐条命中缓存。
    pub fn render(
        &mut self,
        messages: &[AgentMessage],
        width: usize,
        theme: &Theme,
        expand_all: bool,
        show_thinking: bool,
    ) -> Vec<Line<'static>> {
        let theme_fp = theme.render_fingerprint();
        // 工具块可能由扩展渲染器产出：注册表/扩展启用状态一变，旧行必须重画
        let renderers_fp = extensions::tool_renderers_fingerprint();
        if self.width != width
            || self.expand_all != expand_all
            || self.show_thinking != show_thinking
            || self.theme_fp != theme_fp
            || self.renderers_fp != renderers_fp
        {
            self.entries.clear();
            self.width = width;
            self.expand_all = expand_all;
            self.show_thinking = show_thinking;
            self.theme_fp = theme_fp;
            self.renderers_fp = renderers_fp;
        }
        self.entries.truncate(messages.len());

        let tool_results = scan_tool_results(messages);
        let edit_map = edit_map_from_results(messages);
        let tool_started_at: HashMap<String, u64> = HashMap::new();
        let nested_calls: HashMap<String, Vec<NestedCallView>> = HashMap::new();
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut pending: Vec<PendingTool> = Vec::new();

        for (i, m) in messages.iter().enumerate() {
            let fp = result_fp(m, &tool_results);
            let base = pending.len();
            let cached = match self.entries.get(i) {
                Some((c, cached_fp)) if *cached_fp == fp => {
                    pending.extend(c.pending.iter().cloned());
                    c.clone()
                }
                _ => {
                    let lines = render_message(
                        m,
                        width,
                        theme,
                        expand_all,
                        show_thinking,
                        &mut pending,
                        &tool_results,
                        &tool_started_at,
                        &nested_calls,
                        &edit_map,
                    );
                    let delta: Vec<PendingTool> = pending[base..].to_vec();
                    let links = collect_link_spans(&lines, theme);
                    let c = CachedMsg {
                        lines: Arc::new(lines),
                        pending: Arc::new(delta),
                        blocks: Arc::new(Vec::new()),
                        links: Arc::new(links),
                        images: Arc::new(Vec::new()),
                    };
                    if i < self.entries.len() {
                        self.entries[i] = (c.clone(), fp);
                    } else {
                        self.entries.push((c.clone(), fp));
                    }
                    c
                }
            };

            if cached.lines.is_empty() {
                continue;
            }
            out.extend(cached.lines.iter().cloned());
            if !cached.lines.last().map(is_pure_gap).unwrap_or(true) {
                out.push(Line::from(""));
            }
        }
        out
    }
}

/// 一条消息的工具结果指纹：其工具调用中已有多少条结果。
/// 结果只追加、内容不变，所以计数变化即可判定该 assistant 块需要重渲染。
fn result_fp(msg: &AgentMessage, tool_results: &HashMap<String, ToolResultView>) -> u64 {
    let mut n = 0u64;
    for b in &msg.content {
        if let ContentBlock::ToolCall { id, .. } = b
            && tool_results.contains_key(id.as_str())
        {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::AgentMessage;
    use ratatui::style::Color;

    #[test]
    fn format_tool_duration_matches_pi_thresholds() {
        assert_eq!(format_tool_duration(0), "0.0s");
        assert_eq!(format_tool_duration(3_200), "3.2s");
        assert_eq!(format_tool_duration(59_900), "59.9s");
        assert_eq!(format_tool_duration(60_000), "1m 0s");
        assert_eq!(format_tool_duration(90_500), "1m 30s");
        assert_eq!(format_tool_duration(3_599_000), "59m 59s");
        assert_eq!(format_tool_duration(3_600_000), "1h 0m 0s");
        assert_eq!(format_tool_duration(3_930_000), "1h 5m 30s");
    }

    /// 用给定图片布局渲染一条消息，返回 (渲染行, 收集到的图片槽位)。
    fn render_with_images(
        msg: &AgentMessage,
        width: usize,
        layout: ImageLayout,
        tool_results: &HashMap<String, ToolResultView>,
    ) -> (Vec<Line<'static>>, Vec<ImageSlot>) {
        let expansions: Vec<bool> = collapsible_blocks(msg)
            .into_iter()
            .map(|k| default_block_expanded(k, true, true))
            .collect();
        let mut images: Vec<ImageSlot> = Vec::new();
        let lines = render_message_blocks(
            msg,
            width,
            &theme(),
            &expansions,
            &mut Vec::new(),
            &mut Vec::new(),
            tool_results,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &mut images,
            layout,
        );
        (lines, images)
    }

    /// 内联图片开启时：图片块按等比换算预留多行并登记槽位（宽度上限 60 列）。
    #[test]
    fn image_block_reserves_rows_and_reports_slot() {
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![ContentBlock::Image {
            data: crate::test_support::png_base64(600, 200),
            mime_type: "image/png".to_string(),
        }];

        let (lines, images) = render_with_images(
            &msg,
            80,
            ImageLayout {
                enabled: true,
                font: (10, 20),
            },
            &HashMap::new(),
        );

        // 600×200 像素、单元格 10×20：宽度顶满 60 列 → 10 行
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].cols, images[0].rows), (60, 10));
        assert_eq!(images[0].key, terminal_image::image_key(&images[0].data));
        // 消息区没有具体底色（终端默认色）→ 不填补边色
        assert_eq!(images[0].pad, None);
        // 预留的行就在槽位起点处，且内容为空白（图片绘制时覆盖）
        let rows = t(&lines);
        assert_eq!(rows[images[0].line].trim(), "");
        // 预留行 + 底部 padding 行
        assert_eq!(rows.len(), images[0].line + images[0].rows as usize + 1);
        assert!(is_pure_gap(
            &lines[images[0].line + images[0].rows as usize]
        ));
    }

    /// 同一消息里连续两张图片：每张图后都有一行纯空行（图片之间、图片与后文之间的间隔），
    /// 最后一张图底部也有 padding（回归：以前图片块不留间隔，多图会挤在一起）。
    #[test]
    fn consecutive_images_keep_a_blank_gap_between_them() {
        let data = crate::test_support::png_base64(600, 200);
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![
            ContentBlock::Text {
                text: "before".to_string(),
                text_signature: None,
            },
            ContentBlock::Image {
                data: data.clone(),
                mime_type: "image/png".to_string(),
            },
            ContentBlock::Image {
                data,
                mime_type: "image/png".to_string(),
            },
        ];

        let (lines, images) = render_with_images(
            &msg,
            80,
            ImageLayout {
                enabled: true,
                font: (10, 20),
            },
            &HashMap::new(),
        );

        assert_eq!(images.len(), 2);
        let rows = t(&lines);
        let first_end = images[0].line + images[0].rows as usize;
        let second_end = images[1].line + images[1].rows as usize;
        // 第二张图紧跟在第一张的 padding 行之后
        assert_eq!(images[1].line, first_end + 1);
        assert!(is_pure_gap(&lines[first_end]), "{:?}", rows[first_end]);
        assert!(is_pure_gap(&lines[second_end]), "{:?}", rows[second_end]);
    }

    /// 图片盒子底部的透明补边已经接近一行时，不再额外补空行——否则图片之间会空出两行。
    /// （333×299 的图在 22×47 的单元格里：预留 16×7 格，底部补边 30px ≈ 0.64 行。）
    #[test]
    fn large_bottom_pad_replaces_the_blank_gap_line() {
        let data = crate::test_support::png_base64(333, 299);
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![
            ContentBlock::Image {
                data: data.clone(),
                mime_type: "image/png".to_string(),
            },
            ContentBlock::Image {
                data,
                mime_type: "image/png".to_string(),
            },
        ];

        let (lines, images) = render_with_images(
            &msg,
            80,
            ImageLayout {
                enabled: true,
                font: (22, 47),
            },
            &HashMap::new(),
        );

        assert_eq!(images.len(), 2);
        assert_eq!((images[0].cols, images[0].rows), (16, 7));
        assert_eq!(
            images[1].line,
            images[0].line + images[0].rows as usize,
            "补边够一行时第二张图应紧跟预留行: {:?}",
            t(&lines)
        );
    }

    /// 用户消息（带消息背景）里的图片：预留行与底部 padding 都要带消息背景，
    /// 色块不能中途断开露出终端底色（回归：多张图片之间的分割行没背景）。
    #[test]
    fn user_message_image_padding_keeps_the_message_background() {
        let mut msg = AgentMessage::user_text("look");
        msg.content.push(ContentBlock::Image {
            data: crate::test_support::png_base64(600, 200),
            mime_type: "image/png".to_string(),
        });

        let (lines, images) = render_with_images(
            &msg,
            80,
            ImageLayout {
                enabled: true,
                font: (10, 20),
            },
            &HashMap::new(),
        );

        assert_eq!(images.len(), 1);
        let padding = images[0].line + images[0].rows as usize;
        assert!(
            lines[padding].spans.iter().any(|s| s.style.bg.is_some()),
            "padding 行应带消息背景: {:?}",
            lines[padding]
        );
        assert_eq!(
            images[0].pad, None,
            "默认主题的消息底色是终端默认色，无可信 RGB"
        );
    }

    /// 块有具体底色（如 synthwave 的 `toolSuccessBg`）时，halfblocks 的补边要填这个底色；
    /// 默认 `system` 主题的块底色是终端默认色（`Reset`），没有可信 RGB，只能不填。
    #[test]
    fn block_background_color_becomes_the_halfblocks_pad() {
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![ContentBlock::ToolCall {
            id: "r1".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": "shot.png" }),
            thought_signature: None,
            namespace: None,
        }];

        let mut tool_results: HashMap<String, ToolResultView> = HashMap::new();
        tool_results.insert(
            "r1".to_string(),
            ToolResultView {
                text: "read shot.png".to_string(),
                images: vec![Arc::from(
                    crate::test_support::png_base64(600, 200).as_str(),
                )],
                is_error: false,
                duration_ms: Some(12),
                details: None,
            },
        );

        let slots = |theme: &Theme| {
            let expansions: Vec<bool> = collapsible_blocks(&msg)
                .into_iter()
                .map(|k| default_block_expanded(k, true, true))
                .collect();
            let mut images: Vec<ImageSlot> = Vec::new();
            render_message_blocks(
                &msg,
                80,
                theme,
                &expansions,
                &mut Vec::new(),
                &mut Vec::new(),
                &tool_results,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &mut images,
                ImageLayout {
                    enabled: true,
                    font: (10, 20),
                },
            );
            images
        };

        assert_eq!(
            slots(&theme())[0].pad,
            None,
            "system 主题的块底色是终端默认色，没有可信 RGB"
        );

        let synthwave = Theme::load("synthwave");
        let expected = synthwave
            .style_bg(TOOL_SUCCESS_BG.0, TOOL_SUCCESS_BG.1)
            .bg
            .and_then(terminal_image::style_bg_rgb);
        assert!(
            expected.is_some(),
            "synthwave 的 toolSuccessBg 应是具体颜色"
        );
        assert_eq!(slots(&synthwave)[0].pad, expected);
    }

    /// 内联图片关闭时：整块不渲染——不产生槽位，也不留 `[image]` 占位行。
    #[test]
    fn image_block_is_not_rendered_when_disabled() {
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![ContentBlock::Image {
            data: crate::test_support::png_base64(600, 200),
            mime_type: "image/png".to_string(),
        }];

        let (lines, images) =
            render_with_images(&msg, 80, ImageLayout::disabled(), &HashMap::new());

        assert!(images.is_empty());
        let rows = t(&lines);
        assert!(
            !rows.iter().any(|r| r.contains("[image]")),
            "关闭时不应出现占位行: {rows:?}"
        );
    }

    /// 图片数据不是合法图片（base64 坏 / 非图片格式）时回落占位行，不产生槽位。
    #[test]
    fn image_block_falls_back_when_data_is_not_an_image() {
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![ContentBlock::Image {
            data: "bm90LWFuLWltYWdl".to_string(),
            mime_type: "image/png".to_string(),
        }];

        let (lines, images) = render_with_images(
            &msg,
            80,
            ImageLayout {
                enabled: true,
                font: (10, 20),
            },
            &HashMap::new(),
        );

        assert!(images.is_empty());
        assert!(t(&lines).iter().any(|r| r.contains("[image]")));
    }

    /// 用户消息里的图片（粘贴附件）：正文之后预留行并登记槽位，预留行带消息背景。
    #[test]
    fn user_message_images_are_reserved_after_the_text() {
        let mut msg = AgentMessage::user_text("look at this");
        msg.content.push(ContentBlock::Image {
            data: crate::test_support::png_base64(100, 100),
            mime_type: "image/png".to_string(),
        });

        let (lines, images) = render_with_images(
            &msg,
            80,
            ImageLayout {
                enabled: true,
                font: (10, 20),
            },
            &HashMap::new(),
        );

        assert_eq!(images.len(), 1, "用户消息的图片也要收集");
        // 100×100 小于盒子：不放大，按原始像素换算成 10×5 单元格
        assert_eq!((images[0].cols, images[0].rows), (10, 5));
        // 预留行在正文之后（正文含背景 padding 行）
        assert!(images[0].line > 0);
        assert_eq!(t(&lines)[images[0].line].trim(), "");
    }

    /// 工具结果里的图片：随工具块渲染预留行并登记槽位（read 工具截图场景）。
    #[test]
    fn tool_result_images_are_reserved_inside_the_tool_block() {
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![ContentBlock::ToolCall {
            id: "r1".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": "shot.png" }),
            thought_signature: None,
            namespace: None,
        }];

        let mut tool_results: HashMap<String, ToolResultView> = HashMap::new();
        tool_results.insert(
            "r1".to_string(),
            ToolResultView {
                text: "read shot.png".to_string(),
                images: vec![Arc::from(
                    crate::test_support::png_base64(600, 200).as_str(),
                )],
                is_error: false,
                duration_ms: Some(12),
                details: None,
            },
        );

        let (lines, images) = render_with_images(
            &msg,
            80,
            ImageLayout {
                enabled: true,
                font: (10, 20),
            },
            &tool_results,
        );

        assert_eq!(images.len(), 1, "工具结果的图片也要收集");
        assert_eq!((images[0].cols, images[0].rows), (60, 10));
        assert_eq!(t(&lines)[images[0].line].trim(), "");
    }

    /// 工具结果图片的槽位行号必须相对**条目起点**，而不是工具块起点：
    /// 用块内局部行号时图片会画到块的上方——吃掉图片上方的间隔、盖住工具块的
    /// `Took X.Xs`，同时块底预留的行整段空着（回归：read 截图底部一大块空白）。
    #[test]
    fn tool_result_image_slot_is_entry_relative() {
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![
            ContentBlock::Thinking {
                thinking: "先看看这张图".to_string(),
                thinking_signature: None,
                redacted: None,
            },
            ContentBlock::Text {
                text: "我看一下这张截图。".to_string(),
                text_signature: None,
            },
            ContentBlock::ToolCall {
                id: "r1".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({ "path": "shot.png" }),
                thought_signature: None,
                namespace: None,
            },
        ];

        let mut tool_results: HashMap<String, ToolResultView> = HashMap::new();
        tool_results.insert(
            "r1".to_string(),
            ToolResultView {
                text: "read shot.png".to_string(),
                images: vec![Arc::from(
                    crate::test_support::png_base64(600, 200).as_str(),
                )],
                is_error: false,
                duration_ms: Some(1200),
                details: None,
            },
        );

        let (lines, images) = render_with_images(
            &msg,
            80,
            ImageLayout {
                enabled: true,
                font: (10, 20),
            },
            &tool_results,
        );
        let rows = t(&lines);

        assert_eq!(images.len(), 1);
        let slot = &images[0];
        let end = slot.line + slot.rows as usize;
        // 预留区里不能有文字：块内局部行号会把 `Took 1.2s` 圈进图片区
        for (row, line) in rows.iter().enumerate().take(end).skip(slot.line) {
            assert!(
                line.trim().is_empty(),
                "第 {row} 行落在图片预留区 {}..{end} 里: {line:?}",
                slot.line
            );
        }
        assert!(rows[end].trim().is_empty(), "预留区后应是盒底 padding");
        // 图片上方空一行（与正文/耗时行分开），且该行沿用工具块背景
        assert!(rows[slot.line - 1].trim().is_empty(), "图片上方应空一行");
        assert!(
            lines[slot.line - 1]
                .spans
                .iter()
                .all(|s| s.style.bg.is_some()),
            "间隔行要沿用工具块背景"
        );
        assert!(
            !rows[slot.line - 2].trim().is_empty(),
            "间隔行再往上应是工具块的正文/耗时行"
        );
        let took = rows
            .iter()
            .position(|r| r.contains("Took"))
            .expect("工具块应有耗时行");
        assert!(
            took < slot.line,
            "`Took` 行({took}) 应在图片预留区({}..{end})之上",
            slot.line
        );
    }

    fn t(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn theme() -> Theme {
        Theme::default()
    }

    /// codemode 工具结果的 `details.nestedCalls` 渲染为块内子调用行（`✓` / 失败为 `✗`）。
    #[test]
    fn nested_calls_from_details_render_inside_tool_block() {
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.content = vec![ContentBlock::ToolCall {
            id: "cm1".into(),
            name: "codemode".into(),
            arguments: serde_json::json!({ "code": "return 1;" }),
            thought_signature: None,
            namespace: None,
        }];
        let mut res = AgentMessage::user_text("Script completed");
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("cm1".into());
        res.duration_ms = Some(1500);
        res.details = Some(serde_json::json!({
            "nestedCalls": [
                {"toolName": "bash", "args": {"command": "ls -la"}, "isError": false, "durationMs": 400},
                {"toolName": "read", "args": {"path": "a.txt"}, "isError": true, "durationMs": 12},
            ]
        }));

        let tool_results = scan_tool_results(&[ai.clone(), res]);
        let lines = render_message(
            &ai,
            80,
            &theme(),
            true,
            false,
            &mut Vec::new(),
            &tool_results,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        let rows = t(&lines).join("\n");
        assert!(rows.contains("✓ bash  ls -la  0.4s"), "嵌套行: {rows}");
        assert!(rows.contains("✗ read"), "失败的嵌套调用应标 ✗: {rows}");
        assert!(
            rows.contains("0.0s") || rows.contains("12"),
            "失败行也带耗时: {rows}"
        );
    }

    /// 进行中工具的嵌套调用（实时事件累积）在 `Elapsed` 行之后渲染。
    #[test]
    fn live_nested_calls_render_while_tool_runs() {
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.content = vec![ContentBlock::ToolCall {
            id: "cm2".into(),
            name: "codemode".into(),
            arguments: serde_json::json!({ "code": "await tools.bash('ls')" }),
            thought_signature: None,
            namespace: None,
        }];
        let started = now_ms();
        let mut started_map = HashMap::new();
        started_map.insert("cm2".to_string(), started);
        let mut nested_map: HashMap<String, Vec<NestedCallView>> = HashMap::new();
        nested_map.insert(
            "cm2".to_string(),
            vec![NestedCallView {
                name: "bash".to_string(),
                summary: "ls".to_string(),
                is_error: None,
                duration_ms: None,
                started_at_ms: Some(started),
                cost: None,
            }],
        );

        let lines = render_message(
            &ai,
            80,
            &theme(),
            true,
            false,
            &mut Vec::new(),
            &HashMap::new(),
            &started_map,
            &nested_map,
            &HashMap::new(),
        );
        let rows = t(&lines).join("\n");
        assert!(rows.contains("Elapsed"), "进行中应显示 Elapsed: {rows}");
        assert!(rows.contains("… bash  ls"), "进行中应显示嵌套行: {rows}");
    }

    /// codemode 结果的 `details.modelCalls` 逐条渲染（图标 + 名称 + 参数 + 耗时 + 成本），
    /// 并在多于一条带成本的调用时追加合计行（对齐 pi 的 `Model calls: $x`）。
    #[test]
    fn model_calls_from_details_render_with_cost_and_total() {
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.content = vec![ContentBlock::ToolCall {
            id: "cm3".into(),
            name: "codemode".into(),
            arguments: serde_json::json!({ "code": "await models.classify(m, ctx)" }),
            thought_signature: None,
            namespace: None,
        }];
        let mut res = AgentMessage::user_text("Script completed");
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("cm3".into());
        res.duration_ms = Some(900);
        res.details = Some(serde_json::json!({
            "modelCalls": [
                {"name": "models.classify", "args": "openrouter/~typesafe/jev-latest", "status": "ok", "durationMs": 300, "cost": 0.0012},
                {"name": "models.classify", "args": "openrouter/~typesafe/jev-latest", "status": "error", "durationMs": 120, "cost": 0.0008},
            ]
        }));

        let tool_results = scan_tool_results(&[ai.clone(), res]);
        let lines = render_message(
            &ai,
            100,
            &theme(),
            true,
            false,
            &mut Vec::new(),
            &tool_results,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        let rows = t(&lines).join("\n");
        assert!(
            rows.contains("✓ models.classify  openrouter/~typesafe/jev-latest  0.3s  $0.0012"),
            "成功的分类器调用行: {rows}"
        );
        assert!(rows.contains("✗ models.classify"), "失败调用应标 ✗: {rows}");
        assert!(rows.contains("Model calls: $0.0020"), "合计行: {rows}");
    }

    /// 子调用折叠到末尾 [`SUB_CALL_PREVIEW_COUNT`] 条并给省略提示；合计覆盖全部调用。
    #[test]
    fn model_calls_fold_to_preview_and_total_covers_all() {
        let calls: Vec<serde_json::Value> = (0..10)
            .map(|i| {
                serde_json::json!({
                    "name": "models.classify",
                    "args": format!("openrouter/jev-{i}"),
                    "status": "ok",
                    "durationMs": 100,
                    "cost": 0.001,
                })
            })
            .collect();
        let details = serde_json::json!({ "modelCalls": calls });
        let view = sub_calls_from_details(Some(&details));
        assert_eq!(view.len(), 10);

        let lines = render_sub_calls(&view, false, &theme(), Style::default(), 80, 78);
        let rows = t(&lines);
        assert!(
            rows[0].contains("... (2 earlier calls, ctrl+o/alt+click to expand)"),
            "省略提示: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("jev-0")),
            "更早的调用被折叠: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("jev-9")),
            "末尾调用可见: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("Model calls: $0.01")),
            "合计覆盖全部 10 条: {rows:?}"
        );
    }

    /// 渲染消息区全部消息与流式缓冲，返回需要显示的行。
    /// 系统提示（命令输出等）按时间戳与 LLM 消息合并成时间线（对齐 pi chatContainer
    /// 的 addChild 顺序：提示出现在对应时刻的消息之间，而不是堆积在尾部）。
    /// toolResult 预先扫描：assistant 的工具调用块能显示其执行结果（对齐 pi 的
    /// 响应式 ToolExecutionComponent，而非等结果到达后无法更新）。
    /// `streaming` 提供 (thinking, tools, text) 流式内容（对齐 pi streamingComponent）。
    /// 注意：生产渲染路径已改用消息级缓存管线（App::render_history_lines + streaming），
    /// 本函数仅保留供测试断言（与缓存管线共用同一套底层渲染函数）。
    pub fn render_all_messages(
        messages: &[AgentMessage],
        system: &[(u64, SysMsg)],
        streaming: Option<(&str, &[String], &str)>,
        width: usize,
        theme: &Theme,
        expand_all: bool,
        show_thinking: bool,
    ) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut pending: Vec<PendingTool> = Vec::new();
        // 预扫描 toolResult：id → (文本, 是否错误, 耗时)
        let mut tool_results: HashMap<String, ToolResultView> = HashMap::new();
        // 测试辅助：不注入进行中工具的实时计时（Elapsed 断言用显式 started_at 场景）
        let tool_started_at: HashMap<String, u64> = HashMap::new();

        for m in messages {
            if m.role == "toolResult"
                && let Some(id) = m.tool_call_id.as_deref()
            {
                tool_results.insert(
                    id.to_string(),
                    ToolResultView {
                        text: m.text(),
                        images: Vec::new(),
                        is_error: m.is_error,
                        duration_ms: m.duration_ms,
                        details: m.details.clone(),
                    },
                );
            }
        }
        // 时间线合并：msg 时间戳为主键，同毫秒时保持各自原有顺序
        enum ItemRef<'a> {
            Msg(&'a AgentMessage),
            Sys(&'a SysMsg),
        }
        let mut items: Vec<(u64, usize, ItemRef<'_>)> = Vec::new();
        for (i, m) in messages.iter().enumerate() {
            items.push((m.timestamp, i, ItemRef::Msg(m)));
        }
        for (i, (ts, s)) in system.iter().enumerate() {
            items.push((*ts, SYSTEM_SEQ_BASE + i, ItemRef::Sys(s)));
        }
        items.sort_by_key(|(ts, seq, _)| (*ts, *seq));

        let edit_map = edit_map_from_results(messages);
        for (_, _, item) in items {
            let lines: Vec<Line<'static>> = match item {
                ItemRef::Msg(m) => render_message(
                    m,
                    width,
                    theme,
                    expand_all,
                    show_thinking,
                    &mut pending,
                    &tool_results,
                    &tool_started_at,
                    &std::collections::HashMap::new(),
                    &edit_map,
                ),
                ItemRef::Sys(s) => render_sys_message(&s.spans, s.level, width, theme),
            };
            if lines.is_empty() {
                continue;
            }
            out.extend(lines);
            // 消息块间距：渲染函数出口已补尾部空行；此处仅防御（末尾空行则不重复）
            push_blank_gap(&mut out);
        }
        if let Some((thinking, tools, text)) = streaming {
            out.extend(render_streaming(
                thinking,
                tools,
                text,
                width,
                theme,
                expand_all,
                show_thinking,
                &mut pending,
                &tool_started_at,
                &mut Vec::new(),
            ));
        }
        out
    }

    #[test]
    fn user_message_has_background_block_with_padding() {
        let msg = AgentMessage::user_text("hello user");
        let lines = render_message(
            &msg,
            40,
            &theme(),
            false,
            true,
            &mut Vec::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let t = t(&lines);
        // Box(1,1)：首部为背景 padding 行（对齐 pi user 消息块）；
        // 块尾补纯空行（渲染层间隔），其前一行才是 Box 下 padding
        assert_eq!(display_width(&t[0]), 40, "top padding row full width");
        assert!(lines[0].spans[0].style.bg.is_some(), "padding row has bg");
        let bottom_pad = &lines[lines.len() - 2];
        assert!(
            bottom_pad.spans[0].style.bg.is_some(),
            "bottom padding row has bg"
        );
        assert!(is_pure_gap(lines.last().unwrap()), "块尾应有渲染间隔空行");
        let content = t.iter().find(|l| l.contains("hello user")).unwrap();
        assert!(content.starts_with(' '), "left pad: {:?}", content);
        assert_eq!(
            display_width(content),
            40,
            "content row padded to full width"
        );
    }

    #[test]
    fn assistant_text_renders_markdown() {
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![ContentBlock::Text {
            text: "# Hi\n\nSome **bold**".into(),
            text_signature: None,
        }];
        let lines = render_message(
            &msg,
            60,
            &theme(),
            false,
            true,
            &mut Vec::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let t = t(&lines);
        assert!(t.iter().any(|l| l.contains("# Hi")));
        assert!(t.iter().any(|l| l.contains("bold")));
    }

    #[test]
    fn thinking_folded_label_and_expanded() {
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![ContentBlock::Thinking {
            thinking: "deep thoughts".into(),
            thinking_signature: None,
            redacted: None,
        }];
        // 折叠（show_thinking=false）：Thinking... 标签
        let lines = render_message(
            &msg,
            60,
            &theme(),
            true,
            false,
            &mut Vec::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let t1 = t(&lines);
        assert!(t1.iter().any(|l| l.contains("Thinking...")));
        // 展开（show_thinking=true）：内容
        let lines = render_message(
            &msg,
            60,
            &theme(),
            true,
            true,
            &mut Vec::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let t2 = t(&lines);
        assert!(t2.iter().any(|l| l.contains("deep thoughts")));
    }

    #[test]
    fn tool_call_block_and_result_update() {
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![ContentBlock::ToolCall {
            id: "t1".into(),
            name: "read".into(),
            arguments: serde_json::json!({ "path": "a.rs" }),
            thought_signature: None,
            namespace: None,
        }];
        let mut pending = Vec::new();
        let lines = render_message(
            &msg,
            60,
            &theme(),
            true,
            true,
            &mut pending,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let t = t(&lines);
        assert!(
            t.iter().any(|l| l.trim().starts_with("read")),
            "tool title: {:?}",
            t
        );
        assert!(t.iter().any(|l| l.contains("a.rs")));
        assert_eq!(pending.len(), 1);

        // toolResult 更新 pending，不产生行
        let mut res = AgentMessage::user_text("file content");
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("t1".into());
        let lines = render_message(
            &res,
            60,
            &theme(),
            true,
            true,
            &mut pending,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(lines.is_empty(), "toolResult must not render its own line");
        assert_eq!(pending[0].result.as_deref(), Some("file content"));
    }

    #[test]
    fn tool_block_shows_result_via_prescan() {
        // 回归：assistant 工具调用块必须显示预扫描的 toolResult（对齐 pi 响应式工具块）
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.timestamp = 100;
        ai.content = vec![ContentBlock::ToolCall {
            id: "t2".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "ls -la" }),
            thought_signature: None,
            namespace: None,
        }];
        let mut res =
            AgentMessage::user_text("total 8\ndrwxr-xr-x .\n-rw-r--r-- a.txt\n-rw-r--r-- b.txt");
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("t2".into());
        res.timestamp = 200;
        res.duration_ms = Some(500);
        let lines = render_all_messages(&[ai, res], &[], None, 60, &theme(), true, true);
        let t = t(&lines);
        let joined = t.join("\n");
        assert!(joined.contains("$ ls -la"), "bash 头部: {:?}", t);
        assert!(joined.contains("a.txt"), "工具输出必须在块内: {:?}", t);
        assert!(joined.contains("Took 0.5s"), "耗时: {:?}", t);
    }

    /// 回归：工具块背景必须铺满整行。
    /// tab / 零宽字符 / CJK / emoji / ANSI 转义 / 超宽 token / 恰好 width 的行，
    /// 都不允许在右侧留下未填充背景（prux 宽度口径与 ratatui 落格口径漂移）。
    #[test]
    fn tool_block_lines_fill_full_width() {
        let width = 40usize;
        let out = concat!(
            "a\tb\n",
            "\u{200b}zero-width\n",
            "\u{4e2d}\u{6587} cjk\n",
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} emoji\n",
            "\u{1b}[31mred\u{1b}[0m ansi\n",
            "exactly-width-ascii-line-padded-out-xx\n",
            "plain ascii line that is definitely long enough to wrap around",
        );
        let args = serde_json::json!({ "command": "printf x" });
        let lines = render_tool_block(
            "bash",
            &args,
            Some((out, false)),
            Some(1200),
            None,
            None,
            &[],
            width,
            &theme(),
            false,
        );
        assert!(!lines.is_empty());
        for l in &lines {
            let w: usize = l.spans.iter().map(|s| grapheme_width(&s.content)).sum();
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(w >= width, "行未铺满背景（渲染宽 {w} < {width}）: {text:?}");
        }
    }

    #[test]
    fn tool_block_collapsed_shows_tail_with_hint() {
        // 折叠：bash 显示末尾 5 行 + earlier lines 提示；ls 显示开头 20 行 + more lines 提示
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.timestamp = 100;
        ai.content = vec![ContentBlock::ToolCall {
            id: "t3".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "seq 10" }),
            thought_signature: None,
            namespace: None,
        }];
        let mut res = AgentMessage::user_text("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("t3".into());
        res.timestamp = 200;
        let lines = render_all_messages(&[ai, res], &[], None, 60, &theme(), false, true);
        let tt = t(&lines);
        let joined = tt.join("\n");
        assert!(
            joined.contains("(5 earlier lines, ctrl+o/alt+click to expand)"),
            "earlier 提示: {:?}",
            tt
        );
        assert!(joined.contains("10"), "显示末尾行: {:?}", tt);
        assert!(!joined.contains("\n1\n"), "不显示开头行: {:?}", tt);

        // ls：前 20 行 + more lines 提示（构造 25 行）
        let mut ai2 = AgentMessage::user_text("");
        ai2.role = "assistant".to_string();
        ai2.timestamp = 100;
        ai2.content = vec![ContentBlock::ToolCall {
            id: "t4".into(),
            name: "ls".into(),
            arguments: serde_json::json!({ "path": "." }),
            thought_signature: None,
            namespace: None,
        }];
        let mut res2 = AgentMessage::user_text(
            &(1..=25)
                .map(|i| format!("f{}", i))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        res2.role = "toolResult".to_string();
        res2.tool_call_id = Some("t4".into());
        res2.timestamp = 200;
        let lines = render_all_messages(&[ai2, res2], &[], None, 60, &theme(), false, true);
        let tt = t(&lines);
        let joined = tt.join("\n");
        assert!(joined.contains("ls ."), "ls 头部（空路径回退 .）: {:?}", tt);
        assert!(joined.contains("f1"), "显示开头行: {:?}", tt);
        assert!(
            joined.contains("(5 more lines, ctrl+o/alt+click to expand)"),
            "more 提示: {:?}",
            tt
        );
    }

    #[test]
    fn write_call_renders_content_with_total_hint() {
        // 对齐 pi write.ts：write 的 content 参数要在工具块内渲染，
        // 折叠时显示前 10 行 + `... (N more lines, M total, ctrl+o/alt+click to expand)`
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.timestamp = 100;
        ai.content = vec![ContentBlock::ToolCall {
            id: "w1".into(),
            name: "write".into(),
            arguments: serde_json::json!({
                "path": "a.txt",
                "content": (1..=40).map(|i| format!("line{}", i)).collect::<Vec<_>>().join("\n")
            }),
            thought_signature: None,
            namespace: None,
        }];
        let lines = render_all_messages(&[ai], &[], None, 60, &theme(), false, true);
        let tt = t(&lines);
        let joined = tt.join("\n");
        assert!(joined.contains("write a.txt"), "write 头部: {:?}", tt);
        assert!(joined.contains("line1"), "显示 content 开头: {:?}", tt);
        assert!(
            joined.contains("(30 more lines, 40 total, ctrl+o/alt+click to expand)"),
            "total 折叠提示: {:?}",
            tt
        );
        assert!(!joined.contains("line40"), "折叠不显示第 40 行: {:?}", tt);
    }

    #[test]
    fn write_call_expanded_renders_all_content() {
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.timestamp = 100;
        ai.content = vec![ContentBlock::ToolCall {
            id: "w2".into(),
            name: "write".into(),
            arguments: serde_json::json!({
                "path": "a.txt",
                "content": (1..=25).map(|i| format!("line{}", i)).collect::<Vec<_>>().join("\n")
            }),
            thought_signature: None,
            namespace: None,
        }];
        let lines = render_all_messages(&[ai], &[], None, 60, &theme(), true, true);
        let joined = t(&lines).join("\n");
        assert!(
            joined.contains("line25"),
            "展开显示全部 content: {:?}",
            joined
        );
        assert!(
            !joined.contains("more lines"),
            "展开无折叠提示: {:?}",
            joined
        );
    }

    #[test]
    fn bash_fold_limits_visual_lines_with_wrapped_tail() {
        // 对齐 pi truncateToVisualLines：bash 折叠按**视觉行**（宽度折行后）取末尾，
        // 超长行被折行时折叠后的显示行数必须仍受 BASH_PREVIEW_LINES 限制。
        // 构造：20 行短行 + 1 行 80 字符长行（终端 20 → 折成 4 个视觉行）
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.timestamp = 100;
        ai.content = vec![ContentBlock::ToolCall {
            id: "b1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "make noise" }),
            thought_signature: None,
            namespace: None,
        }];
        let mut res = AgentMessage::user_text(&{
            let mut v: Vec<String> = (1..=20).map(|i| format!("line{}", i)).collect();
            v.push("x".repeat(80));
            v.join("\n")
        });
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("b1".into());
        res.timestamp = 200;
        let lines = render_all_messages(&[ai, res], &[], None, 20, &theme(), false, true);
        let tt = t(&lines);
        // 输出内容行数（去掉头部 `$` 行、hint 行、背景/空行）不得超过 5
        let content_rows = tt
            .iter()
            .filter(|l| {
                let s = l.trim();
                !s.is_empty() && !s.starts_with('$') && !s.starts_with("... (")
            })
            .count();
        assert!(content_rows <= 5, "bash 折叠视觉行超限: {:?}", tt);
        assert!(
            tt.iter()
                .any(|l| l.contains("earlier lines, ctrl+o/alt+click to expand)")),
            "缺 earlier 提示: {:?}",
            tt
        );
    }

    /// 2.13：扩展注册的渲染器接管工具块（含折叠提示），注销 / 禁用后回退内置渲染。
    #[test]
    fn extension_renderer_takes_over_tool_block_and_falls_back() {
        use crate::core::extensions::{
            Extension, ExtensionMode, ExtensionTool, RichSpan, ToolRender, ToolRenderCtx,
            ToolRenderer, register_extension, set_extension_mode, unregister_extension,
        };
        use std::sync::Arc;

        /// 固定产出 4 行文本（折叠保留 2 行）的假渲染器。
        struct Fake;

        impl ToolRenderer for Fake {
            fn name(&self) -> &'static str {
                "fake-mcp-renderer"
            }
            fn tools(&self) -> &[&'static str] {
                // 用真实工具不存在的名字：渲染器注册表是进程级全局态，
                // 挂到 "mcp" 会在注册窗口内接管并行测试的 mcp 渲染
                &["fake-mcp"]
            }
            fn render(&self, ctx: &ToolRenderCtx<'_>) -> Option<ToolRender> {
                let head = format!("RENDERED {} ({})", ctx.tool, ctx.tool);
                Some(ToolRender {
                    lines: vec![
                        vec![RichSpan::plain(head)],
                        vec![RichSpan::plain("row-2")],
                        vec![RichSpan::plain("row-3")],
                        vec![RichSpan::plain("row-4")],
                    ],
                    preview_lines: Some(2),
                })
            }
        }

        struct Faker;
        impl Extension for Faker {
            fn name(&self) -> &str {
                "fake-renderer-ext"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn tool_renderers(&self) -> Vec<crate::core::extensions::RegisteredToolRenderer> {
                vec![crate::core::extensions::RegisteredToolRenderer::new(
                    Arc::new(Fake),
                )]
            }
        }

        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        set_extension_mode(ExtensionMode::All);

        let built = || {
            let mut ai = AgentMessage::user_text("");
            ai.role = "assistant".to_string();
            ai.timestamp = 100;
            ai.content = vec![ContentBlock::ToolCall {
                id: "m1".into(),
                name: "fake-mcp".into(),
                arguments: serde_json::json!({ "server": "docs", "tool": "lookup" }),
                thought_signature: None,
                namespace: None,
            }];
            let mut res = AgentMessage::user_text("native output\nsecond line");
            res.role = "toolResult".to_string();
            res.tool_call_id = Some("m1".into());
            res.timestamp = 200;
            (ai, res)
        };

        // 未注册：内置渲染（工具名未注册渲染器 → 通用工具块）
        let (ai, res) = built();
        let plain = t(&render_all_messages(
            &[ai, res],
            &[],
            None,
            40,
            &theme(),
            false,
            false,
        ));
        assert!(
            plain.iter().any(|l| l.contains("native output")),
            "{plain:?}"
        );

        // 注册后：扩展渲染器接管
        register_extension(Faker);
        let (ai, res) = built();
        let rendered = t(&render_all_messages(
            &[ai, res],
            &[],
            None,
            40,
            &theme(),
            false,
            false,
        ));
        let joined = rendered.join("\n");
        assert!(joined.contains("RENDERED fake-mcp (fake-mcp)"), "{joined}");
        assert!(
            !joined.contains("native output"),
            "内置渲染应被接管: {joined}"
        );
        // 折叠保留 2 行 + 提示行（renderers 的 preview_lines=2）
        assert!(joined.contains("row-2"), "{joined}");
        assert!(!joined.contains("row-3"), "折叠应截掉第 3 行: {joined}");
        assert!(
            joined.contains("2 more lines, ctrl+o/alt+click to expand"),
            "{joined}"
        );

        // Ctrl+O 展开：全部展示
        let (ai, res) = built();
        let expanded = t(&render_all_messages(
            &[ai, res],
            &[],
            None,
            40,
            &theme(),
            true,
            false,
        ))
        .join("\n");
        assert!(expanded.contains("row-4"), "{expanded}");

        // 禁用扩展：立即回退内置渲染
        crate::core::extensions::set_extension_enabled("fake-renderer-ext", false);
        let (ai, res) = built();
        let disabled = t(&render_all_messages(
            &[ai, res],
            &[],
            None,
            40,
            &theme(),
            false,
            false,
        ))
        .join("\n");
        assert!(disabled.contains("native output"), "{disabled}");
        assert!(!disabled.contains("RENDERED fake-mcp"), "{disabled}");

        crate::core::extensions::set_extension_enabled("fake-renderer-ext", true);
        assert!(unregister_extension("fake-renderer-ext"));
    }

    #[test]
    fn non_bash_fold_limits_visual_lines_for_long_single_line() {
        // 对齐 pi VisualLinePreview（#10283）：非 bash 工具（含 mcp）的折叠预览也要按
        // 视觉行限制；一条超长逻辑行（minified JSON）不得折出整屏。
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.timestamp = 100;
        ai.content = vec![ContentBlock::ToolCall {
            id: "m1".into(),
            name: "mcp".into(),
            arguments: serde_json::json!({ "action": "call", "server": "fs", "name": "read", "arguments": {} }),
            thought_signature: None,
            namespace: None,
        }];
        // 单条 400 字符逻辑行：终端 20 列下折成 20 个视觉行
        let mut res = AgentMessage::user_text(&"x".repeat(400));
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("m1".into());
        res.timestamp = 200;
        let lines = render_all_messages(&[ai, res], &[], None, 20, &theme(), false, true);
        let tt = t(&lines);
        // 预览行的内容只有 x（折行后的视觉行）；这些行不得多于 MCP_PREVIEW_LINES
        let x_rows = tt
            .iter()
            .filter(|l| {
                let s = l.trim();
                !s.is_empty() && s.chars().all(|c| c == 'x')
            })
            .count();
        assert_eq!(
            x_rows, MCP_PREVIEW_LINES,
            "mcp 折叠视觉行应严格等于 {MCP_PREVIEW_LINES}（实际 {x_rows}）: {tt:?}"
        );
        assert!(
            tt.iter()
                .any(|l| l.contains("more lines, ctrl+o/alt+click to expand)")),
            "缺折叠提示: {tt:?}"
        );
    }

    #[test]
    fn tool_block_pending_shows_elapsed_timer() {
        // 对齐 pi isPartial 计时：进行中工具块显示 `Elapsed X.Xs`（实时刷新），
        // 完成后显示 `Took X.Xs`，二者不可同时出现
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.timestamp = 100;
        ai.content = vec![ContentBlock::ToolCall {
            id: "e1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "sleep 5" }),
            thought_signature: None,
            namespace: None,
        }];
        // 进行中：无 toolResult，started_at = 当前墙钟 - 1.2s
        let started = crate::utils::time::now_ms().saturating_sub(1200);
        let mut started_map = std::collections::HashMap::new();
        started_map.insert("e1".to_string(), started);
        let mut pending = Vec::new();
        let lines = render_message(
            &ai,
            60,
            &theme(),
            true,
            true,
            &mut pending,
            &std::collections::HashMap::new(),
            &started_map,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let joined = t(&lines).join("\n");
        assert!(
            joined.contains("Elapsed 1."),
            "进行中应显示 Elapsed: {:?}",
            joined
        );
        assert!(
            !joined.contains("Took"),
            "进行中不应显示 Took: {:?}",
            joined
        );
        assert_eq!(
            pending[0].started_at_ms,
            Some(started),
            "pending 应携带 started_at"
        );

        // 完成后：toolResult 预扫描 → Took，且不再显示 Elapsed
        let mut res = AgentMessage::user_text("done");
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("e1".into());
        res.duration_ms = Some(3200);
        let mut done_map = std::collections::HashMap::new();
        done_map.insert(
            "e1".to_string(),
            ToolResultView {
                text: res.text(),
                duration_ms: Some(3200),
                ..Default::default()
            },
        );
        let mut pending2 = Vec::new();
        let lines2 = render_message(
            &ai,
            60,
            &theme(),
            true,
            true,
            &mut pending2,
            &done_map,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let joined2 = t(&lines2).join("\n");
        assert!(
            joined2.contains("Took 3.2s"),
            "完成后应显示 Took: {:?}",
            joined2
        );
        assert!(!joined2.contains("Elapsed"), "完成后不应显示 Elapsed");
    }

    #[test]
    fn summary_blocks_have_labels() {
        let msg = AgentMessage::user_text("everything summarized");
        let lines = render_summary(&msg.text(), true, 60, &theme(), false, &mut Vec::new());
        let t = t(&lines);
        assert!(t.iter().any(|l| l.contains("[compaction]")));
        assert!(t.iter().any(|l| l.contains("Compacted context")));
    }

    #[test]
    fn edit_result_renders_gutter_diff_with_colors() {
        // 对齐 pi：edit 完成时渲染 details.diff 的 gutter diff，不显示成功文本/PATCH；保留 Took
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.content = vec![ContentBlock::ToolCall {
            id: "e1".into(),
            name: "edit".into(),
            arguments: serde_json::json!({
                "path": "a.txt",
                "edits": [{"oldText": "old a\nold b", "newText": "new a\nnew b"}]
            }),
            thought_signature: None,
            namespace: None,
        }];
        let mut res = AgentMessage::user_text("Successfully replaced 1 block(s) in a.txt.");
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("e1".into());
        res.duration_ms = Some(700);
        res.details = Some(serde_json::json!({
            "diff": " 1 # title\n-2 old a\n-3 old b\n+2 new a\n+3 new b\n 4 tail",
        }));
        let mut edit_map = HashMap::new();
        edit_map.insert(
            "e1".to_string(),
            EditRenderData {
                preview: None,
                result_diff: Some(
                    " 1 # title\n-2 old a\n-3 old b\n+2 new a\n+3 new b\n 4 tail".to_string(),
                ),
            },
        );
        let lines = render_message(
            &ai,
            60,
            &theme(),
            true,
            true,
            &mut Vec::new(),
            &scan_tool_results(&[ai.clone(), res]),
            &HashMap::new(),
            &std::collections::HashMap::new(),
            &edit_map,
        );
        let joined = t(&lines).join("\n");
        assert!(joined.contains("edit a.txt"), "头部: {:?}", joined);
        assert!(joined.contains("-2 old a"), "删除行: {:?}", joined);
        assert!(joined.contains("+3 new b"), "新增行: {:?}", joined);
        assert!(
            !joined.contains("Successfully replaced"),
            "成功文本不应显示: {:?}",
            joined
        );
        assert!(!joined.contains("PATCH"), "PATCH 不应显示: {:?}", joined);
        assert!(joined.contains("Took 0.7s"), "保留耗时: {:?}", joined);
        // 配色（主题已映射 toolDiffRemoved→red / toolDiffAdded→green / Context→gray）：
        // 断言三种行都被着色且两两不同，不硬编码具体 RGB。
        let removed_fg = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.contains("-2 old a"))
            .map(|s| s.style.fg);
        let added_fg = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.contains("+3 new b"))
            .map(|s| s.style.fg);
        let context_fg = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.contains("1 # title"))
            .map(|s| s.style.fg);
        assert!(
            removed_fg.flatten().is_some(),
            "删除行应有前景色: {:?}",
            joined
        );
        assert!(
            added_fg.flatten().is_some(),
            "新增行应有前景色: {:?}",
            joined
        );
        assert_ne!(removed_fg, added_fg, "删除/新增颜色应不同: {:?}", joined);
        assert_ne!(context_fg, added_fg, "上下文与新增颜色应不同: {:?}", joined);
    }

    #[test]
    fn edit_pending_preview_renders_diff() {
        // call 阶段（无结果）预览 diff：单行替换走词级反色，仍应渲染 gutter 文本
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.content = vec![ContentBlock::ToolCall {
            id: "p1".into(),
            name: "edit".into(),
            arguments: serde_json::json!({
                "path": "a.txt",
                "edits": [{"oldText": "old line", "newText": "new line"}]
            }),
            thought_signature: None,
            namespace: None,
        }];
        let mut edit_map = HashMap::new();
        edit_map.insert(
            "p1".to_string(),
            EditRenderData {
                preview: Some(" 1 # title\n-2 old line\n+2 new line\n 3 tail".to_string()),
                result_diff: None,
            },
        );
        let lines = render_message(
            &ai,
            60,
            &theme(),
            true,
            true,
            &mut Vec::new(),
            &HashMap::new(),
            &HashMap::new(),
            &std::collections::HashMap::new(),
            &edit_map,
        );
        let joined = t(&lines).join("\n");
        assert!(joined.contains("-2 old line"), "预览删除行: {:?}", joined);
        assert!(joined.contains("+2 new line"), "预览新增行: {:?}", joined);
    }

    #[test]
    fn edit_error_shows_error_text() {
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.content = vec![ContentBlock::ToolCall {
            id: "e2".into(),
            name: "edit".into(),
            arguments: serde_json::json!({
                "path": "a.txt",
                "edits": [{"oldText": "old", "newText": "new"}]
            }),
            thought_signature: None,
            namespace: None,
        }];
        let mut res = AgentMessage::user_text("Could not find the exact text in a.txt.");
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("e2".into());
        res.is_error = true;
        let lines = render_message(
            &ai,
            60,
            &theme(),
            true,
            true,
            &mut Vec::new(),
            &scan_tool_results(&[ai.clone(), res]),
            &HashMap::new(),
            &HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let joined = t(&lines).join("\n");
        assert!(
            joined.contains("Could not find the exact text"),
            "错误文本: {:?}",
            joined
        );
    }

    #[test]
    fn aborted_stop_reason_hint() {
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.stop_reason = Some("aborted".into());
        msg.content = vec![ContentBlock::Text {
            text: "partial".into(),
            text_signature: None,
        }];
        let lines = render_message(
            &msg,
            60,
            &theme(),
            false,
            true,
            &mut Vec::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let t = t(&lines);
        assert!(t.iter().any(|l| l.contains("Operation aborted")));
    }

    #[test]
    fn system_prompts_interleave_by_timestamp() {
        // 对齐 pi chatContainer：提示按发生时间插入消息流，而不是堆积在末尾
        let mut user = AgentMessage::user_text("user msg");
        user.timestamp = 1000;
        let mut ai = AgentMessage::user_text("assistant reply");
        ai.role = "assistant".to_string();
        ai.timestamp = 2000;
        let sys_after = (
            3000u64,
            SysMsg::plain(MsgLevel::Success, "✓ New session started"),
        );
        let sys_before = (500u64, SysMsg::plain(MsgLevel::Info, "old note"));
        let lines = render_all_messages(
            &[user, ai],
            &[sys_after, sys_before],
            None,
            60,
            &theme(),
            false,
            true,
        );
        let t = t(&lines);
        let p_old = t.iter().position(|l| l.contains("old note")).unwrap();
        let p_user = t.iter().position(|l| l.contains("user msg")).unwrap();
        let p_ai = t
            .iter()
            .position(|l| l.contains("assistant reply"))
            .unwrap();
        let p_new = t
            .iter()
            .position(|l| l.contains("✓ New session started"))
            .unwrap();
        assert!(
            p_old < p_user && p_user < p_ai && p_ai < p_new,
            "时间线顺序错误: old={} user={} ai={} new={}",
            p_old,
            p_user,
            p_ai,
            p_new
        );
    }

    #[test]
    fn long_error_message_wraps_instead_of_truncating() {
        // 回归：provider 长错误（无换行）超宽时按消息宽度折行，不得截断
        let msg = "Error: provider returned 503 Service Unavailable: {\"error\":{\"type\":\"server_error\",\"message\":\"Error from provider (Console Go): Upstream request failed: Endpoint is unavailable.\"}}";
        let lines = render_sys_message(&[SysSpan::plain(msg)], MsgLevel::Error, 60, &theme());
        let t = t(&lines);
        assert!(t.len() > 1, "超宽错误应折行: {:?}", t);
        for l in &t {
            assert!(
                display_width(l) <= 60,
                "折行后仍超宽: {:?} ({})",
                l,
                display_width(l)
            );
        }
        let joined: String = t.concat();
        assert!(
            joined.starts_with("Error: provider returned 503"),
            "头部缺失"
        );
        assert!(joined.ends_with("unavailable.\"}}"), "尾部缺失: {joined:?}");
    }

    #[test]
    fn long_error_hint_wraps_instead_of_truncating() {
        // 回归：assistant 消息 stop_reason=error 的 provider 长错误（无换行）
        // 渲染 hint 时按消息宽度折行，不得横向截断
        let msg = AgentMessage {
            role: "assistant".into(),
            thinking_level: None,
            content: vec![crate::core::provider::ContentBlock::Text {
                text: "I hit an error.".into(),
                text_signature: None,
            }],
            stop_reason: Some("error".into()),
            error_message: Some("provider returned 503 Service Unavailable: {\"error\":{\"type\":\"server_error\",\"message\":\"Error from provider (Console Go): Upstream request failed: Endpoint is unavailable.\"}}".into()),
            ..AgentMessage::user_text("")
        };
        let mut pending = Vec::new();
        let lines = render_message(
            &msg,
            60,
            &theme(),
            false,
            true,
            &mut pending,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let t = t(&lines);
        let joined_all: String = t.concat();
        assert!(
            joined_all.contains("Endpoint is unavailable."),
            "错误尾部缺失: {joined_all:?}"
        );
        assert!(
            joined_all.contains("I hit an error."),
            "正文缺失: {joined_all:?}"
        );
        for l in &t {
            assert!(
                display_width(l) <= 61,
                "折行后仍超宽: {:?} ({})",
                l,
                display_width(l)
            );
        }
        let first_err = t
            .iter()
            .position(|l| l.contains("provider returned 503"))
            .expect("错误行应存在");
        assert!(t.len() - first_err > 2, "超宽错误应折成多行: {:?}", t);
    }

    #[test]
    fn system_prompt_styled_and_wrapped() {
        let lines = render_all_messages(
            &[],
            &[(
                1u64,
                SysMsg::plain(MsgLevel::Info, &"long system note ".repeat(10)),
            )],
            None,
            20,
            &theme(),
            false,
            true,
        );
        let t = t(&lines);
        assert!(t.iter().any(|l| l.contains("system note")), "{:?}", t);
        assert!(t.len() > 2, "超宽提示按消息宽度折行: {:?}", t);
    }

    #[test]
    fn system_message_keeps_line_breaks_and_multicolor() {
        // 对齐 pi：系统消息保留 \n 换行且支持按段配色（/session 富文本）
        let spans = vec![
            SysSpan {
                fg: Some("success".to_string()),
                bold: true,
                text: "Title\n".to_string(),
            },
            SysSpan::plain("\n"),
            SysSpan::fg("dim", "Label:"),
            SysSpan::fg("text", " value\n"),
        ];
        let lines = render_sys_message(&spans, MsgLevel::Info, 60, &theme());
        let t = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(t.len(), 4, "\n 应拆成三行+块尾间隔空行: {:?}", t);
        assert_eq!(t[0], "Title", "标题行: {:?}", t);
        assert_eq!(t[1], "", "块间应留一空行: {:?}", t);
        assert_eq!(t[2], "Label: value", "标签+值同行: {:?}", t);
        assert_eq!(t[3], "", "块尾应有渲染间隔空行: {:?}", t);
        // 配色：标题 success 粗体、标签 dim、值 text（比对主题解析结果，不硬编码调色板）
        let th = theme();
        let title = &lines[0].spans[0];
        assert!(
            title
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert_eq!(title.style.fg, Some(th.resolve_color("success")));
        assert_eq!(lines[2].spans[0].style.fg, Some(th.resolve_color("dim")));
        assert_eq!(
            lines[2].spans[1].style.fg,
            Some(Color::Reset),
            "text 值色为终端默认"
        );
    }

    #[test]
    fn system_message_expands_tabs() {
        // ratatui 落格会丢掉 \t，系统消息中的制表符必须先展开成空格（/mcp status 表格）
        let spans = vec![SysSpan::plain("local\tenabled\thttp://x/mcp\ttools=0")];
        let lines = render_sys_message(&spans, MsgLevel::Info, 80, &theme());
        let text = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "local    enabled    http://x/mcp    tools=0");
        assert_eq!(
            crate::utils::display::display_width(&text),
            crate::utils::display::grapheme_width(&text),
            "展开后的显示宽度必须与 ratatui 落格宽度一致"
        );
    }

    #[test]
    fn system_message_collapses_trailing_blank() {
        // 内容尾部连续 \n 只换行，不产生空白行（对齐 pi Text），
        // 但渲染出口仍补一块级间隔空行
        let spans = vec![SysSpan::plain("A\nB\n\n")];
        let lines = render_sys_message(&spans, MsgLevel::Info, 60, &theme());
        let t = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            t,
            vec!["A".to_string(), "B".to_string(), "".to_string()],
            "内容尾部折叠+块尾间隔空行: {:?}",
            t
        );
    }

    #[test]
    fn session_info_blocks_separated_by_blank_and_cost_renders() {
        // 复刻 /session 的 span 结构与 Cost 块（对齐 pi /session 输出）
        let spans = vec![
            SysSpan {
                fg: Some("text".to_string()),
                bold: true,
                text: "Session Info\n".to_string(),
            },
            SysSpan::plain("\n"),
            SysSpan::fg("dim", "File:"),
            SysSpan::fg("text", " /path\n"),
            SysSpan::fg("dim", "ID:"),
            SysSpan::fg("text", " abc\n\n"),
            SysSpan {
                fg: Some("text".to_string()),
                bold: true,
                text: "Messages\n".to_string(),
            },
            SysSpan::fg("dim", "Tools:"),
            SysSpan::fg("text", " 1 calls, 0 results\n\n"),
            SysSpan {
                fg: Some("text".to_string()),
                bold: true,
                text: "Tokens\n".to_string(),
            },
            SysSpan::fg("dim", "Input:"),
            SysSpan::fg("text", " 1,000\n"),
            SysSpan::fg("dim", "Total:"),
            SysSpan::fg("text", " 1,062\n"),
            SysSpan {
                fg: Some("text".to_string()),
                bold: true,
                text: "\nCost\n".to_string(),
            },
            SysSpan::fg("dim", "Total:"),
            SysSpan::fg("text", " $0.002\n"),
        ];
        let lines = render_sys_message(&spans, MsgLevel::Info, 80, &theme());
        let t = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(t.contains(&"Session Info".to_string()), "{:?}", t);
        assert!(t.contains(&"Messages".to_string()), "{:?}", t);
        assert!(t.contains(&"Tokens".to_string()), "{:?}", t);
        assert!(t.contains(&"Cost".to_string()), "Cost 块必须渲染: {:?}", t);
        assert!(
            t.contains(&"Total: $0.002".to_string()),
            "Cost total: {:?}",
            t
        );
        // 块间有空行：Cost 前一行是空行
        let ci = t.iter().position(|l| l == "Cost").unwrap();
        assert_eq!(t[ci - 1], "", "Cost 前应空行: {:?}", t);
        // 块尾保留渲染间隔空行
        assert_eq!(
            t.last().map(|s| s.as_str()),
            Some(""),
            "块尾应有间隔空行: {:?}",
            t
        );
    }

    #[test]
    fn sys_message_level_selects_color_style() {
        let lines = render_all_messages(
            &[],
            &[
                (
                    1u64,
                    SysMsg::plain(MsgLevel::Success, "✓ New session started"),
                ),
                (2u64, SysMsg::plain(MsgLevel::Info, "plain note")),
                (3u64, SysMsg::plain(MsgLevel::Warning, "not supported")),
                (4u64, SysMsg::plain(MsgLevel::Error, "failed to load")),
            ],
            None,
            60,
            &theme(),
            false,
            true,
        );
        let mk = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("✓ New")))
            .unwrap();
        let plain = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("plain")))
            .unwrap();
        let warn = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("not supported")))
            .unwrap();
        let err = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("failed to load")))
            .unwrap();
        let theme = theme();
        // 级别颜色取自 pi 的规范状态键（`success`/`dim`/`warning`/`error`）；
        // 断言用主题键的解析结果，避免硬编码 RGB 与主题定义耦合
        assert_eq!(
            mk.spans[0].style.fg,
            theme.style("success", "#b5bd68").fg,
            "success 消息使用主题 success 键"
        );
        assert_eq!(
            plain.spans[0].style.fg,
            theme.style("dim", "#666666").fg,
            "info 使用主题 dim 键"
        );
        assert_ne!(
            mk.spans[0].style.fg, plain.spans[0].style.fg,
            "success 与 info 颜色不同"
        );
        assert_eq!(
            warn.spans[0].style.fg,
            theme.style("warning", "#d8a657").fg,
            "warning 使用主题 warning 键"
        );
        assert!(
            warn.spans[0].style.add_modifier.contains(Modifier::BOLD),
            "warning 加粗"
        );
        assert_eq!(
            err.spans[0].style.fg,
            theme.style("error", "#cc6666").fg,
            "error 使用主题 error 键"
        );
        assert!(
            err.spans[0].style.add_modifier.contains(Modifier::BOLD),
            "error 加粗"
        );
        assert!(
            !plain.spans[0].style.add_modifier.contains(Modifier::BOLD),
            "info 不加粗"
        );
    }

    #[test]
    fn sys_level_uses_canonical_state_keys() {
        // 级别 → pi 规范键：info=dim、success=success、warning=warning、error=error
        let t = theme();
        for (level, key) in [
            (MsgLevel::Info, "dim"),
            (MsgLevel::Success, "success"),
            (MsgLevel::Warning, "warning"),
            (MsgLevel::Error, "error"),
        ] {
            assert_eq!(
                sys_level_style(level, &t).fg,
                Some(t.resolve_color(key)),
                "{level:?} 应取主题 {key} 键"
            );
        }

        // 主题完全没定义这些键时用内置兜底色
        let bare = Theme {
            name: "bare".to_string(),
            vars: HashMap::new(),
            syntax_highlight: true,
            mermaid: true,
            latex: true,
            appearance: None,
            dim_keys: std::collections::HashSet::new(),
        };
        assert_eq!(
            sys_level_style(MsgLevel::Info, &bare).fg,
            Some(Color::Rgb(0x66, 0x66, 0x66)),
            "info 回退到兜底色"
        );
        assert_eq!(
            sys_level_style(MsgLevel::Error, &bare).fg,
            Some(Color::Rgb(0xcc, 0x66, 0x66)),
            "error 回退到兜底色"
        );
    }

    #[test]
    fn user_multiline_message_keeps_line_breaks() {
        // 粘贴折叠展开后发送的多行 user 消息：渲染保持多行，不被压成单行
        let mut m = AgentMessage::user_text("line1\nline2\nline3\nline4");
        m.role = "user".to_string();
        let lines = render_all_messages(&[m], &[], None, 60, &theme(), false, true);
        let t = t(&lines);
        for (i, l) in ["line1", "line2", "line3", "line4"].iter().enumerate() {
            assert!(t.iter().any(|x| x.contains(l)), "第 {} 行丢失: {:?}", i, t);
        }
        let joined: String = t.join("\n");
        let p1 = joined.find("line1").unwrap();
        let p2 = joined.find("line2").unwrap();
        let p3 = joined.find("line3").unwrap();
        let p4 = joined.find("line4").unwrap();
        assert!(p1 < p2 && p2 < p3 && p3 < p4, "行序保持: {:?}", t);
    }

    #[test]
    fn messages_separated_by_blank_line() {
        let a = AgentMessage::user_text("first");
        let mut b = AgentMessage::user_text("");
        b.role = "assistant".to_string();
        b.content = vec![ContentBlock::Text {
            text: "second".into(),
            text_signature: None,
        }];
        let lines = render_all_messages(&[a, b], &[], None, 60, &theme(), false, true);
        let t = t(&lines);
        let ia = t.iter().position(|l| l.contains("first")).unwrap();
        let ib = t.iter().position(|l| l.contains("second")).unwrap();
        // 消息间至少一个空白行
        assert!(ia + 1 < ib, "expected gap: {:?}", t);
        assert!(
            t[ia + 1..ib].iter().any(|l| l.trim().is_empty()),
            "no blank line between messages: {:?}",
            t
        );
    }

    fn is_pure_gap_line(line: &Line<'static>) -> bool {
        line.spans.iter().all(|s| s.content.trim().is_empty())
            && !line.spans.iter().any(|s| s.style.bg.is_some())
    }

    #[test]
    fn assistant_blocks_have_blank_gaps() {
        // 回归：thinking → 工具块、Took → 文本之间必须有纯空行间隔（对齐 pi Spacer）
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![
            ContentBlock::Thinking {
                thinking: "think".into(),
                thinking_signature: None,
                redacted: None,
            },
            ContentBlock::ToolCall {
                id: "t1".into(),
                name: "ls".into(),
                arguments: serde_json::json!({}),
                thought_signature: None,
                namespace: None,
            },
            ContentBlock::Text {
                text: "result text".into(),
                text_signature: None,
            },
        ];
        let mut tool_map = std::collections::HashMap::new();
        let mut res = AgentMessage::user_text("a\nb");
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("t1".into());
        res.duration_ms = Some(10);
        tool_map.insert(
            "t1".to_string(),
            ToolResultView {
                text: res.text(),
                duration_ms: Some(10),
                ..Default::default()
            },
        );
        let lines = render_message(
            &msg,
            60,
            &theme(),
            true,
            true,
            &mut Vec::new(),
            &tool_map,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let texts = t(&lines);
        let p_thinking = texts.iter().position(|l| l.contains("think")).unwrap();
        let p_tool = texts
            .iter()
            .position(|l| l.trim().starts_with("ls ."))
            .unwrap();
        let p_took = texts.iter().position(|l| l.contains("Took")).unwrap();
        let p_text = texts
            .iter()
            .position(|l| l.contains("result text"))
            .unwrap();
        // thinking 与工具块之间至少一个纯空行（无背景）
        assert!(
            lines[p_thinking + 1..p_tool].iter().any(is_pure_gap_line),
            "thinking→工具块缺纯空行: {:?}",
            texts
        );
        // Took 与文本之间至少一个纯空行（无背景）
        assert!(
            lines[p_took + 1..p_text].iter().any(is_pure_gap_line),
            "Took→文本缺纯空行: {:?}",
            texts
        );
    }

    #[test]
    fn background_blocks_separated_by_pure_gap() {
        // 回归：背景块（用户消息/工具块）的 padding 行带背景色，不算间隔；
        // 块之间必须补一行无背景的纯空行（对齐 pi 组件间 Spacer）
        let user = AgentMessage::user_text("ls");
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.content = vec![ContentBlock::Thinking {
            thinking: "think".into(),
            thinking_signature: None,
            redacted: None,
        }];
        let lines = render_all_messages(&[user, ai], &[], None, 60, &theme(), true, true);
        let p_user = lines
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.contains("ls")))
            .unwrap();
        let p_thinking = lines
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.contains("think")))
            .unwrap();
        // 用户消息背景块与 thinking 之间至少一行无背景纯空行
        assert!(
            lines[p_user + 1..p_thinking].iter().any(is_pure_gap_line),
            "用户块→thinking 缺纯空行: {:?}",
            t(&lines)
        );
        // 背景 padding 行不应被当作间隔
        assert!(
            lines[p_user + 1..p_thinking]
                .iter()
                .any(|l| !is_pure_gap_line(l) && l.spans.iter().any(|s| s.style.bg.is_some())),
            "应存在带背景的 padding 行: {:?}",
            t(&lines)
        );
    }

    #[test]
    fn skill_invocation_collapses_by_default() {
        let text = "<skill name=\"html-ppt\" location=\"/x/skills/html-ppt/SKILL.md\">\nReferences are relative to /x/skills/html-ppt.\n\n# Build slides\n</skill>\n\nhttps://besok.github.io/posts/what-zig-felt-like-coming-from-rust/\n";
        let msg = AgentMessage::user_text(text);
        let lines = render_message(
            &msg,
            60,
            &theme(),
            false,
            true,
            &mut Vec::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let joined: Vec<String> = t(&lines);
        let all = joined.join("\n");
        // 折叠：只显示 [skill] 行 + 独立渲染的 userMessage，不输出 skill body
        assert!(
            all.contains("[skill] html-ppt (Ctrl+O/Alt+click to expand)"),
            "got: {:?}",
            joined
        );
        assert!(
            !all.contains("# Build slides"),
            "body 不应输出: {:?}",
            joined
        );
        assert!(
            !all.contains("References are relative to"),
            "location 不应输出: {:?}",
            joined
        );
        assert!(
            all.contains("besok.github.io/posts/what-zig-felt-like-coming"),
            "userMessage 应单独渲染: {:?}",
            joined
        );
    }

    #[test]
    fn skill_invocation_expands_with_markdown_body() {
        let text = "<skill name=\"html-ppt\" location=\"/x/SKILL.md\">\n# Build slides\n\n- step1\n</skill>";
        let msg = AgentMessage::user_text(text);
        let lines = render_message(
            &msg,
            60,
            &theme(),
            true,
            true,
            &mut Vec::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let all = t(&lines).join("\n");
        assert!(all.contains("[skill]"), "展开含 label: {:?}", all);
        assert!(all.contains("html-ppt"), "展开含技能名标题: {:?}", all);
        assert!(all.contains("Build slides"), "展开含 body: {:?}", all);
        assert!(all.contains("step1"), "展开含 body 内容: {:?}", all);
    }

    #[test]
    fn plain_user_message_untouched() {
        let msg = AgentMessage::user_text("just a normal message");
        let lines = render_message(
            &msg,
            60,
            &theme(),
            true,
            true,
            &mut Vec::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let all = t(&lines).join("\n");
        assert!(all.contains("just a normal message"));
        assert!(!all.contains("[skill]"));
    }

    #[test]
    fn fast_mode_skips_syntax_highlight() {
        // 恢复启动 Fast 渲染：syntax_highlight=false 时代码行单色（纯 codeBlock），
        // 完整模式下 syntect 产生多 token 颜色（同行的 fg 颜色种类更多）
        let md = "```rust\nlet x = 1;\n```";
        let mut th = theme();
        assert!(th.syntax_highlight, "默认主题应开启高亮");
        th.syntax_highlight = false;
        let fast = render_markdown_styled(md, 60, &th, ratatui::style::Style::default());
        let fast_code_line = fast
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content == "let"))
            .expect("fast 渲染应含代码行");
        let fast_fgs: std::collections::HashSet<_> = fast_code_line
            .spans
            .iter()
            .filter_map(|s| s.style.fg)
            .collect();
        assert_eq!(
            fast_fgs.len(),
            1,
            "fast 代码行应单色（纯 codeBlock 色）: {:?}",
            fast_fgs
        );
        let full = render_markdown_styled(md, 60, &theme(), ratatui::style::Style::default());
        let full_code_line = full
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content == "let"))
            .expect("full 渲染应含代码行");
        let full_fgs: std::collections::HashSet<_> = full_code_line
            .spans
            .iter()
            .filter_map(|s| s.style.fg)
            .collect();
        assert!(
            full_fgs.len() > fast_fgs.len(),
            "syntect 应产生多种 token 颜色: fast={:?} full={:?}",
            fast_fgs,
            full_fgs
        );
    }

    #[test]
    fn full_history_matches_render_all_messages() {
        // 缓存管线（render_full_history + 块拼接）必须与旧全量渲染输出完全一致，
        // 防止缓存切分引入行为回归（含块间间隔、时间线排序、工具结果填充）
        let mut ai = AgentMessage::user_text("");
        ai.role = "assistant".to_string();
        ai.content = vec![ContentBlock::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "ls" }),
            thought_signature: None,
            namespace: None,
        }];
        ai.timestamp = 10;
        let mut res = AgentMessage::user_text("");
        res.role = "toolResult".to_string();
        res.content = vec![ContentBlock::Text {
            text: "file1\nfile2".into(),
            text_signature: None,
        }];
        res.tool_call_id = Some("c1".into());
        res.timestamp = 20;
        let user = AgentMessage::user_text("hello *world*");
        let mut user2 = user.clone();
        user2.timestamp = 30;
        let msgs = vec![ai, user, user2, res];
        let sys: Vec<(u64, SysMsg)> = vec![(15, SysMsg::plain(MsgLevel::Success, "✓ sys note"))];

        let snap = HighlightSnapshot {
            msgs: msgs.clone(),
            sys: sys.clone(),
            width: 60,
            expand_all: false,
            show_thinking: true,
            images: ImageLayout::disabled(),
            overrides: HashMap::new(),
            theme: theme(),
        };
        let cached = render_full_history(&snap);
        // 按时间线顺序拼接缓存块（块后补纯空行间隔，与 render_all_messages 同规则）
        let items = timeline_items(&msgs, &sys);
        let mut actual: Vec<Line<'static>> = Vec::new();
        for (ts, seq, _) in items {
            let Some(c) = cached.get(&(ts, seq)) else {
                panic!("缓存缺失 ({}, {})", ts, seq);
            };
            if c.lines.is_empty() {
                continue;
            }
            actual.extend(c.lines.iter().cloned());
            let ends_blank = c.lines.last().map(is_pure_gap).unwrap_or(true);
            if !ends_blank {
                actual.push(Line::from(""));
            }
        }
        let expected = render_all_messages(&msgs, &sys, None, 60, &theme(), false, true);
        assert_eq!(t(&actual), t(&expected), "缓存管线输出与全量渲染不一致");
    }

    #[test]
    fn timeline_keeps_message_list_order_despite_timestamps() {
        // steer 消息创建时刻（输入时）早于前一条 assistant 完成时刻；
        // 渲染必须按消息列表顺序（处理顺序），不能按时间戳重排。
        let mut m1 = AgentMessage::user_text("msg1");
        m1.timestamp = 100;
        let mut a1 = AgentMessage::user_text("");
        a1.role = "assistant".to_string();
        a1.content = vec![ContentBlock::Text {
            text: "assistant1".into(),
            text_signature: None,
        }];
        a1.timestamp = 500;
        let mut m2 = AgentMessage::user_text("msg2 steer");
        m2.timestamp = 200; // 输入早，注入晚
        let mut a2 = AgentMessage::user_text("");
        a2.role = "assistant".to_string();
        a2.content = vec![ContentBlock::Text {
            text: "assistant2".into(),
            text_signature: None,
        }];
        a2.timestamp = 600;
        let msgs = vec![m1, a1, m2, a2];

        let items = timeline_items(&msgs, &[]);
        let texts: Vec<String> = items
            .iter()
            .map(|(_, _, r)| match r {
                TimelineRef::Msg(m) => m.text(),
                TimelineRef::Sys(_) => String::new(),
            })
            .collect();
        assert_eq!(
            texts,
            vec!["msg1", "assistant1", "msg2 steer", "assistant2"],
            "消息应按列表顺序渲染，而非按时间戳"
        );
    }

    #[test]
    fn bash_long_command_header_wraps_not_truncates() {
        // 超长 bash 命令行应折行且完整保留，不能被截断成单行省略号
        let long = format!("echo {}", "x".repeat(200));
        let args = serde_json::json!({ "command": long });
        let lines = render_tool_block(
            "bash",
            &args,
            None,
            None,
            None,
            None,
            &[],
            30,
            &theme(),
            false,
        );
        let rows = t(&lines);
        let compact: String = rows
            .iter()
            .flat_map(|l| l.chars().filter(|c| !c.is_whitespace()))
            .collect();
        assert!(rows.len() > 3, "超长命令应折成多行: {rows:?}");
        assert!(compact.contains(&"x".repeat(200)), "命令尾部应完整保留");
        assert!(
            rows.iter().any(|l| l.contains('$')),
            "应保留命令前缀: {rows:?}"
        );
        for l in &rows {
            assert!(display_width(l) <= 30, "行超宽: {l:?}");
        }
    }

    #[test]
    fn tool_header_long_path_wraps_not_truncates() {
        // read/edit 等工具头路径超宽应折行，保留完整路径
        let long = format!("/very/long/{}", "segment/".repeat(20));
        let args = serde_json::json!({ "path": long });
        let lines = render_tool_block(
            "read",
            &args,
            None,
            None,
            None,
            None,
            &[],
            20,
            &theme(),
            false,
        );
        let rows = t(&lines);
        let compact: String = rows
            .iter()
            .flat_map(|l| l.chars().filter(|c| !c.is_whitespace()))
            .collect();
        assert!(rows.len() > 2, "长路径应折成多行: {rows:?}");
        assert!(compact.contains(&long), "路径应完整保留: {compact}");
        for l in &rows {
            assert!(display_width(l) <= 20, "行超宽: {l:?}");
        }
    }

    #[test]
    fn mcp_call_header_shows_server_slash_tool_with_args() {
        // 对齐 pi：`action=call` 的 mcp 调用标题为 `server/tool`，参数按 key=value 显示
        let args = serde_json::json!({
            "action": "call",
            "server": "fs",
            "name": "read_file",
            "arguments": { "path": "a.txt", "limit": 5 }
        });
        let rows = t(&render_tool_block(
            "mcp",
            &args,
            None,
            None,
            None,
            None,
            &[],
            80,
            &theme(),
            false,
        ));
        let joined = rows.join("\n");
        assert!(joined.contains("fs/read_file"), "标题: {rows:?}");
        assert!(joined.contains("path=\"a.txt\""), "参数对: {rows:?}");
        assert!(joined.contains("limit=5"), "参数对: {rows:?}");
        assert!(!joined.contains("call"), "action 不应出现: {rows:?}");
    }

    #[test]
    fn mcp_call_header_expanded_lists_one_arg_per_line() {
        let args = serde_json::json!({
            "action": "call",
            "server": "fs",
            "name": "read_file",
            "arguments": { "path": "a.txt", "limit": 5 }
        });
        let rows = t(&render_tool_block(
            "mcp",
            &args,
            None,
            None,
            None,
            None,
            &[],
            80,
            &theme(),
            true,
        ));
        assert!(rows.iter().any(|l| l.trim() == "fs/read_file"), "{rows:?}");
        assert!(rows.iter().any(|l| l.trim() == "path: a.txt"), "{rows:?}");
        assert!(rows.iter().any(|l| l.trim() == "limit: 5"), "{rows:?}");
    }

    #[test]
    fn mcp_non_call_action_shows_action_and_key_value_args() {
        let args = serde_json::json!({ "action": "search", "query": "weather" });
        let rows = t(&render_tool_block(
            "mcp",
            &args,
            None,
            None,
            None,
            None,
            &[],
            80,
            &theme(),
            false,
        ));
        let joined = rows.join("\n");
        assert!(joined.contains("mcp search"), "标题: {rows:?}");
        assert!(joined.contains("query=\"weather\""), "参数对: {rows:?}");
    }

    #[test]
    fn mcp_result_preview_folds_to_five_lines() {
        // 对齐 pi：MCP 结果折叠预览 5 行（其他工具 20 行）
        let args = serde_json::json!({
            "action": "call", "server": "fs", "name": "read_file", "arguments": {}
        });
        let output = (1..=8)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = t(&render_tool_block(
            "mcp",
            &args,
            Some((&output, false)),
            None,
            None,
            None,
            &[],
            80,
            &theme(),
            false,
        ));
        let joined = rows.join("\n");
        assert!(joined.contains("line5"), "前 5 行应显示: {rows:?}");
        assert!(!joined.contains("line6"), "第 6 行应折叠: {rows:?}");
        assert!(
            joined.contains("... (3 more lines, ctrl+o/alt+click to expand)"),
            "折叠提示: {rows:?}"
        );
    }

    #[test]
    fn generic_tool_header_shows_key_value_args() {
        // 无自定义调用头的工具（此处为扩展工具）按 pi 的通用形式显示参数
        let args = serde_json::json!({ "objective": "ship it", "count": 2 });
        let collapsed = t(&render_tool_block(
            "goal",
            &args,
            None,
            None,
            None,
            None,
            &[],
            80,
            &theme(),
            false,
        ))
        .join("\n");
        assert!(collapsed.contains("goal"), "标题: {collapsed}");
        assert!(collapsed.contains("objective=\"ship it\""), "{collapsed}");
        assert!(collapsed.contains("count=2"), "{collapsed}");

        let expanded = t(&render_tool_block(
            "goal",
            &args,
            None,
            None,
            None,
            None,
            &[],
            80,
            &theme(),
            true,
        ));
        assert!(expanded.iter().any(|l| l.trim() == "objective: ship it"));
        assert!(expanded.iter().any(|l| l.trim() == "count: 2"));
    }

    #[test]
    fn collapsed_args_cut_at_hundred_chars() {
        let args = serde_json::json!({ "objective": "x".repeat(300) });
        let rows = t(&render_tool_block(
            "goal",
            &args,
            None,
            None,
            None,
            None,
            &[],
            400,
            &theme(),
            false,
        ));
        let joined = rows.join("");
        let joined = joined.trim();
        let pairs = joined.split("objective=").nth(1).expect("参数对");
        assert!(pairs.ends_with("..."), "应截断: {pairs}");
        assert_eq!(
            pairs.chars().count(),
            COLLAPSED_ARGS_CHARS - "objective=".len(),
            "参数对（含键前缀）截断到 COLLAPSED_ARGS_CHARS 字符"
        );
    }

    fn assistant_with_tool_call(id: &str) -> AgentMessage {
        let mut m = AgentMessage::user_text("");
        m.role = "assistant".to_string();
        m.content = vec![ContentBlock::ToolCall {
            id: id.to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": "a.rs" }),
            thought_signature: None,
            namespace: None,
        }];
        m
    }

    fn tool_result(id: &str, text: &str) -> AgentMessage {
        let mut m = AgentMessage::user_text(text);
        m.role = "toolResult".to_string();
        m.tool_call_id = Some(id.to_string());
        m.tool_name = Some("read".to_string());
        m
    }

    /// 覆盖层查看器的消息缓存：同一消息命中缓存（Arc 指针不变）；
    /// 新增 toolResult 后对应的 assistant 块失效重渲染（结果内联显示）。
    #[test]
    fn message_cache_reuses_entries_and_invalidates_on_tool_result() {
        // 缓存键含**全局**扩展渲染器指纹：持全局锁与注册/注销渲染器的测试串行，
        // 否则并行时指纹变化会把缓存整表清空（断言"应命中缓存"随机失败）
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let theme = theme();
        let mut cache = MessageCache::default();
        let msgs = vec![assistant_with_tool_call("t1")];

        let first = cache.render(&msgs, 60, &theme, false, true);
        assert!(
            first
                .iter()
                .any(|l| t(std::slice::from_ref(l))[0].contains("read"))
        );
        let ptr1 = Arc::as_ptr(&cache.entries[0].0.lines);

        // 再次渲染（内容不变）：命中缓存
        let again = cache.render(&msgs, 60, &theme, false, true);
        assert_eq!(Arc::as_ptr(&cache.entries[0].0.lines), ptr1, "应命中缓存");
        assert_eq!(first.len(), again.len());

        // toolResult 到达：assistant 块失效重渲染，输出包含结果文本
        let msgs = vec![
            assistant_with_tool_call("t1"),
            tool_result("t1", "file contents here"),
        ];
        let after = cache.render(&msgs, 60, &theme, false, true);
        assert_ne!(
            Arc::as_ptr(&cache.entries[0].0.lines),
            ptr1,
            "toolResult 到达后 assistant 块应重渲染"
        );
        let joined: String = after
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("file contents here"),
            "结果应内联显示: {joined}"
        );
    }

    /// 渲染参数变化（宽度）→ 整体失效重建。
    #[test]
    fn message_cache_clears_when_width_changes() {
        // 缓存键含**全局**扩展渲染器指纹：持全局锁与注册/注销渲染器的测试串行，
        // 否则并行时指纹变化会把缓存整表清空（断言"应命中缓存"随机失败）
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let theme = theme();
        let mut cache = MessageCache::default();
        let msgs = vec![AgentMessage::user_text("hello world")];
        let _ = cache.render(&msgs, 60, &theme, false, true);
        assert_eq!(cache.entries.len(), 1);
        let _ = cache.render(&msgs, 40, &theme, false, true);
        assert_eq!(cache.width, 40, "宽度变化后缓存参数应更新");
        assert_eq!(cache.entries.len(), 1, "重建后仍有 1 条");
    }
}
