//! 聊天区「系统消息流」与状态栏短提示的写入。
//!
//! 数据模型（`SysSpan` / `SysMsg` / `MsgLevel` 与 `sys_*_text`）留在 app.rs —— 它们是
//! `App::system_messages` 的字段类型；本模块只放**行为**：追加 / 去重 / 原地更新 / 移除
//! 系统消息，以及状态栏消息的设置、清除与超时。
//!
//! 滚动策略：仅在用户已贴底时让新消息跟随（见 `App::follow_bottom_if_at_end`）。

use super::super::{
    app::{App, MsgLevel, SysMsg, SysSpan, sys_spans_text},
    render::messages::SYSTEM_SEQ_BASE,
};
use crate::utils::time::now_ms;
use std::time::Instant;

impl App {
    /// 向消息流追加一条系统消息（展示用，不进 LLM 上下文）。
    /// 带时间戳，提示按发生时间插入消息流， 而不是堆积在最新 LLM 输出之后。
    /// `level` 显式声明消息级别，决定默认渲染色（不留“默认 info”的隐性路径）。
    pub fn push_msg(&mut self, text: String, level: MsgLevel) {
        let ts = now_ms();
        self.system_messages
            .push((ts, SysMsg::new(level, vec![SysSpan::plain(&text)])));
        self.follow_bottom_if_at_end();
        self.dirty = true;
    }
    /// 向消息流追加一条多段配色系统消息（/session 等富文本提示用）。
    /// `level` 决定级别默认色；片段的 `fg` 覆盖级别默认色。
    pub fn push_msg_rich(&mut self, spans: Vec<SysSpan>, level: MsgLevel) {
        let ts = now_ms();
        self.system_messages.push((ts, SysMsg::new(level, spans)));
        self.follow_bottom_if_at_end();
        self.dirty = true;
    }
    /// 按类别推送提示：最后一条系统消息以 `key` 开头时原地替换（时间戳不变，级别同步更新），
    /// 否则追加。用于循环切换类操作（thinking 级别 / 主题），避免连续切换产生多条同类提示堆积。
    pub fn push_msg_dedup(&mut self, text: String, key: &str, level: MsgLevel) {
        let is_same = self
            .system_messages
            .last()
            .map(|(_, last)| sys_spans_text(&last.spans).starts_with(key))
            .unwrap_or(false);

        if is_same {
            let idx = self.system_messages.len() - 1;
            if let Some(last) = self.system_messages.last_mut() {
                // 内容变了但时间线 key (ts, seq) 不变：不失效缓存会命中旧渲染，
                // 提示永远停在第一次的内容（Shift+Tab 循环 thinking 级别只显示一次）。
                self.msg_cache.remove(&(last.0, SYSTEM_SEQ_BASE + idx));
                last.1 = SysMsg::new(level, vec![SysSpan::plain(&text)]);
            }
            self.follow_bottom_if_at_end();
            self.dirty = true;
        } else {
            self.push_msg(text, level);
        }
    }
    /// 按时间戳原地覆盖或追加系统消息（重试提示刷新专用）：
    /// `ts` 已存在 → 替换 spans/level 并失效该条渲染缓存（时间线位置不变）；
    /// 不存在 → 追加尾部。重置滚动并置脏。
    pub fn upsert_sys_msg(&mut self, ts: u64, spans: Vec<SysSpan>, level: MsgLevel) {
        if let Some(idx) = self.system_messages.iter().position(|(t, _)| *t == ts) {
            self.msg_cache.remove(&(ts, SYSTEM_SEQ_BASE + idx));
            self.system_messages[idx].1 = SysMsg::new(level, spans);
        } else {
            self.system_messages.push((ts, SysMsg::new(level, spans)));
        }
        self.follow_bottom_if_at_end();
        self.dirty = true;
    }
    /// 按时间戳移除一条系统消息（重试成功收尾专用：网络恢复后消息区不残留重试提示）。
    /// 删除该条并失效其渲染缓存；后续系统消息 seq 前移，命中失败会自动重渲染重建。
    pub fn remove_sys_msg(&mut self, ts: u64) {
        if let Some(idx) = self.system_messages.iter().position(|(t, _)| *t == ts) {
            self.msg_cache.remove(&(ts, SYSTEM_SEQ_BASE + idx));
            self.system_messages.remove(idx);
            self.follow_bottom_if_at_end();
            self.dirty = true;
        }
    }
    /// 新输出/系统消息追加时的滚动策略：仅当消息区当前已处于末尾（scroll==0）
    /// 才维持自动滚动跟随；用户已向上滚动阅读时保留其位置，不强制跳回底部。
    fn follow_bottom_if_at_end(&mut self) {
        if self.scroll != 0 {
            return; // 已上滚：不打断阅读位置
        }
        self.scroll = 0; // 贴底：维持跟随最新输出
    }
    /// 设置状态栏临时提示：显示 STATUS_MSG_TTL 后自动消失。
    /// 用于复制成功、OAuth URL 等"发生了什么事"的一次性反馈，不进消息流。
    pub fn set_status_msg(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_deadline = Some(Instant::now() + Self::STATUS_MSG_TTL);
        self.dirty = true;
    }
    /// 清空状态栏文本与提示截止时间（turn_end 等收尾用）
    pub fn clear_status(&mut self) {
        self.status.clear();
        self.status_deadline = None;
        self.dirty = true;
    }
    /// 状态提示过期清理（事件循环 tick 调用）；已过期清除并返回 true
    pub fn expire_status(&mut self) -> bool {
        let expired = self.status_deadline.is_some_and(|dl| Instant::now() >= dl);
        if expired {
            self.status.clear();
            self.status_deadline = None;
        }
        expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::App;

    #[test]
    fn push_msg_dedup_replaces_last_same_category() {
        let mut app = App::new();
        app.push_msg_dedup("thinking: xhigh".to_string(), "thinking: ", MsgLevel::Info);
        app.push_msg_dedup("thinking: max".to_string(), "thinking: ", MsgLevel::Info);
        app.push_msg_dedup("thinking: off".to_string(), "thinking: ", MsgLevel::Info);
        assert_eq!(
            app.system_messages.len(),
            1,
            "同类提示应原地替换: {:?}",
            app.system_messages
        );
        assert_eq!(
            sys_spans_text(&app.system_messages[0].1.spans),
            "thinking: off"
        );
        // 中间插入其他提示后不再替换
        app.push_msg("unrelated".to_string(), MsgLevel::Info);
        app.push_msg_dedup(
            "thinking: minimal".to_string(),
            "thinking: ",
            MsgLevel::Info,
        );
        assert_eq!(
            app.system_messages.len(),
            3,
            "非连续同类提示应追加: {:?}",
            app.system_messages
        );
        // 不同类别互不影响
        app.push_msg_dedup(
            "switched theme: dark".to_string(),
            "switched theme: ",
            MsgLevel::Info,
        );
        app.push_msg_dedup(
            "switched theme: light".to_string(),
            "switched theme: ",
            MsgLevel::Info,
        );
        assert_eq!(app.system_messages.len(), 4);
        assert_eq!(
            sys_spans_text(&app.system_messages.last().unwrap().1.spans),
            "switched theme: light"
        );
    }

    #[test]
    fn push_msg_dedup_replaced_item_refreshes_render_cache() {
        // 复现 bug：同类提示原地替换后，时间线 key (ts, seq) 不变，若不失效
        // msg_cache，渲染会命中旧缓存导致提示永远停在第一次的内容
        // （Shift+Tab 循环 thinking 级别时 TUI 只显示一次提示）。
        let mut app = App::new();
        app.push_msg_dedup("thinking: low".to_string(), "thinking: ", MsgLevel::Info);
        let text = |app: &mut App| -> String {
            let r = app.render_history_lines(80);
            r.segments
                .iter()
                .flat_map(|(_, lines, _)| lines.iter())
                .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
                .collect()
        };
        assert!(text(&mut app).contains("thinking: low"), "首帧渲染提示");

        app.push_msg_dedup("thinking: max".to_string(), "thinking: ", MsgLevel::Info);
        let after = text(&mut app);
        assert!(
            after.contains("thinking: max"),
            "替换后渲染应显示新提示，实际: {after}"
        );
        assert!(!after.contains("thinking: low"), "不得残留旧提示: {after}");
    }

    #[test]
    fn push_msg_preserves_scroll_when_scrolled_up() {
        // 用户已向上滚动阅读时，新系统消息不得强制跳回底部。
        let mut app = App::new();
        app.scroll = 42;
        app.push_msg("notice".to_string(), MsgLevel::Info);
        assert_eq!(app.scroll, 42, "上滚阅读时 push_msg 不得跳底");

        app.push_msg_rich(vec![SysSpan::plain("rich")], MsgLevel::Info);
        assert_eq!(app.scroll, 42, "上滚阅读时 push_msg_rich 不得跳底");

        app.push_msg_dedup("thinking: max".to_string(), "thinking: ", MsgLevel::Info);
        assert_eq!(app.scroll, 42, "上滚阅读时 push_msg_dedup 不得跳底");

        app.upsert_sys_msg(1, vec![SysSpan::plain("retry")], MsgLevel::Warning);
        assert_eq!(app.scroll, 42, "上滚阅读时 upsert_sys_msg 不得跳底");

        // 贴底（scroll==0）：保持自动滚动跟随最新输出。
        app.scroll = 0;
        app.push_msg("more".to_string(), MsgLevel::Info);
        app.push_msg_dedup("thinking: off".to_string(), "thinking: ", MsgLevel::Info);
        app.upsert_sys_msg(2, vec![SysSpan::plain("retry")], MsgLevel::Warning);
        assert_eq!(app.scroll, 0, "贴底时保持跟随最新输出");
    }

    #[test]
    fn remove_sys_msg_deletes_in_place_and_invalidates_cache() {
        // 移除系统消息：条目删除、渲染缓存失效（后续同位置重建不残留旧行）。
        let mut app = App::new();
        app.upsert_sys_msg(100, vec![SysSpan::plain("retrying...")], MsgLevel::Warning);
        app.upsert_sys_msg(200, vec![SysSpan::plain("later notice")], MsgLevel::Info);
        let r = app.render_history_lines(60);
        assert_eq!(r.segments.len(), 2, "两条系统消息渲染");
        assert!(app.msg_cache.len() >= 2);

        app.remove_sys_msg(100);
        assert_eq!(app.system_messages.len(), 1, "目标消息移除");
        assert_eq!(app.system_messages[0].0, 200, "其余消息保留");
        let r2 = app.render_history_lines(60);
        assert_eq!(r2.segments.len(), 1, "移除后时间线只剩一条");
        let text = sys_spans_text(&app.system_messages[0].1.spans);
        assert!(text.contains("later notice"));

        // 不存在的 ts：无操作。
        app.remove_sys_msg(999);
        assert_eq!(app.system_messages.len(), 1);
    }
}
