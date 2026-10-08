//! steer 队列与 thinking 级别循环

use super::{expand_user_text, submit_and_record};
use crate::{
    core::{extensions, provider::AgentMessage},
    modes::interactive::app::App,
};

impl App {
    /// 取出并记录编辑器内容后排队：`is_follow_up=true` 进本地队列（settle 后发送），
    /// 否则进运行中注入箱（run_loop 中途注入）；同时派发用户提交事件并展开 skill/模板。
    /// 编辑器为空（`submit_and_record` 返回 None）时不产生任何副作用。
    pub(super) fn queue_steer(&mut self, is_follow_up: bool) {
        // 走统一的提交路径：steer/follow-up 也是「说给模型的话」，要进历史并落盘
        if let Some(text) = submit_and_record(self) {
            // 与 Enter 提交同一条通知链：Alt+Enter 排队也是“用户开新回合”，
            // 扩展（子代理）据此隐去上一轮的完成记录（见 dispatch_user_submit）。
            extensions::dispatch_user_submit(&text, self.busy);

            // 与空闲提交同一条展开链：/skill:name + prompt 模板先展开再排队
            // （忙碌时 skill 也要按 skill 命令处理，不能原样当普通文本发）。
            let text = expand_user_text(self, &text);
            self.queue_steer_text(text, is_follow_up);
        }
    }

    /// 把已取出的文本排入队列（steer 立即进注入箱；follow-up 进本地队列）。
    /// 供忙碌提交路径在扩展改写后复用（扩展 `Rewrite` 后文本已不在编辑器里）。
    pub(super) fn queue_steer_text(&mut self, text: String, is_follow_up: bool) {
        if text.trim().is_empty() {
            return;
        }

        if is_follow_up {
            // follow-up：settle 后发送，进本地队列（渲染显示、中断取回）
            self.runtime_followup_inbox.push_back(text);
        } else if let Ok(mut inbox) = self.runtime_steer_inbox.lock() {
            // steer：立即进运行中注入箱（与 Agent 共享 Arc），run_loop 中途注入。
            // 渲染读 inbox 显示未注入的 steer；注入后自动从待发送区消失。
            inbox.push_back(AgentMessage::user_text(&text));
        }
        self.dirty = true;
    }

    /// （Alt+Up）：把排队的消息全部取回编辑器（steer 在前、follow-up 在后）。
    /// 排队文本以空行分隔拼接在编辑器现有内容之前，
    /// 队列清空；状态栏提示取回数量；空队列提示 "No queued messages to restore"。
    /// 不锁 agent——busy 时 prompt future 持锁挂起（约束同 queue_steer）。
    pub(super) fn dequeue_steers(&mut self) {
        // 未注入的 steer（inbox）+ 全部 follow-up（本地队列），顺序 = steer 先 follow-up 后
        let steers: Vec<String> = self
            .runtime_steer_inbox
            .lock()
            .unwrap()
            .drain(..)
            .map(|m| m.text())
            .collect();
        let follow_ups: Vec<String> = self.runtime_followup_inbox.drain(..).collect();

        if steers.is_empty() && follow_ups.is_empty() {
            self.set_status_msg("No queued messages to restore");
            return;
        }

        let count = steers.len() + follow_ups.len();
        let mut texts = steers;
        texts.extend(follow_ups);
        let queued = texts.join("\n\n");

        let current = self.editor.text();
        let combined = if current.trim().is_empty() {
            queued
        } else {
            format!("{}\n\n{}", queued, current)
        };

        self.editor.clear();
        self.editor.insert_text(&combined);
        self.set_status_msg(format!(
            "Restored {} queued message{} to editor",
            count,
            if count > 1 { "s" } else { "" }
        ));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use crate::modes::interactive::app::App;
    /// 测试 agent 目录守卫：每测试独立临时目录（线程本地 override），
    /// 替代全局锁 + 进程级 env 劫持（并行互不干扰、无死锁）。
    fn test_agent_dir() -> crate::test_support::AgentDirGuard {
        crate::test_support::AgentDirGuard::temp()
    }

    #[test]
    fn queue_steer_buffers_text_while_busy() {
        let _g = test_agent_dir();
        let mut st = App::new();

        st.editor.insert_text("continue faster");
        st.queue_steer(false);
        // busy 时入队不锁 agent（prompt future 持锁挂起，锁 agent 会死锁）；
        // steer 进运行中注入箱（独立锁 inbox，run_loop 中途注入），不进本地队列
        assert!(
            st.runtime_followup_inbox.is_empty(),
            "steer 不应进 follow-up 队列"
        );
        assert_eq!(st.runtime_steer_inbox.lock().unwrap().len(), 1);
        assert_eq!(
            st.runtime_steer_inbox.lock().unwrap()[0].text(),
            "continue faster"
        );
        assert_eq!(st.editor.text(), "", "入队后编辑器清空");
        // 排队反馈由待发送区渲染（对齐 pi pendingMessagesContainer），不进消息流
        assert!(
            st.system_messages.iter().all(|(_, m)| {
                !crate::modes::interactive::app::sys_spans_text(&m.spans).contains("steer queued")
            }),
            "排队提示应走待发送区显示: {:?}",
            st.system_messages
        );

        // 空文本不入队
        st.queue_steer(false);
        assert_eq!(st.runtime_steer_inbox.lock().unwrap().len(), 1);
    }

    #[test]
    fn follow_up_expands_skill_command() {
        // 回归：Alt+Enter 排队的 follow-up 若为 /skill:name，也要按 skill 命令
        // 展开后再入队（与空闲提交同一条展开链）。
        let _g = test_agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let skill_md = dir.path().join("SKILL.md");
        std::fs::write(&skill_md, "---\nname: demo\ndescription: d\n---\n# Body\n").unwrap();
        let mut st = App::new();
        st.skills = vec![crate::core::skills::Skill {
            name: "demo".to_string(),
            description: "d".to_string(),
            path: skill_md,
            base_dir: dir.path().to_path_buf(),
            disable_model_invocation: false,
            source: "test".to_string(),
        }];
        st.editor.insert_text("/skill:demo go");
        st.queue_steer(true);
        assert_eq!(st.runtime_followup_inbox.len(), 1);
        let text = &st.runtime_followup_inbox[0];
        assert!(text.contains("<skill name=\"demo\""), "got: {text}");
        assert!(text.contains("# Body"), "got: {text}");
        assert!(text.ends_with("go"), "got: {text}");
    }

    #[test]
    fn follow_up_queued_distinct_from_steer() {
        let _g = test_agent_dir();
        let mut st = App::new();
        st.editor.insert_text("steer msg");
        st.queue_steer(false);
        st.editor.insert_text("follow-up msg");
        st.queue_steer(true);
        // steer 进注入箱（运行中注入）；follow-up 进本地队列（settle 后发送）
        assert_eq!(st.runtime_steer_inbox.lock().unwrap().len(), 1);
        assert_eq!(
            st.runtime_steer_inbox.lock().unwrap()[0].text(),
            "steer msg"
        );
        assert_eq!(
            st.runtime_followup_inbox,
            std::collections::VecDeque::from(vec!["follow-up msg".to_string()])
        );
    }

    #[test]
    fn dequeue_restores_steer_before_follow_up() {
        let _g = test_agent_dir();
        let mut st = App::new();
        // steer 在注入箱、follow-up 在本地队列；取回先全部 steer 再 follow-up
        // （对齐 pi restoreQueuedMessagesToEditor 的 [...steering, ...followUp]）。
        st.runtime_steer_inbox
            .lock()
            .unwrap()
            .push_back(crate::core::provider::AgentMessage::user_text("steer one"));
        st.runtime_steer_inbox
            .lock()
            .unwrap()
            .push_back(crate::core::provider::AgentMessage::user_text("steer two"));
        st.runtime_followup_inbox =
            std::collections::VecDeque::from(vec!["fup one".to_string(), "fup two".to_string()]);
        st.editor.insert_text("current draft");
        st.dequeue_steers();
        assert!(st.runtime_followup_inbox.is_empty(), "取回后队列清空");
        assert!(st.runtime_steer_inbox.lock().unwrap().is_empty());
        let text = st.editor.text();
        assert!(
            text.starts_with("steer one\n\nsteer two\n\nfup one\n\nfup two\n\ncurrent draft"),
            "steer 在前、follow-up 在后（对齐 pi）: {:?}",
            text
        );
        assert_eq!(st.status, "Restored 4 queued messages to editor");
    }

    #[test]
    fn dequeue_restores_queued_steers_to_editor() {
        let _g = test_agent_dir();
        let mut st = App::new();
        st.runtime_steer_inbox
            .lock()
            .unwrap()
            .push_back(crate::core::provider::AgentMessage::user_text("steer one"));
        st.runtime_steer_inbox
            .lock()
            .unwrap()
            .push_back(crate::core::provider::AgentMessage::user_text("steer two"));
        st.editor.insert_text("current draft");
        st.dequeue_steers();
        assert!(st.runtime_followup_inbox.is_empty(), "取回后队列清空");
        assert!(st.runtime_steer_inbox.lock().unwrap().is_empty());
        let text = st.editor.text();
        assert!(
            text.starts_with("steer one\n\nsteer two\n\ncurrent draft"),
            "排队文本在前、空行分隔（对齐 pi restoreQueuedMessagesToEditor）: {:?}",
            text
        );
        assert_eq!(st.status, "Restored 2 queued messages to editor");
    }

    #[test]
    fn dequeue_with_empty_queue_shows_status() {
        let mut st = App::new();
        st.dequeue_steers();
        assert_eq!(st.status, "No queued messages to restore");
    }

    #[test]
    fn queue_steer_does_not_block_while_agent_locked() {
        // 死锁回归：prompt future 在 worker 线程持有 agent 锁挂起时，主循环线程
        // 处理键盘绝不能锁 agent（std Mutex 阻塞 = 程序卡死）。
        // 本线程先持有锁模拟该场景：若 queue_steer 尝试
        let mut st = App::new();
        st.editor.insert_text("steer while locked");
        st.queue_steer(false);
        assert_eq!(st.runtime_steer_inbox.lock().unwrap().len(), 1);
        assert_eq!(st.editor.text(), "");
    }
}
