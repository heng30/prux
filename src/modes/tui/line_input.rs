//! 单行输入框组件。
//! 统一处理 Shift 层字符映射（kitty 键盘协议下 shift+数字/符号上报为基础键 +
//! SHIFT 修饰）与基本编辑键（光标移动 / 词移动 / 行首行尾 / 删词 / kill-ring），
//! 供会话选择器过滤与重命名、面板过滤等输入框复用，避免各处重复处理 Shift。

use crate::{core::keybindings, utils};
use crossterm::event::{KeyCode, KeyEvent, KeyEventState, KeyModifiers};

/// 按键处理结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAction {
    /// 未消费（调用方处理导航/提交/取消键）
    None,
    /// 文本已修改
    Edited,
    /// 仅光标移动（需要重绘光标位置）
    Moved,
}

/// Shift 层字符映射（US 布局）。
///
/// kitty 键盘协议（REPORT_ALL_KEYS_AS_ESCAPE_CODES）下，shift+数字/符号键按协议
/// 上报为「基础键码 + SHIFT 修饰」而非转换后的符号（如 shift+2 → Char('2')+SHIFT），
/// shift 层转换由应用完成。中文输入法提交的文本走 Paste 事件，不经过此映射；
/// 字母键通常已由终端转换为大写（Char('A') 无修饰），此处仅作兜底。
pub fn shift_layer(c: char) -> char {
    match c {
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '`' => '~',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        c if c.is_ascii_lowercase() => c.to_ascii_uppercase(),
        c => c,
    }
}

/// 可打印字符 → 实际插入字符（Shift 层映射 + CapsLock 处理）；非输入键返回 None。
///
/// kitty 键盘协议（REPORT_ALL_KEYS_AS_ESCAPE_CODES）下终端按「基础键 + 修饰位」上报：
/// CapsLock 以独立状态位 [`KeyEventState::CAPS_LOCK`] 上报，字母不会被终端预先大写
/// → 应用需自行上转大写；Shift+CapsLock 对字母互抵（基础键即最终输出小写），
/// 对数字/符号无影响（仍走 Shift 层映射）。
pub fn printable_char(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::SHIFT) => {
            let out = if key.state.contains(KeyEventState::CAPS_LOCK) && c.is_ascii_alphabetic() {
                c
            } else {
                shift_layer(c)
            };
            Some(out)
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            let out = if key.state.contains(KeyEventState::CAPS_LOCK) {
                c.to_ascii_uppercase()
            } else {
                c
            };
            Some(out)
        }
        _ => None,
    }
}

/// 单行输入框：文本 + 光标（字符索引）+ kill-ring
#[derive(Debug, Clone, Default)]
pub struct InputBox {
    /// 当前已输入的单行文本。
    pub value: String,
    /// 光标位置（字符索引）
    pub cursor: usize,
    /// kill-ring：最近一次剪切/删除的文本，供 yank 粘贴。
    kill_buffer: String,
}

impl InputBox {
    /// 空输入框（文本、光标、kill-ring 均清空）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设值并把光标移到末尾
    pub fn set_value(&mut self, value: &str) {
        self.value = value.to_string();
        self.cursor = self.value.chars().count();
    }

    /// 清空文本、光标与 kill-ring。
    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
        self.kill_buffer.clear();
    }

    /// 文本是否为空（忽略光标位置）。
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// 光标处的字节索引
    fn byte_idx(&self) -> usize {
        self.value
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    /// 光标前缀的显示宽度（光标定位用）
    pub fn cursor_col(&self) -> usize {
        utils::display::display_width(&self.value[..self.byte_idx()])
    }

    /// 水平滚动窗口：返回 (窗口内显示文本, 光标在窗口内的列)。
    /// 文本框总宽超出 max_col 时，窗口右端对齐光标（光标前保留尽可能多的内容）。
    pub fn visible_window(&self, max_col: usize) -> (String, usize) {
        if max_col == 0 {
            return (String::new(), 0);
        }

        let total = self.cursor_col();
        if total <= max_col {
            return (self.value.clone(), total);
        }

        let chars: Vec<char> = self.value.chars().collect();
        let mut start = self.cursor;
        let mut used = 0usize;
        let room = max_col.saturating_sub(1);

        while start > 0 {
            let w = utils::display::char_width(chars[start - 1]);
            if used + w > room {
                break;
            }
            used += w;
            start -= 1;
        }
        (chars[start..].iter().collect(), used)
    }

    /// 在光标处插入整段文本并把光标移到插入内容之后（不改 kill-ring）。
    pub fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let idx = self.byte_idx();
        self.value.insert_str(idx, text);
        self.cursor += text.chars().count();
    }

    /// 在光标处插入单个字符并右移光标。
    fn insert_char(&mut self, c: char) {
        let idx = self.byte_idx();
        self.value.insert(idx, c);
        self.cursor += 1;
    }

    /// 删除光标前一个字符
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let idx = self.byte_idx();
        if let Some((start, _)) = self.value[..idx].char_indices().next_back() {
            self.value.remove(start);
            self.cursor -= 1;
        }
    }

    /// 删除光标后一个字符
    pub fn delete(&mut self) {
        let idx = self.byte_idx();
        if idx < self.value.len() {
            self.value.remove(idx);
        }
    }

    /// 光标左移一个字符（已在行首则不动）。
    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// 光标右移一个字符（已在行尾则不动）。
    pub fn move_right(&mut self) {
        let n = self.value.chars().count();
        if self.cursor < n {
            self.cursor += 1;
        }
    }

    /// 光标移到行首。
    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// 光标移到行尾。
    pub fn end(&mut self) {
        self.cursor = self.value.chars().count();
    }

    /// 前一个词首
    pub fn move_word_left(&mut self) {
        let chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor = i;
    }

    /// 下一个词尾
    pub fn move_word_right(&mut self) {
        let chars: Vec<char> = self.value.chars().collect();
        let n = chars.len();
        let mut i = self.cursor;
        while i < n && !chars[i].is_whitespace() {
            i += 1;
        }
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        self.cursor = i;
    }

    /// 删除光标前一个词（返回被删文本进 kill-ring）
    pub fn delete_word_backward(&mut self) -> String {
        let before = self.cursor;
        let mut i = before;
        let chars: Vec<char> = self.value.chars().collect();
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        if i == before {
            return String::new();
        }
        let removed: String = chars[i..before].iter().collect();
        self.value = chars[..i].iter().chain(chars[before..].iter()).collect();
        self.cursor = i;
        removed
    }

    /// 删除光标后一个词
    pub fn delete_word_forward(&mut self) -> String {
        let start = self.cursor;
        let chars: Vec<char> = self.value.chars().collect();
        let n = chars.len();
        let mut i = start;

        while i < n && !chars[i].is_whitespace() {
            i += 1;
        }
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i == start {
            return String::new();
        }

        let removed: String = chars[start..i].iter().collect();
        self.value = chars[..start].iter().chain(chars[i..].iter()).collect();
        removed
    }

    /// 删除光标前到行首（返回被删文本）
    pub fn kill_to_start(&mut self) -> String {
        if self.cursor == 0 {
            return String::new();
        }
        let removed: String = self.value.chars().take(self.cursor).collect();
        self.value = self.value.chars().skip(self.cursor).collect();
        self.cursor = 0;
        self.kill_buffer = removed.clone();
        removed
    }

    /// 删除光标后到行尾（返回被删文本）
    pub fn kill_to_end(&mut self) -> String {
        let idx = self.byte_idx();
        if idx >= self.value.len() {
            return String::new();
        }
        let removed = self.value[idx..].to_string();
        self.value.truncate(idx);
        self.kill_buffer = removed.clone();
        removed
    }

    /// 粘贴 kill-ring（Ctrl+Y）
    pub fn yank(&mut self) -> bool {
        if self.kill_buffer.is_empty() {
            return false;
        }
        let text = self.kill_buffer.clone();
        self.insert_text(&text);
        true
    }

    /// 处理文本编辑按键。
    ///
    /// 消费：字符输入（含 Shift 层映射）、退格/删除、光标移动、词移动、
    /// Ctrl+A/E/U/K/W/Y、Alt+B/F、Alt+Backspace/Delete。
    /// 不消费：Tab/Enter/Esc/方向上下/PageUp/PageDown/Ctrl+字母功能键
    /// （Ctrl+S/N/P/D/R 等由调用方处理）。
    /// 统一编辑键处理（编辑键由 keybindings 表驱动，其余可打印字符输入）。
    /// Shift 层映射与编辑键均走全局 keybindings。
    pub fn handle_key(&mut self, key: &KeyEvent) -> InputAction {
        let kb = keybindings::get_global();
        if kb.matches(key, "tui.editor.cursorLeft") {
            self.move_left();
            InputAction::Moved
        } else if kb.matches(key, "tui.editor.cursorRight") {
            self.move_right();
            InputAction::Moved
        } else if kb.matches(key, "tui.editor.cursorWordLeft") {
            self.move_word_left();
            InputAction::Moved
        } else if kb.matches(key, "tui.editor.cursorWordRight") {
            self.move_word_right();
            InputAction::Moved
        } else if kb.matches(key, "tui.editor.cursorLineStart") {
            self.home();
            InputAction::Moved
        } else if kb.matches(key, "tui.editor.cursorLineEnd") {
            self.end();
            InputAction::Moved
        } else if kb.matches(key, "tui.editor.deleteToLineStart") {
            self.kill_to_start();
            InputAction::Edited
        } else if kb.matches(key, "tui.editor.deleteToLineEnd") {
            self.kill_to_end();
            InputAction::Edited
        } else if kb.matches(key, "tui.editor.deleteWordBackward") {
            self.delete_word_backward();
            InputAction::Edited
        } else if kb.matches(key, "tui.editor.deleteWordForward") {
            self.delete_word_forward();
            InputAction::Edited
        } else if kb.matches(key, "tui.editor.deleteCharBackward") {
            self.backspace();
            InputAction::Edited
        } else if kb.matches(key, "tui.editor.deleteCharForward") {
            self.delete();
            InputAction::Edited
        } else if kb.matches(key, "tui.editor.yank") {
            if self.yank() {
                InputAction::Edited
            } else {
                InputAction::None
            }
        } else {
            // 可打印字符输入（Shift 层映射 / 普通字符 / CapsLock）
            if let Some(c) = printable_char(key) {
                self.insert_char(c);
                InputAction::Edited
            } else {
                InputAction::None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn shift_layer_maps_us_symbols() {
        let pairs = [
            ('1', '!'),
            ('2', '@'),
            ('3', '#'),
            ('4', '$'),
            ('5', '%'),
            ('6', '^'),
            ('7', '&'),
            ('8', '*'),
            ('9', '('),
            ('0', ')'),
            ('`', '~'),
            ('-', '_'),
            ('=', '+'),
            ('[', '{'),
            (']', '}'),
            ('\\', '|'),
            (';', ':'),
            ('\'', '"'),
            (',', '<'),
            ('.', '>'),
            ('/', '?'),
        ];
        for (raw, shifted) in pairs {
            assert_eq!(shift_layer(raw), shifted, "shift+{} 应为 {}", raw, shifted);
        }
        // 字母：大写（兜底）；已大写/符号/非 ASCII 原样
        assert_eq!(shift_layer('a'), 'A');
        assert_eq!(shift_layer('A'), 'A');
        assert_eq!(shift_layer('@'), '@');
        assert_eq!(shift_layer('中'), '中');
    }

    #[test]
    fn shift_char_input_maps_symbols() {
        // 模拟 kitty 协议：shift+2 上报 Char('2')+SHIFT，应插入 '@'
        let mut b = InputBox::new();
        let action = b.handle_key(&key(KeyCode::Char('2'), KeyModifiers::SHIFT));
        assert_eq!(action, InputAction::Edited);
        assert_eq!(b.value, "@");
        // shift+; → ':'
        let mut b = InputBox::new();
        b.handle_key(&key(KeyCode::Char(';'), KeyModifiers::SHIFT));
        assert_eq!(b.value, ":");
        // 无修饰直接插入
        let mut b = InputBox::new();
        b.handle_key(&key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(b.value, "x");
    }

    #[test]
    fn caps_lock_uppercases_letters() {
        // kitty 协议：CapsLock 以状态位上报，字母为基础小写键 → 应用上转大写
        let ev = |c: char, mods: KeyModifiers| -> KeyEvent {
            KeyEvent::new_with_kind_and_state(
                KeyCode::Char(c),
                mods,
                crossterm::event::KeyEventKind::Press,
                KeyEventState::CAPS_LOCK,
            )
        };
        let mut b = InputBox::new();
        b.handle_key(&ev('a', KeyModifiers::NONE));
        assert_eq!(b.value, "A");
        b.handle_key(&ev('b', KeyModifiers::NONE));
        assert_eq!(b.value, "AB");
    }

    #[test]
    fn caps_lock_interacts_with_shift() {
        let ev = |c: char, mods: KeyModifiers| -> KeyEvent {
            KeyEvent::new_with_kind_and_state(
                KeyCode::Char(c),
                mods,
                crossterm::event::KeyEventKind::Press,
                KeyEventState::CAPS_LOCK,
            )
        };
        // CapsLock+Shift 对字母互抵 → 小写
        let mut b = InputBox::new();
        b.handle_key(&ev('a', KeyModifiers::SHIFT));
        assert_eq!(b.value, "a");
        // CapsLock 不影响数字；CapsLock+Shift+数字仍映射符号
        b.handle_key(&ev('2', KeyModifiers::NONE));
        assert_eq!(b.value, "a2");
        b.handle_key(&ev('2', KeyModifiers::SHIFT));
        assert_eq!(b.value, "a2@");
    }

    #[test]
    fn editing_ops_move_cursor_and_kill() {
        let mut b = InputBox::new();
        b.set_value("hello world");
        b.home();
        // Ctrl+K kill 到行尾
        assert_eq!(
            b.handle_key(&key(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            InputAction::Edited
        );
        assert_eq!(b.value, "");
        assert_eq!(b.kill_buffer, "hello world");
        // Ctrl+Y yank 回来
        b.handle_key(&key(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(b.value, "hello world");
        // Ctrl+U kill 到行首
        b.end();
        b.handle_key(&key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(b.value, "");
        // 光标移动不修改文本
        b.set_value("abc");
        b.home();
        assert_eq!(
            b.handle_key(&key(KeyCode::Right, KeyModifiers::NONE)),
            InputAction::Moved
        );
        assert_eq!(b.cursor, 1);
        // Backspace / Delete
        b.handle_key(&key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(b.value, "bc");
        b.move_right();
        b.handle_key(&key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(b.value, "b");
        // Ctrl+Backspace 删词
        b.clear();
        b.set_value("foo bar");
        b.end();
        b.handle_key(&key(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(b.value, "foo ");
        // 词边界移动：停在词首（光标在空格后，对齐主页 moveWordLeft）
        b.set_value("hello world");
        b.end();
        b.handle_key(&key(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(b.value.chars().take(b.cursor).collect::<String>(), "hello ");
    }

    #[test]
    fn unhandled_keys_return_none() {
        let mut b = InputBox::new();
        b.set_value("abc");
        assert_eq!(
            b.handle_key(&key(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(
            b.handle_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(
            b.handle_key(&key(KeyCode::Up, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(
            b.handle_key(&key(KeyCode::Tab, KeyModifiers::NONE)),
            InputAction::None
        );
        // Ctrl+功能键留给调用方
        assert_eq!(
            b.handle_key(&key(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            InputAction::None
        );
        assert_eq!(b.value, "abc");
    }

    #[test]
    fn cursor_byte_edges_with_cjk() {
        let mut b = InputBox::new();
        b.set_value("中文test");
        b.home();
        b.handle_key(&key(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(b.cursor, 1);
        b.handle_key(&key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(b.value, "中x文test");
    }
}
