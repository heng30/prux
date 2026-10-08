//! LLM 自动重试的输出区系统提示（`auto_retry_start` / `auto_retry_end` 事件驱动）。
//!
//! 会话层重试循环 emit 的事件经 `apply_sink_event` 分发到这里：
//! - start：向消息流插入/原地覆盖 `⠋ Retrying (n/m) in Xs... (escape to cancel)`
//! - 重试期间 UI tick 每秒刷新（spinner 帧 + 剩余秒数），同一消息位置原地覆盖
//! - 恢复：重试请求重新出流（assistant `message_start`）即视为网络恢复，
//!   **立即**移除文本区提示（不必等整轮 `auto_retry_end`）；失败原地覆盖为 ✗
//!   （失败行由下一次 start 覆盖，不堆积）
//! - 中断（Esc/Ctrl+C）：worker abort 会 drop prompt future，end 事件不会来，
//!   由 `handle_cancel_prompt` 调 [`App::cancel_retry_ui`] 收尾。
//!
//! 状态栏不显示 `Retrying...`：重试进度只在输出区提示行呈现，忙碌态保持默认
//! `Working...`（见 [`App::hide_retry_notice`] 与 `WorkingKind`）。

use crate::{
    core::provider::DEFAULT_MAX_RETRIES,
    modes::interactive::app::{App, MsgLevel, SysSpan},
    utils::{
        glyphs::{DEF_FAILED, DEF_SPINNER_BRAILLE},
        time::now_ms,
    },
};
use serde_json::Value;
use std::time::{Duration, Instant};

/// 消息区重试提示的刷新帧（braille spinner，与状态栏忙碌帧同源）
pub const RETRY_FRAMES: &[&str] = DEF_SPINNER_BRAILLE;

/// 消息区重试提示状态：auto_retry_start/end 事件驱动，记录进度与倒计时。
/// 重试期间 UI tick 每秒原地刷新提示行（spinner 帧 + 剩余秒数）。
#[derive(Debug, Clone, Copy)]
pub struct RetryUi {
    /// 当前重试序号（1-based）
    pub attempt: u32,
    /// 最大重试次数
    pub max: u32,
    /// 本次重试倒计时结束（实际发送）时刻
    pub deadline: Instant,
    /// 上次刷新提示行的时间（节流 1s）
    pub last_refresh: Instant,
    /// 系统消息时间戳（原地覆盖定位）
    pub ts: u64,
}

/// 重试提示行文本：`⠋ Retrying (1/3) in 5s... (escape to cancel)`；
/// 帧按全局秒数轮换（与渲染侧 busy spinner 同节奏），倒计时到点后省略秒数（正在发送）。
pub fn retry_line_text(rui: &RetryUi, now: Instant) -> String {
    let frame = RETRY_FRAMES[(now_ms() / 1000) as usize % RETRY_FRAMES.len()];
    let remaining = rui.deadline.saturating_duration_since(now).as_secs();
    if remaining > 0 {
        format!(
            "{} Retrying ({}/{}) in {}s... (escape to cancel)",
            frame, rui.attempt, rui.max, remaining
        )
    } else {
        format!(
            "{} Retrying ({}/{})... (escape to cancel)",
            frame, rui.attempt, rui.max
        )
    }
}

impl App {
    /// auto_retry_start：输出区插入/原地覆盖重试系统提示
    /// `⠋ Retrying (n/m) in Xs... (escape to cancel)`，重试期由 tick 每秒刷新。
    /// 若上一次重试失败行存在（retry_failed_ts），覆盖其位置避免失败/重试行堆积。
    pub(crate) fn apply_sink_auto_retry_start(&mut self, event: &Value) {
        let attempt = event.get("attempt").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
        let max = event
            .get("maxAttempts")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MAX_RETRIES as u64) as u32;
        let delay_ms = event.get("delayMs").and_then(|v| v.as_u64()).unwrap_or(0);
        let now = Instant::now();
        let ts = if let Some(prev) = self.retry_failed_ts.take() {
            prev
        } else {
            now_ms()
        };
        self.retry_ui = Some(RetryUi {
            attempt,
            max,
            deadline: now + Duration::from_millis(delay_ms),
            last_refresh: now,
            ts,
        });

        let rui = self.retry_ui.as_ref().unwrap();
        self.upsert_sys_msg(
            ts,
            vec![SysSpan::plain(&retry_line_text(rui, now))],
            MsgLevel::Warning,
        );
    }

    /// 成功恢复收尾：移除文本区重试提示并停止每秒刷新（幂等，retry_ui 为空时无操作）。
    ///
    /// 网络恢复的判定点有两个，先到先收尾：
    /// 1. 重试请求重新出流（assistant `message_start`）——无需等整轮跑完，出流即恢复；
    /// 2. 整轮结束的 `auto_retry_end(success)`（无流式事件的兜底）。
    pub(crate) fn hide_retry_notice(&mut self) {
        let Some(rui) = self.retry_ui.take() else {
            return;
        };
        self.remove_sys_msg(rui.ts);
    }

    /// auto_retry_end：停止刷新并收尾重试行。成功仅兜底移除提示（正常已在 assistant
    /// `message_start` 时由 [`App::hide_retry_notice`] 清理）；失败原地覆盖为 ✗（下次 start
    /// 覆盖失败行，不堆积）。
    pub(crate) fn apply_sink_auto_retry_end(&mut self, event: &Value) {
        let Some(rui) = self.retry_ui.take() else {
            return;
        };
        let success = event
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if success {
            self.remove_sys_msg(rui.ts);
        } else {
            self.retry_failed_ts = Some(rui.ts);
            self.upsert_sys_msg(
                rui.ts,
                vec![SysSpan::plain(&format!(
                    "{DEF_FAILED} Retry failed ({}/{})",
                    rui.attempt, rui.max
                ))],
                MsgLevel::Error,
            );
        }
    }

    /// 事件循环 tick：活跃重试提示每秒刷新一次（spinner 帧 + 剩余秒数）。
    /// 刷新经 upsert_sys_msg 原地覆盖同一消息，dirty 由其置位。
    pub(crate) fn retry_ui_tick(&mut self) {
        let Some(rui) = self.retry_ui.as_ref() else {
            return;
        };
        if rui.last_refresh.elapsed() < Duration::from_secs(1) {
            return;
        }
        let Some(mut rui) = self.retry_ui.take() else {
            return;
        };
        rui.last_refresh = Instant::now();
        let ts = rui.ts;
        self.upsert_sys_msg(
            ts,
            vec![SysSpan::plain(&retry_line_text(&rui, Instant::now()))],
            MsgLevel::Warning,
        );
        self.retry_ui = Some(rui);
    }

    /// 中断（Esc/Ctrl+C）时收尾重试提示：覆盖为已取消并停止刷新（worker abort
    /// 会 drop prompt future，`auto_retry_end` 事件不会来，必须在此清理）。
    pub(crate) fn cancel_retry_ui(&mut self) {
        let Some(rui) = self.retry_ui.take() else {
            return;
        };
        self.retry_failed_ts = None;
        self.upsert_sys_msg(
            rui.ts,
            vec![SysSpan::plain(&format!("{DEF_FAILED} Retry cancelled"))],
            MsgLevel::Error,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::{WorkingKind, sys_spans_text};

    fn start_event(attempt: u32, max: u32, delay_ms: u64) -> Value {
        serde_json::json!({
            "type": "auto_retry_start",
            "attempt": attempt,
            "maxAttempts": max,
            "delayMs": delay_ms,
            "errorMessage": "503 service unavailable"
        })
    }

    fn end_event(attempt: u32, success: bool) -> Value {
        serde_json::json!({
            "type": "auto_retry_end",
            "attempt": attempt,
            "success": success,
            "finalError": null
        })
    }

    #[test]
    fn auto_retry_start_shows_retry_system_notice() {
        // 会话层 auto_retry 事件 → 输出区系统提示 `⠋ Retrying (1/3) in ..s... (escape to cancel)`；
        // 失败→下次 start→成功全程原地覆盖，消息区不堆积多条。
        let mut app = App::new();
        app.apply_sink_auto_retry_start(&start_event(1, 3, 2000));
        assert_eq!(app.system_messages.len(), 1, "首条重试提示应插入");
        let t1 = sys_spans_text(&app.system_messages[0].1.spans);
        assert!(t1.contains("Retrying (1/3)"), "计数: {:?}", t1);
        assert!(t1.contains("in 2s"), "倒计时: {:?}", t1);
        assert!(t1.contains("(escape to cancel)"), "取消提示: {:?}", t1);
        assert_eq!(
            app.working_kind,
            WorkingKind::Working,
            "状态栏不因重试切换文案（不得出现 Retrying...）"
        );
        assert!(app.retry_ui.is_some(), "重试状态应活跃");

        // 失败：原地覆盖为 ✗，启动下一次 start 时再覆盖回 Retrying（同一位置）。
        app.apply_sink_auto_retry_end(&end_event(1, false));
        assert!(app.retry_ui.is_none(), "失败后停止刷新");
        let t2 = sys_spans_text(&app.system_messages[0].1.spans);
        assert!(t2.contains("✗ Retry failed (1/3)"), "失败提示: {:?}", t2);
        assert_eq!(app.system_messages.len(), 1, "失败提示原地覆盖不追加");

        app.apply_sink_auto_retry_start(&start_event(2, 3, 4000));
        assert_eq!(app.system_messages.len(), 1, "第二次重试覆盖失败行");
        let t3 = sys_spans_text(&app.system_messages[0].1.spans);
        assert!(t3.contains("Retrying (2/3)"), "第二次计数: {:?}", t3);
        assert!(app.retry_failed_ts.is_none(), "失败行 ts 已被消费");

        // 成功：移除重试提示（网络恢复后文本区不残留重试信息）。
        app.apply_sink_auto_retry_end(&end_event(2, true));
        assert!(
            app.system_messages.is_empty(),
            "成功后重试提示应移除而非残留"
        );
        assert!(app.retry_ui.is_none() && app.retry_failed_ts.is_none());
    }

    #[test]
    fn assistant_message_start_hides_retry_notice_before_turn_ends() {
        // 回归：会话层的 auto_retry_end 要等整轮 run_loop 跑完才发，网络恢复后
        // `⠴ Retrying (1/3)... (escape to cancel)` 会多留整个回合；因此重试请求
        // 重新出流（assistant message_start）时就要隐藏提示。

        let mut app = App::new();
        app.apply_sink_auto_retry_start(&start_event(1, 3, 0));
        assert_eq!(app.system_messages.len(), 1, "重试提示已插入");

        // 请求重新出流：立即隐藏，无需等待 auto_retry_end。
        app.apply_sink_event(&serde_json::json!({
            "type": "message_start",
            "message": { "role": "assistant", "content": [], "timestamp": 0 }
        }));
        assert!(app.retry_ui.is_none(), "出流后停止刷新");
        assert!(
            app.system_messages.is_empty(),
            "网络恢复（出流）后重试提示应隐藏"
        );

        // 整轮结束的 auto_retry_end 兜底调用幂等：不报错、不重新插入。
        app.apply_sink_auto_retry_end(&end_event(1, true));
        assert!(app.system_messages.is_empty(), "end 兜底不应重新插入提示");

        // 重试中途再次失败：下一次 start 仍能重新显示提示（新时间戳，不残留旧行）。
        app.apply_sink_auto_retry_start(&start_event(2, 3, 0));
        assert_eq!(app.system_messages.len(), 1);
        assert!(
            sys_spans_text(&app.system_messages[0].1.spans).contains("Retrying (2/3)"),
            "后续重试重新显示提示"
        );
    }

    #[test]
    fn retry_ui_tick_refreshes_countdown_in_place() {
        // UI tick 每秒原地刷新：时间戳不变（同一消息位置），文本倒计时更新。
        let mut app = App::new();
        let now = Instant::now();
        app.retry_ui = Some(RetryUi {
            attempt: 1,
            max: 3,
            deadline: now + Duration::from_secs(10),
            last_refresh: now - Duration::from_secs(2),
            ts: 12345,
        });
        app.upsert_sys_msg(12345, vec![SysSpan::plain("old")], MsgLevel::Warning);
        app.retry_ui_tick();
        assert_eq!(app.system_messages.len(), 1, "tick 原地覆盖不追加");
        let text = sys_spans_text(&app.system_messages[0].1.spans);
        assert!(
            text.contains("Retrying (1/3) in "),
            "刷新后含倒计时: {:?}",
            text
        );
        let secs = app
            .retry_ui
            .as_ref()
            .unwrap()
            .deadline
            .saturating_duration_since(Instant::now())
            .as_secs();
        assert!(secs > 0, "倒计时未到点");
        assert!(
            text.contains("(escape to cancel)"),
            "刷新保留取消提示: {:?}",
            text
        );
        let t0 = app.system_messages[0].0;
        assert_eq!(t0, 12345, "时间戳不变，时间线位置不变");
        let refreshed = app.retry_ui.as_ref().unwrap().last_refresh;
        assert!(refreshed.elapsed() < Duration::from_secs(1), "节流复位");

        // 节流：1s 内再次 tick 不更新（last_refresh 距今不足 1s）
        let before = sys_spans_text(&app.system_messages[0].1.spans);
        app.retry_ui_tick();
        assert_eq!(sys_spans_text(&app.system_messages[0].1.spans), before);
    }

    #[test]
    fn cancel_retry_ui_marks_cancelled() {
        // 中断（Esc/Ctrl+C）：停止刷新并原地覆盖为取消提示（end 事件不会来）。
        let mut app = App::new();
        let now = Instant::now();
        app.retry_ui = Some(RetryUi {
            attempt: 2,
            max: 3,
            deadline: now + Duration::from_secs(5),
            last_refresh: now,
            ts: 777,
        });
        app.upsert_sys_msg(
            777,
            vec![SysSpan::plain("⠋ Retrying (2/3) in 5s...")],
            MsgLevel::Warning,
        );
        app.cancel_retry_ui();
        assert!(app.retry_ui.is_none(), "中断后停止刷新");
        let text = sys_spans_text(&app.system_messages[0].1.spans);
        assert!(text.contains("✗ Retry cancelled"), "取消提示: {:?}", text);
    }
}
