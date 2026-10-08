//! `banner`：启动横幅扩展。
//!
//! 干净启动时（banner 启用、会话无历史消息、无启动初始消息、用户未提交过输入）
//! 在 TUI 窗口顶部渲染 `assets/banner/normal.txt` 的 ASCII art（"PRUX"）。
//!
//! 渲染规则：
//! - 文本预处理：去首尾空行，只保留 art 行（当前 6 行），行尾空白去除；
//! - 左对齐，左侧留 4 列 padding，顶部 2 行 + 底部 2 行空行 padding；
//! - 竖线（`│`，accent）只画在 6 个 art 行，与 art 间隔 4 列，且以 art 最长行为
//!   基准对齐（列 = 左 padding + art 宽 + 4，短行间隔自动变宽）；顶部/底部
//!   padding 行不画竖线；
//! - 竖线右侧是 6 行信息区（版本、描述、常用工具、帮助提示），与 art 行对齐，
//!   上下各留 2 空行；
//! - 整块配色：art/竖线/版本行 `accent`，其余信息行 `dim`（均经 `Theme::style` 主题定制）；
//! - 宽度三档：`≥ art宽+左padding+间隔+竖线+信息区最小宽(30)` 全量渲染；
//!   `art宽+左padding` ≤ 宽 < 上达阈值 只渲染 art（无竖线无信息区）；
//!   更窄则整体隐藏（左边缘 art 会被截断）；
//! - 终端总高不足以放下 padding + art + 消息区(至少 1) + 状态栏(1) + 输入区(3) 时不渲染。
//!
//! banner 无状态、不订阅 agent 事件：可见性由 TUI 启动态决定（提交输入后隐藏）。
//! Apple Terminal（macOS 自带终端，给块字符逐行留缝）改用纯文本词标布局，见
//! [`Banner::render_wordmark`]。

use crate::{
    core::extensions::{BannerCtx, BannerExtension, BannerLine, BannerSpan, ExtensionMode},
    extensions::{ACCENT, BANNER_FACTORIES, BannerFactory, DIM, PRIORITY_BANNER},
    utils::{
        display::{display_width, truncate_display},
        terminal_caps,
    },
};
use std::sync::Arc;

/// 顶部 padding 行数（与窗口上边缘留白）
const TOP_PAD: usize = 2;
/// 底部 padding 行数（与下方内容留白）
const BOTTOM_PAD: usize = 2;
/// 左侧 padding 列数（与窗口左边缘留白）
const LEFT_PAD: usize = 4;
/// art 文字与竖线之间的间隔列数
const ART_GAP: usize = 4;
/// 竖线与右侧信息区之间的间隔列数
const INFO_GAP: usize = 1;
/// 竖线分隔符（accent，仅 art 行绘制）
const DIVIDER: &str = "│";
/// 信息区最小宽度（列）：低于该宽只显示 art，不画竖线与信息
const INFO_MIN_WIDTH: usize = 30;

/// 横幅 ASCII art：assets/banner/normal.txt（编译期嵌入，构建产物自包含）
const ART: &str = crate::embedded!("banner/normal.txt");

/// 向扩展注册表登记本扩展工厂（优先级 PRIORITY_BANNER），启动时由框架加载。
#[linkme::distributed_slice(BANNER_FACTORIES)]
static BANNER_FACTORY: BannerFactory = BannerFactory {
    priority: PRIORITY_BANNER,
    make: || Arc::new(Banner::new()),
};

/// 无状态的启动横幅扩展：干净启动时在 TUI 顶部渲染 ASCII art 与版本信息区。
pub struct Banner;

impl Banner {
    /// 构造横幅扩展（无状态）。
    pub fn new() -> Self {
        Banner
    }

    /// Apple Terminal 的启动横幅：不用块字符 art、不画竖线，首行是词标 + 版本，其余信息行照旧。
    ///
    /// 行数与全量布局同形（顶部 [`TOP_PAD`] 空行 + 信息行 + 底部 [`BOTTOM_PAD`] 空行），
    /// 文字从 [`LEFT_PAD`] 列起；信息行少于 [`INFO_MIN_WIDTH`] + 左 padding 或高度不足以
    /// 放下消息区/状态栏/输入区时返回空 Vec（与 art 布局同规则）。词标取信息区首行里的
    /// 应用名（accent 加粗），同行剩下的版本号用 dim。
    fn render_wordmark(&self, ctx: &BannerCtx) -> Vec<BannerLine> {
        let info = info_lines();
        let total_rows = TOP_PAD + info.len() + BOTTOM_PAD;
        if ctx.height < total_rows + 5 || ctx.width < LEFT_PAD + INFO_MIN_WIDTH {
            return Vec::new();
        }

        let left = " ".repeat(LEFT_PAD);
        let avail = ctx.width.saturating_sub(LEFT_PAD);
        let mut out: Vec<BannerLine> = Vec::with_capacity(total_rows);
        for r in 0..total_rows {
            let Some((key, default, bold, text)) = r.checked_sub(TOP_PAD).and_then(|i| info.get(i))
            else {
                // 顶部/底部 padding 行：整行空
                out.push(Vec::new());
                continue;
            };

            let mut line: BannerLine = vec![BannerSpan::new(ACCENT.0, ACCENT.1, left.clone())];
            if text.is_empty() {
                out.push(line);
                continue;
            }

            if *bold {
                // 词标行：应用名 accent 加粗，同行剩下的版本号 dim
                let (head, rest) = text.split_once(' ').unwrap_or((text.as_str(), ""));
                line.push(BannerSpan::new_bold(ACCENT.0, ACCENT.1, head.to_string()));
                if !rest.is_empty() {
                    let short =
                        truncate_display(rest, avail.saturating_sub(display_width(head) + 1)).0;
                    if !short.is_empty() {
                        line.push(BannerSpan::new(DIM.0, DIM.1, format!(" {short}")));
                    }
                }
            } else {
                let short = truncate_display(text, avail).0;
                line.push(BannerSpan::new(key, default, short));
            }
            out.push(line);
        }
        out
    }
}

impl Default for Banner {
    /// 默认构造等价于 [`Banner::new`]。
    fn default() -> Self {
        Self::new()
    }
}

/// 解析并预处理 art：去首尾空行、去行尾空白，返回 art 行
fn art_lines() -> Vec<String> {
    let lines: Vec<String> = ART
        .lines()
        .map(|l| l.trim_end().to_string())
        .skip_while(|l| l.trim().is_empty())
        .collect();
    let mut lines = lines;
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines
}

/// 信息区 6 行（与 art 行 1:1 对齐，同为中间 6 行）：
/// (主题键, 缺省色, 文本)。首行（版本）用 accent，其余 dim。
fn info_lines() -> [(&'static str, &'static str, bool, String); 6] {
    [
        // 版本行 accent + 加粗
        (
            ACCENT.0,
            ACCENT.1,
            true,
            format!("{} v{}", crate::APP_NAME, env!("CARGO_PKG_VERSION")),
        ),
        (DIM.0, DIM.1, false, "AI coding agent".to_string()),
        (
            DIM.0,
            DIM.1,
            false,
            "tools: read write edit bash/powershell".to_string(),
        ),
        (DIM.0, DIM.1, false, String::new()),
        (
            DIM.0,
            DIM.1,
            false,
            "/extension: manage extensions".to_string(),
        ),
        (
            DIM.0,
            DIM.1,
            false,
            "Ctrl+P switch models · Shift+Tab switch thinking".to_string(),
        ),
    ]
}

impl BannerExtension for Banner {
    /// 固定返回 `"banner"`。
    fn name(&self) -> &str {
        "banner"
    }

    /// 返回 `/extension` 面板展示的一句话说明。
    fn description(&self) -> &str {
        "Startup ASCII banner at the top of the TUI, shown once on clean startup."
    }

    /// 只在 Minimal 模式注册。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Minimal]
    }

    /// 按 `ctx` 的宽高渲染启动横幅（art + 竖线 + 信息区）；宽度或高度不足时返回空 Vec（不显示）。
    /// Apple Terminal 下改用纯文本词标布局（见 [`Self::render_wordmark`]）。
    fn render(&self, ctx: &BannerCtx) -> Vec<BannerLine> {
        if terminal_caps::is_apple_terminal() {
            return self.render_wordmark(ctx);
        }

        let art = art_lines();
        if art.is_empty() {
            return Vec::new();
        }

        let total_rows = TOP_PAD + art.len() + BOTTOM_PAD;
        // 终端高度不足：padding + art + 消息区(≥1) + 状态栏(1) + 输入区(3) 放不下 → 隐藏
        if ctx.height < total_rows + 5 {
            return Vec::new();
        }

        let max_w = art.iter().map(|l| display_width(l)).max().unwrap_or(0);
        let art_span = LEFT_PAD + max_w;
        // 宽度不足：左边缘 art 会被截断 → 隐藏
        if max_w == 0 || ctx.width < art_span {
            return Vec::new();
        }

        // 竖线列：以 art 最长行为基准（左 padding + art 宽 + 间隔），短行间隔自动变宽
        let div_col = art_span + ART_GAP;
        // 宽度三档：全量（art + 竖线 + 信息区）/ 仅 art / 隐藏
        let info_on = ctx.width >= div_col + 1 + INFO_GAP + INFO_MIN_WIDTH;
        let info_w = ctx.width.saturating_sub(div_col + 1 + INFO_GAP);
        let info: Vec<(bool, String)> = if info_on {
            info_lines()
                .iter()
                .map(|(_, _, bold, t)| (*bold, truncate_display(t, info_w.max(1)).0))
                .collect()
        } else {
            Vec::new()
        };

        let left = " ".repeat(LEFT_PAD);
        let mut out: Vec<BannerLine> = Vec::with_capacity(total_rows);
        for r in 0..total_rows {
            let rel = r.wrapping_sub(TOP_PAD);
            let Some(art_line) = art.get(rel) else {
                // 顶部/底部 padding 行：整行空（不画竖线）
                out.push(Vec::new());
                continue;
            };
            let mut line: BannerLine = Vec::new();
            line.push(BannerSpan::new(ACCENT.0, ACCENT.1, left.clone()));
            line.push(BannerSpan::new(ACCENT.0, ACCENT.1, art_line.clone()));
            if info_on {
                // art→竖线间隔：以最长行为基准，短行补足到 div_col
                let gap = div_col.saturating_sub(LEFT_PAD + display_width(art_line));
                line.push(BannerSpan::new(ACCENT.0, ACCENT.1, " ".repeat(gap)));
                line.push(BannerSpan::new(ACCENT.0, ACCENT.1, DIVIDER));
                if let Some((bold, t)) = info.get(rel)
                    && !t.is_empty()
                {
                    // 竖线与信息文本间 1 列空格
                    line.push(BannerSpan::new(DIM.0, DIM.1, " "));
                    if *bold {
                        // 版本行 accent + 加粗
                        line.push(BannerSpan::new_bold(ACCENT.0, ACCENT.1, t.clone()));
                    } else {
                        line.push(BannerSpan::new(DIM.0, DIM.1, t.clone()));
                    }
                }
            }
            out.push(line);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_width(line: &BannerLine) -> usize {
        line.iter().map(|s| display_width(&s.text)).sum()
    }

    fn row_text(line: &BannerLine) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    /// 全量渲染宽度（左 padding + art + 间隔 + 竖线 + 信息间隔 + 信息区最小宽）
    fn full_width() -> usize {
        art_lines().iter().map(|l| display_width(l)).max().unwrap()
            + LEFT_PAD
            + ART_GAP
            + 1
            + INFO_GAP
            + INFO_MIN_WIDTH
    }

    #[test]
    fn medium_width_renders_art_only_no_divider() {
        let banner = Banner::new();
        let art = art_lines();
        assert_eq!(art.len(), 6, "去首尾空行后剩 6 行 art");
        let max_w = art.iter().map(|l| display_width(l)).max().unwrap();
        assert_eq!(max_w, 33, "art 最宽 33 列");
        // 宽度 = art 宽 + 左 padding：只渲染 art，无竖线无信息区
        let lines = banner.render(&BannerCtx {
            width: max_w + LEFT_PAD,
            height: TOP_PAD + art.len() + BOTTOM_PAD + 5,
        });
        assert_eq!(
            lines.len(),
            TOP_PAD + art.len() + BOTTOM_PAD,
            "顶部 2 空行 + 6 art 行 + 底部 2 空行"
        );
        // 首/尾 padding 行为空行；art 行左 4 列 padding + art 原文（靠左，不居中）
        assert_eq!(line_width(&lines[0]), 0, "顶部 padding 行为空");
        assert_eq!(line_width(&lines[1]), 0, "顶部第二空行");
        assert_eq!(
            line_width(&lines[lines.len() - 1]),
            0,
            "底部 padding 行为空"
        );
        for (i, l) in lines.iter().skip(TOP_PAD).take(art.len()).enumerate() {
            assert_eq!(
                line_width(l),
                LEFT_PAD + display_width(&art[i]),
                "art 行 {i}: 左 padding + art 宽"
            );
            let pad_text = l.first().unwrap().text.clone();
            assert_eq!(pad_text.len(), LEFT_PAD, "art 行左 padding {LEFT_PAD} 列");
            assert!(
                l.last()
                    .unwrap()
                    .text
                    .starts_with(&art[i].chars().next().unwrap().to_string()),
                "art 行 {i} 原文保留"
            );
            assert!(
                !row_text(l).contains(DIVIDER),
                "仅 art 模式不画竖线: {:?}",
                row_text(l)
            );
        }
        assert!(
            lines[TOP_PAD].last().unwrap().text.starts_with('█'),
            "首 art 行为 PRUX 首行: {:?}",
            lines[TOP_PAD]
        );
        assert!(
            lines[TOP_PAD + 5].last().unwrap().text.starts_with('╚'),
            "末 art 行为 PRUX 末行: {:?}",
            lines[TOP_PAD + 5]
        );
    }

    #[test]
    fn wide_width_renders_divider_and_info() {
        let banner = Banner::new();
        let art = art_lines();
        let max_w = art.iter().map(|l| display_width(l)).max().unwrap();
        let w = full_width() + 10; // 信息区足够宽，不截断
        debug_assert!(w - (LEFT_PAD + max_w + 1) >= 40, "信息区 ≥ 40 列");
        let lines = banner.render(&BannerCtx {
            width: w,
            height: TOP_PAD + art.len() + BOTTOM_PAD + 5,
        });
        assert_eq!(lines.len(), TOP_PAD + art.len() + BOTTOM_PAD);
        let div_col_width = LEFT_PAD + max_w + ART_GAP; // 竖线前总宽（4+33+4=41 → 竖线列 42）

        // 竖线只画在 6 个 art 行（行 2..8）；padding 行（0/1/8/9）整行空、无竖线
        for r in [0usize, 1, lines.len() - 2, lines.len() - 1] {
            assert_eq!(line_width(&lines[r]), 0, "padding 行 {r} 应为空行");
        }
        for (r, line) in lines.iter().enumerate().skip(TOP_PAD).take(art.len()) {
            let text = row_text(line);
            // 竖线列对齐：每行 '│' 前的宽度一致（以最长 art 行为基准）
            let before = text.split_once(DIVIDER).map(|(a, _)| a).unwrap_or("");
            assert_eq!(
                display_width(before),
                div_col_width,
                "art 行 {r} 竖线列对齐: {:?}",
                text
            );
            assert!(
                text.starts_with(&" ".repeat(LEFT_PAD)),
                "art 行 {r} 左 padding: {:?}",
                text
            );
        }

        // 空白信息行（info 第 3 行，即整体行 5）：竖线后无文本（左侧仍是 art）
        let blank_row_text = row_text(&lines[TOP_PAD + 3]);
        let after = blank_row_text
            .split_once(DIVIDER)
            .map(|(_, b)| b)
            .unwrap_or("");
        assert!(after.is_empty(), "空白信息行竖线后应无文本: {:?}", after);

        // 信息行：版本（accent）+ 描述 + 工具 + 帮助，与 art 行对齐
        // （整体行 = TOP_PAD + info 行号：行 2=版本、3=描述、4=工具、6=帮助、7=快捷键）
        assert!(
            row_text(&lines[2]).contains("prux v"),
            "版本行: {:?}",
            lines[2]
        );
        assert!(
            row_text(&lines[3]).contains("AI coding agent"),
            "描述行: {:?}",
            row_text(&lines[3])
        );
        assert!(
            row_text(&lines[4]).contains("tools: read write edit bash/powershell"),
            "工具行: {:?}",
            row_text(&lines[4])
        );
        assert!(
            row_text(&lines[6]).contains("/extension: manage extensions"),
            "帮助行"
        );
        assert!(
            row_text(&lines[7]).contains("Ctrl+P switch models"),
            "快捷键行: {:?}",
            row_text(&lines[7])
        );
        // art 原文仍保留（竖线前）
        assert!(row_text(&lines[2]).contains(&art[0]));
        assert!(row_text(&lines[7]).contains(&art[5]));

        // 版本行加粗，其余信息行不加粗；art/竖线/间隔不加粗
        assert!(
            lines[2].last().unwrap().bold,
            "版本行应加粗: {:?}",
            lines[2].last().unwrap()
        );
        assert!(!lines[3].last().unwrap().bold, "描述行不加粗");
        assert!(!lines[6].last().unwrap().bold, "帮助行不加粗");
        assert!(
            lines[2]
                .iter()
                .all(|s| s.bold == (s.text.contains("prux v")))
        );
    }

    #[test]
    fn info_truncated_to_fit_narrow_wide_terminal() {
        let banner = Banner::new();
        // 恰好全量阈值：信息区 30 列，长行按显示宽度截断
        let w = full_width();
        let n = TOP_PAD + art_lines().len() + BOTTOM_PAD + 5;
        let lines = banner.render(&BannerCtx {
            width: w,
            height: n,
        });
        assert_eq!(lines.len(), TOP_PAD + art_lines().len() + BOTTOM_PAD);
        let info_part = |row: usize| {
            row_text(&lines[row])
                .split_once(DIVIDER)
                .map(|(_, b)| b.trim_start().to_string())
                .unwrap_or_default()
        };
        // 工具行（整体行 4）信息截断保头，且不超信息区宽
        let info = info_part(4);
        assert!(
            info.starts_with("tools: read"),
            "工具行截断保头: {:?}",
            info
        );
        assert!(
            display_width(&info) <= INFO_MIN_WIDTH,
            "不超信息区宽: {:?}",
            info
        );
        // 短行不受影响
        assert!(info_part(2).contains("prux v"));
        assert!(info_part(6).contains("/extension"));
    }

    #[test]
    fn narrow_terminal_hides_banner() {
        let banner = Banner::new();
        let art = art_lines();
        let max_w = art.iter().map(|l| display_width(l)).max().unwrap();
        // 宽度 < art 宽 + 左 padding：左边缘 art 会被截断 → 隐藏
        let lines = banner.render(&BannerCtx {
            width: max_w + LEFT_PAD - 1,
            height: TOP_PAD + art.len() + BOTTOM_PAD + 5,
        });
        assert!(lines.is_empty(), "宽度 < art 宽 + 左 padding 时不渲染");
    }

    #[test]
    fn short_terminal_hides_banner() {
        let banner = Banner::new();
        let art = art_lines();
        // 下限 = padding(4) + art(6) + 消息(1) + 状态(1) + 输入(3) = 15
        let min_h = TOP_PAD + art.len() + BOTTOM_PAD + 5;
        let lines = banner.render(&BannerCtx {
            width: 80,
            height: min_h - 1,
        });
        assert!(lines.is_empty(), "总高 < 下限时不渲染");
        let ok = banner.render(&BannerCtx {
            width: 80,
            height: min_h,
        });
        assert_eq!(
            ok.len(),
            TOP_PAD + art.len() + BOTTOM_PAD,
            "总高 = 下限时渲染"
        );
    }

    /// pi：Apple Terminal 给块字符逐行留缝，`████` art 会断层 → 改用纯文本词标布局。
    #[test]
    fn apple_terminal_uses_text_wordmark() {
        let banner = Banner::new();
        let info = info_lines();
        let rows = TOP_PAD + info.len() + BOTTOM_PAD;
        let lines = banner.render_wordmark(&BannerCtx {
            width: 80,
            height: rows + 5,
        });
        assert_eq!(lines.len(), rows, "行数与 art 布局同形");
        // 顶部/底部 padding 行为空；没有块字符与竖线
        for r in [0usize, 1, rows - 2, rows - 1] {
            assert_eq!(line_width(&lines[r]), 0, "padding 行 {r} 为空");
        }
        for (i, l) in lines.iter().enumerate() {
            let text = row_text(l);
            assert!(
                !text.contains('█') && !text.contains(DIVIDER),
                "行 {i}: {text:?}"
            );
        }
        // 首行是词标 + 版本（词标与版本分开着色）
        let first = &lines[TOP_PAD];
        assert_eq!(first[0].text, " ".repeat(LEFT_PAD));
        assert_eq!(first[1].text, crate::APP_NAME);
        assert!(first[1].bold, "词标应加粗");
        assert_eq!(first[1].key, ACCENT.0);
        assert!(
            first[2].text.starts_with(" v") && first[2].key == DIM.0,
            "版本行为 dim: {:?}",
            first[2]
        );
        // 其余信息行从左 padding 起原样渲染
        let tools = row_text(&lines[TOP_PAD + 2]);
        assert!(
            tools.starts_with(&" ".repeat(LEFT_PAD)) && tools.contains("tools: read"),
            "{tools:?}"
        );

        // 宽度不足信息区最小宽 / 高度不足 → 隐藏
        assert!(
            banner
                .render_wordmark(&BannerCtx {
                    width: LEFT_PAD + INFO_MIN_WIDTH - 1,
                    height: rows + 5,
                })
                .is_empty()
        );
        assert!(
            banner
                .render_wordmark(&BannerCtx {
                    width: 80,
                    height: rows + 4,
                })
                .is_empty()
        );
    }

    #[test]
    fn modes_are_minimal() {
        assert_eq!(Banner::new().modes(), vec![ExtensionMode::Minimal]);
    }
}
