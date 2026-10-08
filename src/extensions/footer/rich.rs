//! `footer(rich)`：两行底栏。
//!
//! 行 1：git 分支（success）+ 模型（dim，reasoning 模型显示 `model:thinkingLevel`） +
//!       上下文使用率（accent）+ 缓存命中率（muted）（左）；token 速率 + in/out + 费用 +
//!       计划模式 + 交互数 + 耗时（右，各带主题色）。
//! 行 2：目录 basename（左）；工具调用计数（右，插入序）+ LLM 调用计数（最右）。
//!
//! 状态维护对齐 footer.ts：
//! - `agent_start` / `agent_end` 计时与交互计数、`turn_start` LLM 调用计数、
//!   `tool_execution_end` 工具计数、`plan-mode:changed` 计划模式标记；
//! - 会话历史就绪（启动恢复与 `/new` `/resume` `/import` `/fork` `/clone`）时按消息历史
//!   重建计数（工具 / LLM 调用 / 交互），否则切会话后计数停在上一会话、与用量行对不上；
//! - 渲染时遍历消息历史累加 assistant usage（对应 session branch 遍历）；
//! - token 速率每 1 秒采样一次，采样进度 = 已完成消息 usage 增量 + 在途流式
//!   输出（tiktoken 计数，usage 只在消息结束时落地，否则流式期间速率恒为 0）。

use super::{UsageScan, routed_suffix, scan_usage};
use crate::{
    core::{
        extensions::{
            ExtensionMode, FooterCtx, FooterExtension, FooterLine, FooterSpan, is_nested_call_event,
        },
        provider::AgentMessage,
    },
    extensions::footer::{ACCENT, DIM, MUTED, SUCCESS, truncate_spans},
    utils::display::display_width,
};
use serde_json::Value;
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

/// 底栏运行期统计：调用计数、耗时、交互数与 token 速率采样状态。
#[derive(Default)]
struct RichState {
    /// 工具调用计数
    counts: Vec<(String, usize)>,
    /// LLM 调用计数（每次向 provider 发送请求 +1）
    llm_calls: usize,
    /// 本轮 agent 启动时刻，用于计算耗时。
    task_start: Option<Instant>,
    /// agent_end 时定格的耗时
    elapsed_ms: u64,
    /// 本轮 agent 是否仍在运行（决定耗时显示实时值还是定格值）。
    running: bool,
    /// 用户交互（发消息）次数。
    interactions: usize,
    /// 是否处于计划模式。
    plan_mode: bool,
    /// 是否存在活跃目标。
    goal_active: bool,
    // token 速率采样（输出进度 + 1s 窗口）。最近一次采样的输出 token 速率（token/s）。
    /// 最近一次 ≥1s 采样窗口估算的输出速率（token/s），供底栏展示。
    tok_per_sec: f64,
    /// 上次采样点：时刻 + 当时的输出进度（token 估算）
    rate_sample: Option<(Instant, f64)>,
    /// 上次观测到的累计 tok_out（usage 累加）
    seen_tok_out: u64,
    /// 上次观测到的在途流式输出估算（token）
    seen_stream: f64,
    /// 流式缓冲清空时的累计漂移修正（实际 usage - 当时估算）
    out_drift: f64,
}

impl RichState {
    /// 清空全部累计统计（工具/LLM 计数、耗时、交互数、token 速率采样），回到初始状态。
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// 按消息历史重建“可从历史推导”的计数（工具 / LLM 调用 / 交互）。
    ///
    /// 推导口径与行 1 的用量同源（都来自 `ctx.messages`）：
    /// - 工具计数：每条顶层调用的 `toolResult` 消息按 `tool_name` 归组，按首次出现排序；
    /// - LLM 调用数：每条 `assistant` 消息 = 一次向 provider 发出的请求；
    /// - 交互数：`user` 消息条数（口径与 `/session` 的 user 计数一致，含扩展注入的消息）。
    ///
    /// 嵌套调用（`ctx.execute_tool`）不落成消息，只进调用方结果的 `details.nestedCalls`，
    /// 因此与事件口径一致：不计入工具计数。
    fn rebuild_from_history(&mut self, messages: &[AgentMessage]) {
        self.counts.clear();
        self.llm_calls = 0;
        self.interactions = 0;
        for msg in messages {
            match msg.role.as_str() {
                "user" => self.interactions += 1,
                "assistant" => self.llm_calls += 1,
                "toolResult" => {
                    let Some(name) = msg.tool_name.as_deref() else {
                        continue;
                    };
                    match self.counts.iter_mut().find(|(n, _)| n == name) {
                        Some((_, c)) => *c += 1,
                        None => self.counts.push((name.to_string(), 1)),
                    }
                }
                _ => {}
            }
        }
    }

    /// 更新输出进度并按 ≥1s 窗口采样 token 速率。
    ///
    /// 进度 = 本轮已完成消息的 usage 增量 + 在途流式输出 token（tiktoken 计数）
    /// - 漂移修正（“减号”为算式，不表示列表项）。流式期间（含 reasoning）usage 要
    ///   等 `message_end` 才落地，只看 usage 会让速率长时间停在 0；消息落地时把
    ///   「实际 usage - 计数」计入漂移，使进度在消息边界保持连续、采样无跳变尖峰。
    fn sample_token_rate(&mut self, tok_out: u64, stream_tokens: u64) {
        let est = stream_tokens as f64;

        if self.rate_sample.is_none() {
            // 本轮首个采样点：以当前 usage 为基线（历史消息不计入本轮速率）
            self.seen_tok_out = tok_out;
            self.out_drift = 0.0;
        } else if est < self.seen_stream {
            // 流式缓冲被清空（消息落地或被中断）：把已计入的估算从漂移中扣除，
            // 再加上消息落地带来的真实 usage 增量 → 进度在边界保持连续。
            // usage 未增长（中断）时只扣除估算，同样不产生回落。
            let usage_delta = tok_out.saturating_sub(self.seen_tok_out) as f64;
            self.out_drift += usage_delta - self.seen_stream;
        }
        self.seen_tok_out = tok_out;
        self.seen_stream = est;
        let progress = tok_out as f64 + est - self.out_drift;

        match self.rate_sample {
            None => self.rate_sample = Some((Instant::now(), progress)),
            Some((at, prev)) => {
                let dt = at.elapsed();
                if dt >= Duration::from_secs(1) {
                    let delta = progress - prev;
                    // 进度回退（中断清缓冲等）按 0 处理并重设基线
                    self.tok_per_sec = if delta > 0.0 {
                        delta / dt.as_secs_f64()
                    } else {
                        0.0
                    };
                    self.rate_sample = Some((Instant::now(), progress));
                }
            }
        }
    }
}

/// 两行底栏扩展：渲染 git 分支、模型、上下文用量、token 速率与费用等。
pub struct RichFooter {
    /// 运行期统计状态，扩展回调与渲染线程共享。
    state: Mutex<RichState>,
}

impl RichFooter {
    /// 构造底栏扩展：初始化运行期统计状态。
    pub fn new() -> Self {
        RichFooter {
            state: Mutex::new(RichState::default()),
        }
    }
}

impl Default for RichFooter {
    /// 等价于 [`RichFooter::new`]。
    fn default() -> Self {
        Self::new()
    }
}

/// <1000 原样，否则 1 位小数的 k
fn fmt(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// 目录 basename（去掉结尾 '/' 与空 fallback）
fn basename(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches('/');
    Path::new(trimmed)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| cwd.to_string())
}

impl FooterExtension for RichFooter {
    /// 底栏注册名，配置与面板里显示为 `footer(rich)`。
    fn name(&self) -> &str {
        "footer(rich)"
    }

    /// 面板里的功能说明（列出两行底栏各自展示的字段）。
    fn description(&self) -> &str {
        "Rich footer: branch + model + context + cache hit, token rate + in/out + cost + plan + interactions + elapsed, cwd + tool counts + LLM calls."
    }

    /// 仅在 `Minimal` 模式下提供（它本身就是底栏）。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Minimal]
    }

    /// 累计事件驱动的统计：`agent_start/end` 计时与交互数、`turn_start` LLM 调用数、
    /// `tool_execution_end` 工具计数、`plan-mode:changed` / `goal:changed` 标记，
    /// `session:new` 时整体重置（会话切换另见 [`RichFooter::on_session_switched`]）。
    fn on_agent_event(&self, event: &Value) {
        let ty = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let mut st = self.state.lock().unwrap();
        match ty {
            "agent_start" => {
                // 任务串行：仅在一轮未运行时重置计时与速率基线
                if !st.running {
                    st.task_start = Some(Instant::now());
                    st.rate_sample = None;
                    st.seen_tok_out = 0;
                    st.seen_stream = 0.0;
                    st.out_drift = 0.0;
                    st.tok_per_sec = 0.0;
                }
                st.running = true;
                st.interactions += 1;
            }
            "agent_end" => {
                if let Some(start) = st.task_start {
                    st.elapsed_ms = start.elapsed().as_millis() as u64;
                }
                st.running = false;
                st.rate_sample = None;
                st.tok_per_sec = 0.0;
            }
            // 每次 LLM 轮次 = 一次向 provider 发送请求（含工具结果回传后的下一轮）；与交互数不同：一次交互可触发多轮 LLM 调用。
            "turn_start" => st.llm_calls += 1,
            "tool_execution_end" => {
                // 嵌套调用（`ctx.execute_tool`）不计入工具使用数：它们会归到调用方结果的
                // `details.nestedCalls` 里展示，否则一次编排会被算成 N 次工具。
                if is_nested_call_event(event) {
                    return;
                }

                if let Some(name) = event.get("toolName").and_then(|v| v.as_str()) {
                    match st.counts.iter_mut().find(|(n, _)| n == name) {
                        Some((_, c)) => *c += 1,
                        None => st.counts.push((name.to_string(), 1)),
                    }
                }
            }
            "plan-mode:changed" => {
                if let Some(enabled) = event.get("enabled").and_then(|v| v.as_bool()) {
                    st.plan_mode = enabled;
                }
            }
            "goal:changed" => {
                st.goal_active = event
                    .get("status")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s == "active");
            }
            "session:new" => st.reset(),
            _ => {}
        }
    }

    /// 会话历史就绪（启动恢复与 `/new` `/resume` `/import` `/fork` `/clone`）：
    /// 按新会话的历史重建计数（口径见 [`RichState::rebuild_from_history`]），
    /// 并清掉上一会话的计时与速率采样。
    ///
    /// `plan` / `goal` 两个标记**不重置**：它们的来源（plan-mode / goal 扩展）只在数值
    /// 变化时广播，这里清零后若新会话值恰好与旧会话相同，就不会再有事件把标记补回来。
    fn on_session_switched(&self, _session_path: Option<&str>, messages: &[AgentMessage]) {
        let mut st = self.state.lock().unwrap();
        let (plan_mode, goal_active) = (st.plan_mode, st.goal_active);
        *st = RichState::default();
        st.plan_mode = plan_mode;
        st.goal_active = goal_active;
        st.rebuild_from_history(messages);
    }

    /// 渲染两行底栏：行 1 = 分支/模型/上下文/缓存命中 + 速率/in/out/费用/标记/交互/耗时，
    /// 行 2 = 目录 basename + 工具计数（插入序）+ LLM 调用数；宽度不够时截断，不溢出。
    // 累加 usage（assistant + 携带 usage 的嵌套工具结果，口径见 [`super::scan_usage`]）
    fn render(&self, ctx: &FooterCtx) -> Vec<FooterLine> {
        let mut st = self.state.lock().unwrap();
        let UsageScan {
            tok_in,
            tok_out,
            cache_read,
            cost,
            ..
        } = scan_usage(ctx.messages);

        let cache_hit_pct = if cache_read + tok_in > 0 {
            cache_read as f64 / (cache_read + tok_in) as f64 * 100.0
        } else {
            0.0
        };

        // 耗时（运行中取即时值，否则取定格值）
        let elapsed_ms = if st.running {
            st.task_start
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0)
        } else {
            st.elapsed_ms
        };
        let mins = elapsed_ms / 60_000;
        let secs = (elapsed_ms % 60_000) / 1000;
        let elapsed_str = if mins > 0 {
            format!("{}m {:02}s", mins, secs)
        } else {
            format!("{}s", secs)
        };

        // token 速率采样（运行中每 ≥1s 采样一次；含在途流式输出，reasoning 也算）
        if st.running {
            st.sample_token_rate(tok_out, ctx.streaming_output_tokens);
        }

        // 行 1 左：分支 + 模型 + 上下文% + 缓存命中%
        let mut left: Vec<FooterSpan> = Vec::new();
        if let Some(branch) = ctx.git_branch {
            left.push(FooterSpan::new(DIM.0, DIM.1, " "));
            left.push(FooterSpan::new(SUCCESS.0, SUCCESS.1, branch.to_string()));
            left.push(FooterSpan::new(DIM.0, DIM.1, " "));
        }

        let model = match (ctx.model, ctx.model_reasoning) {
            (Some(m), true) => format!(
                "{}:{}{}",
                m,
                ctx.thinking_level.unwrap_or("off"),
                routed_suffix(ctx)
            ),
            (Some(m), false) => format!("{}{}", m, routed_suffix(ctx)),
            (None, _) => String::new(),
        };

        if !model.is_empty() {
            left.push(FooterSpan::new(DIM.0, DIM.1, format!("{} ", model)));
        }
        left.push(FooterSpan::new(
            ACCENT.0,
            ACCENT.1,
            format!("{}%", ctx.context_percent.round()),
        ));
        left.push(FooterSpan::new(
            MUTED.0,
            MUTED.1,
            format!(" CH {}%", cache_hit_pct.round()),
        ));

        // 行 1 右：t/s + in/out + 费用 + 计划模式 + 交互数 + 耗时
        let mut right: Vec<FooterSpan> = Vec::new();
        right.push(FooterSpan::new(
            MUTED.0,
            MUTED.1,
            format!("{}t/s ", fmt(st.tok_per_sec.round() as u64)),
        ));
        right.push(FooterSpan::new(SUCCESS.0, SUCCESS.1, fmt(tok_in)));
        right.push(FooterSpan::new(DIM.0, DIM.1, " in "));
        right.push(FooterSpan::new(ACCENT.0, ACCENT.1, fmt(tok_out)));
        right.push(FooterSpan::new(DIM.0, DIM.1, " out"));
        right.push(FooterSpan::new(
            SUCCESS.0,
            SUCCESS.1,
            format!(" ${:.4}", cost),
        ));
        if st.goal_active {
            right.push(FooterSpan::new(MUTED.0, MUTED.1, " 🎯 "));
            right.push(FooterSpan::new(DIM.0, DIM.1, "goal"));
        }
        if st.plan_mode {
            right.push(FooterSpan::new(MUTED.0, MUTED.1, " 📋 "));
            right.push(FooterSpan::new(DIM.0, DIM.1, "plan"));
        }
        right.push(FooterSpan::new(
            MUTED.0,
            MUTED.1,
            format!(" 💬 {}", st.interactions),
        ));
        right.push(FooterSpan::new(
            MUTED.0,
            MUTED.1,
            format!(" ⌛ {}", elapsed_str),
        ));

        let lw: usize = left.iter().map(|s| display_width(&s.text)).sum();
        let rw: usize = right.iter().map(|s| display_width(&s.text)).sum();
        let pad_w = ctx.width.saturating_sub(lw + rw).max(1);
        let mut line1 = left;
        line1.push(FooterSpan::new(DIM.0, DIM.1, " ".repeat(pad_w)));
        line1.extend(right);
        let line1 = truncate_spans(line1, ctx.width);

        // 行 2：目录 basename（左）+ 工具计数（右，插入序）+ LLM 调用计数（最右）
        let l2_left: Vec<FooterSpan> = vec![
            FooterSpan::new(DIM.0, DIM.1, "📁 "),
            FooterSpan::new(ACCENT.0, ACCENT.1, basename(ctx.cwd)),
        ];
        let mut l2_right: Vec<FooterSpan> = Vec::new();
        for (name, count) in st
            .counts
            .iter()
            .chain(&[("turn".to_string(), st.llm_calls)])
        {
            l2_right.push(FooterSpan::new(SUCCESS.0, SUCCESS.1, format!(" {}", count)));
            l2_right.push(FooterSpan::new(DIM.0, DIM.1, " "));
            l2_right.push(FooterSpan::new(DIM.0, DIM.1, name.clone()));
        }
        let l2lw: usize = l2_left.iter().map(|s| display_width(&s.text)).sum();
        let l2rw: usize = l2_right.iter().map(|s| display_width(&s.text)).sum();
        let pad2_w = ctx.width.saturating_sub(l2lw + l2rw).max(1);
        let mut line2 = l2_left;
        line2.push(FooterSpan::new(DIM.0, DIM.1, " ".repeat(pad2_w)));
        line2.extend(l2_right);
        let line2 = truncate_spans(line2, ctx.width);

        vec![line1, line2]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::{AgentMessage, Cost, Usage};
    use crate::utils::display::display_width;
    use serde_json::json;
    use std::time::Duration;

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

    fn ev(ty: &str) -> Value {
        json!({ "type": ty })
    }

    /// 造一条顶层调用的工具结果消息（承载工具名，与真实会话一致）。
    fn tool_result(name: &str) -> AgentMessage {
        let mut m = AgentMessage::user_text("");
        m.role = "toolResult".to_string();
        m.tool_name = Some(name.to_string());
        m
    }

    /// 造一条 assistant 消息（每条 = 一次向 provider 发出的请求）。
    fn assistant() -> AgentMessage {
        let mut m = AgentMessage::user_text("");
        m.role = "assistant".to_string();
        m
    }

    #[test]
    fn line1_has_branch_model_context_and_cache_hit() {
        let ext = RichFooter::new();
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.usage = Some(Usage {
            input: 1000,
            output: 500,
            cache_read: 300,
            cost: Cost {
                total: 0.0123,
                ..Cost::default()
            },
            ..Usage::default()
        });
        let lines = ext.render(&ctx_with(140, vec![msg]));
        assert_eq!(lines.len(), 2);
        let l1 = line_text(&lines[0]);
        assert!(l1.contains("main"), "branch, got: {l1}");
        assert!(l1.contains("gpt-test"), "model, got: {l1}");
        assert!(l1.contains("42%"), "context pct(round), got: {l1}");
        // cacheRead/(cacheRead+tokIn) = 300/1300 = 23.08 → 23
        assert!(l1.contains("CH 23%"), "cache hit pct, got: {l1}");
        // 右侧：0t/s + in/out + cost(4位)
        assert!(l1.contains("0t/s"), "tok rate, got: {l1}");
        assert!(l1.contains("1.0k in"), "tokIn fmt, got: {l1}");
        assert!(l1.contains("500 out"), "tokOut fmt, got: {l1}");
        assert!(l1.contains("$0.0123"), "cost 4位, got: {l1}");
    }

    #[test]
    fn line2_has_cwd_basename_and_tool_counts_in_insertion_order() {
        let ext = RichFooter::new();
        ext.on_agent_event(&ev("tool_execution_end"));
        // 无 toolName 的事件忽略
        ext.on_agent_event(&json!({ "type": "tool_execution_end" }));
        ext.on_agent_event(&json!({ "type": "tool_execution_end", "toolName": "bash" }));
        ext.on_agent_event(&json!({ "type": "tool_execution_end", "toolName": "read" }));
        ext.on_agent_event(&json!({ "type": "tool_execution_end", "toolName": "bash" }));
        let lines = ext.render(&ctx_with(140, Vec::new()));
        let l2 = line_text(&lines[1]);
        assert!(l2.contains("📁 proj"), "cwd basename, got: {l2}");
        let p = l2.find("bash").expect("bash count shown");
        let r = l2.find("read").expect("read count shown");
        assert!(p < r, "插入序: bash 在 read 前, got: {l2}");
        assert!(l2.contains(" 2 "), "bash count 2, got: {l2}");
    }

    #[test]
    fn interactions_and_elapsed_from_agent_events() {
        let ext = RichFooter::new();
        ext.on_agent_event(&ev("agent_start"));
        ext.on_agent_event(&ev("agent_start"));
        std::thread::sleep(Duration::from_millis(20));
        ext.on_agent_event(&ev("agent_end"));
        let lines = ext.render(&ctx_with(140, Vec::new()));
        let l1 = line_text(&lines[0]);
        assert!(l1.contains("💬 2"), "interactions=2, got: {l1}");
        assert!(l1.contains("⌛ 0s"), "elapsed <1s, got: {l1}");
    }

    #[test]
    fn elapsed_pads_seconds_to_two_digits() {
        let ext = RichFooter::new();
        ext.state.lock().unwrap().elapsed_ms = 65_000;
        let lines = ext.render(&ctx_with(140, Vec::new()));
        let l1 = line_text(&lines[0]);
        assert!(l1.contains("⌛ 1m 05s"), "秒数补零, got: {l1}");
    }

    #[test]
    fn plan_mode_flag_shown_in_line1() {
        let ext = RichFooter::new();
        ext.on_agent_event(&json!({ "type": "plan-mode:changed", "enabled": true }));
        let lines = ext.render(&ctx_with(140, Vec::new()));
        let l1 = line_text(&lines[0]);
        assert!(l1.contains("📋"), "plan marker, got: {l1}");
        assert!(l1.contains("plan"), "plan label, got: {l1}");
    }

    #[test]
    fn goal_flag_shown_in_line1() {
        let ext = RichFooter::new();
        ext.on_agent_event(&json!({ "type": "goal:changed", "status": "active" }));
        let lines = ext.render(&ctx_with(140, Vec::new()));
        let l1 = line_text(&lines[0]);
        assert!(l1.contains("🎯"), "goal marker, got: {l1}");
        assert!(l1.contains("goal"), "goal label, got: {l1}");
    }

    #[test]
    fn token_rate_sampled_after_one_second() {
        let ext = RichFooter::new();
        {
            let mut st = ext.state.lock().unwrap();
            st.running = true;
            st.task_start = Some(Instant::now());
            // 上次采样在 3s 前，基线进度 0
            st.rate_sample = Some((Instant::now() - Duration::from_secs(3), 0.0));
        }
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.usage = Some(Usage {
            input: 100,
            output: 3000,
            ..Usage::default()
        });
        let lines = ext.render(&ctx_with(140, vec![msg]));
        let l1 = line_text(&lines[0]);
        // (3000-0)/3s = 1000 t/s → fmt = "1.0k"
        assert!(l1.contains("1.0kt/s"), "tok rate 1.0k, got: {l1}");
        // 采样点已推进
        let st = ext.state.lock().unwrap();
        assert_eq!(st.seen_tok_out, 3000);
        assert!(st.rate_sample.is_some());
    }

    #[test]
    fn token_rate_covers_inflight_reasoning_output() {
        let ext = RichFooter::new();
        {
            let mut st = ext.state.lock().unwrap();
            st.running = true;
            st.task_start = Some(Instant::now());
            st.rate_sample = Some((Instant::now() - Duration::from_secs(2), 0.0));
        }
        // 流式 output/thinking 已计 1000 token（tiktoken 计数），但 usage 尚无更新
        let mut ctx = ctx_with(140, Vec::new());
        ctx.streaming_output_tokens = 1000;
        let l1 = line_text(&ext.render(&ctx)[0]);
        assert!(l1.contains("500t/s"), "在途输出计入速率, got: {l1}");
    }

    #[test]
    fn token_rate_smooth_across_message_end() {
        let ext = RichFooter::new();
        {
            let mut st = ext.state.lock().unwrap();
            st.running = true;
            st.task_start = Some(Instant::now());
            st.rate_sample = Some((Instant::now() - Duration::from_secs(2), 0.0));
        }
        // 首帧：在途流式已计 2000 token → 2000/2s = 1.0k t/s（采样点距今 2s）
        let mut streaming = ctx_with(140, Vec::new());
        streaming.streaming_output_tokens = 2000;
        let l1 = line_text(&ext.render(&streaming)[0]);
        assert!(l1.contains("1.0kt/s"), "流式期速率, got: {l1}");

        // 消息落地：usage.output=1800，流式缓冲清空；漂移修正使进度连续
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.usage = Some(Usage {
            input: 0,
            output: 1800,
            ..Usage::default()
        });
        let landed = ctx_with(140, vec![msg]);
        let l1 = line_text(&ext.render(&landed)[0]);
        assert!(l1.contains("1.0kt/s"), "消息边界不出现尖峰, got: {l1}");
        let st = ext.state.lock().unwrap();
        assert_eq!(st.out_drift, -200.0, "漂移修正 = 1800-2000");
    }

    #[test]
    fn llm_calls_count_turn_starts_on_line2_right() {
        let ext = RichFooter::new();
        // 每次 LLM 轮次（向 provider 发送请求）累加 1
        ext.on_agent_event(&json!({ "type": "turn_start", "turnIndex": 0 }));
        ext.on_agent_event(&json!({ "type": "turn_start", "turnIndex": 1 }));
        ext.on_agent_event(&json!({ "type": "turn_start", "turnIndex": 2 }));
        // 工具调用 / 消息返回不计入
        ext.on_agent_event(&json!({ "type": "tool_execution_end", "toolName": "bash" }));
        ext.on_agent_event(&json!({ "type": "message_start" }));
        let lines = ext.render(&ctx_with(140, Vec::new()));
        let l2 = line_text(&lines[1]);
        assert!(l2.contains("3 turn"), "LLM 调用计数=3, got: {l2}");
        // 插入序工具计数在前，LLM 调用计数在最右
        let tools = l2.find("bash").expect("tool count shown");
        let calls = l2.find("turn").expect("llm call count shown");
        assert!(tools < calls, "LLM 调用计数应在最右, got: {l2}");
    }

    #[test]
    fn session_new_resets_all_accumulated_state() {
        let ext = RichFooter::new();
        // 制造各种累计状态
        ext.on_agent_event(&json!({ "type": "tool_execution_end", "toolName": "bash" }));
        ext.on_agent_event(&json!({ "type": "tool_execution_end", "toolName": "bash" }));
        ext.on_agent_event(&json!({ "type": "tool_execution_end", "toolName": "read" }));
        ext.on_agent_event(&ev("agent_start"));
        ext.on_agent_event(&ev("agent_start"));
        std::thread::sleep(Duration::from_millis(10));
        ext.on_agent_event(&ev("agent_end"));
        ext.on_agent_event(&json!({ "type": "turn_start", "turnIndex": 0 }));
        ext.on_agent_event(&json!({ "type": "plan-mode:changed", "enabled": true }));
        {
            let mut st = ext.state.lock().unwrap();
            st.tok_per_sec = 123.0;
        }
        // 重置前确实有内容
        let before = ext.render(&ctx_with(140, Vec::new()));
        assert!(line_text(&before[0]).contains("💬 2"));
        assert!(line_text(&before[1]).contains("bash"));
        assert!(line_text(&before[1]).contains("1 turn"));
        // /new → session:new 事件
        ext.on_agent_event(&json!({ "type": "session:new" }));
        let after = ext.render(&ctx_with(140, Vec::new()));
        let l1 = line_text(&after[0]);
        assert!(l1.contains("💬 0"), "交互数重置, got: {l1}");
        assert!(l1.contains("⌛ 0s"), "耗时重置, got: {l1}");
        assert!(l1.contains("0t/s"), "token 速率重置, got: {l1}");
        assert!(!l1.contains("📋"), "plan 模式重置, got: {l1}");
        assert!(
            line_text(&after[1]).contains("0 turn"),
            "LLM 调用计数重置, got: {}",
            line_text(&after[1])
        );
    }

    #[test]
    fn session_switch_rebuilds_counts_from_history() {
        let ext = RichFooter::new();
        // 上一会话的事件残留（若不重建，会一直显示在新会话上）
        ext.on_agent_event(&json!({ "type": "tool_execution_end", "toolName": "bash" }));
        ext.on_agent_event(&ev("agent_start"));
        ext.on_agent_event(&json!({ "type": "turn_start", "turnIndex": 0 }));
        ext.state.lock().unwrap().elapsed_ms = 65_000;

        // 新会话历史：1 条 user + 2 条 assistant + read×2 + bash×1
        let msgs = vec![
            AgentMessage::user_text("hi"),
            assistant(),
            tool_result("read"),
            assistant(),
            tool_result("read"),
            tool_result("bash"),
        ];
        ext.on_session_switched(Some("/tmp/s.jsonl"), &msgs);

        let lines = ext.render(&ctx_with(140, Vec::new()));
        let l1 = line_text(&lines[0]);
        let l2 = line_text(&lines[1]);
        assert!(l1.contains("💬 1"), "交互数按历史重建, got: {l1}");
        assert!(l1.contains("⌛ 0s"), "耗时回到本会话, got: {l1}");
        assert!(l2.contains(" 2 read"), "read×2, got: {l2}");
        assert!(l2.contains(" 1 bash"), "bash 只算历史里的 1 次, got: {l2}");
        assert!(
            l2.contains("2 turn"),
            "assistant 条数 = LLM 调用数, got: {l2}"
        );
    }

    #[test]
    fn session_switch_keeps_plan_and_goal_markers() {
        let ext = RichFooter::new();
        ext.on_agent_event(&json!({ "type": "plan-mode:changed", "enabled": true }));
        ext.on_agent_event(&json!({ "type": "goal:changed", "status": "active" }));
        ext.on_session_switched(None, &[]);

        // 两个标记由其它扩展去重广播，footer 清零后不会再有事件补回，故不得重置
        let l1 = line_text(&ext.render(&ctx_with(140, Vec::new()))[0]);
        assert!(l1.contains("📋 plan"), "plan 标记保留, got: {l1}");
        assert!(l1.contains("🎯 goal"), "goal 标记保留, got: {l1}");
        assert!(l1.contains("💬 0"), "历史为空则计数为 0, got: {l1}");
    }

    #[test]
    fn rebuild_skips_tool_result_without_name() {
        let ext = RichFooter::new();
        let mut unnamed = AgentMessage::user_text("");
        unnamed.role = "toolResult".to_string();
        ext.on_session_switched(None, &[unnamed, tool_result("bash")]);
        let l2 = line_text(&ext.render(&ctx_with(140, Vec::new()))[1]);
        assert!(l2.contains(" 1 bash"), "无名工具结果跳过, got: {l2}");
        assert!(!l2.contains("  0 "), "不得插出 0 计数的空工具, got: {l2}");
    }

    #[test]
    fn model_name_shows_thinking_level_when_reasoning() {
        let ext = RichFooter::new();
        // reasoning 模型：`model:level`（对齐 footer.ts）
        let mut ctx = ctx_with(140, Vec::new());
        ctx.model_reasoning = true;
        ctx.thinking_level = Some("high");
        let l1 = line_text(&ext.render(&ctx)[0]);
        assert!(l1.contains("gpt-test:high"), "model:level, got: {l1}");
        // level 缺省时按 off
        let mut ctx2 = ctx_with(140, Vec::new());
        ctx2.model_reasoning = true;
        ctx2.thinking_level = None;
        let l1b = line_text(&ext.render(&ctx2)[0]);
        assert!(l1b.contains("gpt-test:off"), "缺省 off, got: {l1b}");
        // 非 reasoning 模型：只显示 id
        let l1c = line_text(&ext.render(&ctx_with(140, Vec::new()))[0]);
        assert!(l1c.contains("gpt-test "), "仅 id, got: {l1c}");
        assert!(!l1c.contains("gpt-test:"), "不得带 level, got: {l1c}");
    }

    #[test]
    fn no_branch_and_no_model_fallbacks() {
        let ext = RichFooter::new();
        let mut ctx = ctx_with(140, Vec::new());
        ctx.git_branch = None;
        ctx.model = None;
        let lines = ext.render(&ctx);
        let l1 = line_text(&lines[0]);
        assert!(!l1.contains("main"), "无分支不显示, got: {l1}");
        assert!(
            !l1.contains("no-model"),
            "无可用模型时不得显示占位模型名, got: {l1}"
        );
    }

    #[test]
    fn narrow_width_truncates_without_panic() {
        let ext = RichFooter::new();
        ext.on_agent_event(&json!({ "type": "tool_execution_end", "toolName": "bash" }));
        ext.on_agent_event(&ev("agent_start"));
        for w in [0usize, 1, 10, 40] {
            let ctx = ctx_with(w, Vec::new());
            let lines = ext.render(&ctx);
            assert_eq!(lines.len(), 2);
            for line in &lines {
                let line_w: usize = line.iter().map(|sp| display_width(&sp.text)).sum();
                assert!(
                    line_w <= w,
                    "width {} exceeds {}: {:?}",
                    line_w,
                    w,
                    line_text(line)
                );
            }
        }
    }
}
