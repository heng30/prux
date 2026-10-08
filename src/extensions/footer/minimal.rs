//! `footer(minimal)`：极简单行底栏，只显示 normal footer 的第二行
//! （模型所在行：token/缓存/费用/上下文统计 + 模型名）。
//!
//! 渲染复用 [`crate::extensions::footer::stats_line`]，无事件状态。

use crate::{
    core::extensions::{ExtensionMode, FooterCtx, FooterExtension, FooterLine},
    extensions::footer::stats_line,
};

/// 极简单行底栏扩展，仅渲染 normal 底栏的模型统计行。
pub struct MinimalFooter;

impl MinimalFooter {
    /// 创建极简底栏（无状态，构造即用）。
    pub fn new() -> Self {
        MinimalFooter
    }
}

impl Default for MinimalFooter {
    /// 等价于 [`MinimalFooter::new`]。
    fn default() -> Self {
        Self::new()
    }
}

impl FooterExtension for MinimalFooter {
    /// 扩展名，固定为 `footer(minimal)`。
    fn name(&self) -> &str {
        "footer(minimal)"
    }

    /// 人类可读的扩展说明（英文，用于扩展列表）。
    fn description(&self) -> &str {
        "Single-line footer: the model line from footer(normal) (token/cost/context stats + model)."
    }

    /// 仅在 Minimal 模式启用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Minimal]
    }

    /// 只渲染 normal 底栏的模型统计行（单行）。
    fn render(&self, ctx: &FooterCtx) -> Vec<FooterLine> {
        vec![stats_line(ctx)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::{AgentMessage, Cost, Usage};
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
    fn renders_only_the_model_line() {
        let ext = MinimalFooter::new();
        let lines = ext.render(&ctx_with(120, Vec::new()));
        assert_eq!(lines.len(), 1, "minimal 只渲染一行");
        let l = line_text(&lines[0]);
        // 第二行内容：stats + 模型
        assert!(l.contains("42.0%/200k"), "context pct/window, got: {l}");
        assert!(l.contains("gpt-test"), "model, got: {l}");
        // 不包含 normal 第一行的 cwd/分支
        assert!(!l.contains("proj"), "不应含 cwd, got: {l}");
    }

    #[test]
    fn usage_stats_rendered() {
        let ext = MinimalFooter::new();
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.usage = Some(Usage {
            input: 1200,
            output: 800,
            cache_read: 300,
            cost: Cost {
                total: 0.0123,
                ..Cost::default()
            },
            ..Usage::default()
        });
        let lines = ext.render(&ctx_with(120, vec![msg]));
        let l = line_text(&lines[0]);
        assert!(l.contains("↑1.2k"), "tokIn, got: {l}");
        assert!(l.contains("$0.012"), "cost, got: {l}");
    }

    #[test]
    fn narrow_width_truncates_without_panic() {
        let ext = MinimalFooter::new();
        for w in [0usize, 1, 10, 40] {
            let lines = ext.render(&ctx_with(w, Vec::new()));
            assert_eq!(lines.len(), 1);
            let line_w: usize = lines[0].iter().map(|sp| display_width(&sp.text)).sum();
            assert!(
                line_w <= w,
                "width {} exceeds {}: {:?}",
                line_w,
                w,
                line_text(&lines[0])
            );
        }
    }
}
