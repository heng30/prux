//! 会话选择器（/resume）

use super::KeyAction;
use crate::{
    core::{
        keybindings::{self, KeybindingsManager},
        settings_manager,
    },
    modes::interactive::{
        agent_actor::AgentCommand, app::App, line_input::InputAction, session_selector,
    },
    utils::clipboard::read_clipboard,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

impl App {
    /// 打开 /resume 会话选择器：以 cwd 与 agent 目录为范围扫描会话、标出当前会话，
    /// 并置脏位请求重绘。
    pub(super) fn open_session_panel(&mut self) {
        let cwd = self.cwd.clone();
        let agent_dir = settings_manager::agent_dir();
        let current = self.current_session_path.clone();
        self.session_selector.open(&cwd, &agent_dir, current);
        self.dirty = true;
    }

    /// 会话选择器键盘处理
    pub(super) fn handle_session_selector_key(&mut self, key: &KeyEvent) -> KeyAction {
        self.session_selector.expire_status();
        let kb = keybindings::get_global();

        // 重命名模式：Esc / Ctrl+C 退出，Enter 提交，其余键交给重命名输入框
        if self.session_selector.rename_mode {
            self.handle_selector_rename_key(key, kb);
            return KeyAction::Continue;
        }

        // 删除确认：Enter 确认 / Esc / Ctrl+C 取消，其余键忽略
        if self.session_selector.is_confirming_delete() {
            self.handle_selector_delete_confirm_key(key, kb);
            return KeyAction::Continue;
        }

        // 会话选择器快捷键（app.session.*）+ 导航（tui.select.*）
        self.handle_selector_main_key(key, kb);
        KeyAction::Continue
    }

    /// 会话重命名模式键处理：Esc/Ctrl+C 退出、Enter 提交、其余键交给重命名输入框
    fn handle_selector_rename_key(&mut self, key: &KeyEvent, kb: &'static KeybindingsManager) {
        if kb.matches(key, "tui.select.cancel") {
            self.session_selector.exit_rename();
            self.dirty = true;
        } else if kb.matches(key, "tui.select.confirm") {
            self.session_selector.confirm_rename();
            self.dirty = true;
        } else {
            match key.code {
                KeyCode::Char('v') | KeyCode::Char('V')
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    // 粘贴剪贴板到重命名框
                    if let Some(text) = read_clipboard() {
                        self.session_selector.rename_input.insert_text(&text);
                        self.dirty = true;
                    }
                }
                _ => {
                    // 统一输入框处理（Shift 层映射 / 编辑键）；Enter 已在上面处理
                    match self.session_selector.rename_input.handle_key(key) {
                        InputAction::None => {}
                        InputAction::Edited | InputAction::Moved => self.dirty = true,
                    }
                }
            }
        }
    }

    /// 删除确认模式键处理：Enter 确认 / Esc / Ctrl+C 取消，其余键忽略
    fn handle_selector_delete_confirm_key(
        &mut self,
        key: &KeyEvent,
        kb: &'static KeybindingsManager,
    ) {
        if kb.matches(key, "tui.select.confirm") {
            self.session_selector.confirm_delete();
            self.dirty = true;
        } else if kb.matches(key, "tui.select.cancel") {
            self.session_selector.cancel_delete();
            self.dirty = true;
        }
    }

    /// 会话选择器主列表键处理（app.session.* 快捷键 + tui.select.* 导航 + 搜索输入）
    fn handle_selector_main_key(&mut self, key: &KeyEvent, kb: &'static KeybindingsManager) {
        if key.code == KeyCode::Tab {
            self.session_selector.toggle_scope();
            self.dirty = true;
        } else if kb.matches(key, "app.session.toggleSort") {
            self.session_selector.cycle_sort();
            self.dirty = true;
        } else if kb.matches(key, "app.session.toggleNamedFilter") {
            self.session_selector.toggle_name_filter();
            self.dirty = true;
        } else if kb.matches(key, "app.session.togglePath") {
            self.session_selector.toggle_path();
            self.dirty = true;
        } else if kb.matches(key, "app.session.delete") {
            self.session_selector.start_delete_confirm();
            self.dirty = true;
        } else if kb.matches(key, "app.session.rename") {
            self.session_selector.start_rename();
            self.dirty = true;
        } else if kb.matches(key, "app.session.deleteNoninvasive") {
            if self.session_selector.filter.is_empty() {
                self.session_selector.start_delete_confirm();
            } else {
                self.session_selector.filter.handle_key(key);
                self.session_selector.recompute();
            }
            self.dirty = true;
        } else if matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            // Ctrl+V / Ctrl+Shift+V：粘贴剪贴板到搜索框
            if let Some(text) = read_clipboard() {
                self.session_selector.filter.insert_text(&text);
                self.session_selector.recompute();
                self.dirty = true;
            }
        } else if kb.matches(key, "tui.select.first") {
            self.session_selector.select_first();
            self.dirty = true;
        } else if kb.matches(key, "tui.select.last") {
            self.session_selector.select_last();
            self.dirty = true;
        } else if kb.matches(key, "app.session.next") {
            // Ctrl+G：跳到当前打开会话的下一条会话
            self.session_selector.select_next_after_current();
            self.dirty = true;
        } else if kb.matches(key, "tui.select.pageUp") {
            self.session_selector
                .move_selection(-(session_selector::MAX_VISIBLE as i32));
            self.dirty = true;
        } else if kb.matches(key, "tui.select.pageDown") {
            self.session_selector
                .move_selection(session_selector::MAX_VISIBLE as i32);
            self.dirty = true;
        } else if kb.matches(key, "tui.select.up")
            || (key.code == KeyCode::Char('k') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            // Ctrl+K 扩展导航（对齐面板导航；与输入框 kill-to-end 冲突时以导航优先）
            self.session_selector.move_selection(-1);
            self.dirty = true;
        } else if kb.matches(key, "tui.select.down")
            || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.session_selector.move_selection(1);
            self.dirty = true;
        } else if kb.matches(key, "tui.select.confirm") {
            if let Some(path) = self.session_selector.selected_path() {
                self.resume_session_from_selector(&path);
            }
            self.dirty = true;
        } else if kb.matches(key, "tui.select.cancel") {
            self.session_selector.close();
            self.dirty = true;
        } else {
            // 搜索框输入（Shift 层映射与编辑键统一处理）；文本变化才重算列表
            match self.session_selector.filter.handle_key(key) {
                InputAction::None => {}
                InputAction::Edited => {
                    self.session_selector.recompute();
                    self.dirty = true;
                }
                InputAction::Moved => self.dirty = true,
            }
        }
    }

    /// 选择会话后切换（handleResumeSession / 原 panel Session confirm）
    fn resume_session_from_selector(&mut self, path: &str) {
        self.worker.send(AgentCommand::ResumeSession {
            path: path.to_string(),
        });
        self.session_selector.close();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::modes::interactive::agent_actor::WorkerHandle;
    use crate::modes::interactive::app::App;
    use std::{cell::RefCell, rc::Rc};

    // 探针会话的 mtime 序号（每测试线程独立，只用于让同批探针有确定顺序）。
    std::thread_local! {
        /// 已发出的 mtime 个数。
        static PROBE_SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// 会话探针文件的下一个 mtime：同一线程内单调递增、且都落在过去（age 显示正常）。
    ///
    /// 为何需要：同批 probe 常在**同一毫秒**内落盘，文件名里的时间戳相同，
    /// 会话列表排序会退化到随机 id → 依赖列表顺序的断言随机失败。
    fn probe_mtime() -> std::time::SystemTime {
        let n = PROBE_SEQ.with(|c| {
            let v = c.get() + 1;
            c.set(v);
            v
        });
        std::time::SystemTime::now() - std::time::Duration::from_secs(3600u64.saturating_sub(n))
    }

    /// 创建会话并追加 assistant 消息触发落盘（对齐新语义：首条 assistant 消息才建文件）。
    fn persisted_session_probe(dir: &std::path::Path) -> crate::core::session_manager::Session {
        let mut s =
            crate::core::session_manager::Session::create("/tmp", Some(dir.to_path_buf()), true)
                .unwrap();
        s.append_message(&crate::core::provider::AgentMessage {
            role: "assistant".into(),
            thinking_level: None,
            content: vec![],
            ..crate::core::provider::AgentMessage::user_text("")
        });
        // 有用户消息：/resume 列表过滤空会话后仍可见
        s.append_message(&crate::core::provider::AgentMessage::user_text("probe"));
        assert!(s.get_session_file().is_some(), "assistant 消息应触发落盘");

        // 定序（见 [`probe_mtime`]）：会话不再写入，改 mtime 不会被后续写盘覆盖
        let path = s.get_session_file().unwrap().to_path_buf();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(probe_mtime()).unwrap();
        s
    }
    use crate::modes::interactive::handlers::handle_event;
    use crossterm::event::{Event, KeyEvent};
    /// 会话测试依赖进程级 PRUX_AGENT_DIR（与 auth/settings/extra_themes 测试
    /// 同目录或经 set_var 劫持的目录），必须与它们共用 AUTH_TEST_LOCK 串行。
    fn lock_sessions() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// channels 版 worker：可断言 handler 发出的命令（分层测试 C 的 handler 层）
    fn test_agent_cmd() -> (
        WorkerHandle,
        crate::modes::interactive::agent_actor::CommandRx,
    ) {
        crate::test_support::pin_test_agent_dir();
        let (cmd_tx, cmd_rx) = crate::modes::interactive::agent_actor::channels();
        (WorkerHandle::new(cmd_tx), cmd_rx)
    }

    #[test]
    fn resume_opens_selector_showing_current_session() {
        let _g = lock_sessions();
        let mut st = App::new();
        st.cwd = "/tmp".to_string();
        let (agent, mut cmd_rx) = test_agent_cmd();
        st.worker = agent;
        let agent_dir = crate::core::settings_manager::agent_dir();
        let dir = crate::core::session_manager::default_session_dir("/tmp", &agent_dir);
        // 清理历史运行残留，保证只有本测试创建的两个会话
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let s1 = persisted_session_probe(&dir);
        let s2 = persisted_session_probe(&dir);
        let cur_path = s1
            .get_session_file()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap();
        let other_path = s2
            .get_session_file()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap();
        st.current_session_path = Some(cur_path.clone());

        // /resume：打开选择器，当前会话也显示（左侧 ✓ 标记）
        st.handle_slash_command("resume");
        assert!(st.session_selector.active);
        assert_eq!(st.session_selector.display.len(), 2, "当前会话应显示");

        let rows = &st.session_selector.rows;
        let cur_row = rows
            .iter()
            .find(|r| r.path.to_string_lossy() == cur_path)
            .expect("当前会话应在列表中");
        assert!(cur_row.is_current, "当前会话应被标记");
        let other_row = rows
            .iter()
            .find(|r| r.path.to_string_lossy() == other_path)
            .expect("其它会话应在列表中");
        assert!(!other_row.is_current, "其它会话不应被标记");

        // 列表只显示有用户消息的会话（空会话已过滤），首条消息为 probe
        assert!(rows.iter().any(|r| r.text == "probe"));

        // 选择非当前会话 + Enter 恢复会话（对齐 pi handleResumeSession）
        let idx = st
            .session_selector
            .display
            .iter()
            .position(|n| st.session_selector.rows[n.idx].path.to_string_lossy() == other_path)
            .expect("非当前会话应在显示列表中");
        st.session_selector.selected = idx;
        st.handle_session_selector_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!st.session_selector.active, "选择后选择器关闭");
        // actor：恢复经 ResumeSession 命令下发（回执更新消息流/路径）
        let cmd = cmd_rx.try_recv().expect("应发出 ResumeSession 命令");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::ResumeSession { .. }
            ),
            "应发 ResumeSession: {cmd:?}"
        );
    }

    #[test]
    fn resume_selector_refuses_to_delete_current_session() {
        let _g = lock_sessions();
        let mut st = App::new();
        st.cwd = "/tmp".to_string();
        let (agent, _cmd_rx) = test_agent_cmd();
        st.worker = agent;
        let agent_dir = crate::core::settings_manager::agent_dir();
        let dir = crate::core::session_manager::default_session_dir("/tmp", &agent_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let s1 = persisted_session_probe(&dir);
        let cur_path = s1
            .get_session_file()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap();
        st.current_session_path = Some(cur_path.clone());

        st.handle_slash_command("resume");
        let idx = st
            .session_selector
            .display
            .iter()
            .position(|n| st.session_selector.rows[n.idx].path.to_string_lossy() == cur_path)
            .expect("当前会话应在显示列表中");
        st.session_selector.selected = idx;
        st.handle_session_selector_key(&KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(
            !st.session_selector.is_confirming_delete(),
            "当前会话不应进入删除确认"
        );
        assert!(
            std::path::Path::new(&cur_path).exists(),
            "当前会话文件不应被删除"
        );
    }

    #[test]
    fn resume_selector_esc_closes_and_shift_input_filters() {
        let _g = lock_sessions();
        let mut st = App::new();
        st.cwd = "/tmp".to_string();
        let (agent, _cmd_rx) = test_agent_cmd();
        st.worker = agent;
        let agent_dir = crate::core::settings_manager::agent_dir();
        let dir = crate::core::session_manager::default_session_dir("/tmp", &agent_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let s1 = persisted_session_probe(&dir);
        let _s2 = persisted_session_probe(&dir);
        st.current_session_path = s1
            .get_session_file()
            .map(|p| p.to_string_lossy().to_string());

        st.handle_slash_command("resume");
        assert!(st.session_selector.active);

        // Esc 关闭（修复 or-pattern + guard 的取消 bug）
        st.handle_session_selector_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!st.session_selector.active, "Esc 应关闭选择器");

        // 重新打开：Shift+2 输入 '@'（kitty 协议上报 Char('2')+SHIFT）
        st.handle_slash_command("resume");
        assert_eq!(st.session_selector.display.len(), 2, "当前会话也显示");
        st.handle_session_selector_key(&KeyEvent::new(KeyCode::Char('2'), KeyModifiers::SHIFT));
        assert_eq!(st.session_selector.filter.value, "@", "Shift+2 应输入 @");
        // 过滤后无会话匹配（id/消息不含 @）
        assert!(st.session_selector.display.is_empty());

        // 退格删除后恢复
        st.handle_session_selector_key(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(st.session_selector.filter.value, "");
        assert_eq!(st.session_selector.display.len(), 2);

        // Ctrl+C 同样关闭
        st.handle_session_selector_key(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!st.session_selector.active, "Ctrl+C 应关闭选择器");
    }

    #[test]
    fn resume_selector_home_end_and_next_keys() {
        let _g = lock_sessions();
        let mut st = App::new();
        st.cwd = "/tmp".to_string();
        let (agent, _cmd_rx) = test_agent_cmd();
        st.worker = agent;
        let agent_dir = crate::core::settings_manager::agent_dir();
        let dir = crate::core::session_manager::default_session_dir("/tmp", &agent_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let s1 = persisted_session_probe(&dir);
        let _s2 = persisted_session_probe(&dir);
        let _s3 = persisted_session_probe(&dir);
        st.current_session_path = s1
            .get_session_file()
            .map(|p| p.to_string_lossy().to_string());

        st.handle_slash_command("resume");
        let n = st.session_selector.display.len();
        assert_eq!(n, 3, "含当前会话共 3 个");
        let selectable = |st: &App, i: usize| {
            !st.session_selector.rows[st.session_selector.display[i].idx].is_current
        };

        // End：最后一个可选项
        st.handle_session_selector_key(&KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        let last = (0..n).rev().find(|&i| selectable(&st, i)).unwrap();
        assert_eq!(st.session_selector.selected, last, "End 跳到末项");

        // Home：首个可选项
        st.handle_session_selector_key(&KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        let first = (0..n).find(|&i| selectable(&st, i)).unwrap();
        assert_eq!(st.session_selector.selected, first, "Home 跳到首项");

        // Ctrl+G：跳到当前打开会话的下一条会话
        st.handle_session_selector_key(&KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        let cur = (0..n)
            .find(|&i| st.session_selector.rows[st.session_selector.display[i].idx].is_current)
            .expect("当前会话应在列表中");
        assert_eq!(
            st.session_selector.selected,
            (cur + 1) % n,
            "Ctrl+G 跳到当前会话的下一条"
        );
    }

    #[test]
    fn resume_selector_ctrl_jk_navigation_and_paste() {
        let _g = lock_sessions();
        let mut st = App::new();
        st.cwd = "/tmp".to_string();
        let (agent, _cmd_rx) = test_agent_cmd();
        st.worker = agent;
        let agent_dir = crate::core::settings_manager::agent_dir();
        let dir = crate::core::session_manager::default_session_dir("/tmp", &agent_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let s1 = persisted_session_probe(&dir);
        let _s2 = persisted_session_probe(&dir);
        let _s3 = persisted_session_probe(&dir);
        st.current_session_path = s1
            .get_session_file()
            .map(|p| p.to_string_lossy().to_string());
        st.handle_slash_command("resume");
        assert_eq!(
            st.session_selector.display.len(),
            3,
            "三个会话（含当前）全部显示"
        );

        // Ctrl+J 下移 / Ctrl+K 上移
        st.handle_session_selector_key(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(st.session_selector.selected, 1, "Ctrl+J 下移");
        st.handle_session_selector_key(&KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(st.session_selector.selected, 0, "Ctrl+K 上移");

        // bracketed paste 路由到会话选择器搜索框
        let shared = Rc::new(RefCell::new(st));
        handle_event(&shared, Event::Paste("hello".to_string()));
        let st = shared.borrow_mut();
        assert_eq!(st.session_selector.filter.value, "hello", "Paste 进搜索框");
        // 会话首条消息为 probe、id 随机：hello 不匹配 → 空列表
        assert!(st.session_selector.display.is_empty());
    }
}
