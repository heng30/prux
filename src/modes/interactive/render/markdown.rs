//! Markdown 渲染规则
//!
//! - heading：`# ` 前缀，h1 加粗+下划线，h>=2 加粗，h>=3 前缀同色；块后空行（除非后随列表/空行）
//! - paragraph：块后空行（除非后随列表/空行）
//! - code block：```lang 边框行（codeBlockBorder）+ 2 空格缩进内容（codeBlock）+ ```；
//!   流式未闭合围栏的最后一行若为部分围栏则修剪
//! - list：4 空格×depth 缩进，`- `/`N. ` marker（listBullet），task `[x] `，嵌套递归
//! - blockquote：`│ ` 边框（quoteBorder）+ 斜体（quote）
//! - hr：`─`×min(width,80)（mdHr）
//! - table：┌─┬─┐ 边框、粗体表头、单元格折行；过窄时回退文本
//! - inline：strong 加粗、em 斜体、codespan（mdCode）、link 下划线（mdLink，文本≠href 时附` (url)` mdLinkUrl）、删除线
//! - inline math：`$...$`（mdCodeBlock），display math `$$...$$`（mdCodeBlock，分式堆叠/上下限分列）
//! - tab → 3 空格；词级折行（CJK 每字符可断，超长词按字符断）

use super::{latex, mermaid, syntax_hl};
use crate::{
    core::markdown_table,
    modes::interactive::theme::Theme,
    utils::display::{char_width, display_width, grapheme_width},
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::{cell::RefCell, rc::Rc};
use unicode_segmentation::UnicodeSegmentation;

/// 一行内容经解析后的样式化片段序列，是渲染与折行的基本单位。
type Spans = Vec<Span<'static>>;

/// 标题文本前景色（h1 另加粗并带下划线）。
const MD_HEADING: (&str, &str) = ("mdHeading", "#f0c674");
/// 链接文本前景色，渲染时附加下划线。
const MD_LINK: (&str, &str) = ("mdLink", "#81a2be");
/// 链接后附 `(url)` 提示的前景色（仅当显示文本与 href 不同）。
const MD_LINK_URL: (&str, &str) = ("mdLinkUrl", "#666666");
/// 行内 codespan（`code`）的前景色。
const MD_CODE: (&str, &str) = ("mdCode", "#8abeb7");
/// 围栏代码块正文（含 mermaid 图）的前景色。
const MD_CODE_BLOCK: (&str, &str) = ("mdCodeBlock", "#b5bd68");
/// 围栏代码块的起始与结束边框行前景色。
const MD_CODE_BLOCK_BORDER: (&str, &str) = ("mdCodeBlockBorder", "#808080");
/// 引用块正文前景色，渲染时附加斜体。
const MD_QUOTE: (&str, &str) = ("mdQuote", "#808080");
/// 引用块左侧 `│ ` 边框的前景色。
const MD_QUOTE_BORDER: (&str, &str) = ("mdQuoteBorder", "#808080");
/// 分隔线（hr）横线字符的前景色。
const MD_HR: (&str, &str) = ("mdHr", "#808080");
/// 列表 `- `/`N. ` 标记的前景色。
const MD_LIST_BULLET: (&str, &str) = ("mdListBullet", "#8abeb7");
/// 公式（行内 `$...$` / 块级 `$$...$$`）渲染结果的前景色：沿用代码块色 `mdCodeBlock`。
const MD_MATH: (&str, &str) = ("mdCodeBlock", "#b5bd68");

/// 已解析块的大类，仅用于决定相邻块之间是否插入空行。
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    /// 标题块（`#` 前缀），h1 加粗带下划线。
    Heading,
    /// 普通段落块。
    Paragraph,
    /// 围栏代码块（含 mermaid 图）。
    Code,
    /// 有序/无序列表块。
    List,
    /// 引用块。
    Quote,
    /// GFM 表格块。
    Table,
    /// 水平分隔线块（`---`）。
    Rule,
}

/// 渲染结果行：`Wrap` 需词级折行，`Fixed` 已格式化（列表/引用/表格行）不再折行，`Code` 代码行折行时保留行首缩进
enum OutLine {
    /// 需词级折行的普通文本行。
    Wrap(Spans),
    /// 代码行，折行时保留行首缩进。
    Code(Spans),
    /// 已格式化的行（列表/引用/表格/边框），不再折行。
    Fixed(Spans),
}

/// 代码块渲染结果：`Mermaid` 表示 mermaid 已直接成行，调用方须立即续行（跳过块间空行逻辑）
enum CodeBlockOutcome {
    /// 普通代码块，已渲染完成的输出行。
    Done(Vec<OutLine>),
    /// mermaid 图已直接成行，调用方须跳过块间空行逻辑。
    Mermaid(Vec<OutLine>),
}

/// 解析结果缓存上限：条目数。
const PARSE_CACHE_ENTRIES: usize = 64;
/// 解析结果缓存上限：缓存文本的累计字节数（超出时从最旧条目开始淘汰）。
const PARSE_CACHE_BYTES: usize = 4 * 1024 * 1024;

thread_local! {
    /// markdown 文本 → 已解析事件序列。主题与宽度变化时布局要重做，
    /// 但解析结果与二者无关，因此在这里跨渲染复用
    static PARSE_CACHE: RefCell<Vec<ParsedDocument>> = const { RefCell::new(Vec::new()) };
}

/// 一条缓存的解析结果。
struct ParsedDocument {
    /// 解析输入（tab 已展开），命中时逐字节校验，避免哈希碰撞取到别的文档。
    text: String,
    /// 解析时是否启用 math（`$...$` / `$$...$$`）：同一文本在两种设置下事件序列不同。
    math: bool,
    /// 解析出的事件序列（owned，可被多次渲染共享）。
    events: Rc<Vec<Event<'static>>>,
}

/// 取（必要时解析并缓存）文本的事件序列；命中时把条目提到队尾（近似 LRU）。
fn parsed_events(normalized: &str, math: bool, options: Options) -> Rc<Vec<Event<'static>>> {
    PARSE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(index) = cache
            .iter()
            .position(|doc| doc.math == math && doc.text == normalized)
        {
            let doc = cache.remove(index);
            let events = doc.events.clone();
            cache.push(doc);
            return events;
        }

        let events: Vec<Event<'static>> = Parser::new_ext(normalized, options)
            .map(Event::into_static)
            .collect();
        cache.push(ParsedDocument {
            text: normalized.to_string(),
            math,
            events: Rc::new(events),
        });
        while cache.len() > 1
            && (cache.len() > PARSE_CACHE_ENTRIES
                || cache.iter().map(|d| d.text.len()).sum::<usize>() > PARSE_CACHE_BYTES)
        {
            cache.remove(0);
        }
        cache
            .last()
            .map(|doc| doc.events.clone())
            .unwrap_or_default()
    })
}

/// 事件源：渲染函数按顺序从中取事件。
///
/// 两种实现：主流程用 [`SliceSource`]（共享解析缓存的事件切片）；列表项段落用
/// [`RefSource`]（引用列表，嵌套列表的事件被就地渲染并从段内剔除，不能再用区间表达）。
/// `next_event` 返回的引用生命周期来自 `'a`（而非 `&mut self`），因此递归渲染时
/// 不会与事件源的可变借用冲突，也不需要拷贝事件。
trait EventSource<'a> {
    /// 取下一个事件并前进；耗尽返回 `None`。
    fn next_event(&mut self) -> Option<&'a Event<'static>>;
    /// 查看下一个事件但不前进；耗尽返回 `None`。
    fn peek_event(&self) -> Option<&'a Event<'static>>;
}

/// 基于共享事件切片的游标：覆盖 `[pos, end)` 区间（主流程 `end` 为序列长度）。
struct SliceSource<'a> {
    /// 事件切片（来自解析缓存，跨渲染复用）。
    events: &'a [Event<'static>],
    /// 下一个待消费事件的索引。
    pos: usize,
}

impl<'a> SliceSource<'a> {
    /// 覆盖整段事件序列。
    fn whole(events: &'a [Event<'static>]) -> Self {
        Self { events, pos: 0 }
    }
}

impl<'a> EventSource<'a> for SliceSource<'a> {
    fn next_event(&mut self) -> Option<&'a Event<'static>> {
        let event = self.events.get(self.pos)?;
        self.pos += 1;
        Some(event)
    }

    fn peek_event(&self) -> Option<&'a Event<'static>> {
        self.events.get(self.pos)
    }
}

/// 基于引用列表的事件源：列表项段落内的块事件（嵌套列表已就地渲染并剔除）。
struct RefSource<'a> {
    /// 段内事件（引用自解析缓存，不拷贝事件本体）。
    events: Vec<&'a Event<'static>>,
    /// 下一个待消费事件的索引。
    pos: usize,
}

impl<'a> RefSource<'a> {
    /// 用给定的引用列表构造（游标从头开始）。
    fn new(events: Vec<&'a Event<'static>>) -> Self {
        Self { events, pos: 0 }
    }

    /// 是否已耗尽。
    fn is_empty(&self) -> bool {
        self.pos >= self.events.len()
    }
}

impl<'a> EventSource<'a> for RefSource<'a> {
    fn next_event(&mut self) -> Option<&'a Event<'static>> {
        let event = *self.events.get(self.pos)?;
        self.pos += 1;
        Some(event)
    }

    fn peek_event(&self) -> Option<&'a Event<'static>> {
        self.events.get(self.pos).copied()
    }
}

/// 聊天区 markdown 渲染器：持有主题引用与可用内容宽度。
pub struct MdRenderer<'a> {
    theme: &'a Theme,
    width: usize,
}

/// 渲染 markdown 为样式化行（内容宽度内）。
pub fn render_markdown(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    render_markdown_styled(text, width, theme, Style::default())
}

/// 渲染 markdown，`base` 为段落/普通文本的默认样式（thinking 斜体灰、customMessageText 等）
pub fn render_markdown_styled(
    text: &str,
    width: usize,
    theme: &Theme,
    base: Style,
) -> Vec<Line<'static>> {
    MdRenderer { theme, width }.render_with(text, base)
}

/// markdown 渲染结果里的一个可点击链接区域。
///
/// 行号相对**本段渲染结果的起点**（调用方拼接多段时自己平移）；列从行首算起。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkSpan {
    /// 链接所在行（0 起，本段内）。
    pub line: usize,
    /// 起始列（含）。
    pub start_col: usize,
    /// 结束列（不含）。
    pub end_col: usize,
    /// 点击时要打开的目标；邮箱形态已补上 `mailto:`。
    pub url: String,
}

/// 从渲染结果里收集链接区域。
///
/// 链接文本用 `mdLink` 样式（渲染时附加下划线，见 `render_link_spans`），目标 URL 用
/// `mdLinkUrl` 样式（` (url)` 形式，仅当可见文本与 href 不同时才渲染）；两者都不叠加 `base`，
/// 所以按样式精确匹配即可定位，不需要渲染期往行里塞额外信息。
///
/// 折行会把链接文本与 ` (url)` 切成多行、多个片段：链接文本片段之间若存在 ` (url)` 片段，
/// 说明上一个链接已结束；连续行上的链接/URL 片段属于同一个链接。
pub fn collect_link_spans(lines: &[Line<'static>], theme: &Theme) -> Vec<LinkSpan> {
    /// 收集中的一个链接。
    struct Pending {
        /// 折行段（行, 起始列, 结束列）：链接文本与 ` (url)` 提示都计入命中区。
        segments: Vec<(usize, usize, usize)>,
        /// 链接可见文本（裸链接时它就是目标）。
        text: String,
        /// ` (url)` 提示的文本（可能折行成多段）；为空表示还没见到提示。
        url_part: String,
        /// true 表示正在收集 ` (url)` 提示、尚未遇到右括号。
        in_url: bool,
    }

    /// 收尾一个链接：同一行的多段合并成单个区间（折行会把 ` (url)` 切成多段）。
    fn flush(pending: Option<Pending>, out: &mut Vec<LinkSpan>) {
        let Some(pending) = pending else {
            return;
        };
        let url = link_target(&pending.url_part, &pending.text);
        let mut merged: Vec<(usize, usize, usize)> = Vec::new();
        for (line, start, end) in pending.segments {
            match merged.last_mut() {
                Some(last) if last.0 == line => {
                    last.1 = last.1.min(start);
                    last.2 = last.2.max(end);
                }
                _ => merged.push((line, start, end)),
            }
        }
        for (line, start_col, end_col) in merged {
            out.push(LinkSpan {
                line,
                start_col,
                end_col,
                url: url.clone(),
            });
        }
    }

    // 只比「前景色 + 下划线」而不是整份 `Style`：消息块会给整行 `patch` 背景色
    // （user 消息、custom message 的背景框都走 `bg_line`），整份相等比较会漏掉它们。
    let link_fg = theme.style(MD_LINK.0, MD_LINK.1).fg;
    let is_link =
        |style: Style| style.fg == link_fg && style.add_modifier.contains(Modifier::UNDERLINED);

    let mut out: Vec<LinkSpan> = Vec::new();
    let mut current: Option<Pending> = None;
    // 上一个链接片段/提示片段的结束位置（行, 列）：同一行靠后、或紧接下一行都算续段。
    let mut last_end: Option<(usize, usize)> = None;

    for (row, line) in lines.iter().enumerate() {
        let mut col = 0usize;
        for span in &line.spans {
            let start = col;
            col += span.width();
            let text = span.content.as_ref();
            let contiguous =
                last_end.is_some_and(|(lr, lc)| (row == lr && start >= lc) || row == lr + 1);

            if is_link(span.style) {
                // 已经拿到 ` (url)` 提示后再出现链接文本 → 这是另一个链接。
                let starts_new =
                    !contiguous || current.as_ref().is_some_and(|p| !p.url_part.is_empty());
                if starts_new {
                    flush(current.take(), &mut out);
                }
                match current.as_mut() {
                    Some(pending) => {
                        pending.segments.push((row, start, col));
                        pending.text.push_str(text);
                    }
                    None => {
                        current = Some(Pending {
                            segments: vec![(row, start, col)],
                            text: text.to_string(),
                            url_part: String::new(),
                            in_url: false,
                        });
                    }
                }
                last_end = Some((row, col));
                continue;
            }

            // 纯空白片段：折行会把 ` (url)` 切成「空格 + URL」两段，空格段不参与判定。
            if text.trim().is_empty() {
                continue;
            }

            // ` (url)` 提示的样式与代码块边框等**完全相同**（`fg(Reset).dim()`），所以不能按样式
            // 识别，只能看它紧跟在链接文本之后、且以 `(` 开头。
            let collecting = current.as_ref().is_some_and(|p| p.in_url) && contiguous;
            let starts_hint = current.as_ref().is_some_and(|p| {
                !p.in_url
                    && p.url_part.is_empty()
                    && contiguous
                    && text.trim_start().starts_with('(')
            });
            if collecting || starts_hint {
                if let Some(pending) = current.as_mut() {
                    pending.in_url = true;
                    pending.url_part.push_str(text);
                    pending.segments.push((row, start, col));
                    if text.contains(')') {
                        pending.in_url = false;
                    }
                }
                last_end = Some((row, col));
                continue;
            }

            // 其它内容：链接到此结束（裸链接也在这里收尾，目标用可见文本）。
            flush(current.take(), &mut out);
            last_end = None;
        }
    }
    flush(current.take(), &mut out);
    out
}

/// 链接的目标 URL：优先取 ` (url)` 片段（去掉外层括号与空白），没有时用可见文本
/// （裸链接）；邮箱形态（含 `@`、无 `:`、无空白）补 `mailto:`，与 `render_link_spans` 的
/// `href` 处理一致。
fn link_target(url_part: &str, text: &str) -> String {
    let trimmed = url_part
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    let url = if trimmed.is_empty() {
        text.trim()
    } else {
        trimmed
    };

    if !url.contains(':') && url.contains('@') && !url.contains(char::is_whitespace) {
        format!("mailto:{url}")
    } else {
        url.to_string()
    }
}

impl MdRenderer<'_> {
    /// 解析整段 markdown 并按 `width` 排版为最终行序列。
    ///
    /// `base` 是段落与普通文本的默认样式（thinking 斜体灰等）；表格/分隔线等
    /// 块渲染为固定行，段落与代码按宽度折行。
    fn render_with(&self, text: &str, base: Style) -> Vec<Line<'static>> {
        let width = self.width.max(1);
        let normalized = text.replace('\t', "   ");

        // marked 默认（gfm 表格/删除线/任务列表，无 smartypants/metadata/math）——
        // latex 启用时加 ENABLE_MATH：`$...$`/`$$...$$` 产出 InlineMath/DisplayMath 事件，
        // 由 latex.rs 渲染为 Unicode 公式；未闭合 `$` 仍作普通文本。
        // latex 关闭时不启用 math 解析，分界符按普通文本原样输出。
        let mut options =
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        let math = self.theme.latex;
        if math {
            options |= Options::ENABLE_MATH;
        }

        // 解析结果跨主题与宽度复用（布局仍然按当前主题/宽度重做）
        let events = parsed_events(&normalized, math, options);
        let mut source = SliceSource::whole(&events);
        let lines = self.render_blocks(&mut source, width, 0, base);
        let mut out: Vec<Line<'static>> = Vec::new();

        for line in lines {
            match line {
                OutLine::Wrap(s) => out.extend(wrap_line_spans(s, width)),
                OutLine::Code(s) => out.extend(wrap_code_spans(s, width)),
                OutLine::Fixed(s) => out.push(Line::from(s)),
            }
        }
        out
    }

    /// 按 `(类别, 变体)` 键从主题取样式，未配置时回退到主题默认值。
    fn style(&self, key: (&str, &str)) -> Style {
        self.theme.style(key.0, key.1)
    }

    /// 渲染块级事件序列。
    #[stacksafe::stacksafe]
    fn render_blocks<'a>(
        &self,
        src: &mut dyn EventSource<'a>,
        width: usize,
        depth: usize,
        base: Style,
    ) -> Vec<OutLine> {
        let mut out: Vec<OutLine> = Vec::new();
        let mut last: Option<BlockKind> = None;

        while let Some(ev) = src.next_event() {
            let (lines, kind) = match ev {
                Event::Start(Tag::Heading { level, .. }) => self.render_heading(src, *level),
                Event::Start(Tag::Paragraph) => {
                    let spans = self.collect_inline(src, base);
                    (vec![OutLine::Wrap(spans)], BlockKind::Paragraph)
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    match self.render_code_block(src, kind.clone()) {
                        CodeBlockOutcome::Done(lines) => (lines, BlockKind::Code),
                        CodeBlockOutcome::Mermaid(lines) => {
                            out.extend(lines);
                            last = Some(BlockKind::Code);
                            continue;
                        }
                    }
                }
                Event::Start(Tag::List(start)) => {
                    let (ordered, start_num) = match start {
                        Some(n) => (true, *n),
                        None => (false, 1),
                    };
                    (
                        self.render_list(src, width, depth, ordered, start_num, base),
                        BlockKind::List,
                    )
                }
                Event::Start(Tag::BlockQuote(_)) => self.render_blockquote(src, width, depth, base),
                Event::Start(Tag::Table(_)) => {
                    let (header, rows) = self.collect_table(src, base);
                    (self.render_table(&header, &rows, base), BlockKind::Table)
                }
                Event::Rule => {
                    let style = self.style(MD_HR);
                    (
                        vec![OutLine::Wrap(vec![Span::styled(
                            "─".repeat(width.min(80)),
                            style,
                        )])],
                        BlockKind::Rule,
                    )
                }
                Event::Text(t) => {
                    let text = t.to_string();
                    if text.trim().is_empty() {
                        continue;
                    }
                    (
                        vec![OutLine::Wrap(vec![Span::styled(text, base)])],
                        BlockKind::Paragraph,
                    )
                }
                Event::SoftBreak => (
                    vec![OutLine::Wrap(vec![Span::styled("\n", Style::default())])],
                    BlockKind::Paragraph,
                ),
                _ => continue,
            };

            // 块间空行：除非前后为列表
            if let Some(prev) = last
                && prev != BlockKind::List
                && kind != BlockKind::List
                && !out.is_empty()
            {
                out.push(OutLine::Fixed(vec![Span::raw("")]));
            }
            last = Some(kind);
            out.extend(lines);
        }
        out
    }

    /// 渲染 heading 块行（`# ` 前缀，h1 加粗+下划线，h>=2 加粗，h>=3 前缀同色）。
    fn render_heading<'a>(
        &self,
        src: &mut dyn EventSource<'a>,
        level: HeadingLevel,
    ) -> (Vec<OutLine>, BlockKind) {
        let mut style = self.style(MD_HEADING);
        let level: usize = match level {
            HeadingLevel::H1 => 1,
            HeadingLevel::H2 => 2,
            HeadingLevel::H3 => 3,
            HeadingLevel::H4 => 4,
            HeadingLevel::H5 => 5,
            HeadingLevel::H6 => 6,
        };
        if level == 1 {
            style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        } else {
            style = style.add_modifier(Modifier::BOLD);
        }
        let mut line: Spans = Vec::new();
        let prefix = format!("{} ", "#".repeat(level));
        if level >= 3 {
            line.push(Span::styled(prefix, style));
        } else {
            line.push(Span::raw(prefix));
        }
        line.extend(self.collect_inline(src, style));
        (vec![OutLine::Wrap(line)], BlockKind::Heading)
    }

    /// 渲染代码块：采集块体文本，mermaid 直接成行返回 `Mermaid`（跳过块间空行逻辑），
    /// 否则语法高亮（syntect）逐行成行后返回 `Done`。
    fn render_code_block<'a>(
        &self,
        src: &mut dyn EventSource<'a>,
        kind: CodeBlockKind,
    ) -> CodeBlockOutcome {
        let lang = match kind {
            CodeBlockKind::Fenced(l) => l.to_string(),
            _ => String::new(),
        };

        let mut body = String::new();
        while let Some(ev) = src.next_event() {
            match ev {
                Event::Text(t) | Event::Code(t) => body.push_str(t),
                Event::End(TagEnd::CodeBlock) => break,
                _ => {}
            }
        }

        let (body, partial_fence) = trim_partial_fence(&body, &lang);
        let border = self.style(MD_CODE_BLOCK_BORDER);
        let code = self.style(MD_CODE_BLOCK);
        let mut lines: Vec<OutLine> = vec![OutLine::Wrap(vec![Span::styled(
            format!("```{}", lang),
            border,
        )])];

        // Mermaid：渲染为 Unicode 文本流程图（宽度受限，超宽自动回退）。
        // render_mermaid 在严格宽度预算下超宽时返回 None，此处回退为源码高亮。
        // 注意用 Fixed：盒图依赖行首空格对齐，Wrap 折行会吞掉行首空格导致错位；
        // 输出已由库严格限制在宽度内，无需再次折行。
        // theme.mermaid=false 时跳过渲染，mermaid 代码块按普通代码块原样展示。
        if self.theme.mermaid
            && lang == "mermaid"
            && let Some(fig) = mermaid::render_mermaid(&body, Some(self.width))
        {
            let md_color = self.style(MD_CODE_BLOCK);
            for l in fig.lines() {
                lines.push(OutLine::Fixed(vec![Span::styled(l.to_string(), md_color)]));
            }
            lines.push(OutLine::Wrap(vec![Span::styled(
                format!("```{}", lang),
                self.style(MD_CODE_BLOCK_BORDER),
            )]));
            return CodeBlockOutcome::Mermaid(lines);
        }

        // 语法高亮（syntect）：未识别语言回退纯文本；主题色来自 syntax* 变量。
        // theme.syntax_highlight=false 时跳过 syntect（恢复启动的快速渲染路径），
        // 代码块以纯 codeBlock 色渲染，后台补全渲染会以完整高亮重渲染消息缓存。
        let syntheme = if self.theme.syntax_highlight && !lang.trim().is_empty() {
            Some(syntax_hl::build_theme(self.theme))
        } else {
            None
        };

        let mut hl = syntheme
            .as_ref()
            .and_then(|th| syntax_hl::session(&lang, th));

        for l in body.lines() {
            let spans: Spans = match hl.as_mut() {
                Some(s) => {
                    let mut v = vec![Span::styled("  ", code)];
                    v.extend(
                        s.line(l, code)
                            .into_iter()
                            .map(|(st, t)| Span::styled(t, st)),
                    );
                    v
                }
                None => {
                    vec![Span::styled("  ", code), Span::styled(l.to_string(), code)]
                }
            };
            lines.push(OutLine::Code(spans));
        }

        if !partial_fence {
            lines.push(OutLine::Wrap(vec![Span::styled("```", border)]));
        }

        CodeBlockOutcome::Done(lines)
    }

    /// 渲染 blockquote：递归渲染内层块并加 `│ ` 边框（quoteBorder）+ 斜体（quote）。
    fn render_blockquote<'a>(
        &self,
        src: &mut dyn EventSource<'a>,
        width: usize,
        depth: usize,
        base: Style,
    ) -> (Vec<OutLine>, BlockKind) {
        let inner = self.render_blocks(src, width.saturating_sub(2).max(1), depth, base);
        let quote = self.style(MD_QUOTE).add_modifier(Modifier::ITALIC);
        let border = self.style(MD_QUOTE_BORDER);
        let mut lines: Vec<OutLine> = Vec::new();

        for line in inner {
            let spans = match line {
                OutLine::Wrap(s) => wrap_line_spans(s, width.saturating_sub(2).max(1)),
                OutLine::Code(s) => wrap_code_spans(s, width.saturating_sub(2).max(1)),
                OutLine::Fixed(s) => vec![Line::from(s)],
            };

            for l in spans {
                if l.spans.iter().all(|s| s.content.trim().is_empty()) {
                    lines.push(OutLine::Fixed(vec![Span::styled("│ ", border)]));
                } else {
                    let mut line: Spans = vec![Span::styled("│ ", border)];
                    for sp in l.spans {
                        line.push(Span::styled(sp.content, quote));
                    }
                    lines.push(OutLine::Fixed(line));
                }
            }
        }
        (lines, BlockKind::Quote)
    }

    /// 收集行内事件直到 `end` 事件。style 为当前累积样式。
    fn collect_inline<'a>(&self, src: &mut dyn EventSource<'a>, style: Style) -> Spans {
        let mut out: Spans = Vec::new();
        let mut pending = String::new();
        let flush = |pending: &mut String, out: &mut Spans| {
            if !pending.is_empty() {
                out.push(Span::styled(std::mem::take(pending), style));
            }
        };
        while let Some(ev) = src.next_event() {
            match ev {
                Event::Text(t) => pending.push_str(t),
                // 对齐 pi-tui：段落内软换行保留为 \n（applyTextWithNewlines），
                // 不做 markdown 默认的「单换行→空格」折叠
                Event::SoftBreak => pending.push('\n'),
                Event::HardBreak => {
                    flush(&mut pending, &mut out);
                    out.push(Span::raw("\n"));
                }
                Event::Code(t) => {
                    flush(&mut pending, &mut out);
                    out.push(Span::styled(t.to_string(), self.style(MD_CODE)));
                }
                Event::Start(Tag::Emphasis) => {
                    flush(&mut pending, &mut out);
                    let inner = self.collect_inline(src, style.add_modifier(Modifier::ITALIC));
                    out.extend(inner);
                }
                Event::Start(Tag::Strong) => {
                    flush(&mut pending, &mut out);
                    let inner = self.collect_inline(src, style.add_modifier(Modifier::BOLD));
                    out.extend(inner);
                }
                Event::Start(Tag::Strikethrough) => {
                    flush(&mut pending, &mut out);
                    let inner = self.collect_inline(src, style.add_modifier(Modifier::CROSSED_OUT));
                    out.extend(inner);
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    flush(&mut pending, &mut out);
                    let inner = self.collect_inline(src, style);
                    let text: String = inner.iter().map(|s| s.content.as_ref()).collect();
                    let href = dest_url.to_string();
                    out.extend(self.render_link_spans(text, &href));
                }
                Event::Html(t) => pending.push_str(t),
                Event::InlineMath(t) => {
                    flush(&mut pending, &mut out);
                    out.push(self.render_inline_math(t));
                }
                Event::DisplayMath(t) => {
                    flush(&mut pending, &mut out);
                    out.push(self.render_display_math(t));
                }
                Event::End(_) => {
                    flush(&mut pending, &mut out);
                    return out;
                }
                _ => {}
            }
        }
        flush(&mut pending, &mut out);
        out
    }

    /// 渲染链接：文本≠href 时附 ` (url)`（mdLinkUrl）。
    ///
    /// 刻意**不**发 OSC8 超链接：ratatui 的 `Buffer::set_stringn` 会丢弃含控制字符的 grapheme
    /// （`!symbol.contains(char::is_control)`），ESC 被吃掉后 `]8;id=none;…` 会当成可见文本
    /// 写进单元格。登录面板的授权 URL 走另一条路（`render::hyperlink` 在 buffer 定型后改
    /// `Cell::symbol`），支持 OSC8 的终端里可直接点击；消息区的可点击性仍由
    /// 「可见文本 + 鼠标命中区域」承担。
    fn render_link_spans(&self, text: String, href: &str) -> Spans {
        let href_cmp = href.strip_prefix("mailto:").unwrap_or(href);

        if text != href && text != href_cmp {
            vec![
                Span::styled(text, self.style(MD_LINK).add_modifier(Modifier::UNDERLINED)),
                Span::styled(format!(" ({})", href), self.style(MD_LINK_URL)),
            ]
        } else {
            vec![Span::styled(
                text,
                self.style(MD_LINK).add_modifier(Modifier::UNDERLINED),
            )]
        }
    }

    /// 渲染行内公式 `$...$`（mdCodeBlock），不支持/语法错误回退原文（事件已不含分界符）。
    fn render_inline_math(&self, src: &str) -> Span<'static> {
        match latex::render_latex(src, false) {
            Some(s) => Span::styled(s, self.style(MD_MATH)),
            None => Span::styled(format!("${}$", src), self.style(MD_MATH)),
        }
    }

    /// 渲染 display math `$$...$$`（mdCodeBlock），不支持/语法错误回退原文。
    fn render_display_math(&self, src: &str) -> Span<'static> {
        let src = src.trim().to_string();
        match latex::render_latex(&src, true) {
            Some(s) => Span::styled(s, self.style(MD_MATH)),
            None => Span::styled(format!("$${}$$", src), self.style(MD_MATH)),
        }
    }

    /// 递归渲染列表（Start(List) 已被消费）。
    fn render_list<'a>(
        &self,
        src: &mut dyn EventSource<'a>,
        width: usize,
        depth: usize,
        ordered: bool,
        start: u64,
        base: Style,
    ) -> Vec<OutLine> {
        let mut out: Vec<OutLine> = Vec::new();
        let mut index = start;
        let indent = "    ".repeat(depth);

        while let Some(ev) = src.next_event() {
            match ev {
                Event::Start(Tag::Item) => {
                    let mut marker = if ordered {
                        format!("{}. ", index)
                    } else {
                        "- ".to_string()
                    };
                    index += 1;

                    // 可选 task marker（pulldown 的 TaskListMarker 是独立事件）
                    if matches!(src.peek_event(), Some(Event::TaskListMarker(_)))
                        && let Some(Event::TaskListMarker(checked)) = src.next_event()
                    {
                        marker.push_str(if *checked { "[x] " } else { "[ ] " });
                    }

                    // item 内块事件收成引用列表（跟踪嵌套列表深度：只有当前层级的
                    // End(Item) 才是本 item 的结束，嵌套列表内的 End 不中断收集）；
                    // 引用自解析缓存，不拷贝事件本体。
                    let mut item_events: Vec<&'a Event<'static>> = Vec::new();
                    let mut list_depth = 0usize;
                    while let Some(ev2) = src.next_event() {
                        match ev2 {
                            Event::Start(Tag::List(_)) => list_depth += 1,
                            Event::End(TagEnd::List(_)) => {
                                list_depth = list_depth.saturating_sub(1)
                            }
                            Event::End(TagEnd::Item) if list_depth == 0 => break,
                            _ => {}
                        }
                        item_events.push(ev2);
                    }

                    // 行内 + 块级混合渲染：tight 单项的 inline
                    // 事件无 Paragraph 包裹，render_blocks 会丢弃 Code/Emphasis 等
                    let mut item_source = RefSource::new(item_events);
                    let (inner, nested) =
                        self.render_item_inlines(&mut item_source, width, depth, base);
                    let marker_w = display_width(&marker);
                    let item_width = width.saturating_sub(marker_w).max(1);
                    let bullet = self.style(MD_LIST_BULLET);
                    out.extend(
                        self.render_item_lines(
                            inner, &indent, &marker, marker_w, item_width, bullet,
                        ),
                    );

                    for l in nested {
                        out.push(l);
                    }
                }
                Event::End(TagEnd::List(_)) => break,
                _ => {}
            }
        }
        out
    }

    /// 渲染列表项的行内容：Code/Wrap 按 item 宽度折行后逐行加缩进+marker（首行）或
    /// 等宽占位（续行），Fixed 直接拼接；空行保留，全空 item 补一个 marker-only 行。
    fn render_item_lines(
        &self,
        inner: Vec<OutLine>,
        indent: &str,
        marker: &str,
        marker_w: usize,
        item_width: usize,
        bullet: Style,
    ) -> Vec<OutLine> {
        let mut out: Vec<OutLine> = Vec::new();
        let mut first = true;

        for line in inner {
            match line {
                OutLine::Code(s) => {
                    let wrapped = wrap_code_spans(s, item_width);
                    for l in wrapped {
                        append_wrapped_item_line(
                            &mut out, &mut first, indent, marker, marker_w, bullet, l,
                        );
                    }
                }
                OutLine::Wrap(s) => {
                    let wrapped = wrap_line_spans(s, item_width);
                    for l in wrapped {
                        append_wrapped_item_line(
                            &mut out, &mut first, indent, marker, marker_w, bullet, l,
                        );
                    }
                }
                OutLine::Fixed(s) => {
                    append_fixed_item_line(
                        &mut out, &mut first, indent, marker, marker_w, bullet, s,
                    );
                }
            }
        }

        if first {
            out.push(OutLine::Fixed(vec![Span::styled(
                format!("{}{}", indent, marker),
                bullet,
            )]));
        }

        out
    }

    /// 渲染列表项内容（item 内 token 按行内渲染）。
    /// pulldown 对 tight 单项列表不包 Paragraph，inline 事件直接出现在块级，
    /// render_blocks 会把 Code/Emphasis 等静默丢弃；这里把连续 inline 事件合并
    /// 为一个段落，块级事件（代码块/表格/引用）递归块渲染，嵌套列表递归 render_list。
    fn render_item_inlines<'a>(
        &self,
        src: &mut dyn EventSource<'a>,
        width: usize,
        depth: usize,
        base: Style,
    ) -> (Vec<OutLine>, Vec<OutLine>) {
        let mut out: Vec<OutLine> = Vec::new();
        let mut nested: Vec<OutLine> = Vec::new();

        loop {
            if matches!(src.peek_event(), Some(Event::Start(Tag::Paragraph))) {
                src.next_event();
            }
            let mut seg: Vec<&'a Event<'static>> = Vec::new();
            let mut saw_block = false;
            while let Some(ev) = src.next_event() {
                match ev {
                    Event::End(TagEnd::Paragraph)
                    | Event::End(TagEnd::Item)
                    | Event::End(TagEnd::List(_)) => break,
                    Event::Start(Tag::List(st)) => {
                        let (ord2, s2) = match st {
                            Some(v) => (true, *v),
                            None => (false, 1),
                        };
                        saw_block = true;
                        nested.extend(self.render_list(src, width, depth + 1, ord2, s2, base));
                        continue;
                    }
                    Event::Start(Tag::CodeBlock(_))
                    | Event::Start(Tag::BlockQuote(_))
                    | Event::Start(Tag::Table(_)) => {
                        saw_block = true;
                        seg.push(ev);
                    }
                    _ => seg.push(ev),
                }
            }

            let mut seg_source = RefSource::new(seg);
            if seg_source.is_empty() {
                break;
            }

            let block_lines = if saw_block {
                self.render_blocks(&mut seg_source, width, depth, base)
            } else {
                let spans = self.collect_inline(&mut seg_source, base);
                if spans.is_empty() {
                    continue;
                }
                vec![OutLine::Wrap(spans)]
            };

            if !out.is_empty() {
                out.push(OutLine::Fixed(vec![Span::raw("")]));
            }
            out.extend(block_lines);
        }
        (out, nested)
    }

    /// 从事件流消费整张表格，返回 (表头单元格, 数据行单元格)。
    /// 每个单元格保留 code/strong/em/link 等行内样式。
    fn collect_table<'a>(
        &self,
        src: &mut dyn EventSource<'a>,
        base: Style,
    ) -> (Vec<Vec<Span<'static>>>, Vec<Vec<Vec<Span<'static>>>>) {
        let mut header: Vec<Vec<Span<'static>>> = Vec::new();
        let mut rows: Vec<Vec<Vec<Span<'static>>>> = Vec::new();
        let mut cur_row: Option<Vec<Vec<Span<'static>>>> = None;

        while let Some(ev) = src.next_event() {
            match ev {
                Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {
                    cur_row = Some(Vec::new())
                }

                // 单元格走 collect_inline：保留 code span / strong / em / link 等行内样式，
                // 并消费 End(TableCell)（collect_inline 遇 End 即返回）
                Event::Start(Tag::TableCell) => {
                    let cell = self.collect_inline(src, base);
                    if let Some(row) = cur_row.as_mut() {
                        row.push(cell);
                    }
                }
                Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                    if let Some(row) = cur_row.take() {
                        if header.is_empty() {
                            header = row;
                        } else {
                            rows.push(row);
                        }
                    }
                }
                Event::End(TagEnd::Table) => break,
                _ => {}
            }
        }
        (header, rows)
    }

    /// 表格渲染
    fn render_table(
        &self,
        header: &[Vec<Span<'static>>],
        rows: &[Vec<Vec<Span<'static>>>],
        base: Style,
    ) -> Vec<OutLine> {
        let width = self.width;
        let num_cols = header.len();
        if num_cols == 0 {
            return Vec::new();
        }

        let avail = markdown_table::column_avail(width, num_cols);
        let cell_text = |cell: &[Span<'static>]| -> String {
            cell.iter().map(|s| s.content.as_ref()).collect()
        };

        if avail < num_cols {
            let text: String = rows
                .iter()
                .flat_map(|r| r.iter())
                .map(|c| cell_text(c))
                .collect::<Vec<_>>()
                .join(" | ");
            return vec![OutLine::Wrap(vec![Span::raw(text)])];
        }

        let cols = markdown_table::compute_column_widths(header, rows, |c| cell_text(c), avail);

        // 边框与 cell 竖线统一继承文本样式（对齐 pi renderTable：表格行不单独上色，
        // 边框 ┌─┬─┐ / ├─┼─┤ / └─┴─┘ 与分隔竖线 │ 同色，避免两色割裂）
        let border = base;
        let mut out: Vec<OutLine> = Vec::new();
        let horiz = |ch: &str| -> String {
            let seg: Vec<String> = cols.iter().map(|w| "─".repeat(*w)).collect();
            let (l, r) = match ch {
                "─┬─" => ("┌", "┐"),
                "─┴─" => ("└", "┘"),
                _ => ("├", "┤"),
            };
            format!("{}{}{}{}{}", l, "─", seg.join(ch), "─", r)
        };

        out.push(OutLine::Fixed(vec![Span::styled(horiz("─┬─"), border)]));
        out.extend(render_table_row(header, &cols, base, true));
        out.push(OutLine::Fixed(vec![Span::styled(horiz("─┼─"), border)]));

        for (ri, row) in rows.iter().enumerate() {
            out.extend(render_table_row(row, &cols, base, false));
            if ri < rows.len() - 1 {
                out.push(OutLine::Fixed(vec![Span::styled(horiz("─┼─"), border)]));
            }
        }

        out.push(OutLine::Fixed(vec![Span::styled(horiz("─┴─"), border)]));
        out
    }
}

/// 处理一条已折行的列表项行：空白且非首行时输出空行；否则前置缩进+marker（首行）
/// 或等宽占位（续行），并过滤空 span 后输出。
fn append_wrapped_item_line(
    out: &mut Vec<OutLine>,
    first: &mut bool,
    indent: &str,
    marker: &str,
    marker_w: usize,
    bullet: Style,
    l: Line<'static>,
) {
    let blank = l.spans.iter().all(|sp| sp.content.trim().is_empty());
    if blank && !*first {
        out.push(OutLine::Fixed(vec![Span::raw("")]));
        return;
    }
    let mut line: Spans = Vec::new();
    if *first {
        line.push(Span::styled(format!("{}{}", indent, marker), bullet));
        *first = false;
    } else {
        line.push(Span::styled(
            format!("{}{}", indent, " ".repeat(marker_w)),
            Style::default(),
        ));
    }
    for sp in l.spans {
        if sp.content.is_empty() {
            continue;
        }
        line.push(sp);
    }
    out.push(OutLine::Fixed(line));
}

/// 处理一行列表项 Fixed 行：空白时输出空行；否则前置缩进+marker（首行）
/// 或等宽占位（续行）后直接拼接。
fn append_fixed_item_line(
    out: &mut Vec<OutLine>,
    first: &mut bool,
    indent: &str,
    marker: &str,
    marker_w: usize,
    bullet: Style,
    s: Spans,
) {
    let blank = s.iter().all(|sp| sp.content.trim().is_empty());
    if blank {
        out.push(OutLine::Fixed(vec![Span::raw("")]));
        return;
    }
    let mut line: Spans = Vec::new();
    if *first {
        line.push(Span::styled(format!("{}{}", indent, marker), bullet));
        *first = false;
    } else {
        line.push(Span::styled(
            format!("{}{}", indent, " ".repeat(marker_w)),
            Style::default(),
        ));
    }
    line.extend(s);
    out.push(OutLine::Fixed(line));
}

/// 渲染表格一行（含单元格折行与竖线分隔），bold_header 时表头加粗。
fn render_table_row(
    cells: &[Vec<Span<'static>>],
    cols: &[usize],
    base: Style,
    bold_header: bool,
) -> Vec<OutLine> {
    let mut cell_lines: Vec<Vec<Vec<Span<'static>>>> = Vec::new();
    for (i, cell) in cells.iter().enumerate() {
        let w = cols.get(i).copied().unwrap_or(1);
        cell_lines.push(wrap_spans_fixed(cell, w));
    }
    let n = cell_lines.iter().map(|v| v.len()).max().unwrap_or(1);
    let border_st = if bold_header {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    };
    let mut out: Vec<OutLine> = Vec::new();

    for li in 0..n {
        let mut line: Spans = vec![Span::styled("│ ", border_st)];
        for (i, cl) in cell_lines.iter().enumerate() {
            let w = cols.get(i).copied().unwrap_or(1);
            let text_w: usize = cl
                .get(li)
                .map(|l| l.iter().map(|s| grapheme_width(&s.content)).sum())
                .unwrap_or(0);
            let pad = w.saturating_sub(text_w);
            let mut cell_spans = cl.get(li).cloned().unwrap_or_default();
            if bold_header {
                for sp in &mut cell_spans {
                    sp.style = sp.style.add_modifier(Modifier::BOLD);
                }
            }
            line.extend(cell_spans);
            if pad > 0 {
                line.push(Span::styled(" ".repeat(pad), border_st));
            }
            line.push(Span::styled(
                if i < cell_lines.len() - 1 {
                    " │ "
                } else {
                    " │"
                },
                border_st,
            ));
        }
        out.push(OutLine::Fixed(line));
    }
    out
}

/// 修剪流式未闭合围栏：内容最后一行若为部分围栏（如 "``"），剔除它。
/// 返回 (修剪后的内容, 是否处于未闭合状态)。
fn trim_partial_fence(body: &str, lang: &str) -> (String, bool) {
    let trimmed = body.trim_end_matches('\n');
    let Some(last_line) = trimmed.lines().last() else {
        return (body.to_string(), false);
    };

    // 闭合判断：源以闭合围栏行结尾属于已闭合（不会出现部分围栏）
    let marker_char = if lang.starts_with('~') { '~' } else { '`' };
    let is_partial = !last_line.is_empty()
        && last_line.chars().count() < 3
        && last_line.chars().all(|c| c == marker_char);

    if !is_partial {
        let is_closed = last_line.trim_end() == "```" || last_line.trim_end() == "~~~";
        let _ = is_closed;
        return (body.to_string(), false);
    }

    let mut lines: Vec<&str> = trimmed.lines().collect();
    lines.pop();
    (lines.join("\n"), true)
}

/// 按固定宽度折行，保留每个 span 的样式（表格单元格用）。
/// 按字素簇切分、以 `grapheme_width` 计量：emoji ZWJ/键帽序列不会被拆到两行，
/// 且折行口径与 `compute_column_widths`/`render_table_row` 的列宽口径一致。
fn wrap_spans_fixed(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    if spans.is_empty() {
        return vec![Vec::new()];
    }

    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    for sp in spans {
        let mut pending = String::new();
        for g in UnicodeSegmentation::graphemes(sp.content.as_ref(), true) {
            let gw = grapheme_width(g);
            if gw == 0 {
                continue; // 控制字符/零宽字素不占列（与 ratatui 落格一致）
            }

            if cur_w + gw > width && cur_w > 0 {
                if !pending.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut pending), sp.style));
                }
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }

            pending.push_str(g);
            cur_w += gw;
        }

        if !pending.is_empty() {
            cur.push(Span::styled(pending, sp.style));
        }
    }

    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// 词级折行：按空白分词（CJK 每字符独立成词），超长词按字符断行。
/// 断行产生的行首空格被跳过；行内既有空格保留。
pub fn wrap_line_spans(spans: Spans, width: usize) -> Vec<Line<'static>> {
    wrap_spans(spans, width, false)
}

/// 代码行折行：保留行首空格/缩进（`wrap_line_spans` 丢弃行首空格，不适用代码）。
pub fn wrap_code_spans(spans: Spans, width: usize) -> Vec<Line<'static>> {
    wrap_spans(spans, width, true)
}

/// 词级折行的中间单位：带样式的文本片段及其是否为空白。
struct Tok {
    /// 该片段的渲染样式。
    style: Style,
    /// 片段文本，空白或换行也会单独成词。
    text: String,
    /// true 表示该 token 是空白，折行落到行首时应丢弃。
    space: bool,
}

/// 把 spans 拆成词级 token：空白/换行单独成 token，CJK 每字符独立成词。
fn tokenize_spans(spans: Spans) -> Vec<Tok> {
    let mut toks: Vec<Tok> = Vec::new();
    for sp in spans {
        let mut pending = String::new();
        for c in sp.content.chars() {
            if c == ' ' || c == '\t' {
                if !pending.is_empty() {
                    toks.push(Tok {
                        style: sp.style,
                        text: std::mem::take(&mut pending),
                        space: false,
                    });
                }
                toks.push(Tok {
                    style: sp.style,
                    text: if c == '\t' {
                        "   ".to_string()
                    } else {
                        " ".to_string()
                    },
                    space: true,
                });
            } else if c == '\n' {
                // 显式换行：结束当前 token，作为硬换行标记拆行
                if !pending.is_empty() {
                    toks.push(Tok {
                        style: sp.style,
                        text: std::mem::take(&mut pending),
                        space: false,
                    });
                }
                toks.push(Tok {
                    style: sp.style,
                    text: "\n".to_string(),
                    space: false,
                });
            } else if c.is_control() {
                continue;
            } else {
                pending.push(c);
                if is_cjk(c) {
                    toks.push(Tok {
                        style: sp.style,
                        text: std::mem::take(&mut pending),
                        space: false,
                    });
                }
            }
        }

        if !pending.is_empty() {
            toks.push(Tok {
                style: sp.style,
                text: pending,
                space: false,
            });
        }
    }
    toks
}

/// 把带样式的文本按显示宽度折行为多行。
///
/// `keep_indent` 为 true 时保留行首空格（代码块缩进），否则丢弃断行后的行首空格；
/// 超过整行宽度的超长词按字符强制断开。
fn wrap_spans(spans: Spans, width: usize, keep_indent: bool) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::from(spans)];
    }
    if spans.iter().all(|s| s.content.is_empty()) {
        return vec![Line::from("")];
    }

    let toks = tokenize_spans(spans);

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<(Style, String)> = Vec::new();
    let mut cur_w = 0usize;
    let push_line = |cur: &mut Vec<(Style, String)>, out: &mut Vec<Line<'static>>| {
        let spans: Spans = cur
            .iter()
            .map(|(s, t)| Span::styled(t.clone(), *s))
            .collect();
        out.push(Line::from(spans));
        cur.clear();
    };

    for tok in toks {
        if tok.text == "\n" {
            // 换行：结束当前行（空行也保留）
            push_line(&mut cur, &mut out);
            cur_w = 0;
            continue;
        }

        let w = display_width(&tok.text);
        if cur_w > 0 && cur_w + w > width {
            push_line(&mut cur, &mut out);
            cur_w = 0;
            if tok.space {
                continue; // 行首不保留空格
            }
        }

        if w > width {
            // 超长词按字符断
            let mut acc = String::new();
            for c in tok.text.chars() {
                let cw = char_width(c);
                if display_width(&acc) + cw > width && !acc.is_empty() {
                    cur.push((tok.style, std::mem::take(&mut acc)));
                    push_line(&mut cur, &mut out);
                }
                acc.push(c);
            }
            if !acc.is_empty() {
                cur.push((tok.style, acc));
                cur_w = display_width(cur.last().unwrap().1.as_str());
            }
            continue;
        }

        if !keep_indent && tok.space && cur_w == 0 {
            continue; // 行首空格（出现在断行后）；代码行需保留缩进，跳过
        }
        cur.push((tok.style, tok.text));
        cur_w += w;
    }

    if !cur.is_empty() || out.is_empty() {
        push_line(&mut cur, &mut out);
    }
    out
}

/// 字符是否属于 CJK 全角区间（用于按 2 列宽计算显示宽度）。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x2EFF | 0x3040..=0x30FF | 0x31F0..=0x31FF |
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA000..=0xA4CF |
        0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x3FFFD)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn theme() -> Theme {
        Theme::default()
    }

    /// 链接发不出 OSC8：ratatui 的 buffer 会吃掉 ESC，只剩 `]8;id=none;…` 字面量。
    /// 因此回归口径是「文本与 URL 都可见 + 输出里没有任何控制字符」。
    #[test]
    fn markdown_links_render_as_visible_text_without_escape_sequences() {
        let lines = render_markdown("[example](https://example.com/x)", 60, &theme());
        let all = text_of(&lines).join("\n");
        assert!(all.contains("example"), "链接文本应可见: {all:?}");
        assert!(
            all.contains("https://example.com/x"),
            "URL 应以文本形式附带: {all:?}"
        );
        assert!(!all.contains('\x1b'), "不得输出 ESC: {all:?}");
        assert!(!all.contains("]8;"), "不得把 OSC8 序列当文本输出: {all:?}");
        assert!(!all.contains(char::is_control), "不得输出控制字符: {all:?}");
    }

    /// 链接区域定位：[可见文本](url) 的文本与 ` (url)` 都算命中区，目标取 href。
    #[test]
    fn link_spans_cover_visible_text_and_target() {
        let th = theme();
        let lines = render_markdown("[example](https://example.com/x)", 60, &th);
        let links = collect_link_spans(&lines, &th);

        assert_eq!(links.len(), 1, "{links:?}");
        let link = &links[0];
        assert_eq!(link.line, 0);
        assert_eq!(link.start_col, 0);
        assert_eq!(link.url, "https://example.com/x");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            &text[link.start_col..link.end_col],
            "example (https://example.com/x)",
            "命中区应覆盖链接文本与 URL 提示"
        );
    }

    /// 折行后同一链接在每行各有一段，目标 URL 相同。
    #[test]
    fn wrapped_link_yields_one_span_per_line_with_same_url() {
        let th = theme();
        let url = "https://example.com/very/long/path";
        let lines = render_markdown(&format!("[example]({url})"), 20, &th);
        let links = collect_link_spans(&lines, &th);

        assert!(links.len() >= 2, "长链接应折行成多段: {links:?}");
        assert!(links.iter().all(|l| l.url == url), "{links:?}");
        assert!(links.iter().all(|l| l.line < lines.len()), "{links:?}");
        assert!(links.windows(2).all(|w| w[0].line < w[1].line), "{links:?}");
    }

    /// 裸链接（可见文本就是 URL）没有 ` (url)` 片段，目标就是文本本身。
    #[test]
    fn bare_link_uses_its_text_as_target() {
        let th = theme();
        let lines = render_markdown("<https://example.com>", 60, &th);
        let links = collect_link_spans(&lines, &th);

        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].url, "https://example.com");
    }

    /// 裸邮箱补 `mailto:`（与 `render_link_spans` 的 href 处理一致）。
    #[test]
    fn bare_email_link_target_gets_mailto_prefix() {
        let th = theme();
        let lines = render_markdown("<a@b.com>", 60, &th);
        let links = collect_link_spans(&lines, &th);

        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].url, "mailto:a@b.com");
    }

    /// 紧邻的两个链接不得被合并成一个（第一个的 URL 片段已出现 → 第二个是新链接）。
    #[test]
    fn adjacent_links_stay_separate() {
        let th = theme();
        let lines = render_markdown("[a](https://a.example)[b](https://b.example)", 60, &th);
        let links = collect_link_spans(&lines, &th);

        assert_eq!(links.len(), 2, "{links:?}");
        assert_eq!(links[0].url, "https://a.example");
        assert_eq!(links[1].url, "https://b.example");
    }

    /// 没有链接时不得产出任何区域（纯文本、代码块里的 URL 都不算）。
    #[test]
    fn no_links_means_no_spans() {
        let th = theme();
        for md in ["plain text", "`https://example.com`", "```\nhttps://x\n```"] {
            let lines = render_markdown(md, 60, &th);
            assert!(
                collect_link_spans(&lines, &th).is_empty(),
                "{md:?} 不应产出链接区域"
            );
        }
    }

    /// 解析结果按 `(文本, math 开关)` 缓存：同一文本重复渲染复用同一份事件序列
    /// （主题与宽度变化不重新解析），math 开关不同则各存一份。
    #[test]
    fn parse_cache_reuses_events_across_theme_and_width() {
        let text = "hello **world**\n\n- a\n- b\n";
        let narrow = render_markdown(text, 20, &theme());
        let cached = parsed_events(text, false, Options::ENABLE_TABLES);
        let again = parsed_events(text, false, Options::ENABLE_TABLES);
        assert!(Rc::ptr_eq(&cached, &again), "同一文本应命中解析缓存");

        // 宽度变化后仍命中同一份解析结果，布局重做（输出不为空且更宽）
        let wide = render_markdown(text, 40, &theme());
        assert!(!narrow.is_empty() && !wide.is_empty());

        // math 开关影响解析（事件序列不同）→ 不共用条目
        let math = parsed_events(text, true, Options::ENABLE_TABLES | Options::ENABLE_MATH);
        assert!(!Rc::ptr_eq(&cached, &math), "math 开关不同不应共用条目");
    }

    /// 超出条目上限时从最旧条目开始淘汰。
    #[test]
    fn parse_cache_evicts_oldest_entry() {
        let first_text = "# evict-0";
        let first = parsed_events(first_text, false, Options::ENABLE_TABLES);
        for i in 1..=PARSE_CACHE_ENTRIES {
            let text = format!("# evict-{i}");
            let _ = parsed_events(&text, false, Options::ENABLE_TABLES);
        }
        let again = parsed_events(first_text, false, Options::ENABLE_TABLES);
        assert!(!Rc::ptr_eq(&first, &again), "最旧条目应被淘汰");
    }

    #[test]
    fn inline_math_renders_unicode_formula() {
        let lines = render_markdown("勾股定理 — $a^2 + b^2 = c^2$", 60, &theme());
        let t = text_of(&lines);
        let joined = t.join("\n");
        assert!(joined.contains("a² + b² = c²"), "{:?}", t);
        assert!(!joined.contains('$'), "分界符应被消费: {:?}", t);
    }

    #[test]
    fn math_list_all_ten_formulas() {
        let input = "1. 勾股定理 — $a^2 + b^2 = c^2$\n\
2. 欧拉公式 — $e^{i\\pi} + 1 = 0$\n\
3. 一元二次方程求根 — $x = \\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}$\n\
4. 圆的面积 — $A = \\pi r^2$\n\
5. 泰勒级数展开 — $e^x = \\sum_{n=0}^{\\infty} \\frac{x^n}{n!}$\n\
6. 费马小定理 — $a^{p-1} \\equiv 1 \\pmod{p}$\n\
7. 牛顿第二定律 — $F = ma$\n\
8. 概率密度函数（正态分布） — $f(x) = \\frac{1}{\\sigma\\sqrt{2\\pi}} e^{-\\frac{(x-\\mu)^2}{2\\sigma^2}}$\n\
9. 斯特林公式 — $n! \\approx \\sqrt{2\\pi n} \\left(\\frac{n}{e}\\right)^n$\n\
10. 无穷等比级数求和 — $\\sum_{n=0}^{\\infty} ar^n = \\frac{a}{1-r}, \\quad |r| < 1$";
        let lines = render_markdown(input, 100, &theme());
        let t = text_of(&lines);
        let joined = t.join("\n");
        for expected in [
            "a² + b² = c²",
            "e^(iπ) + 1 = 0",
            "√(b² - 4ac)",
            "A = π r²",
            "∑ₙ₌₀^∞",
            "aᵖ⁻¹ ≡ 1 (mod p)",
            "F = ma",
            "1/(σ√2π)",
            "√(2π n)",
            "arⁿ = a/(1-r)",
        ] {
            assert!(joined.contains(expected), "缺少 {expected:?}: {:?}", t);
        }
        assert!(!joined.contains("$a^"), "原文不应残留: {:?}", t);
    }

    #[test]
    fn inline_math_spans_styled() {
        let lines = render_markdown("$x^2$", 60, &theme());
        assert!(
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .any(|s| s.content.contains('²') && s.style.fg.is_some()),
            "公式 span 应有主题色"
        );
    }

    #[test]
    fn display_math_stacks_fraction() {
        let input = "$$\n\\sum_{n=0}^{\\infty} ar^n = \\frac{a}{1-r}\n$$";
        let lines = render_markdown(input, 60, &theme());
        let t = text_of(&lines);
        let joined = t.join("\n");
        assert!(joined.contains('∑'), "求和符号: {:?}", t);
        assert!(joined.contains('─'), "display 分式线应堆叠: {:?}", t);
    }

    #[test]
    fn math_in_list_and_table() {
        let lines = render_markdown("- $a^n$", 60, &theme());
        let t = text_of(&lines);
        assert!(t[0].contains("aⁿ"), "列表项内公式: {:?}", t);

        let lines = render_markdown("| x |\n|---|\n| $x^2$ |", 40, &theme());
        let t = text_of(&lines);
        assert!(
            t.iter().any(|l| l.contains("x²")),
            "表格单元格内公式: {:?}",
            t
        );
    }

    #[test]
    fn unclosed_math_shows_literal() {
        // 流式未闭合 `$x^2`：pulldown 不配对，按普通文本显示
        let lines = render_markdown("$x^2", 60, &theme());
        let t = text_of(&lines);
        assert!(t[0].contains("$x^2"), "未闭合公式应原样显示: {:?}", t);
    }

    #[test]
    fn heading_and_paragraph_spacing() {
        let lines = render_markdown("# Title\n\nSome text", 60, &theme());
        let t = text_of(&lines);
        assert!(t[0].starts_with("# "));
        assert!(t[0].contains("Title"));
        // 标题后空行
        assert!(t.iter().any(|l| l.trim().is_empty()));
        assert!(t.iter().any(|l| l.contains("Some text")));
    }

    #[test]
    fn paragraph_keeps_single_newlines() {
        // 对齐 pi-tui（text token 保留 \n）：段落内单换行不折叠为空格
        let lines = render_markdown("line1\nline2\nline3", 60, &theme());
        let t = text_of(&lines);
        assert_eq!(t.len(), 3, "多行段落应渲染为 3 行: {:?}", t);
        assert!(
            t[0].contains("line1") && t[1].contains("line2") && t[2].contains("line3"),
            "{:?}",
            t
        );
    }

    #[test]
    fn long_paragraph_wraps_across_lines() {
        let long = (0..4)
            .map(|i| format!("word{}", i))
            .collect::<Vec<_>>()
            .join(" ");
        let lines = render_markdown(&format!("prefix\n{}", long), 20, &theme());
        let t = text_of(&lines);
        assert!(t[0].contains("prefix"), "首行保留: {:?}", t);
        assert!(
            t[1..].iter().any(|l| l.contains("word0")),
            "后续按宽度折行: {:?}",
            t
        );
    }

    #[test]
    fn code_block_fences() {
        let lines = render_markdown("```rust\nfn main() {}\n```", 60, &theme());
        let t = text_of(&lines);
        assert!(t.iter().any(|l| l == "```rust"));
        assert!(t.iter().any(|l| l == "```"));
        assert!(t.iter().any(|l| l.contains("fn main()")));
    }

    #[test]
    fn streaming_partial_fence_trimmed() {
        // 未闭合围栏 + 最后一行是部分围栏 "``"
        let lines = render_markdown("```rust\nlet x = 1;\n``", 60, &theme());
        let t = text_of(&lines);
        assert!(
            !t.iter().any(|l| l.trim() == "``"),
            "partial fence must be trimmed: {:?}",
            t
        );
        assert!(t.iter().any(|l| l.contains("let x = 1;")));
    }

    #[test]
    fn list_markers() {
        let lines = render_markdown("- a\n- b", 60, &theme());
        let t = text_of(&lines);
        assert!(t[0].trim_start().starts_with("- "));
        assert!(t[0].contains('a'));
        assert!(t[1].contains('b'));
    }

    #[test]
    fn list_item_inline_code_span_is_kept() {
        // 回归：tight 单项列表的 inline 事件无 Paragraph 包裹，曾把 Code span 静默
        // 丢弃并把文本段拆成多行（出现 "- ," 破碎行）
        let input = "- `run.sh` — startup script, `todo.md` — task list";
        let lines = render_markdown(input, 60, &theme());
        let t = text_of(&lines);
        assert_eq!(t.len(), 1, "单行渲染，不得拆段: {:?}", t);
        let row = &t[0];
        assert!(row.contains("run.sh"), "code span 必须保留: {:?}", row);
        assert!(row.contains("todo.md"), "code span 必须保留: {:?}", row);
        assert!(row.contains("startup script"), "文本内容: {:?}", row);
        assert!(!row.contains("- ,"), "不得出现破碎行: {:?}", row);
        // ASCII 破折号不做 smart punctuation 替换（对齐 pi marked 默认）
        let lines = render_markdown("- a - b", 40, &theme());
        let t = text_of(&lines);
        assert!(t[0].contains("- a - b"), "ASCII '-' 原样保留: {:?}", t);
    }

    #[test]
    fn list_item_with_codeblock_keeps_block() {
        // 列表项内代码块仍走块级渲染（含部分围栏修剪）
        let input = "- item\n\n  ```\n  code\n  ``";
        let lines = render_markdown(input, 60, &theme());
        let t = text_of(&lines);
        let joined = t.join("\n");
        assert!(joined.contains("item"), "{:?}", t);
        assert!(joined.contains("code"), "代码块内容保留: {:?}", t);
        assert!(!t.iter().any(|l| l.trim() == "``"), "部分围栏修剪: {:?}", t);
    }

    #[test]
    fn nested_list_still_indents() {
        let lines = render_markdown("- a\n  - b\n  - c\n- d", 60, &theme());
        let t = text_of(&lines);
        assert!(t.iter().any(|l| l.trim_start().starts_with("- a")));
        assert!(
            t.iter().any(|l| l.starts_with("    - b")),
            "嵌套缩进: {:?}",
            t
        );
        assert!(t.iter().any(|l| l.trim_start().starts_with("- d")));
    }

    #[test]
    fn loose_list_item_paragraphs_keep_blank_line() {
        let lines = render_markdown("- a\n\n  b\n\n- c", 60, &theme());
        let t = text_of(&lines);
        assert!(t.iter().any(|l| l.contains('a')));
        assert!(t.iter().any(|l| l.contains('b')));
        assert!(
            t.iter().any(|l| l.trim().is_empty()),
            "宽松列表段间空行: {:?}",
            t
        );
    }

    #[test]
    fn ordered_list_numbers() {
        let lines = render_markdown("1. one\n2. two", 60, &theme());
        let t = text_of(&lines);
        assert!(t[0].trim_start().starts_with("1. "));
        assert!(t[1].trim_start().starts_with("2. "));
    }

    #[test]
    fn blockquote_gutter() {
        let lines = render_markdown("> quoted", 60, &theme());
        let t = text_of(&lines);
        assert!(t[0].starts_with("│ "));
        assert!(t[0].contains("quoted"));
    }

    #[test]
    fn probe() {
        use pulldown_cmark::{Event, Options, Parser};
        let text = "| a | b |\n|---|--|\n| 1 | 2 |";
        let parser = Parser::new_ext(text, Options::all());
        for ev in parser {
            match ev {
                Event::Start(t) => eprintln!("START {:?}", t),
                Event::End(t) => eprintln!("END {:?}", t),
                Event::Text(t) => eprintln!("TEXT {:?}", t.to_string()),
                other => eprintln!("OTHER {:?}", other),
            }
        }
    }

    #[test]
    fn table_cell_code_span_is_kept() {
        // 回归：单元格内 code span 被解析为 Event::Code，曾整段丢弃只剩 ", ,"
        let input =
            "| 类别 | 依赖 |\n|------|------|\n| HTTP/WebSocket | `reqwest`, `tokio-tungstenite` |";
        let lines = render_markdown(input, 60, &theme());
        let t = text_of(&lines);
        let joined = t.join("\n");
        assert!(joined.contains("reqwest"), "code span 必须保留: {:?}", t);
        assert!(
            joined.contains("tokio-tungstenite"),
            "code span 必须保留: {:?}",
            t
        );
        assert!(!joined.contains(", ,"), "不得丢失 code 内容: {:?}", t);
        let code_styled = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains("reqwest") && s.style.fg.is_some())
        });
        assert!(code_styled, "code span 应保留样式: {:?}", t);
    }

    #[test]
    fn table_cell_wraps_keeps_spans() {
        // 单元格较宽时 code + 文本完整保留，折行不丢内容、不超宽
        let lines = render_markdown("| a | b |\n|---|--|\n| `cccc` dddd | 2 |", 30, &theme());
        let t = text_of(&lines);
        let joined = t.join("\n");
        assert!(joined.contains("cccc"), "code 内容保留: {:?}", t);
        assert!(joined.contains("dddd"), "文本内容保留: {:?}", t);
        for l in &lines {
            let w = l
                .spans
                .iter()
                .map(|s| display_width(&s.content))
                .sum::<usize>();
            assert!(
                w <= 30,
                "行宽超限: {:?} (width {})",
                text_of(std::slice::from_ref(l)),
                w
            );
        }
    }

    #[test]
    fn table_with_emoji_never_exceeds_width() {
        // 回归：cell 内 emoji 曾按 char 累加被高估（键帽 1️⃣=3 列、ZWJ 家庭=8 列），
        // 列宽虚高把表格撑破可用宽度，右侧边框画进滚动条列。
        let width = 30usize;
        for input in [
            "| col | note |\n|-----|------|\n| 1️⃣1️⃣1️⃣1️⃣1️⃣1️⃣1️⃣1️⃣ | x |",
            "| col | note |\n|-----|------|\n| 👨‍👩‍👧👨‍👩‍👧👨‍👩‍👧 | x |",
            "| col | note |\n|-----|------|\n| ✅ | this explanation is long enough to be wrapped |",
            // 长不可断词（无 emoji）：最小列宽之和也可能超过可用宽度
            "| col | note |\n|-----|------|\n| averyveryverylongunbreakableword | x |",
        ] {
            let lines = render_markdown(input, width, &theme());
            for l in &lines {
                let w: usize = l.spans.iter().map(|s| grapheme_width(&s.content)).sum();
                assert!(
                    w <= width,
                    "行宽超限: {:?} (width {w} > {width})",
                    text_of(std::slice::from_ref(l))
                );
            }
        }
    }

    #[test]
    fn table_emoji_rows_align_with_border() {
        // emoji 单元格的折行/补齐口径与边框一致：各行渲染宽度相同（右边框对齐）
        for input in [
            "| a | b |\n|---|---|\n| 👨‍👩‍👧 | 1 |",
            "| a | b |\n|---|---|\n| 1️⃣ | done |",
        ] {
            let lines = render_markdown(input, 40, &theme());
            let widths: Vec<usize> = lines
                .iter()
                .map(|l| l.spans.iter().map(|s| grapheme_width(&s.content)).sum())
                .collect();
            assert!(
                widths.windows(2).all(|w| w[0] == w[1]),
                "表格各行宽度不一致 {widths:?}: {:?}",
                text_of(&lines)
            );
        }
    }

    #[test]
    fn table_box() {
        let lines = render_markdown("| a | b |\n|---|--|\n| 1 | 2 |", 40, &theme());
        for l in &lines {
            eprintln!("row: {:?}", text_of(std::slice::from_ref(l)));
        }
        let t = text_of(&lines);
        assert!(t[0].starts_with("┌─"));
        assert!(t.iter().any(|l| l.contains('a')), "header missing: {:?}", t);
        assert!(t.iter().any(|l| l.contains('1')), "cell missing: {:?}", t);
        assert!(
            t.last().unwrap().starts_with("└─"),
            "bottom missing: {:?}",
            t
        );
    }

    #[test]
    fn wraps_long_words() {
        let lines = wrap_line_spans(vec![Span::raw("abcde fghij")], 8);
        let t = text_of(&lines);
        assert!(t.len() >= 2);
        for l in &t {
            assert!(display_width(l) <= 8, "line too wide: {:?}", l);
        }
    }

    #[test]
    fn hr_rule() {
        let lines = render_markdown("---", 20, &theme());
        let t = text_of(&lines);
        assert!(t[0].starts_with("─"));
        assert_eq!(display_width(&t[0]), 20);
    }
}

#[cfg(test)]
mod code_block_tests {
    use super::*;
    use ratatui::style::Color as RtColor;

    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn theme() -> Theme {
        Theme::default()
    }

    #[test]
    fn code_block_keeps_indentation() {
        // 回归：曾把行首空格全部丢弃，代码块完全失去缩进对齐
        let src = "```zig\nfn pop() {\n    self.mutex.lock();\n        deep();\n}\n```";
        let lines = render_markdown(src, 80, &theme());
        let t = text_of(&lines);
        assert!(t.iter().any(|l| l == "  fn pop() {"), "首行缩进: {:?}", t);
        assert!(
            t.iter().any(|l| l == "      self.mutex.lock();"),
            "函数体缩进必须是行首2空格+源码4空格: {:?}",
            t
        );
        assert!(
            t.iter().any(|l| l == "          deep();"),
            "深层缩进保留: {:?}",
            t
        );
    }

    #[test]
    fn code_block_long_line_wraps_keeps_indent() {
        let long = "aaaaaaaaaa bbbbbbbbbb cccccccccc";
        let src = format!("```\n    {}\n```", long);
        let lines = render_markdown(&src, 16, &theme());
        let t = text_of(&lines);
        assert!(t.len() >= 4, "超宽代码行应折行: {:?}", t);
        assert!(
            t[1].starts_with("      "),
            "折行首行保留 2+4 空格缩进: {:?}",
            t
        );
        assert!(t[1].contains("aaaaaaaaaa"), "内容保留: {:?}", t);
    }

    #[test]
    fn code_block_has_syntax_colors() {
        let src = "```zig\nconst x: u32 = 42; // note\n```";
        let lines = render_markdown(src, 80, &theme());
        let joined = text_of(&lines);
        assert!(joined.iter().any(|l| l.contains("const")));
        assert!(joined.iter().any(|l| l.contains("42")));
        let find = |s: &str| {
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .find(|sp| sp.content.as_ref() == s)
        };
        // 关键词/数字/注释都应有主题前景色，且各不相同
        let kw = find("const").expect("const token");
        let num = find("42").expect("42 token");
        assert!(kw.style.fg.is_some(), "keyword 应着色");
        assert!(num.style.fg.is_some(), "number 应着色");
        assert_ne!(kw.style.fg, num.style.fg, "keyword 与 number 颜色不同");
        // 回归：const x: u32 = 42 形式（名字后带类型注解）曾不满足 decl 规则、也不在
        // keyword 词表里，落默认正文色；现应由 keyword.control 规则着关键字色
        assert_eq!(
            kw.style.fg,
            Some(RtColor::Rgb(0x56, 0x9c, 0xd6)),
            "const 应是关键字蓝"
        );
        let comment = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|sp| sp.content.as_ref().contains("note"))
            .expect("comment token");
        assert!(comment.style.fg.is_some(), "comment 应着色");
        assert_ne!(comment.style.fg, kw.style.fg, "comment 与 keyword 颜色不同");
    }

    #[test]
    fn code_block_unknown_lang_falls_back_plain() {
        // 未识别语言（如 zzqq）回退纯文本，样式 = 代码块正文色（mdCodeBlock）
        let src = "```zzqq\nlet  x = 1;\n```";
        let lines = render_markdown(src, 80, &theme());
        let body = lines
            .iter()
            .find(|l| l.spans.iter().any(|sp| sp.content.as_ref() == "let"))
            .expect("body line");
        let t = text_of(std::slice::from_ref(body));
        assert!(
            t.iter().all(|l| l.contains("x = 1") || l.trim().is_empty()),
            "{:?}",
            t
        );
        let md_styled = theme().style(MD_CODE_BLOCK.0, MD_CODE_BLOCK.1);
        for sp in &body.spans {
            assert_eq!(
                sp.style.fg, md_styled.fg,
                "回退使用代码块正文色: {:?}",
                sp.content
            );
        }
    }

    #[test]
    fn code_block_fence_token_attrs_still_highlight() {
        // GitHub 风格 fence token（rust,ignore / {rust,linenos}）——后缀（,ignore 等）
        // 曾使语法查找失败回退纯文本；token 清洗后应正常解析并着色
        let md_styled = theme().style(MD_CODE_BLOCK.0, MD_CODE_BLOCK.1);

        let src = "```rust,ignore\nfn main() {\n    let v: u32 = 1;\n}\n```";
        let lines = render_markdown(src, 80, &theme());
        let main = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|sp| sp.content.as_ref() == "main")
            .expect("main token");
        assert_ne!(
            main.style.fg, md_styled.fg,
            "rust,ignore 应解析出语法并着色而非回退纯文本"
        );

        let src2 = "```{rust,linenos}\nfn main() {}\n```";
        let lines2 = render_markdown(src2, 80, &theme());
        let main2 = lines2
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|sp| sp.content.as_ref() == "main")
            .expect("main token 2");
        assert_ne!(main2.style.fg, md_styled.fg, "{{rust,linenos}} 同样应着色");
    }

    #[test]
    fn code_block_cpp_include_blocks_highlight() {
        // 回归：newlines 语法集下 #include 上下文只靠行尾 \n 退栈，逐行无 \n 喂入时
        // 永不退栈，从第二个 include 起整块塌成默认色；nonewlines 下应跨行正常高亮
        let src = "```cpp\n#include <iostream>\n#include <vector>\nvoid bubbleSort(std::vector<int>& arr) {\n    int n = arr.size();\n}\n```";
        let lines = render_markdown(src, 80, &theme());
        let find = |s: &str| {
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .find(|sp| sp.content.as_ref() == s)
        };
        let incs: Vec<_> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|sp| sp.content.as_ref() == "#include")
            .collect();
        assert!(incs.len() >= 2, "应有两条 #include: {:?}", text_of(&lines));
        for inc in &incs {
            assert_eq!(
                inc.style.fg,
                Some(RtColor::Rgb(0x56, 0x9c, 0xd6)),
                "include 关键字应蓝"
            );
        }
        let void = find("void").expect("void token");
        assert_eq!(
            void.style.fg,
            Some(RtColor::Rgb(0x4e, 0xc9, 0xb0)),
            "void 类型应 teal"
        );
        let fn_name = find("bubbleSort").expect("bubbleSort token");
        assert_eq!(
            fn_name.style.fg,
            Some(RtColor::Rgb(0xdc, 0xdc, 0xaa)),
            "函数名应黄"
        );
        let param = find("arr").expect("arr token");
        assert_eq!(
            param.style.fg,
            Some(RtColor::Rgb(0x9c, 0xdc, 0xfe)),
            "参数应浅蓝"
        );
    }

    #[test]
    fn code_block_in_blockquote_keeps_indent() {
        let src = "> ```\n>     a();\n> ```";
        let lines = render_markdown(src, 40, &theme());
        let t = text_of(&lines);
        assert!(
            t.iter().any(|l| l.contains("a()") && l.starts_with("│ ")),
            "引用内代码保留: {:?}",
            t
        );
    }

    #[test]
    fn mermaid_sequence_and_gantt_render_structured() {
        // 回归：sequenceDiagram / gantt 等由 mermaid-text 渲染为 Unicode 盒图，
        // 关键字与源码不再原样输出；参与者/消息/任务内容应出现在结果中
        let src = "```mermaid\n  sequenceDiagram\n      participant 用户\n      participant 系统\n      用户->>系统: 发送请求\n      系统->>用户: 返回响应\n```\n\n```mermaid\n  gantt\n      title 项目计划\n      section 阶段1\n      任务A :a1, 2024-01-01, 30d\n      section 阶段2\n      任务B :a2, after a1, 20d\n```";
        let lines = render_markdown(src, 80, &theme());
        let t = text_of(&lines);
        let joined = t.join("\n");
        for expect in ["用户", "系统", "发送请求", "返回响应"] {
            assert!(joined.contains(expect), "序列内容 {expect} 缺失: {:?}", t);
        }
        assert!(
            t.iter().any(|l| l.starts_with("      ┆ 发送请求")),
            "盒图行首缩进应保留（错位回归）: {:?}",
            t
        );
        assert!(
            !joined.contains("participant 用户"),
            "keyword 残留: {:?}",
            t
        );
        for expect in ["项目计划", "阶段1", "阶段2", "任务A", "任务B"] {
            assert!(joined.contains(expect), "甘特内容 {expect} 缺失: {:?}", t);
        }
        assert!(!joined.contains("sequenceDiagram"), "源码不应残留: {:?}", t);
        assert!(!joined.contains("gantt\n"), "gantt 关键字不应残留: {:?}", t);
    }

    #[test]
    fn mermaid_too_wide_falls_back_to_source() {
        // 严格宽度预算：图无法压缩进面板宽度时回退为源码块，而非输出超宽盒图
        let src = "```mermaid\n  graph LR\n      A[一个非常非常非常非常非常非常非常长的节点标签] --> B[另一个非常非常非常非常长的标签]\n```";
        let lines = render_markdown(src, 24, &theme());
        let t = text_of(&lines);
        let joined = t.join("\n");
        assert!(
            joined.contains("graph LR"),
            "超宽时不应渲染盒图，应回退源码: {:?}",
            t
        );
        assert!(joined.contains("A["), "回退后源码完整: {:?}", t);
    }

    #[test]
    fn mermaid_disabled_renders_source_block() {
        // settings 关闭 mermaid：不渲染盒图，代码块保留 ```mermaid 围栏与源码原样输出
        let mut th = theme();
        th.mermaid = false;
        let src = "```mermaid\ngraph TD\n    A --> B\n```";
        let lines = render_markdown(src, 80, &th);
        let t = text_of(&lines);
        let joined = t.join("\n");
        assert!(joined.contains("```mermaid"), "围栏保留: {:?}", t);
        assert!(joined.contains("graph TD"), "源码原样: {:?}", t);
        assert!(joined.contains("A --> B"), "源码原样: {:?}", t);
    }

    #[test]
    fn latex_disabled_renders_literal() {
        // settings 关闭 latex：$...$ 不解析为公式，分界符与源码原样输出
        let mut th = theme();
        th.latex = false;
        let lines = render_markdown("勾股定理 $a^2 + b^2 = c^2$", 60, &th);
        let joined = text_of(&lines).join("\n");
        assert!(
            joined.contains("$a^2 + b^2 = c^2$"),
            "行内公式原文保留: {:?}",
            joined
        );
        assert!(
            !joined.contains("a²"),
            "不得渲染为 Unicode 公式: {:?}",
            joined
        );

        let lines = render_markdown("$$\n\\sum x\n$$", 60, &th);
        let joined = text_of(&lines).join("\n");
        assert!(joined.contains("$$"), "display 分界符保留: {:?}", joined);
        assert!(joined.contains("\\sum x"), "公式源码保留: {:?}", joined);
    }
}
