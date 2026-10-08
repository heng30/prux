//! 覆盖层按键/粘贴处理。
//!
//! 优先级：**扩展优先**——每个键先经 `Extension::on_overlay_event` 回传，返回 `true` 即消费；
//! 未消费的键走 TUI 默认语义（Esc 关闭、↑↓/PgUp/PgDn/Home/End 滚动、
//! 有输入行时字符/退格编辑、`i` 激活输入、Enter 提交）。
//!
//! Ctrl+J/K 是 ↑↓ 的等价键（与面板/选择器一致）：raw 模式下 Ctrl+J 就是 `0x0A`，
//! crossterm 会解析成 `Char('j') + CONTROL`，因此这里显式翻成 [`OverlayKey::Down`]/[`Up`]。
//!
//! 输入激活时字符键由 TUI 直接写入本地 [`InputBox`]（不做"每键一回传"），
//! 提交（Enter）才回传扩展——减少跨层往返，且扩展不必实现文本编辑器。

use super::KeyAction;
use crate::{
    core::{
        extensions::OverlayKey,
        keybindings::{self, KeybindingsManager},
    },
    modes::interactive::{app::App, editor::Editor, line_input::printable_char, overlay},
    utils::clipboard::read_clipboard,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 解析用户 keybindings 里的滚动/翻页动作（`tui.select.*`）到 [`OverlayKey`]。
///
/// 查看器/列表的滚动键因此可被用户配置（解析`tui.select.up/down/pageUp/pageDown`）；默认绑定仍是 ↑↓/PgUp/PgDn。
fn scroll_binding(mgr: &KeybindingsManager, key: &KeyEvent) -> Option<OverlayKey> {
    if mgr.matches(key, "tui.select.up") {
        Some(OverlayKey::Up)
    } else if mgr.matches(key, "tui.select.down") {
        Some(OverlayKey::Down)
    } else if mgr.matches(key, "tui.select.pageUp") {
        Some(OverlayKey::PageUp)
    } else if mgr.matches(key, "tui.select.pageDown") {
        Some(OverlayKey::PageDown)
    } else {
        None
    }
}

/// Ctrl+J / Ctrl+K：与 ↓↑ 等价的导航键（面板、会话选择器、设置面板都是这套约定）。
///
/// Ctrl+J 在 raw 模式下就是换行字节（`0x0A`），crossterm 会还原成 `Char('j') + CONTROL`，
/// 所以覆盖层里必须显式翻译——否则它会落到 [`OverlayKey::Other`] 被忽略。
fn ctrl_nav_alias(key: &KeyEvent) -> Option<OverlayKey> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }

    match key.code {
        KeyCode::Char('j') => Some(OverlayKey::Down),
        KeyCode::Char('k') => Some(OverlayKey::Up),
        _ => None,
    }
}

/// 粘贴键：Ctrl+V（`app.clipboard.pasteImage` 绑定）或兼容键 Ctrl+Shift+V。
fn is_paste_key(key: &KeyEvent) -> bool {
    keybindings::get_global().matches(key, "app.clipboard.pasteImage")
        || (matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT))
}

/// crossterm 按键 → 覆盖层按键（与终端库解耦，供扩展匹配）。
pub fn to_overlay_key(key: &KeyEvent) -> OverlayKey {
    // 用户 keybindings 优先：`tui.select.*` 里配置的滚动/翻页键也走同一套语义
    if let Some(k) = scroll_binding(keybindings::get_global(), key) {
        return k;
    }

    // 其次 Ctrl+J/K（默认导航别名；用户可用 keybindings 覆盖成别的语义）
    if let Some(k) = ctrl_nav_alias(key) {
        return k;
    }

    match key.code {
        KeyCode::Up => OverlayKey::Up,
        KeyCode::Down => OverlayKey::Down,
        KeyCode::PageUp => OverlayKey::PageUp,
        KeyCode::PageDown => OverlayKey::PageDown,
        KeyCode::Home => OverlayKey::Home,
        KeyCode::End => OverlayKey::End,
        KeyCode::Enter => OverlayKey::Enter,
        KeyCode::Esc => OverlayKey::Esc,
        KeyCode::Backspace => OverlayKey::Backspace,
        KeyCode::Tab => OverlayKey::Tab,
        _ => match printable_char(key) {
            Some(c) => OverlayKey::Char(c),
            None => OverlayKey::Other,
        },
    }
}

impl App {
    /// 覆盖层按键入口（`self.overlay` 必为 Some）。
    pub fn handle_overlay_key(&mut self, key: &KeyEvent) -> KeyAction {
        let Some(state) = self.overlay.as_ref() else {
            return KeyAction::Continue;
        };
        let has_input = state.view.input.is_some();
        let has_editor = state.has_editor();
        let input_active = state.input_active;
        let mapped = to_overlay_key(key);

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // Ctrl+Shift+C：复制覆盖层选中文本（与主页 `tui.input.copy` 对齐）。
        // 必须先于 Ctrl+C 判定，否则会被当成关闭覆盖层。
        if ctrl && shift && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            self.copy_overlay_selection();
            return KeyAction::Continue;
        }

        // Ctrl+C：与其它模态面（面板/选择器）一致 → 关闭覆盖层
        if ctrl && key.code == KeyCode::Char('c') {
            overlay::close(self);
            return KeyAction::Continue;
        }

        // Ctrl+O / Ctrl+T：与主页同一套全局显示状态（展开工具输出 / thinking 显隐），
        // 这样查看器里 tool 块的 `ctrl+o/alt+click to expand` 提示是真的。
        if ctrl && key.code == KeyCode::Char('o') {
            self.toggle_expand_all();
            let expanded = self.expand_all;
            self.set_status_msg(if expanded {
                "expanded tool output (Ctrl+O to fold)".to_string()
            } else {
                "folded tool output (Ctrl+O to expand)".to_string()
            });
            self.dirty = true;
            return KeyAction::Continue;
        }

        if ctrl && key.code == KeyCode::Char('t') {
            self.toggle_show_thinking();
            let shown = self.show_thinking;
            self.set_status_msg(if shown {
                "thinking blocks visible (Ctrl+T to hide)".to_string()
            } else {
                "thinking blocks hidden (Ctrl+T to show)".to_string()
            });
            self.dirty = true;
            return KeyAction::Continue;
        }

        // Ctrl+V / Ctrl+Shift+V：把剪贴板文本粘进当前聚焦的编辑面。
        // 输入行**未聚焦**时不粘贴（字符键此时属于导航/命令，粘贴不应顺带夺走焦点）。
        if is_paste_key(key) {
            if (has_editor || (has_input && input_active))
                && let Some(text) = read_clipboard()
            {
                self.handle_overlay_paste(&text);
            }

            return KeyAction::Continue;
        }

        // 多行编辑器：文本编辑键由核心接管（扩展靠 Ctrl+S 拿到整段文本、Esc 回退/关闭）。
        if has_editor {
            return self.handle_overlay_editor_key(key);
        }

        // 输入激活：字符/退格本地编辑，Enter 提交，Esc 退出输入态（不关覆盖层）
        if has_input && input_active {
            match &mapped {
                OverlayKey::Char(c) => {
                    if let Some(o) = self.overlay.as_mut() {
                        o.input.insert_text(&c.to_string());
                    }
                    self.dirty = true;
                    return KeyAction::Continue;
                }
                OverlayKey::Backspace => {
                    if let Some(o) = self.overlay.as_mut() {
                        o.input.backspace();
                    }
                    self.dirty = true;
                    return KeyAction::Continue;
                }
                OverlayKey::Enter => {
                    overlay::submit_input(self);
                    return KeyAction::Continue;
                }
                OverlayKey::Esc => {
                    if let Some(o) = self.overlay.as_mut() {
                        o.input_active = false;
                    }
                    overlay::send_key(self, OverlayKey::Esc);
                    self.dirty = true;
                    return KeyAction::Continue;
                }
                _ => {}
            }
        }

        // 扩展优先（列表导航、Enter 打开查看器、s 停止、r 续跑…）
        if overlay::send_key(self, mapped.clone()) {
            return KeyAction::Continue;
        }

        // TUI 默认语义
        match mapped {
            OverlayKey::Esc => overlay::close(self),
            OverlayKey::Up => overlay::scroll_by(self, -1),
            OverlayKey::Down => overlay::scroll_by(self, 1),
            OverlayKey::PageUp => overlay::scroll_by(self, -(self.overlay_page() as isize)),
            OverlayKey::PageDown => overlay::scroll_by(self, self.overlay_page() as isize),
            OverlayKey::Home => overlay::scroll_by(self, isize::MIN / 2),
            OverlayKey::End => overlay::scroll_by(self, isize::MAX / 2),
            // 有输入行但未激活时：只有 `i` 聚焦输入框，且不写入字符。
            // 其余字符键是导航/命令键（`s`/`r`… 已先回传扩展），不应顺带夺走焦点。
            OverlayKey::Char('i') if has_input && !input_active => {
                if let Some(o) = self.overlay.as_mut() {
                    o.input_active = true;
                }
                self.dirty = true;
            }
            OverlayKey::Other | OverlayKey::Tab | OverlayKey::Char(_) => {}
            OverlayKey::Enter
            | OverlayKey::Backspace
            | OverlayKey::Left
            | OverlayKey::Right
            | OverlayKey::Delete => {}
        }
        KeyAction::Continue
    }

    /// 粘贴：多行编辑器写缓冲；输入行**已聚焦**时才写入（未聚焦不粘贴、也不夺焦点）。
    pub fn handle_overlay_paste(&mut self, data: &str) {
        let Some(state) = self.overlay.as_mut() else {
            return;
        };

        if state.view.editor.is_some() {
            state.editor.paste_text(data);
            self.dirty = true;
            return;
        }

        // 只有输入框已聚焦才接受粘贴：未聚焦时按键属于导航/命令（`i` 才是显式聚焦）。
        if state.view.input.is_some() && state.input_active {
            // 输入行是单行控件：粘贴的多行文本压成空格，避免换行破坏渲染。
            let flat = data.replace("\r\n", " ").replace(['\r', '\n'], " ");
            state.input.insert_text(&flat);
            self.dirty = true;
        }
    }

    /// 多行编辑器按键：核心编辑缓冲与光标，Ctrl+S 保存（回传整段文本）。
    ///
    /// 文本编辑键（字符/换行/删除/光标移动/翻页/词移动）由核心接管；其余键（如 Esc、扩展自定义键）
    /// 先经扩展回传，未消费再走 TUI 默认（Esc 关闭）。
    fn handle_overlay_editor_key(&mut self, key: &KeyEvent) -> KeyAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Ctrl+S = 保存（回传整段文本给扩展）
        if ctrl && key.code == KeyCode::Char('s') {
            overlay::submit_editor(self);
            return KeyAction::Continue;
        }

        let is_word_mod = ctrl || alt;
        let handled = match key.code {
            KeyCode::Char(c) if is_word_mod => match c.to_ascii_lowercase() {
                'a' => {
                    self.overlay.as_mut().unwrap().editor.home();
                    true
                }
                'e' => {
                    self.overlay.as_mut().unwrap().editor.end();
                    true
                }
                'k' => {
                    let killed = self.overlay.as_mut().unwrap().editor.kill_line_end();
                    if !killed.is_empty() {
                        self.kill_ring.push(killed);
                    }
                    true
                }
                'u' => {
                    let killed = self.overlay.as_mut().unwrap().editor.kill_line_start();
                    if !killed.is_empty() {
                        self.kill_ring.push(killed);
                    }
                    true
                }
                'w' | 'h' => {
                    self.overlay.as_mut().unwrap().editor.delete_word_backward();
                    true
                }
                'b' if alt => {
                    self.overlay.as_mut().unwrap().editor.move_word_left();
                    true
                }
                'f' if alt => {
                    self.overlay.as_mut().unwrap().editor.move_word_right();
                    true
                }
                _ => false,
            },
            KeyCode::Char(c) => {
                let ed: &mut Editor = &mut self.overlay.as_mut().unwrap().editor;
                if c.is_control() {
                    false
                } else {
                    ed.insert_char(c);
                    true
                }
            }
            KeyCode::Enter => {
                self.overlay.as_mut().unwrap().editor.newline();
                true
            }
            KeyCode::Backspace => {
                let ed = &mut self.overlay.as_mut().unwrap().editor;
                if ctrl || alt {
                    ed.delete_word_backward();
                } else {
                    ed.backspace();
                }
                true
            }
            KeyCode::Delete => {
                let ed = &mut self.overlay.as_mut().unwrap().editor;
                if ctrl || alt {
                    ed.delete_word_forward();
                } else {
                    ed.delete();
                }
                true
            }
            KeyCode::Left => {
                let ed = &mut self.overlay.as_mut().unwrap().editor;
                if is_word_mod {
                    ed.move_word_left();
                } else {
                    ed.move_left();
                }
                true
            }
            KeyCode::Right => {
                let ed = &mut self.overlay.as_mut().unwrap().editor;
                if is_word_mod {
                    ed.move_word_right();
                } else {
                    ed.move_right();
                }
                true
            }
            KeyCode::Up => {
                self.overlay.as_mut().unwrap().editor.move_up();
                true
            }
            KeyCode::Down => {
                self.overlay.as_mut().unwrap().editor.move_down();
                true
            }
            KeyCode::Home => {
                self.overlay.as_mut().unwrap().editor.home();
                true
            }
            KeyCode::End => {
                self.overlay.as_mut().unwrap().editor.end();
                true
            }
            KeyCode::PageUp => {
                self.overlay.as_mut().unwrap().editor.move_page_up();
                true
            }
            KeyCode::PageDown => {
                self.overlay.as_mut().unwrap().editor.move_page_down();
                true
            }
            KeyCode::Tab => {
                let ed = &mut self.overlay.as_mut().unwrap().editor;
                ed.insert_text("  ");
                true
            }
            _ => false,
        };

        if handled {
            self.dirty = true;
            return KeyAction::Continue;
        }

        // 未处理的键（含 Esc）：扩展优先，未消费再走 TUI 默认语义
        let mapped = to_overlay_key(key);
        if overlay::send_key(self, mapped.clone()) {
            return KeyAction::Continue;
        }
        if let OverlayKey::Esc = mapped {
            overlay::close(self);
        }
        KeyAction::Continue
    }

    /// 覆盖层每页滚动行数（视口高 - 2；无覆盖层时为 1）。
    pub(crate) fn overlay_page(&self) -> usize {
        self.overlay.as_ref().map(|o| o.viewport()).unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{
        DockSpan, OverlayEditor, OverlayInput, OverlaySize, OverlayView,
    };

    /// 覆盖层 id 用 1_000_00x 这样的大基址：`next_ui_id()` 从 1 起分配，而扩展测试
    /// （注册了 Overlay 扩展）并行运行时可能恰好持有 id=1..n——字面 id 会和它抢主，
    /// 按键被别的扩展消费，测试便随机挂掉。
    fn view(with_input: bool) -> OverlayView {
        OverlayView {
            title: "t".to_string(),
            lines: (0..30)
                .map(|i| vec![DockSpan::plain(format!("l{i}"))])
                .collect(),
            footer: Vec::new(),
            input: with_input.then(|| OverlayInput {
                label: "steer>".to_string(),
                value: String::new(),
                placeholder: String::new(),
            }),
            editor: None,
            size: OverlaySize::Inline { max_rows: 10 },
            header: Vec::new(),
            messages: Vec::new(),
            selected: None,
            input_focus: false,
        }
    }

    fn press(st: &mut App, code: KeyCode) -> KeyAction {
        st.handle_overlay_key(&KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn press_mod(st: &mut App, code: KeyCode, mods: KeyModifiers) -> KeyAction {
        st.handle_overlay_key(&KeyEvent::new(code, mods))
    }

    /// 滚动/翻页键跟随用户 keybindings 的 `tui.select.*`（默认仍 ↑↓/PgUp/PgDn）。
    #[test]
    fn scroll_keys_follow_user_keybindings() {
        let def = crate::core::keybindings::KeybindingsManager::new_default();
        assert_eq!(
            scroll_binding(&def, &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some(OverlayKey::Up)
        );
        assert_eq!(
            scroll_binding(&def, &KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
            Some(OverlayKey::PageDown)
        );
        assert_eq!(
            scroll_binding(&def, &KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            None
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("keybindings.json"),
            r#"{"tui.select.up":["ctrl+u"],"tui.select.pageDown":["ctrl+f"]}"#,
        )
        .unwrap();
        let mgr = crate::core::keybindings::KeybindingsManager::load(dir.path());
        assert_eq!(
            scroll_binding(
                &mgr,
                &KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)
            ),
            Some(OverlayKey::Up)
        );
        assert_eq!(
            scroll_binding(
                &mgr,
                &KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
            ),
            Some(OverlayKey::PageDown)
        );
    }

    #[test]
    fn key_mapping_covers_navigation_and_chars() {
        assert_eq!(
            to_overlay_key(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            OverlayKey::Up
        );
        assert_eq!(
            to_overlay_key(&KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
            OverlayKey::PageDown
        );
        assert_eq!(
            to_overlay_key(&KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            OverlayKey::Char('s')
        );
        assert_eq!(
            to_overlay_key(&KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            OverlayKey::Other
        );
    }

    /// Ctrl+J/K 是 ↑↓ 的等价键（`/agents schedules` 等列表覆盖层靠它上下移动）。
    #[test]
    fn ctrl_j_and_k_alias_down_and_up() {
        assert_eq!(
            to_overlay_key(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            OverlayKey::Down
        );
        assert_eq!(
            to_overlay_key(&KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            OverlayKey::Up
        );
        // 无 Ctrl 的 j/k 仍是普通字符（扩展自定义键，如 `d` 取消任务）
        assert_eq!(
            to_overlay_key(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            OverlayKey::Char('j')
        );
    }

    /// 列表覆盖层里 Ctrl+J/K 走的是与 ↑↓ 完全相同的默认滚动语义。
    #[test]
    fn ctrl_j_and_k_scroll_like_arrows() {
        let mut st = App::new();
        overlay::open(&mut st, 1_000_009, view(false));
        {
            let o = st.overlay.as_mut().unwrap();
            o.content_rows = 5;
        }

        st.handle_overlay_key(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 1, "Ctrl+J = ↓");

        st.handle_overlay_key(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 2);

        st.handle_overlay_key(&KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 1, "Ctrl+K = ↑");
    }

    #[test]
    fn scrolling_and_escaping_have_default_semantics() {
        let mut st = App::new();
        overlay::open(&mut st, 1_000_001, view(false));
        {
            let o = st.overlay.as_mut().unwrap();
            o.content_rows = 5;
        }
        press(&mut st, KeyCode::Down);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 1);
        press(&mut st, KeyCode::PageDown);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 6);
        press(&mut st, KeyCode::End);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 25);
        press(&mut st, KeyCode::Home);
        assert_eq!(st.overlay.as_ref().unwrap().scroll, 0);

        press(&mut st, KeyCode::Esc);
        assert!(st.overlay.is_none(), "Esc 关闭覆盖层");

        // Ctrl+C 同样关闭
        overlay::open(&mut st, 1_000_002, view(false));
        st.handle_overlay_key(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(st.overlay.is_none());
    }

    #[test]
    fn i_focuses_input_and_enter_submits() {
        let mut st = App::new();
        overlay::open(&mut st, 1_000_003, view(true));
        assert!(!st.overlay.as_ref().unwrap().input_active, "初始不聚焦输入");

        // 其它字符键不聚焦输入（此时它们是导航/命令键）
        press(&mut st, KeyCode::Char('h'));
        assert!(
            !st.overlay.as_ref().unwrap().input_active,
            "普通字符不应让输入框获取焦点"
        );
        assert_eq!(st.overlay.as_ref().unwrap().input.value, "");

        // `i` 显式聚焦输入，且不写入字符
        press(&mut st, KeyCode::Char('i'));
        assert!(st.overlay.as_ref().unwrap().input_active, "`i` 应聚焦输入");
        assert_eq!(
            st.overlay.as_ref().unwrap().input.value,
            "",
            "`i` 本身不入框"
        );

        // 聚焦后字符写入
        press(&mut st, KeyCode::Char('h'));
        press(&mut st, KeyCode::Char('i'));
        assert_eq!(st.overlay.as_ref().unwrap().input.value, "hi");
        // 退格编辑
        press(&mut st, KeyCode::Backspace);
        assert_eq!(st.overlay.as_ref().unwrap().input.value, "h");
        // Esc 只退出输入态，不关覆盖层
        press(&mut st, KeyCode::Esc);
        assert!(!st.overlay.as_ref().unwrap().input_active);
        assert!(st.overlay.is_some());

        // 未聚焦时粘贴被忽略；`i` 聚焦后粘贴才写入
        st.handle_overlay_paste("ignored");
        assert_eq!(
            st.overlay.as_ref().unwrap().input.value,
            "h",
            "未聚焦时粘贴不应写入"
        );
        press(&mut st, KeyCode::Char('i'));
        assert!(st.overlay.as_ref().unwrap().input_active);
        st.handle_overlay_paste("pasted\nlines");
        assert_eq!(
            st.overlay.as_ref().unwrap().input.value,
            "hpasted lines",
            "聚焦后粘贴写入，换行压成空格"
        );

        // Enter 提交：无扩展认领 → 不 panic；提交后输入框清空、退出输入态
        press(&mut st, KeyCode::Enter);
        assert!(st.overlay.is_some());
        assert_eq!(st.overlay.as_ref().unwrap().input.value, "");
        assert!(!st.overlay.as_ref().unwrap().input_active);
    }

    /// Ctrl+V / Ctrl+Shift+V 只有输入框聚焦时才走粘贴路径（未聚焦时忽略、不夺焦点）。
    #[test]
    fn ctrl_v_paste_is_gated_on_input_focus() {
        assert!(is_paste_key(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL
        )));
        assert!(is_paste_key(&KeyEvent::new(
            KeyCode::Char('V'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!is_paste_key(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::NONE
        )));

        let mut st = App::new();
        overlay::open(&mut st, 1_000_010, view(true));
        // 未聚焦：Ctrl+V 不该激活输入
        press_mod(&mut st, KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert!(!st.overlay.as_ref().unwrap().input_active);
        // 聚焦后 Ctrl+V 走粘贴路径（无剪贴板时也不 panic、不退出聚焦）
        press(&mut st, KeyCode::Char('i'));
        assert!(st.overlay.as_ref().unwrap().input_active);
        press_mod(&mut st, KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert!(st.overlay.as_ref().unwrap().input_active);
    }

    /// 多行编辑器：文本编辑键由核心接管（光标/换行/删除/词移动），Ctrl+S 被消费。
    #[test]
    fn editor_keys_edit_the_buffer_in_the_core() {
        let mut st = App::new();
        let mut v = view(false);
        v.editor = Some(OverlayEditor {
            label: "file".to_string(),
            value: String::new(),
            placeholder: String::new(),
            rows: 3,
            generation: 1,
        });
        overlay::open(&mut st, 1_000_007, v);
        // 模拟 sync 后的已装载缓冲（sync 需要已注册的扩展，单测里直接写入）
        {
            let o = st.overlay.as_mut().unwrap();
            o.editor.insert_text("ab");
            o.editor.cursor_line = 0;
            o.editor.cursor_col = 0;
        }
        press(&mut st, KeyCode::Char('X'));
        assert_eq!(st.overlay.as_ref().unwrap().editor.text(), "Xab");
        press(&mut st, KeyCode::Right);
        press(&mut st, KeyCode::Char('Y'));
        assert_eq!(st.overlay.as_ref().unwrap().editor.text(), "XaYb");
        press(&mut st, KeyCode::End);
        press(&mut st, KeyCode::Enter);
        assert_eq!(st.overlay.as_ref().unwrap().editor.lines.len(), 2);
        press(&mut st, KeyCode::Backspace);
        assert_eq!(st.overlay.as_ref().unwrap().editor.lines.len(), 1);
        press(&mut st, KeyCode::Home);
        press(&mut st, KeyCode::Delete);
        assert_eq!(st.overlay.as_ref().unwrap().editor.text(), "aYb");
        // Ctrl+W 删词；Ctrl+S 保存均被消费且不 panic
        press_mod(&mut st, KeyCode::Char('w'), KeyModifiers::CONTROL);
        press_mod(&mut st, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(st.overlay.is_some());
    }

    /// Esc 在多行编辑器里仍先回传扩展（向导靠它回退），未消费才关覆盖层。
    #[test]
    fn editor_escape_falls_back_to_closing() {
        let mut st = App::new();
        let mut v = view(false);
        v.editor = Some(OverlayEditor {
            label: "file".to_string(),
            value: "x".to_string(),
            placeholder: String::new(),
            rows: 3,
            generation: 1,
        });
        overlay::open(&mut st, 1_000_008, v);
        press(&mut st, KeyCode::Esc);
        assert!(st.overlay.is_none(), "无扩展认领时 Esc 关闭覆盖层");
    }
}
