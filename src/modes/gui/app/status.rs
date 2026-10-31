//! 状态栏：左侧消息数 / 上下文占用率 / 缓存命中率，右侧 in/out / 费用 / 速率 /
//! 计划与目标标记 / 交互数 / 耗时。
//!
//! 口径对齐 TUI 的 `footer(rich)` 第一行（分支、模型、thinking 已在输入框里，不重复）。
//! 这里同时维护驱动它的本轮计时、在途流式 token 与速率采样：
//! agent 内核事件经 [`GuiApp::sample_token_rate`] 打采样点，忙碌期的 1s 重绘由
//! [`GuiApp::start_status_tick`] 保证。

use super::GuiApp;
use crate::{
    core::provider::{AgentMessage, calculate_context_tokens, estimate_tokens, usable_usage},
    extensions::{footer::format_tokens, goal, plan_mode},
    modes::gui::{bridge::UsageTotals, transcript::Block},
    utils::tokens::{self, TokenMeter},
};
use gpui::Context;
use serde_json::Value;
use std::time::{Duration, Instant};

/// 闪烁与重绘的节拍（毫秒）：一次 tick 翻转一次闪烁相位。
pub(super) const BLINK_INTERVAL_MS: u64 = 500;

/// 忙碌圆点「暗」相位的不透明度：明暗差看得出来，又不至于闪得刺眼。
pub(super) const BLINK_DIM_OPACITY: f32 = 0.25;

/// 状态栏的本轮用量：本轮耗时、在途流式输出 token 与输出速率采样。
#[derive(Default)]
pub(super) struct UsageMeter {
    /// 本轮开始时刻；None 表示当前没有在跑的轮次（耗时取 `frozen_ms`）。
    started: Option<Instant>,
    /// 上一轮结算后的耗时（毫秒）；未跑过任何一轮时为 0。
    frozen_ms: u64,
    /// 在途流式输出 token（tiktoken 计数，含 thinking）。
    stream_tokens: u64,
    /// 流式 token 计数器；首次流式时才建（解析词表要几十毫秒）。
    counter: Option<TokenMeter>,
    /// 上次速率采样：(采样时刻, 当时的进度 token)。
    last_sample: Option<(Instant, f64)>,
    /// 进度的单调上界：消息落地时 usage 增长与流式清零不会精确抵消，取 max 防回退。
    peak: f64,
    /// 最近一个 ≥1s 窗口的输出速率（token/s）。
    pub(super) tok_per_sec: f64,
}

impl UsageMeter {
    /// 按 agent 内核事件推进计时与流式计数（`model` 用于选 BPE 词表）。
    pub(super) fn observe(&mut self, event: &Value, model: Option<&str>) {
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "agent_start" => {
                // 任务串行：本轮未结束时（如子代理/嵌套）不重置计时基线
                if self.started.is_none() {
                    self.started = Some(Instant::now());
                    self.last_sample = None;
                    self.peak = 0.0;
                    self.tok_per_sec = 0.0;
                }
                self.reset_stream();
            }
            "agent_end" | "agent_settled" => {
                if let Some(start) = self.started.take() {
                    self.frozen_ms = start.elapsed().as_millis() as u64;
                }
                self.last_sample = None;
                self.tok_per_sec = 0.0;
                self.reset_stream();
            }
            "message_start" => self.reset_stream(),
            "message_update" => {
                let delta = event
                    .get("assistantMessageEvent")
                    .and_then(|e| e.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !delta.is_empty() {
                    let counter = self
                        .counter
                        .get_or_insert_with(|| TokenMeter::new(tokens::encoding_for(model)));
                    self.stream_tokens = counter.feed(delta);
                }
            }
            _ => {}
        }
    }

    /// 采一个速率点：`usage_out` 为快照里累计的输出 token。
    ///
    /// shortcut: 用进度单调化代替 TUI 的漂移修正（按 max 保住上界），
    /// 消息边界最多差一次「流式估算 - 实际 usage」；偏差变得明显时再补漂移账。
    pub(super) fn sample_rate(&mut self, usage_out: f64) {
        let progress = (usage_out + self.stream_tokens as f64).max(self.peak);
        self.peak = progress;
        match self.last_sample {
            None => self.last_sample = Some((Instant::now(), progress)),
            Some((at, prev)) => {
                let dt = at.elapsed();
                if dt >= Duration::from_secs(1) {
                    let delta = progress - prev;
                    self.tok_per_sec = if delta > 0.0 {
                        delta / dt.as_secs_f64()
                    } else {
                        0.0
                    };
                    self.last_sample = Some((Instant::now(), progress));
                }
            }
        }
    }

    /// 本轮耗时文案（运行中实时、否则定格；秒数补零）。
    fn elapsed_text(&self) -> String {
        let ms = match self.started {
            Some(at) => at.elapsed().as_millis() as u64,
            None => self.frozen_ms,
        };

        let (mins, secs) = (ms / 60_000, (ms % 60_000) / 1000);
        if mins > 0 {
            format!("{mins}m {secs:02}s")
        } else {
            format!("{secs}s")
        }
    }

    /// 清掉在途流式计数（新消息 / 消息落地 / 中断）。
    fn reset_stream(&mut self) {
        self.stream_tokens = 0;
        if let Some(counter) = &mut self.counter {
            counter.reset();
        }
    }
}

/// 把一条消息计入会话用量：携带 usage 的累加计费（工具结果上的嵌套调用开销也挂在自己身上）；
/// 可用 usage 的 assistant 同时重置上下文锚点，其余消息只按文本估算记入锚点之后。
fn apply_message_usage(usage: &mut UsageTotals, message: &AgentMessage) {
    if let Some(u) = &message.usage {
        usage.input += u64::from(u.input);
        usage.output += u64::from(u.output);
        usage.cache_read += u64::from(u.cache_read);
        usage.cache_write += u64::from(u.cache_write);
        usage.cost += u.cost.total;
    }

    if let Some(u) = usable_usage(message) {
        usage.context_tokens = u64::from(calculate_context_tokens(u));
    } else {
        usage.context_tokens += u64::from(estimate_tokens(message));
    }
}

impl GuiApp {
    /// 忙碌指示文案（`Working… (Esc to abort, N queued)`，不含前面的圆点）；空闲时为 None。
    /// 圆点由渲染层单独画（它才是闪的那个），文字只换品牌色。
    pub(super) fn busy_text(&self) -> Option<String> {
        self.busy
            .then(|| format!("Working… (Esc to abort, {} queued)", self.pending_count()))
    }

    /// 状态栏文案（左, 右）：左侧消息数 + 上下文占用率 + 缓存命中率，
    /// 右侧 in/out + 费用 + 速率 + 标记 + 交互数 + 耗时。
    pub(super) fn status_text(&self) -> (String, String) {
        let messages = self.snapshot.as_ref().map(|s| s.messages).unwrap_or(0);
        let mut left_parts: Vec<String> = Vec::new();

        // 扩展命令写的临时提示优先占左侧（过期即不再显示）
        if let Some((text, deadline)) = &self.status_note
            && Instant::now() < *deadline
        {
            left_parts.push(text.clone());
        }

        // 忙碌时左侧已被 Working… 占住，只省掉消息数
        if !self.busy {
            left_parts.push(format!("{messages} messages"));
        }

        let mut right = String::new();
        let u = &self.usage;
        let pct = if u.context_window > 0 {
            u.context_tokens as f64 / u.context_window as f64 * 100.0
        } else {
            0.0
        };

        left_parts.push(format!("CW {:.0}%", pct));

        let billed = u.cache_read + u.input;
        let hit = if billed > 0 {
            u.cache_read as f64 / billed as f64 * 100.0
        } else {
            0.0
        };

        left_parts.push(format!("CH {:.0}%", hit));

        right.push_str(&format!(
            "{}t/s · ",
            format_tokens(self.meter.tok_per_sec.round() as u64)
        ));

        right.push_str(&format!(
            "{} in · {} out",
            format_tokens(u.input),
            format_tokens(u.output)
        ));
        right.push_str(&format!(" · ${:.4}", u.cost));

        let mut tail = String::new();

        if goal::is_active() {
            tail.push_str(" · 🎯 goal");
        }

        if plan_mode::is_enabled() {
            tail.push_str(" · 📋 plan");
        }

        let interactions = self
            .transcript
            .blocks()
            .iter()
            .filter(|block| matches!(block, Block::User(_)))
            .count();
        tail.push_str(&format!(" · 💬 {interactions}"));
        tail.push_str(&format!(" · ⌛ {}", self.meter.elapsed_text()));

        right.push_str(&tail);
        (left_parts.join(" · "), right)
    }

    /// 按累计 output 与在途流式输出打一个 ≥1s 的速率采样点
    /// （与 TUI rich footer 同口径；事件流本身就是采样时钟）。
    pub(super) fn sample_token_rate(&mut self) {
        self.meter.sample_rate(self.usage.output as f64);
    }

    /// 按 agent 事件推进会话用量：快照只在回合结束才推来，回合中间的流式与工具执行
    /// 期间状态栏（CW / CH / in / out / 费用）靠这里走字。
    pub(super) fn observe_usage(&mut self, event: &Value) {
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_end" => {
                if let Some(message) = event
                    .get("message")
                    .and_then(|m| serde_json::from_value::<AgentMessage>(m.clone()).ok())
                {
                    apply_message_usage(&mut self.usage, &message);
                }
            }
            "turn_end" => {
                if let Some(tools) = event.get("toolResults").and_then(Value::as_array) {
                    for tool in tools {
                        if let Ok(message) = serde_json::from_value::<AgentMessage>(tool.clone()) {
                            apply_message_usage(&mut self.usage, &message);
                        }
                    }
                }
            }
            // 压缩摘要本身不带 usage：上下文规模以事件携带的估算为准
            "compaction_end" => {
                if let Some(after) = event.get("estimatedTokensAfter").and_then(Value::as_u64) {
                    self.usage.context_tokens = after;
                }
            }
            _ => {}
        }
    }

    /// 忙碌期间每半秒重绘一次：事件驱动的刷新在长时间工具调用里会断流，
    /// 状态栏的 ⌛ 耗时与忙碌指示的闪烁只有靠它才会动。
    pub(super) fn start_status_tick(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }

        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(BLINK_INTERVAL_MS))
                    .await;

                let alive = this
                    .update(cx, |app, cx| {
                        if !app.busy {
                            app.ticking = false;
                            app.blink = false;
                            return false;
                        }

                        app.blink = !app.blink;
                        app.meter.sample_rate(app.usage.output as f64);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);

                if !alive {
                    break;
                }
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// agent_start 后结算：耗时定格、流式计数清零。
    #[test]
    fn elapsed_freezes_after_round_ends() {
        let mut meter = UsageMeter::default();
        meter.observe(&json!({ "type": "agent_start" }), None);
        assert!(meter.started.is_some());
        meter.observe(&json!({ "type": "agent_settled" }), None);
        assert!(meter.started.is_none(), "结算后不再实时计时");
        assert!(meter.elapsed_text().ends_with('s'));
    }

    /// 流式增量喂进 tiktoken 计数器，≥1s 窗口后给出非零速率。
    #[test]
    fn rate_counts_streaming_tokens() {
        let mut meter = UsageMeter::default();
        meter.observe(&json!({ "type": "agent_start" }), None);
        meter.observe(
            &json!({
                "type": "message_update",
                "assistantMessageEvent": { "type": "text_delta", "delta": "hello world " }
            }),
            None,
        );
        assert_eq!(
            meter.stream_tokens, 2,
            "「hello world 」被空白闭合后计 2 token"
        );
        meter.last_sample = Some((Instant::now() - Duration::from_secs(2), 0.0));
        meter.sample_rate(0.0);
        assert!(
            (0.9..=1.1).contains(&meter.tok_per_sec),
            "2 token / 2s，got {}",
            meter.tok_per_sec
        );
    }

    /// 消息落地（流式清零 + usage 尚未入快照）不得让进度回退、速率变负。
    #[test]
    fn progress_never_regresses_when_stream_buffer_clears() {
        let mut meter = UsageMeter::default();
        meter.observe(&json!({ "type": "agent_start" }), None);
        meter.observe(
            &json!({
                "type": "message_update",
                "assistantMessageEvent": { "type": "text_delta", "delta": "hello world " }
            }),
            None,
        );
        let before = meter.stream_tokens as f64;
        meter.last_sample = Some((Instant::now() - Duration::from_secs(1), before));
        meter.peak = before;
        meter.observe(&json!({ "type": "message_start" }), None);
        meter.sample_rate(0.0);
        assert_eq!(meter.tok_per_sec, 0.0, "清零后速率归零而非负数");
        assert_eq!(meter.peak, before, "进度上界保留");
    }

    /// 回合中间的 assistant 落地就把 in/out/费用与上下文锚点推上去；
    /// 其后的工具结果只累加尾部估算，上下文不因重置锚点而回退。
    #[test]
    fn usage_moves_before_snapshot_arrives() {
        let mut usage = UsageTotals::default();
        let mut assistant = AgentMessage::user_text("");
        assistant.role = "assistant".to_string();
        assistant.usage = Some(crate::core::provider::Usage {
            input: 1000,
            output: 200,
            cache_read: 300,
            cost: crate::core::provider::Cost {
                total: 0.5,
                ..Default::default()
            },
            ..Default::default()
        });
        apply_message_usage(&mut usage, &assistant);
        assert_eq!(usage.input, 1000);
        assert_eq!(usage.output, 200);
        assert!((usage.cost - 0.5).abs() < 1e-9);
        let anchored = usage.context_tokens;
        assert_eq!(anchored, 1500, "锚点 = 该次请求的上下文规模");

        let mut tool = AgentMessage::user_text("tool output text");
        tool.role = "toolResult".to_string();
        apply_message_usage(&mut usage, &tool);
        assert!(
            usage.context_tokens > anchored,
            "锚点之后的消息计入尾部估算: {}",
            usage.context_tokens
        );
        assert_eq!(usage.output, 200, "工具结果不带 usage 时不动计费");
    }
}
