//! `/giff` 审阅视图的输入处理：键盘导航、输入态、退出与鼠标滚轮。
//!
//! 视图状态见 [`crate::modes::interactive::giff`]，渲染见
//! [`crate::modes::interactive::render::giff`]；本模块只做「按键 / 鼠标 → 视图方法」的映射。

use super::{EditorAction, KeyAction, handle_editor_key};
use crate::{
    core::keybindings,
    modes::interactive::{
        app::App,
        giff::{GiffView, Pane},
    },
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

/// 处理 `/giff` 视图按键。
///
/// 非输入态：`q` 退出视图、`i` 进入输入态、`?` 开关帮助，其余键走视图导航；
/// `Ctrl+C` 与 `q` 同义。输入态：`Esc` 只退出输入态（草稿保留），其余编辑键交给主页
/// 编辑器；`Enter` 提交时先关掉视图，让消息与回复落在主页聊天区。
pub(super) fn handle_key(st: &mut App, key: &KeyEvent) -> KeyAction {
    if st.giff.as_ref().is_some_and(GiffView::is_input_active) {
        // Esc：退出输入态，不退出视图，也不清空草稿
        if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            if let Some(view) = st.giff.as_mut() {
                view.set_input_active(false);
            }
            st.dirty = true;
            return KeyAction::Continue;
        }

        // 外部编辑器在视图内不支持：吞掉，避免草稿被临时文件流程带走
        if keybindings::get_global().matches(key, "app.editor.external") {
            return KeyAction::Continue;
        }

        // Enter：有内容时提交并退出视图（回复要显示在主页聊天区）；
        // 空回车不关视图（否则误按一下就从审阅界面掉出去）
        if keybindings::get_global().matches(key, "tui.input.submit")
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && !st.editor.is_empty()
        {
            st.close_giff();
        }

        return match handle_editor_key(st, key) {
            EditorAction::Quit => KeyAction::Quit,
            EditorAction::Continue | EditorAction::OpenExternal(_) => KeyAction::Continue,
        };
    }

    // Ctrl+C：与 q 同义退出
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        st.close_giff();
        return KeyAction::Continue;
    }

    // 帮助模态打开时只认关闭键
    let help_open = st.giff.as_ref().is_some_and(GiffView::is_help_open);
    if help_open {
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
        ) {
            if let Some(view) = st.giff.as_mut() {
                view.close_help();
            }
            st.dirty = true;
        }
        return KeyAction::Continue;
    }

    match key.code {
        KeyCode::Char('q') => st.close_giff(),
        KeyCode::Char('i') => {
            if let Some(view) = st.giff.as_mut() {
                view.set_input_active(true);
            }
            st.dirty = true;
        }
        KeyCode::Char('?') => {
            if let Some(view) = st.giff.as_mut() {
                view.toggle_help();
            }
            st.dirty = true;
        }
        _ => {
            let now_ms = st.accel_epoch.elapsed().as_millis() as u64;
            if let Some(view) = st.giff.as_mut()
                && navigate(view, key, now_ms)
            {
                st.dirty = true;
            }
        }
    }

    KeyAction::Continue
}

/// `/giff` 非输入态的导航键：焦点在文件列表时移动选择、在内容区时滚动；
/// 返回是否消费了该键。
///
/// `j`/`k`、`↓`/`↑` 按住不放时按间隔加速（[`GiffView::nav_step`]），孤立一次仍只走 1 行/1 项；
/// `now_ms` 为单调时钟毫秒（连按加速的时间基准）。
fn navigate(view: &mut GiffView, key: &KeyEvent, now_ms: u64) -> bool {
    let in_file_list = view.focused_pane() == Pane::FileList;
    let page = view.page_rows().max(1) as isize;

    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            let step = view.nav_step(1, now_ms);
            if in_file_list {
                view.move_file(step);
            } else {
                view.scroll_by(step);
            }
            true
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let step = view.nav_step(-1, now_ms);
            if in_file_list {
                view.move_file(-step);
            } else {
                view.scroll_by(-step);
            }
            true
        }
        KeyCode::PageDown => {
            if in_file_list {
                view.move_file(page);
            } else {
                view.page(true);
            }
            true
        }
        KeyCode::PageUp => {
            if in_file_list {
                view.move_file(-page);
            } else {
                view.page(false);
            }
            true
        }
        KeyCode::Home => {
            if in_file_list {
                view.select_edge(false);
            } else {
                view.scroll_to_top();
            }
            true
        }
        KeyCode::End => {
            if in_file_list {
                view.select_edge(true);
            } else {
                view.scroll_to_bottom();
            }
            true
        }
        KeyCode::Tab => {
            view.toggle_focus();
            true
        }
        KeyCode::Char('h') | KeyCode::Left => {
            view.focus(Pane::FileList);
            true
        }
        KeyCode::Char('l') | KeyCode::Right => {
            view.focus(Pane::DiffContent);
            true
        }
        KeyCode::Char('u') => {
            view.toggle_view();
            true
        }
        _ => {
            // 与导航无关的键：断掉连按手势，避免下一次 j/k 被当成同一次按住
            view.reset_nav_repeat();
            false
        }
    }
}

/// 点 `(column, row)` 是否落在 `area` 内（鼠标命中判定）。
fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

impl App {
    /// 关闭 `/giff` 审阅视图，并把当前文件与滚动位置留到 [`App::giff_restore`]，
    /// 供下次 `/giff` 恢复。
    fn close_giff(&mut self) {
        if let Some(view) = self.giff.take() {
            self.giff_restore = view.into_restore();
        }
        self.full_redraw = true;
        self.dirty = true;
    }

    /// `/giff` 视图的滚轮：鼠标落在文件列表上时移动选中文件，否则滚动当前文件的 diff。
    pub(super) fn giff_wheel(&mut self, mouse: &MouseEvent) {
        if self.giff.as_ref().is_some_and(|view| view.is_help_open()) {
            return;
        }

        let lines = self.wheel_lines(mouse);
        let delta = if mouse.kind == MouseEventKind::ScrollDown {
            lines as isize
        } else {
            -(lines as isize)
        };

        let Some(view) = self.giff.as_mut() else {
            return;
        };

        if view
            .file_list_area()
            .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
        {
            view.move_file(delta);
        } else {
            view.scroll_by(delta);
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::giff::{FileChanges, GiffRestore};
    use crossterm::event::Event;
    use std::{cell::RefCell, rc::Rc};

    /// `/giff` 视图：`i` 进入输入态、`Esc` 只退出输入态（视图与草稿保留）、
    /// `q` 关闭视图并留下恢复状态；输入态下可打印字符进主页输入框。
    #[test]
    fn giff_view_input_and_quit_keys() {
        let _dir = crate::test_support::AgentDirGuard::temp();
        let shared = Rc::new(RefCell::new(App::new()));
        let view = {
            let st = shared.borrow();
            GiffView::new(
                FileChanges::new(),
                "HEAD".to_string(),
                "Working Tree".to_string(),
                &st.theme,
                &GiffRestore::default(),
            )
        };
        shared.borrow_mut().giff = Some(view);

        let press = |shared: &Rc<RefCell<App>>, code: KeyCode| {
            crate::modes::interactive::handlers::handle_event(
                shared,
                Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            );
        };

        // i：进入输入态，且不进字符
        press(&shared, KeyCode::Char('i'));
        assert!(shared.borrow().giff.as_ref().unwrap().is_input_active());
        assert_eq!(shared.borrow().editor.text(), "");

        // 输入态：字符进主页输入框
        press(&shared, KeyCode::Char('h'));
        press(&shared, KeyCode::Char('i'));
        assert_eq!(shared.borrow().editor.text(), "hi");

        // Esc：只退出输入态，视图还在，草稿保留
        press(&shared, KeyCode::Esc);
        assert!(!shared.borrow().giff.as_ref().unwrap().is_input_active());
        assert!(shared.borrow().giff.is_some(), "Esc 不退出视图");
        assert_eq!(shared.borrow().editor.text(), "hi");

        // q：关闭视图并触发整屏重绘
        press(&shared, KeyCode::Char('q'));
        assert!(shared.borrow().giff.is_none());
        assert!(shared.borrow().full_redraw);
    }
}
