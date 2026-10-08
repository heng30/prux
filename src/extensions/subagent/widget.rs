//! 子代理常驻 widget（停靠面板段）与 agent 颜色 badge。
//!
//! 本模块只做**渲染**：
//! 输入是记录快照 + 配置，输出是 `DockLine`，不持有状态、不读盘、不加锁，
//! 因此可以每帧被 `dock_sections()` 调用。
//!
//! 颜色：类型定义里的 `color`，解析为十六进制后按 badge 段着色（见 [`resolve_agent_color`]）。

use super::{
    super::util::spinner_frame,
    config::{SubagentConfig, WidgetMode},
    notify,
    types::{AgentRecord, AgentStatus},
};
use crate::{
    core::extensions::{DockLine, DockSpan},
    extensions::util::truncate_chars,
    utils::glyphs::{DEF_NESTED, DEF_TOOL},
};

/// widget 里显示的已终结代理上限（运行中/排队不受限）。
const MAX_FINISHED_LINES: usize = 3;

/// widget 里显示的总行数上限（含运行中；超出按"运行中优先、非终结优先"截断）。
const MAX_AGENT_LINES: usize = 6;

/// 内置色名到十六进制色值的映射表，供类型 `color` 字段解析为 badge 颜色。
const NAMED_AGENT_COLORS: &[(&str, &str)] = &[
    ("red", "#DC2626"),
    ("blue", "#6A9BCC"),
    ("green", "#16A34A"),
    ("yellow", "#CA8A04"),
    ("purple", "#827DBD"),
    ("orange", "#D97757"),
    ("pink", "#C46686"),
    ("cyan", "#0891B2"),
    ("amber", "#F59E0B"),
    ("teal", "#008080"),
    ("indigo", "#6366F1"),
    ("gold", "#EAB308"),
    ("neon-green", "#10B981"),
    ("neon-cyan", "#06B6D4"),
    ("metallic-blue", "#3B82F6"),
    ("violet", "#8B5CF6"),
    ("rose", "#F43F5E"),
    ("lime", "#84CC16"),
    ("gray", "#6B7280"),
    ("grey", "#6B7280"),
    ("fuchsia", "#D946EF"),
    ("slate", "#64748B"),
    ("navy", "#1E3A8A"),
];

/// 色名 / `#RRGGBB` → 规范化 `#RRGGBB`（无法识别时 None）。
///
/// 与上游 `resolveAgentColor` 同义：先查色名表，再要求六个十六进制位。
pub(super) fn resolve_agent_color(value: Option<&str>) -> Option<String> {
    let raw = value?.trim().to_ascii_lowercase();
    if raw.is_empty() {
        return None;
    }

    let resolved = NAMED_AGENT_COLORS
        .iter()
        .find(|(name, _)| *name == raw)
        .map(|(_, hex)| (*hex).to_string())
        .unwrap_or(raw);
    let hex = resolved.strip_prefix('#')?;

    (hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| format!("#{}", hex.to_ascii_uppercase()))
}

/// 状态图标（运行中用 spinner 帧，因此每帧渲染都会推进）。
pub(super) fn status_icon(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Running => spinner_frame(),
        other => other.icon(),
    }
}

/// 状态图标配色（主题键 + 缺省色）。
fn status_color(status: AgentStatus) -> (&'static str, &'static str) {
    match status {
        AgentStatus::Running => ("accent", "#8abeb7"),
        AgentStatus::Completed | AgentStatus::Steered => ("success", "#b5bd68"),
        AgentStatus::Queued => ("muted", "#999999"),
        AgentStatus::Aborted | AgentStatus::Error => ("error", "#cc6666"),
        AgentStatus::Stopped => ("warning", "#f0c674"),
    }
}

/// 是否按 widget 配置展示该记录。
///
/// `pub(super)`：派发路径（[`super::manager::dispatch_with_id`]）用它判断
/// "这条记录会不会出现在 dock 里"，从而决定是否自动打开面板。
pub(super) fn visible(rec: &AgentRecord, mode: WidgetMode) -> bool {
    match mode {
        WidgetMode::Off => false,
        WidgetMode::All => true,
        WidgetMode::Background => rec.background,
    }
}

/// 生成 widget 行（无可见记录时返回空 = dock 不占位）。
///
/// `records` 应为 [`super::manager::list`] 的顺序（运行中优先）。
/// `queued` 是后台队列长度（用于标题行计数，可与 records 里的 `queued` 状态去重）。
pub(super) fn lines(records: &[AgentRecord], queued: usize, cfg: &SubagentConfig) -> Vec<DockLine> {
    if cfg.widget == WidgetMode::Off {
        return Vec::new();
    }

    let visible_records: Vec<&AgentRecord> =
        records.iter().filter(|r| visible(r, cfg.widget)).collect();
    if visible_records.is_empty() && queued == 0 {
        return Vec::new();
    }

    let running = visible_records
        .iter()
        .filter(|r| r.status == AgentStatus::Running)
        .count();
    let done = visible_records
        .iter()
        .filter(|r| r.status.is_terminal())
        .count();

    let mut out = vec![header(running, queued, done, cfg)];
    let mut shown = 0usize;
    let mut finished_shown = 0usize;

    for rec in visible_records {
        if shown >= MAX_AGENT_LINES {
            break;
        }

        if rec.status.is_terminal() {
            if finished_shown >= MAX_FINISHED_LINES {
                continue;
            }
            finished_shown += 1;
        }
        out.push(agent_line(rec, cfg));
        shown += 1;
    }
    out
}

/// 标题行：`Sub-agents · 2 running · 1 queued · 3 done`
fn header(running: usize, queued: usize, done: usize, cfg: &SubagentConfig) -> DockLine {
    let mut spans = vec![DockSpan::new("accent", "#8abeb7", "Sub-agents")];
    let mut parts: Vec<String> = Vec::new();
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if queued > 0 {
        parts.push(format!("{queued} queued"));
    }
    if done > 0 && cfg.widget != WidgetMode::Background {
        parts.push(format!("{done} done"));
    }
    if !parts.is_empty() {
        spans.push(DockSpan::new(
            "dim",
            "#666666",
            format!(" · {}", parts.join(" · ")),
        ));
    }
    spans
}

/// 单个代理一行：`⠹ Explore (a1b2c3d4) read · 3 turns · 12.1k tok · 8.4s`
fn agent_line(rec: &AgentRecord, cfg: &SubagentConfig) -> DockLine {
    let (key, fallback) = status_color(rec.status);
    let mut spans = vec![DockSpan::new(
        key,
        fallback,
        format!("{} ", status_icon(rec.status)),
    )];

    // badge：类型名按配置色着色；无 color 时跟随主题 accent。
    // 有 `@handle` 时前置（便于用户照着 mention）
    let badge = {
        let base = match &rec.name {
            Some(n) => format!("{} ({n})", rec.display_name),
            None => rec.display_name.clone(),
        };
        let base = match rec.handle.as_deref() {
            Some(h) => format!("@{h} {base}"),
            None => base,
        };

        // 嵌套子代理缩进一级（`↳`）：dock 里能一眼看出谁派的谁
        if rec.depth > 0 {
            format!(
                "{}{DEF_NESTED} {base}",
                "  ".repeat(rec.depth.saturating_sub(1) as usize)
            )
        } else {
            base
        }
    };

    match resolve_agent_color(rec.color.as_deref()) {
        Some(hex) => spans.push(DockSpan::hex(hex, badge)),
        None => spans.push(DockSpan::new("accent", "#8abeb7", badge)),
    }
    spans.push(DockSpan::new("dim", "#666666", format!(" {}", rec.id)));

    // 活动：运行中优先显示当前工具，其次显示任务描述
    let activity = match (&rec.activity, rec.status) {
        (Some(tool), AgentStatus::Running) => format!(" {} {tool}", DEF_TOOL),
        _ => {
            let desc = truncate_chars(&rec.description, 40);
            if desc.is_empty() {
                String::new()
            } else {
                format!(" {desc}")
            }
        }
    };
    if !activity.is_empty() {
        spans.push(DockSpan::new("text", "#d0d0d0", activity));
    }

    // 统计：turns / tokens / 成本 / 耗时；非正常结局补状态词
    let mut stats: Vec<String> = Vec::new();
    if rec.status != AgentStatus::Completed {
        stats.push(rec.status.as_str().to_string());
    }
    stats.push(format!("{}t", rec.turns));

    if cfg.show_tokens {
        stats.push(format!(
            "{} tok",
            notify::format_tokens(rec.usage.display_total())
        ));
    }

    if cfg.show_model
        && let Some(m) = rec.model.as_deref()
    {
        stats.push(m.to_string());
    }

    if cfg.show_cost && rec.usage.cost > 0.0 {
        stats.push(format!("${:.4}", rec.usage.cost));
    }

    stats.push(format!("{:.1}s", rec.duration_ms() as f64 / 1000.0));
    spans.push(DockSpan::new(
        "muted",
        "#999999",
        format!(" · {}", stats.join(" · ")),
    ));

    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::subagent::types::UsageTotals;
    use crate::utils::time::now_ms;

    fn rec(id: &str, status: AgentStatus) -> AgentRecord {
        AgentRecord {
            id: id.to_string(),
            name: None,
            handle: None,
            alias: None,
            agent_type: "Explore".to_string(),
            display_name: "Explore".to_string(),
            description: "Find the call sites".to_string(),
            status,
            background: true,
            turns: 3,
            tool_uses: 1,
            usage: UsageTotals {
                input: 1000,
                output: 200,
                cache_read: 0,
                cache_write: 0,
                cost: 0.0,
            },
            started_ms: now_ms().saturating_sub(3400),
            ended_ms: status.is_terminal().then(now_ms),
            result: None,
            transcript: Vec::new(),
            session_path: None,
            output_path: None,
            max_turns: None,
            color: None,
            activity: None,
            steer: None,
            abort: None,
            stop_requested: false,
            worktree: None,
            worktree_result: None,
            parent_agent_id: None,
            depth: 0,
            workflow_owned: false,
            model: None,
            compaction_count: 0,
            consumed: false,
            done_rx: None,
        }
    }

    fn flat(line: &DockLine) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn named_and_hex_colors_resolve_like_upstream() {
        assert_eq!(resolve_agent_color(Some("red")).as_deref(), Some("#DC2626"));
        assert_eq!(
            resolve_agent_color(Some("  Metallic-Blue ")).as_deref(),
            Some("#3B82F6"),
            "色名大小写不敏感"
        );
        assert_eq!(
            resolve_agent_color(Some("#a1b2c3")).as_deref(),
            Some("#A1B2C3"),
            "十六进制规范化大写"
        );
        assert_eq!(resolve_agent_color(Some("#abc")), None, "三位简写不支持");
        assert_eq!(resolve_agent_color(Some("chartreuse")), None);
        assert_eq!(resolve_agent_color(Some("")), None);
        assert_eq!(resolve_agent_color(None), None);
    }

    #[test]
    fn off_and_empty_hide_the_widget() {
        let cfg = SubagentConfig {
            widget: WidgetMode::Off,
            ..Default::default()
        };
        assert!(lines(&[rec("a", AgentStatus::Running)], 0, &cfg).is_empty());

        // 无记录无排队 → 不占位
        let cfg = SubagentConfig::default();
        assert!(lines(&[], 0, &cfg).is_empty());
        // 有排队但记录为空 → 仍显示标题行
        assert_eq!(lines(&[], 1, &cfg).len(), 1);
    }

    #[test]
    fn background_mode_filters_foreground_records() {
        let mut bg = rec("b", AgentStatus::Running);
        bg.background = true;
        let mut fg = rec("f", AgentStatus::Running);
        fg.background = false;

        let mut cfg = SubagentConfig {
            widget: WidgetMode::Background,
            ..Default::default()
        };
        let out = lines(&[bg.clone(), fg.clone()], 0, &cfg);
        assert_eq!(out.len(), 2, "标题行 + 后台代理");
        assert!(flat(&out[1]).contains("b"));
        assert!(!out.iter().any(|l| flat(l).contains(" f ")));

        cfg.widget = WidgetMode::All;
        assert_eq!(lines(&[bg, fg], 0, &cfg).len(), 3);
    }

    #[test]
    fn running_line_shows_activity_and_stats_respect_config() {
        let mut r = rec("a1b2c3d4", AgentStatus::Running);
        r.activity = Some("grep".to_string());
        r.color = Some("purple".to_string());

        let mut cfg = SubagentConfig::default();
        let out = lines(&[r.clone()], 0, &cfg);
        let text = flat(&out[1]);
        assert!(text.contains("a1b2c3d4"), "{text}");
        assert!(text.contains("Explore"), "{text}");
        assert!(text.contains("grep"), "运行中显示当前工具: {text}");
        assert!(text.contains("3t"), "{text}");
        assert!(text.contains("tok"), "{text}");
        assert!(!text.contains("$"), "默认不显示成本: {text}");

        // badge 段带颜色（hex 着色）
        let badge = out[1]
            .iter()
            .find(|s| s.text.contains("Explore"))
            .expect("badge span");
        assert_eq!(badge.fallback.as_ref(), "#827DBD");

        // show_tokens = off 时不显示用量
        cfg.show_tokens = false;
        let text = flat(&lines(&[r.clone()], 0, &cfg)[1]);
        assert!(!text.contains("tok"), "{text}");

        // show_cost = on 且 provider 报了成本才显示
        cfg.show_cost = true;
        cfg.show_tokens = true;
        r.usage.cost = 0.0123;
        let text = flat(&lines(&[r], 0, &cfg)[1]);
        assert!(text.contains("$0.0123"), "{text}");
    }

    #[test]
    fn terminal_records_are_capped_and_labelled() {
        let cfg = SubagentConfig::default();
        let running = rec("run1", AgentStatus::Running);
        let finished: Vec<AgentRecord> = (0..5)
            .map(|i| rec(&format!("done{i}"), AgentStatus::Completed))
            .collect();
        let mut all = vec![running];
        all.extend(finished);
        let out = lines(&all, 0, &cfg);
        // 标题 + 1 运行 + 最多 3 个已终结
        assert_eq!(out.len(), 1 + 1 + MAX_FINISHED_LINES);
        assert!(flat(&out[0]).contains("1 running"), "{}", flat(&out[0]));
        assert!(flat(&out[0]).contains("5 done"), "{}", flat(&out[0]));

        // 非 completed 结局显示状态词
        let stopped = rec("stop1", AgentStatus::Stopped);
        let text = flat(&lines(&[stopped], 0, &cfg)[1]);
        assert!(text.contains("stopped"), "{text}");
        let text = flat(&lines(&[rec("ok1", AgentStatus::Completed)], 0, &cfg)[1]);
        assert!(!text.contains("completed"), "completed 不冗余标注: {text}");
    }

    #[test]
    fn header_counts_only_visible_records() {
        let cfg = SubagentConfig::default();
        let mut fg = rec("fg", AgentStatus::Running);
        fg.background = false;
        let out = lines(
            &[
                fg,
                rec("bg", AgentStatus::Queued),
                rec("done", AgentStatus::Completed),
            ],
            1,
            &cfg,
        );
        let head = flat(&out[0]);
        assert!(head.contains("1 running"), "{head}");
        assert!(head.contains("1 queued"), "{head}");
        assert!(head.contains("1 done"), "{head}");
        // 空任务描述不产生多余空格段
        let mut empty = rec("e", AgentStatus::Running);
        empty.description = "   ".to_string();
        let line = lines(&[empty], 0, &cfg);
        assert!(!flat(&line[1]).contains("  "), "{}", flat(&line[1]));
        assert_eq!(truncate_chars("   ", 40), "");
    }
}
