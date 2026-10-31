//! 启动横幅扩展：共用一套布局（左 art + 竖线 + 右侧信息区），两个互斥实现。
//!
//! - [`normal`]：静态 ASCII art（`assets/banner/normal.txt`，扩展名 `banner`）；
//! - [`bouncyballs`]：art 由小球自上落下、回弹落位后显形（扩展名 `bouncyballs-banner`）。
//!
//! 渲染规则（两者共用 [`render_art`]）：
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
//! banner 不订阅 agent 事件：可见性由 TUI 启动态决定（提交输入后隐藏）。banner 互斥取
//! *最后注册的启用者*，因此小球版 priority 比静态版小一档、默认生效；静态版仍可在
//! `/extension` 面板切回。
//! Apple Terminal（macOS 自带终端，给块字符逐行留缝）改用纯文本词标布局，见
//! [`render_wordmark`]。

pub mod bouncyballs;
pub mod normal;

pub use bouncyballs::BouncyBallsBanner;
pub use normal::NormalBanner;

use crate::{
    core::extensions::{BannerCtx, BannerLine, BannerSpan},
    extensions::{ACCENT, DIM},
    utils::display::{display_width, truncate_display},
};

/// 顶部 padding 行数（与窗口上边缘留白）
pub(crate) const TOP_PAD: usize = 2;
/// 底部 padding 行数（与下方内容留白）
pub(crate) const BOTTOM_PAD: usize = 2;
/// 左侧 padding 列数（与窗口左边缘留白）
pub(crate) const LEFT_PAD: usize = 4;
/// art 文字与竖线之间的间隔列数
pub(crate) const ART_GAP: usize = 4;
/// 竖线与右侧信息区之间的间隔列数
pub(crate) const INFO_GAP: usize = 1;
/// 竖线分隔符（accent，仅 art 行绘制）
pub(crate) const DIVIDER: &str = "│";
/// 信息区最小宽度（列）：低于该宽只显示 art，不画竖线与信息
pub(crate) const INFO_MIN_WIDTH: usize = 30;

/// 横幅 ASCII art：assets/banner/normal.txt（编译期嵌入，构建产物自包含）
const ART: &str = crate::embedded!("banner/normal.txt");

/// 解析并预处理 art：去首尾空行、去行尾空白，返回 art 行
pub(crate) fn art_lines() -> Vec<String> {
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

/// Apple Terminal 的启动横幅：不用块字符 art、不画竖线，首行是词标 + 版本，其余信息行照旧。
///
/// 行数与全量布局同形（顶部 [`TOP_PAD`] 空行 + 信息行 + 底部 [`BOTTOM_PAD`] 空行），
/// 文字从 [`LEFT_PAD`] 列起；信息行少于 [`INFO_MIN_WIDTH`] + 左 padding 或高度不足以
/// 放下消息区/状态栏/输入区时返回空 Vec（与 art 布局同规则）。词标取信息区首行里的
/// 应用名（accent 加粗），同行剩下的版本号用 dim。
pub(crate) fn render_wordmark(ctx: &BannerCtx) -> Vec<BannerLine> {
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
                let short = truncate_display(rest, avail.saturating_sub(display_width(head) + 1)).0;
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

/// 按给定 `art` 行渲染横幅（art + 竖线 + 信息区），两个 banner 共用。
/// `art` 为空、宽或高不足时返回空 Vec（不渲染）。
pub(crate) fn render_art(ctx: &BannerCtx, art: &[String]) -> Vec<BannerLine> {
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
