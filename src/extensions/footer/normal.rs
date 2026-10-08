//! `footer(normal)`：默认两行底栏。
//!
//! 行 1：工作目录（~ 化）+ git 分支（dim）；
//! 行 2：token in/out + 缓存读/写 + 缓存命中率 + 费用 + 上下文使用率（左），
//!       模型名右对齐（右，reasoning 模型显示 `model:thinkingLevel`，对齐 rich footer）。
//!
//! 渲染逻辑为纯函数（状态在 FooterCtx 中），不需要订阅 agent 事件。

use crate::{
    core::extensions::{ExtensionMode, FooterCtx, FooterExtension, FooterLine},
    extensions::footer::{pwd_line, stats_line},
};

/// 默认两行底栏扩展：首行工作目录与 git 分支，次行 token、费用与上下文统计。
pub struct NormalFooter;

impl NormalFooter {
    /// 创建默认两行底栏（无状态，构造即用）。
    pub fn new() -> Self {
        NormalFooter
    }
}

impl Default for NormalFooter {
    /// 等价于 [`NormalFooter::new`]。
    fn default() -> Self {
        Self::new()
    }
}

impl FooterExtension for NormalFooter {
    /// 扩展名，固定为 `footer(normal)`。
    fn name(&self) -> &str {
        "footer(normal)"
    }

    /// 人类可读的扩展说明（英文，用于扩展列表）。
    fn description(&self) -> &str {
        "Two-line footer: line 1 cwd + git branch, line 2 token/cost/context stats + model."
    }

    /// 仅在 Minimal 模式启用（normal 底栏即默认渲染）。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Minimal]
    }

    /// 依次返回工作目录行与模型统计行。
    fn render(&self, ctx: &FooterCtx) -> Vec<FooterLine> {
        vec![pwd_line(ctx), stats_line(ctx)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::{AgentMessage, Cost, Usage};
    use crate::extensions::footer::{format_cwd, format_tokens};
    use crate::utils::display::display_width;

    fn line_text(line: &FooterLine) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    fn ctx_with(width: usize, messages: Vec<AgentMessage>) -> FooterCtx<'static> {
        FooterCtx {
            width,
            cwd: "/tmp/proj",
            model: Some("gpt-test"),
            model_reasoning: false,
            thinking_level: None,
            routed_model: None,
            routed_thinking_level: None,
            git_branch: Some("main"),
            context_percent: 42.0,
            context_window: 200_000,
            busy: false,
            streaming_output_tokens: 0,
            messages: Box::leak(messages.into_boxed_slice()),
        }
    }

    #[test]
    fn renders_two_lines_branch_model_and_context() {
        let ext = NormalFooter::new();
        let lines = ext.render(&ctx_with(120, Vec::new()));
        assert_eq!(lines.len(), 2);
        // 行 1：pwd (branch)（对齐 pi footer.js）
        let l1 = line_text(&lines[0]);
        assert!(l1.contains("proj"), "cwd shown, got: {l1}");
        assert!(l1.contains("main"), "branch shown, got: {l1}");
        // 行 2：stats（含 context%）+ 右侧模型
        let l2 = line_text(&lines[1]);
        assert!(l2.contains("42.0%/200k"), "context pct/window, got: {l2}");
        assert!(l2.contains("gpt-test"), "model right-aligned, got: {l2}");
    }

    #[test]
    fn usage_stats_from_messages() {
        let ext = NormalFooter::new();
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.usage = Some(Usage {
            input: 1200,
            output: 800,
            cache_read: 300,
            cache_write: 50,
            cost: Cost {
                total: 0.0123,
                ..Cost::default()
            },
            ..Usage::default()
        });
        let lines = ext.render(&ctx_with(120, vec![msg]));
        let l2 = line_text(&lines[1]);
        assert!(l2.contains("↑1.2k"), "tokIn fmt, got: {l2}");
        assert!(l2.contains("↓800"), "tokOut, got: {l2}");
        assert!(l2.contains("R300"), "cache read, got: {l2}");
        assert!(l2.contains("W50"), "cache write, got: {l2}");
        assert!(l2.contains("$0.012"), "cost(3位), got: {l2}");
    }

    /// pi：扩展经 `ctx.execute_tool()` 嵌套调用（如子代理 `reportUsage`）的开销挂在
    /// 调用方的 toolResult 上，必须计入会话成本（否则底栏漏掉这部分花费）。
    #[test]
    fn tool_result_usage_counts_toward_session_totals() {
        let ext = NormalFooter::new();
        let mut assistant = AgentMessage::user_text("");
        assistant.role = "assistant".to_string();
        assistant.usage = Some(Usage {
            input: 1000,
            output: 500,
            cost: Cost {
                total: 0.01,
                ..Cost::default()
            },
            ..Usage::default()
        });
        let mut nested = AgentMessage::user_text("subagent done");
        nested.role = "toolResult".to_string();
        nested.tool_call_id = Some("t1".into());
        nested.usage = Some(Usage {
            input: 2000,
            output: 250,
            cost: Cost {
                total: 0.02,
                ..Cost::default()
            },
            ..Usage::default()
        });
        let lines = ext.render(&ctx_with(120, vec![assistant, nested]));
        let l2 = line_text(&lines[1]);
        assert!(l2.contains("↑3.0k"), "in 应含嵌套工具开销, got: {l2}");
        assert!(l2.contains("↓750"), "out 应含嵌套工具开销, got: {l2}");
        assert!(l2.contains("$0.030"), "cost 应含嵌套工具开销, got: {l2}");
    }

    /// 摘要类消息（会话投影带上条目 usage）也要计入底栏成本：
    /// `ctx.messages` 是 footer 唯一的数据源，投影丢了 usage 底栏就永远看不到它
    #[test]
    fn summary_usage_counts_toward_session_totals() {
        let ext = NormalFooter::new();
        let mut summary = AgentMessage::user_text("compacted");
        summary.role = "compactionSummary".to_string();
        summary.usage = Some(Usage {
            input: 4000,
            output: 200,
            cost: Cost {
                total: 0.04,
                ..Cost::default()
            },
            ..Usage::default()
        });
        let lines = ext.render(&ctx_with(120, vec![summary]));
        let l2 = line_text(&lines[1]);
        assert!(l2.contains("↑4.0k"), "摘要输入 token, got: {l2}");
        assert!(l2.contains("↓200"), "摘要输出 token, got: {l2}");
        assert!(l2.contains("$0.040"), "摘要成本, got: {l2}");
    }

    #[test]
    fn narrow_width_truncates_without_panic() {
        let ext = NormalFooter::new();
        let lines = ext.render(&ctx_with(10, Vec::new()));
        for line in &lines {
            let w: usize = line.iter().map(|sp| display_width(&sp.text)).sum();
            assert!(
                w <= 10,
                "line width {} exceeds 10: {:?}",
                w,
                line_text(line)
            );
        }
    }

    #[test]
    fn format_tokens_and_cwd_match_pi() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1200), "1.2k");
        assert_eq!(format_tokens(12_000), "12k");
        assert_eq!(format_tokens(1_200_000), "1.2M");
        assert_eq!(format_tokens(12_000_000), "12M");
        // ~ 化
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert_eq!(format_cwd(&format!("{}/repo", home)), "~/repo");
            assert_eq!(format_cwd("/elsewhere"), "/elsewhere");
        }
    }

    #[test]
    fn context_percent_color_thresholds() {
        let ext = NormalFooter::new();
        let mut ctx = ctx_with(120, Vec::new());
        ctx.context_percent = 95.0;
        let lines = ext.render(&ctx);
        assert_eq!(lines[1][0].key, "error", ">90 用 error 色");
        ctx.context_percent = 80.0;
        let lines = ext.render(&ctx);
        assert_eq!(lines[1][0].key, "warning", ">70 用 warning 色");
    }

    #[test]
    fn model_shows_thinking_level_when_reasoning() {
        let ext = NormalFooter::new();
        // reasoning 模型：`model:level`（对齐 rich footer）
        let mut ctx = ctx_with(120, Vec::new());
        ctx.model_reasoning = true;
        ctx.thinking_level = Some("high");
        let l2 = line_text(&ext.render(&ctx)[1]);
        assert!(l2.contains("gpt-test:high"), "model:level, got: {l2}");
        // level 缺省时按 off
        let mut ctx2 = ctx_with(120, Vec::new());
        ctx2.model_reasoning = true;
        ctx2.thinking_level = None;
        let l2b = line_text(&ext.render(&ctx2)[1]);
        assert!(l2b.contains("gpt-test:off"), "缺省 off, got: {l2b}");
        // 非 reasoning 模型：只显示 id，不带 level
        let l2c = line_text(&ext.render(&ctx_with(120, Vec::new()))[1]);
        assert!(l2c.contains("gpt-test"), "仅 id, got: {l2c}");
        assert!(!l2c.contains("gpt-test:"), "不得带 level, got: {l2c}");
    }

    #[test]
    fn virtual_model_shows_routed_physical_model() {
        let ext = NormalFooter::new();
        // 选中虚拟模型 + 已路由：`model:level → physical:level`
        let mut ctx = ctx_with(120, Vec::new());
        ctx.model = Some("auto");
        ctx.model_reasoning = true;
        ctx.thinking_level = Some("high");
        ctx.routed_model = Some("gpt-real");
        ctx.routed_thinking_level = Some("medium");
        let l2 = line_text(&ext.render(&ctx)[1]);
        assert!(l2.contains("auto:high → gpt-real:medium"), "got: {l2}");

        // 未路由（非虚拟模型）不显示箭头
        let l2b = line_text(&ext.render(&ctx_with(120, Vec::new()))[1]);
        assert!(!l2b.contains("→"), "got: {l2b}");
    }
}
