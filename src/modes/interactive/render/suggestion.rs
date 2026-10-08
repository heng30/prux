//! 输入框候选列表渲染（/ 命令、子命令、@ 文件），显示在输入框下方。
//!
//! - prefix：选中 `→ `（accent）、未选中 `  `
//! - 主列宽 = 最宽项 + PRIMARY_COLUMN_GAP(2)，slash 命令与子命令 clamp [12, 32]，
//!   @ 文件固定 32（SelectList 默认 min=max=32）
//! - 有描述且宽度 >40：主列截断 + `spacing`（与描述同色）+ 描述折行，
//!   续行缩进到描述列；单条描述至多 [`MAX_DESCRIPTION_LINES`] 行，仍超出则末行 `…` 收尾；
//!   选中项主文本 accent，未选中主文本默认色；选中项的右侧描述（描述列）同样用 accent——
//!   `@` 候选（`SuggestionKind::Files`）尤其如此（同名文件 `src/main.rs` vs `tests/main.rs`
//!   只有路径不同），命令候选的描述也一并高亮，整行同色才好一眼确认选中的是哪条
//! - 无描述：`prefix + value`
//! - 历史候选：每条恰好一行（整宽截断 + 行尾 `…`）；`!`/`!!` 开头的 shell 行
//!   前置 `[shell] `（warning 色，按 Enter 会**直接执行**）、`/` 开头的斜杠命令
//!   前置 `[cmd] `（muted 色）
//! - 底部页码 `  (x/y)`（muted）仅在需要滚动时显示
//! - 无匹配：`  No matching commands`（muted）
//! - 可见项数 maxVisible = 5（折行后总行数可超过它），选中项居中滚动

use super::super::app::{App, Suggestion, display_width, truncate_display};
use crate::{modes::interactive::app::SuggestionKind, utils::display::wrap_words};
use ratatui::{
    style::Style,
    text::{Line, Span},
};

/// 主列（命令/文件名）与右侧描述列之间保留的空格宽度。
const PRIMARY_COLUMN_GAP: usize = 2;
/// 斜杠命令候选主列宽的下限，避免频繁截断。
const SLASH_COL_MIN: usize = 12;
/// 斜杠命令候选主列宽的上限，避免描述列被挤没。
const SLASH_COL_MAX: usize = 32;
/// `@` 文件候选固定的主列宽，与 SelectList 默认一致。
const AT_COL_WIDTH: usize = 32;
/// 剩余宽度低于此值时不再折行显示描述。
const MIN_DESCRIPTION_WIDTH: usize = 10;

/// 单条候选描述最多折几行（含首行）；超出部分末行 `…` 收尾，避免候选面板无限撑高
const MAX_DESCRIPTION_LINES: usize = 2;

/// `…` 的显示宽度（历史候选单行截断时预留）
const ELLIPSIS_WIDTH: usize = 1;

/// 历史候选里 shell 行（`!`/`!!`）的前置标记。
///
/// 历史现在不过滤内容，`!` 行会被存下来并出现在 `/history` 搜索结果里，而选中一条
/// shell 行按 Enter 就是**立即执行**（命令本身可能是破坏性的，且单行截断后未必看得全）。
/// 用文本标记而不是只靠颜色：主题改色 / 无色终端里也要能看见。
const SHELL_MARKER: &str = "[shell] ";

/// 历史候选里斜杠命令（`/…`）的前置标记：与普通 prompt 区分开。
/// 它不像 shell 那样立即执行副作用，所以用低饱和的 muted 色，不抢 warning。
const CMD_MARKER: &str = "[cmd] ";

/// 历史候选行的前置标记：(标记文本, 是否 shell)。
/// `!`/`!!` → shell 行；`/` → 斜杠命令；其余（普通 prompt）→ 无标记。
/// 判定与提交派发一致：只认行首第一个字符。
fn history_marker(name: &str) -> Option<(&'static str, bool)> {
    if name.starts_with('!') {
        Some((SHELL_MARKER, true))
    } else if name.starts_with('/') {
        Some((CMD_MARKER, false))
    } else {
        None
    }
}

/// 当前最大展示行数（/settings autocomplete-max-visible，至少 1）
fn max_visible(app: &App) -> usize {
    app.autocomplete_max_visible.max(1)
}

/// 候选区总高度（折行后的候选行 + 可能页码 1 行；页码仅当列表超可见范围时存在；未激活时为 0）
pub fn suggestion_height(app: &App, width: usize) -> u16 {
    if !app.suggestion.active {
        return 0;
    }
    suggestion_lines(app, width).len().max(1) as u16
}

/// 候选列表行（由输入框组件嵌入渲染）
pub fn suggestion_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let s = &app.suggestion;
    let total = s.items.len();
    let accent = app.theme.style("accent", "#8abeb7");
    let muted = app.theme.style("muted", "#808080");

    // 只有 History 会以空列表激活（搜不到 / 还没历史时面板留在原地）。
    // 文案不带「… yet」，因为同一个状态同时覆盖「没搜到」与「还没历史」。
    if total == 0 {
        let msg = match s.kind {
            SuggestionKind::History | SuggestionKind::HistoryAction => "  No matching history",
            _ => "  No matching commands",
        };
        return vec![Line::from(Span::styled(
            msg,
            app.theme.style("muted", "#808080"),
        ))];
    }
    render_items(app, width, accent, muted)
}

/// 渲染条目行 + 页码（对齐 SelectList 的滚动窗口与行结构）
fn render_items(app: &App, width: usize, accent: Style, muted: Style) -> Vec<Line<'static>> {
    let s = &app.suggestion;
    let total = s.items.len();
    let max = max_visible(app).min(total);
    let warning = app.theme.style("warning", "#ffcc66");

    // 滚动窗口：选中项居中（对齐 startIndex 公式）
    let start = s
        .selected
        .saturating_sub(max / 2)
        .min(total.saturating_sub(max));
    let end = (start + max).min(total);

    // 主列宽：slash [12,32]；@ 固定 32
    let col_width = suggestion_col_width(s);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, item) in s.items.iter().enumerate().skip(start).take(end - start) {
        let selected = i == s.selected;
        let prefix = if selected { "→ " } else { "  " };
        let prefix_w = 2;
        let main_style = if selected { accent } else { Style::default() };

        // 选中行的右侧描述与左侧名称同色（accent）：整行同色才看得出选的是哪条。
        // 文件候选靠路径区分同名文件；命令候选的长描述也一并高亮，方便确认当前选中项。
        let desc_style = if selected { accent } else { muted };

        // 历史候选与控制词候选：**每条恰好一行**。不走主列/描述列排版，也不折不包：
        // 整宽截断 + 行尾 `…`。不截断的话超长 prompt 会被终端折行，面板高度（suggestion_height）与实际行数不一致。
        if matches!(
            s.kind,
            SuggestionKind::History | SuggestionKind::HistoryAction
        ) {
            // shell 行（`!`/`!!`）与斜杠命令（`/…`）前置标记：前者按 Enter 会直接执行，
            // 后者是元命令，都需要与普通 prompt 区分。标记占宽从文本预算里扣，避免加了标记反而把命令挤出面板。
            let marker = history_marker(&item.name);
            let marker_w = marker.map(|(m, _)| display_width(m)).unwrap_or(0);
            let avail = width
                .saturating_sub(prefix_w + ELLIPSIS_WIDTH + marker_w)
                .max(1);

            // 控制词（@clear/@clear-all）带一句说明：它会在按 Enter 后删数据，
            // 光看词名不够。历史条目的描述为空，行为与之前一致。
            let full = if item.description.is_empty() {
                item.name.clone()
            } else {
                format!("{}  {}", item.name, item.description)
            };

            let (head, truncated) = truncate_display(&full, avail);
            let text = if truncated {
                format!("{head}…")
            } else {
                full
            };

            let mut spans = vec![Span::styled(prefix.to_string(), main_style)];
            if let Some((marker, is_shell)) = marker {
                let style = if is_shell { warning } else { muted };
                spans.push(Span::styled(marker.to_string(), style));
            }
            spans.push(Span::styled(text, main_style));
            lines.push(Line::from(spans));

            continue;
        }

        let has_desc = !item.description.is_empty() && width > 40;
        if !has_desc {
            lines.push(Line::from(vec![Span::styled(
                format!("{}{}", prefix, item.name),
                main_style,
            )]));
            continue;
        }

        let eff_col = col_width.min(width.saturating_sub(prefix_w + 4)).max(1);
        let max_primary = eff_col.saturating_sub(PRIMARY_COLUMN_GAP).max(1);
        let (value, _) = truncate_display(&item.name, max_primary);
        let value_w = display_width(&value);
        let spacing = " ".repeat(eff_col.saturating_sub(value_w).max(1));
        let desc_start = prefix_w + value_w + spacing.len();
        let remain = width.saturating_sub(desc_start + 2);

        if remain <= MIN_DESCRIPTION_WIDTH {
            lines.push(Line::from(vec![Span::styled(
                format!("{}{}", prefix, value),
                main_style,
            )]));
            continue;
        }

        // 描述折行：续行缩进到描述列，与首行左对齐
        let desc_lines = wrap_description(&item.description, remain, MAX_DESCRIPTION_LINES);
        let mut spans = vec![Span::styled(format!("{}{}", prefix, value), main_style)];
        let first = desc_lines.first().cloned().unwrap_or_default();
        if !first.is_empty() {
            spans.push(Span::styled(format!("{}{}", spacing, first), desc_style));
        }
        lines.push(Line::from(spans));

        let indent = " ".repeat(desc_start);
        for cont in desc_lines.iter().skip(1) {
            lines.push(Line::from(vec![Span::styled(
                format!("{}{}", indent, cont),
                desc_style,
            )]));
        }
    }

    // 页码：仅当列表可滚动时（对齐 SelectList）；右侧附上下移动快捷键提示
    if start > 0 || end < total {
        let page = format!("  ({}/{})   Ctrl+J/K move", s.selected + 1, total);
        let (page, _) = truncate_display(&page, width.saturating_sub(2));
        lines.push(Line::from(Span::styled(page, muted)));
    }
    lines
}

/// 主列宽：slash 命令与子命令 [12,32]，@ 文件固定 32（SelectList 默认 min=max=32）
fn suggestion_col_width(s: &Suggestion) -> usize {
    if matches!(
        s.kind,
        SuggestionKind::Commands | SuggestionKind::Subcommands
    ) {
        let widest = s
            .items
            .iter()
            .map(|it| display_width(&it.name) + PRIMARY_COLUMN_GAP)
            .max()
            .unwrap_or(0);
        widest.clamp(SLASH_COL_MIN, SLASH_COL_MAX)
    } else {
        // @ 文件与历史都不走主列排版（历史有自己的单行分支，不会用到这个值）
        AT_COL_WIDTH
    }
}

/// 描述折行：空白归一化为空格后按词折行，至多 `max_lines` 行；
/// 仍有剩余则在末行以 `…` 收尾（避免候选面板被长描述无限撑高）。
fn wrap_description(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let flat: String = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let mut lines = wrap_words(flat.trim(), width);
    if lines.len() > max_lines {
        lines.truncate(max_lines.max(1));
        if let Some(last) = lines.last_mut() {
            let (head, _) = truncate_display(last, width.saturating_sub(1));
            *last = format!("{}…", head.trim_end());
        }
    }
    lines
}

/// 供测试：渲染候选列表为纯文本
#[cfg(test)]
pub fn render_suggestion_text(app: &App, width: usize) -> Vec<String> {
    let lines = render_items(app, width, Style::default(), Style::default());
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::{App, SuggestionItem, SuggestionKind};

    fn app_with(items: Vec<(&str, &str)>, kind: SuggestionKind) -> App {
        let mut app = App::new();
        app.suggestion.active = true;
        app.suggestion.kind = kind;
        app.suggestion.items = items
            .into_iter()
            .map(|(n, d)| SuggestionItem {
                name: n.to_string(),
                description: d.to_string(),
                insert: n.to_string(),
            })
            .collect();
        app
    }

    #[test]
    fn long_description_wraps_to_aligned_continuation_line() {
        let desc =
            "Delete a built-in default's override .md, restoring the default: /agents reset <type>";
        let app = app_with(vec![("reset", desc)], SuggestionKind::Commands);
        let out = render_suggestion_text(&app, 80);

        // 主列 12：prefix 2 + "reset" 5 + spacing 7 = 14 列起描述
        assert_eq!(out.len(), 2, "描述折成两行: {:?}", out);
        assert!(out[0].contains("Delete a built-in"), "首行: {:?}", out[0]);
        assert!(
            out[1].starts_with(&" ".repeat(14)),
            "续行缩进到描述列: {:?}",
            out[1]
        );
        assert!(
            out[1].trim_start().starts_with("/agents reset <type>"),
            "续行是描述尾部: {:?}",
            out[1]
        );
    }

    #[test]
    fn description_wraps_on_word_boundary() {
        let desc = "alpha beta gamma delta epsilon zeta eta theta";
        let lines = wrap_description(desc, 24, 10);
        assert!(lines.len() >= 2, "长描述折行: {:?}", lines);
        // 每行都是完整词：拼回去与原文逐词一致
        assert_eq!(lines.join(" "), desc, "不切断单词: {:?}", lines);
        for line in &lines {
            assert!(display_width(line) <= 24, "不超描述列宽: {:?}", line);
        }
    }

    #[test]
    fn description_capped_with_ellipsis() {
        let long = "word ".repeat(60);
        let lines = wrap_description(long.trim(), 24, MAX_DESCRIPTION_LINES);
        assert_eq!(lines.len(), MAX_DESCRIPTION_LINES, "行数上限: {:?}", lines);
        assert!(lines[1].ends_with('…'), "末行省略号: {:?}", lines);
        assert!(
            display_width(&lines[1]) <= 24,
            "省略后仍不超宽: {:?}",
            lines
        );
    }

    #[test]
    fn cjk_description_wraps_without_spaces() {
        let desc = "用于研究复杂问题、搜索代码并执行多步任务的通用代理";
        let lines = wrap_description(desc, 20, 10);
        assert!(lines.len() >= 2, "CJK 无空格也能折行: {:?}", lines);
        assert_eq!(lines.join(""), desc, "折行不丢字: {:?}", lines);
        for line in &lines {
            assert!(display_width(line) <= 20, "不超描述列宽: {:?}", line);
        }
    }

    #[test]
    fn rendered_continuation_rejoins_without_losing_text() {
        let desc = "alpha beta gamma delta epsilon zeta eta theta";
        let app = app_with(vec![("help", desc)], SuggestionKind::Commands);
        let out = render_suggestion_text(&app, 60);
        assert!(out.len() >= 2, "长描述折行: {:?}", out);
        // 首行去掉主列后与续行拼接 = 原描述
        let first_desc = out[0].split("alpha").nth(1).map(|s| format!("alpha{s}"));
        let rejoined = std::iter::once(first_desc.expect("desc on first line"))
            .chain(out[1..].iter().map(|l| l.trim().to_string()))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(rejoined, desc, "续行与首行拼接还原描述: {:?}", out);
    }

    #[test]
    fn suggestion_height_matches_wrapped_rows() {
        let desc =
            "Delete a built-in default's override .md, restoring the default: /agents reset <type>";
        let app = app_with(vec![("reset", desc)], SuggestionKind::Commands);
        assert_eq!(
            suggestion_height(&app, 80) as usize,
            render_suggestion_text(&app, 80).len()
        );
        // 面板未激活：高度 0
        let mut app = app;
        app.suggestion.active = false;
        assert_eq!(suggestion_height(&app, 80), 0);
    }

    #[test]
    fn selected_row_uses_arrow_prefix_and_accent() {
        let app = app_with(
            vec![("help", "show help"), ("model", "pick model")],
            SuggestionKind::Commands,
        );
        let out = render_suggestion_text(&app, 80);
        assert!(out[0].starts_with("→ "), "selected prefix: {:?}", out[0]);
        assert!(out[1].starts_with("  "), "unselected prefix: {:?}", out[1]);
    }

    #[test]
    fn description_has_gap_after_name() {
        let app = app_with(vec![("help", "show help")], SuggestionKind::Commands);
        // 列宽 clamp 12：name "help" 4 宽 + spacing 至 12-2=10 → 间隔存在
        let out = render_suggestion_text(&app, 80);
        let hit = out[0].find("show help").expect("desc present");
        let name_end = out[0].find("help").unwrap() + 4;
        assert!(hit > name_end, "desc must be gapped: {:?}", out[0]);
        assert!(hit - name_end >= 1, "gap width: {:?}", out[0]);
    }

    #[test]
    fn page_indicator_only_when_scrollable() {
        let app = app_with(vec![("a", ""), ("b", "")], SuggestionKind::Commands);
        let out = render_suggestion_text(&app, 80);
        assert!(
            out.iter().all(|l| !l.contains('/')),
            "no page when fits: {:?}",
            out
        );

        let mut app = app_with(vec![("a", ""), ("b", "")], SuggestionKind::Commands);
        for i in 0..6 {
            app.suggestion.items.push(SuggestionItem {
                name: format!("long-name-{}", i),
                description: String::new(),
                insert: String::new(),
            });
        }
        let out = render_suggestion_text(&app, 80);
        assert!(
            out.iter().any(|l| l.trim_start().starts_with('(')),
            "page shown: {:?}",
            out
        );
    }

    #[test]
    fn no_match_message_constant() {
        // 无匹配文案对齐 pi："  No matching commands"
        let msg = "  No matching commands";
        assert!(msg.starts_with("  "));
        assert!(msg.contains("No matching commands"));
    }

    #[test]
    fn history_rows_are_single_line_and_ellipsized() {
        // 超长条目：一条一行、整宽截断、行尾 `…`
        let long = "x".repeat(200);
        let app = app_with(vec![(long.as_str(), "")], SuggestionKind::History);
        let out = render_suggestion_text(&app, 40);
        assert_eq!(out.len(), 1, "历史一条一行: {out:?}");
        assert!(out[0].starts_with("→ "), "选中前缀: {:?}", out[0]);
        assert!(out[0].ends_with('…'), "行尾省略号: {:?}", out[0]);
        assert!(display_width(&out[0]) <= 40, "不超宽: {:?}", out[0]);

        // 短条目不加省略号
        let app = app_with(vec![("short", "")], SuggestionKind::History);
        let out = render_suggestion_text(&app, 40);
        assert_eq!(out[0], "→ short");

        // CJK 按显示宽度截断（不是按字符数）
        let cjk = "中".repeat(100);
        let app = app_with(vec![(cjk.as_str(), "")], SuggestionKind::History);
        let out = render_suggestion_text(&app, 21);
        assert!(display_width(&out[0]) <= 21, "CJK 不超宽: {:?}", out[0]);
    }

    #[test]
    fn history_shell_rows_are_marked() {
        // `!`/`!!` 行选中后 Enter 会直接执行 → 用 `[shell]`，不可能是 `[cmd]`
        let app = app_with(
            vec![
                ("!git status", ""),
                ("!!rm -rf /tmp/x", ""),
                ("fix the bug", ""),
            ],
            SuggestionKind::History,
        );
        let out = render_suggestion_text(&app, 80);
        assert!(out[0].starts_with("→ [shell] "), "`!` 行: {:?}", out[0]);
        assert!(out[0].contains("!git status"), "原文完整保留: {:?}", out[0]);
        assert!(
            !out[0].contains("[cmd]"),
            "shell 行不带 [cmd]: {:?}",
            out[0]
        );
        assert!(out[1].starts_with("  [shell] "), "`!!` 行: {:?}", out[1]);
        assert!(
            out[1].contains("!!rm -rf /tmp/x"),
            "原文完整保留: {:?}",
            out[1]
        );
        assert!(
            !out[2].contains("[shell]") && !out[2].contains("[cmd]"),
            "普通 prompt 不标记: {:?}",
            out[2]
        );

        // 窄终端：标记占宽从文本预算里扣，仍然一条一行、不超宽、行尾省略号
        let app = app_with(vec![("!echo hello world", "")], SuggestionKind::History);
        let out = render_suggestion_text(&app, 20);
        assert_eq!(out.len(), 1, "仍是一条一行: {out:?}");
        assert!(out[0].starts_with("→ [shell] "), "{:?}", out[0]);
        assert!(out[0].ends_with('…'), "超长命令被截断: {:?}", out[0]);
        assert!(display_width(&out[0]) <= 20, "不超宽: {:?}", out[0]);
    }

    #[test]
    fn history_cmd_rows_are_marked() {
        // `/` 行是斜杠命令 → 用 `[cmd]`，与 shell 行、普通 prompt 都区分开
        let app = app_with(
            vec![
                ("/model gpt-5", ""),
                ("/compact 聚焦并发", ""),
                ("fix the bug", ""),
            ],
            SuggestionKind::History,
        );
        let out = render_suggestion_text(&app, 80);
        assert!(out[0].starts_with("→ [cmd] "), "`/` 行: {:?}", out[0]);
        assert!(
            out[0].contains("/model gpt-5"),
            "原文完整保留: {:?}",
            out[0]
        );
        assert!(!out[0].contains("[shell]"), "命令不是 shell: {:?}", out[0]);
        assert!(
            out[1].starts_with("  [cmd] "),
            "带参数的 / 行: {:?}",
            out[1]
        );
        assert!(
            out[1].contains("/compact 聚焦并发"),
            "原文完整保留: {:?}",
            out[1]
        );
        assert!(
            !out[2].contains("[shell]") && !out[2].contains("[cmd]"),
            "普通 prompt 不标记: {:?}",
            out[2]
        );

        // 窄终端：标记占宽从文本预算里扣，仍然一条一行、不超宽、行尾省略号
        let app = app_with(
            vec![("/compact 一个很长的自定义摘要指令", "")],
            SuggestionKind::History,
        );
        let out = render_suggestion_text(&app, 24);
        assert_eq!(out.len(), 1, "仍是一条一行: {out:?}");
        assert!(out[0].starts_with("→ [cmd] "), "{:?}", out[0]);
        assert!(out[0].ends_with('…'), "超长命令被截断: {:?}", out[0]);
        assert!(display_width(&out[0]) <= 24, "不超宽: {:?}", out[0]);
    }

    #[test]
    fn history_action_rows_show_description_and_stay_single_line() {
        // 控制词行必须带上说明（它按 Enter 会删数据），且仍然一条一行
        let app = app_with(
            vec![(
                "@clear-all",
                "Remove ALL prompt history files (every project)",
            )],
            SuggestionKind::HistoryAction,
        );
        let out = render_suggestion_text(&app, 80);
        assert_eq!(out.len(), 1, "仍是一条一行: {out:?}");
        assert!(out[0].starts_with("→ @clear-all"), "{:?}", out[0]);
        assert!(out[0].contains("Remove ALL prompt history"), "{:?}", out[0]);

        // 窄终端：说明被截断、行尾省略号、不折行
        let out = render_suggestion_text(&app, 24);
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with('…'), "{:?}", out[0]);
        assert!(display_width(&out[0]) <= 24, "{:?}", out[0]);
    }

    #[test]
    fn history_empty_list_renders_no_match_message() {
        // 零匹配/空历史时面板以空列表保持激活（show_suggestions allow_empty），
        // 渲染为一行 muted 文案——这条分支此前只有 Commands 文案，且实际不可达。
        let mut app = app_with(Vec::new(), SuggestionKind::History);
        app.suggestion.active = true;
        let lines = suggestion_lines(&app, 80);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("No matching history"), "{text:?}");
        assert_eq!(suggestion_height(&app, 80), 1);
    }

    #[test]
    fn selected_file_row_colors_path_like_name() {
        // 同名文件（src/main.rs vs tests/main.rs）只能靠右侧路径区分，
        // 选中行整行 accent 才能一眼看出选的是哪一条。
        let app = app_with(
            vec![("main.rs", "src/main.rs"), ("main.rs", "tests/main.rs")],
            SuggestionKind::Files,
        );
        let accent = app.theme.style("accent", "#8abeb7");
        let muted = app.theme.style("muted", "#808080");
        let lines = render_items(&app, 80, accent, muted);

        let selected = &lines[0];
        assert!(selected.spans[0].content.starts_with("→ main.rs"));
        assert_eq!(selected.spans[0].style.fg, accent.fg, "选中文件名 accent");
        let path = selected
            .spans
            .iter()
            .find(|sp| sp.content.contains("src/main.rs"))
            .expect("path span");
        assert_eq!(path.style.fg, accent.fg, "选中行右侧路径与文件名同色");

        let other = &lines[1];
        let other_path = other
            .spans
            .iter()
            .find(|sp| sp.content.contains("tests/main.rs"))
            .expect("path span");
        assert_eq!(other_path.style.fg, muted.fg, "未选中行路径保持 muted");
        assert_eq!(other.spans[0].style.fg, None, "未选中文件名默认色");
    }

    #[test]
    fn selected_command_description_uses_accent() {
        // 选中命令的描述与左侧名称同色（accent），整行高亮方便确认选中项
        let app = app_with(
            vec![("help", "show help for commands")],
            SuggestionKind::Commands,
        );
        let accent = app.theme.style("accent", "#8abeb7");
        let muted = app.theme.style("muted", "#808080");
        let lines = render_items(&app, 80, accent, muted);
        let desc = lines[0]
            .spans
            .iter()
            .find(|sp| sp.content.contains("show help"))
            .expect("desc span");
        assert_eq!(desc.style.fg, accent.fg, "选中命令描述 accent");
        assert_ne!(desc.style.fg, muted.fg, "选中命令描述不再是 muted");
    }

    #[test]
    fn unselected_command_description_stays_muted() {
        // 未选中命令的描述保持 muted，不抢主列
        let app = app_with(
            vec![("help", "show help for commands"), ("quit", "exit prux")],
            SuggestionKind::Commands,
        );
        let accent = app.theme.style("accent", "#8abeb7");
        let muted = app.theme.style("muted", "#808080");
        let lines = render_items(&app, 80, accent, muted);
        // 选中项在第 0 行（辅助函数默认 selected=0）；未选中项在第 1 行
        let desc = lines[1]
            .spans
            .iter()
            .find(|sp| sp.content.contains("exit prux"))
            .expect("desc span");
        assert_eq!(desc.style.fg, muted.fg, "未选中命令描述保持 muted");
    }

    #[test]
    fn page_row_has_move_hint() {
        // 可滚动列表的页码行右侧附 Ctrl+J/K 移动提示
        let mut app = app_with(vec![("a", ""), ("b", "")], SuggestionKind::Commands);
        for i in 0..6 {
            app.suggestion.items.push(SuggestionItem {
                name: format!("long-name-{}", i),
                description: String::new(),
                insert: String::new(),
            });
        }
        let out = render_suggestion_text(&app, 80);
        let page_row = out
            .iter()
            .find(|l| l.trim_start().starts_with('('))
            .expect("page row");
        assert!(
            page_row.contains("Ctrl+J/K"),
            "hint on page row: {:?}",
            page_row
        );
        assert!(
            out[0].starts_with("→ "),
            "first row still selected: {:?}",
            out[0]
        );
    }
}
