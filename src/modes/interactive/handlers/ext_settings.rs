//! 扩展设置面板键盘处理（占据输入框区域，对齐 `/settings`）。
//!
//! 状态在 [`crate::modes::interactive::ext_settings`]，渲染在
//! `render::ext_settings`。Space/Enter 循环切换（搜索框不捕获空格）；其余文本键进入搜索框（type to search）。

use super::KeyAction;
use crate::{
    core::keybindings,
    modes::interactive::{app::App, line_input::InputAction},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

impl App {
    /// 扩展设置面板键处理：Esc/Ctrl+C 隐藏；↑/↓（Ctrl+K/J）移动；
    /// Space/Enter 循环切换；其余键进入搜索框。
    pub(super) fn handle_ext_settings_key(&mut self, key: &KeyEvent) -> KeyAction {
        let kb = keybindings::get_global();

        if kb.matches(key, "tui.select.cancel") {
            // Esc / Ctrl+C：隐藏面板（已显示的面板按这两个键即关闭），键盘交还编辑器。
            self.ext_settings.close();
            self.dirty = true;
        } else if kb.matches(key, "tui.select.confirm") {
            self.ext_settings.cycle_selected();
            self.dirty = true;
        } else if kb.matches(key, "tui.select.pageUp") {
            self.ext_settings.move_selection(-10);
            self.dirty = true;
        } else if kb.matches(key, "tui.select.pageDown") {
            self.ext_settings.move_selection(10);
            self.dirty = true;
        } else if kb.matches(key, "tui.select.up") || is_ctrl(key, 'k') || is_ctrl(key, 'u') {
            self.ext_settings.move_selection(-1);
            self.dirty = true;
        } else if kb.matches(key, "tui.select.down") || is_ctrl(key, 'j') || is_ctrl(key, 'd') {
            self.ext_settings.move_selection(1);
            self.dirty = true;
        } else if key.code == KeyCode::Char(' ') && key.modifiers.is_empty() {
            self.ext_settings.cycle_selected();
            self.dirty = true;
        } else {
            // type to search：其余键交给搜索输入框（Shift 层映射 / 编辑键统一处理）
            let panel = &mut self.ext_settings;
            match panel.filter.handle_key(key) {
                InputAction::Edited => {
                    panel.recompute();
                    self.dirty = true;
                }
                InputAction::Moved => self.dirty = true,
                InputAction::None => {}
            }
        }
        KeyAction::Continue
    }
}

/// 该按键是否为 Ctrl+`c`（只比对字符与 CONTROL 修饰位，忽略其它修饰键）。
fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c) && key.modifiers.contains(KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::core::extensions::{Extension, ExtensionSetting, ExtensionTool};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::Mutex;

    static VALUE: Mutex<Option<String>> = Mutex::new(None);

    struct FakeExt;

    impl Extension for FakeExt {
        fn name(&self) -> &str {
            "fake-keys-settings"
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }
        fn settings(&self) -> Vec<ExtensionSetting> {
            let value = VALUE
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "a".to_string());
            vec![
                ExtensionSetting::cycle("m", "Mode", "", value, ["a", "b"]),
                ExtensionSetting::cycle("n", "Noise", "", "x", ["x", "y"]),
            ]
        }
        fn apply_setting(&self, key: &str, value: &str) -> Result<(), String> {
            if key != "m" {
                return Err("unknown".into());
            }
            *VALUE.lock().unwrap() = Some(value.to_string());
            Ok(())
        }
    }

    #[test]
    fn space_cycles_enter_cycles_and_esc_hides() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::core::extensions::register_extension(FakeExt);
        *VALUE.lock().unwrap() = None;

        let mut app = App::new();
        app.ext_settings.open("fake-keys-settings");
        assert!(app.ext_settings.visible, "打开后应可见");
        assert_eq!(app.ext_settings.filtered.len(), 2);

        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        let action = app.handle_ext_settings_key(&space);
        assert!(matches!(action, KeyAction::Continue));
        assert_eq!(
            VALUE.lock().unwrap().as_deref(),
            Some("b"),
            "空格应循环切换"
        );

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_ext_settings_key(&enter);
        assert_eq!(VALUE.lock().unwrap().as_deref(), Some("a"), "回车同样循环");

        // Esc / Ctrl+C：面板已显示则直接隐藏
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_ext_settings_key(&esc);
        assert!(!app.ext_settings.visible, "Esc 应隐藏面板");

        app.ext_settings.open("fake-keys-settings");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.handle_ext_settings_key(&ctrl_c);
        assert!(!app.ext_settings.visible, "Ctrl+C 应隐藏面板");

        crate::core::extensions::unregister_extension("fake-keys-settings");
    }

    #[test]
    fn typing_filters_and_space_still_cycles() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::core::extensions::register_extension(FakeExt);
        *VALUE.lock().unwrap() = None;

        let mut app = App::new();
        app.ext_settings.open("fake-keys-settings");

        // 输入 "mode" → 过滤到 Mode，且不影响配置值
        for c in "mode".chars() {
            app.handle_ext_settings_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(app.ext_settings.filter.value, "mode");
        assert_eq!(app.ext_settings.filtered.len(), 1);
        assert_eq!(app.ext_settings.current().unwrap().label, "Mode");

        // 搜索词非空时 Space 仍切换（输入框不捕获空格）
        app.handle_ext_settings_key(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(
            app.ext_settings.filter.value, "mode",
            "Space 不应进入搜索框"
        );
        assert_eq!(
            VALUE.lock().unwrap().as_deref(),
            Some("b"),
            "Space 应循环切换而非输入"
        );

        // Enter 仍切换选中项
        app.handle_ext_settings_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            VALUE.lock().unwrap().as_deref(),
            Some("a"),
            "Enter 同样循环"
        );
        assert_eq!(app.ext_settings.filter.value, "mode");

        crate::core::extensions::unregister_extension("fake-keys-settings");
    }

    #[test]
    fn up_down_wrap_at_both_ends() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::core::extensions::register_extension(FakeExt);

        let mut app = App::new();
        app.ext_settings.open("fake-keys-settings");
        assert_eq!(app.ext_settings.filtered.len(), 2);

        // 顶部 ↑ 回绕到末项，末项 ↓ 回绕到首项
        app.ext_settings.selected = 0;
        app.handle_ext_settings_key(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.ext_settings.selected, 1, "首项 ↑ 应环绕到末项");
        app.handle_ext_settings_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.ext_settings.selected, 0, "末项 ↓ 应环绕到首项");

        crate::core::extensions::unregister_extension("fake-keys-settings");
    }
}
