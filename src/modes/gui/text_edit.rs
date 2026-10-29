//! 多行文本编辑器：光标、选区、插入/删除、撤销栈。
//!
//! gpui 只提供渲染与按键事件，没有现成的文本输入控件，这里自己实现输入状态机。
//! 列宽按**字符**计（不是字素簇）：中文字符与 emoji 在光标移动上算一个单位，
//! 与 TUI 的 `line_input` 保持一致；等宽渲染下不会错位。

/// 光标位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pos {
    /// 行号（0 起）
    pub row: usize,
    /// 行内字符下标（0 起，等于行长表示行尾）
    pub col: usize,
}

/// 撤销栈里的一份快照。
#[derive(Debug, Clone)]
struct Snapshot {
    /// 文本快照（按行）
    lines: Vec<String>,
    /// 快照时的光标
    cursor: Pos,
    /// 快照时的选区锚点
    anchor: Option<Pos>,
}

/// 撤销栈深度上限：再深就是回忆，不是编辑。
const UNDO_LIMIT: usize = 200;

/// 多行文本编辑器状态。
pub struct Editor {
    /// 文本按行存储，至少一行
    lines: Vec<String>,
    /// 光标位置
    cursor: Pos,
    /// 选区锚点；`None` 表示没有选区
    anchor: Option<Pos>,
    /// 撤销栈（末尾 = 最近一次编辑前）
    undo: Vec<Snapshot>,
    /// 重做栈
    redo: Vec<Snapshot>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    /// 空编辑器（一行空文本，光标在原点）。
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: Pos::default(),
            anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    /// 全文（行间补 `\n`）。
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// 是否没有任何内容。
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// 带缩进发送用：去掉首尾空白后为空则返回 `None`。
    pub fn trimmed(&self) -> Option<String> {
        let text = self.text();
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// 清空内容与历史。
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor = Pos::default();
        self.anchor = None;
        self.undo.clear();
        self.redo.clear();
    }

    /// 所有行（渲染用）。
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// 当前光标。
    pub fn cursor(&self) -> Pos {
        self.cursor
    }

    /// 是否有选区（锚点与光标不重合）。
    pub fn has_selection(&self) -> bool {
        matches!(self.anchor, Some(anchor) if anchor != self.cursor)
    }

    /// 第 `row` 行需要高亮的选区范围 `[start, end)`（字符下标）。
    pub fn selection_row(&self, row: usize) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        if anchor == self.cursor || row >= self.lines.len() {
            return None;
        }
        let (start, end) = ordered(anchor, self.cursor);
        let line_len = self.lines[row].chars().count();
        if row < start.row || row > end.row {
            return None;
        }
        let from = if row == start.row { start.col } else { 0 };
        let to = if row == end.row { end.col } else { line_len };
        (from < to).then_some((from, to))
    }

    /// 插入文本（`\n` 会拆行），替换当前选区。
    pub fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.push_undo();
        self.delete_selection();
        for ch in text.chars() {
            if ch == '\n' {
                self.split_line();
            } else {
                let col = self.cursor.col;
                let mut line = self.lines[self.cursor.row].clone();
                let byte = char_to_byte(&line, col);
                line.insert(byte, ch);
                self.lines[self.cursor.row] = line;
                self.cursor.col += 1;
            }
        }
        self.anchor = None;
    }

    /// 退格：删选区，否则删光标前一个字符（行首则与上一行合并）。
    pub fn backspace(&mut self) {
        if self.has_selection() {
            self.push_undo();
            self.delete_selection();
            self.anchor = None;
            return;
        }
        if self.cursor == Pos::default() {
            return;
        }
        self.push_undo();
        if self.cursor.col == 0 {
            let line = self.lines.remove(self.cursor.row);
            self.cursor.row -= 1;
            self.cursor.col = self.lines[self.cursor.row].chars().count();
            self.lines[self.cursor.row].push_str(&line);
        } else {
            let col = self.cursor.col;
            let mut line = self.lines[self.cursor.row].clone();
            let byte = char_to_byte(&line, col - 1);
            line.remove(byte);
            self.lines[self.cursor.row] = line;
            self.cursor.col -= 1;
        }
    }

    /// 删除键：删选区，否则删光标处字符（行尾则与下一行合并）。
    pub fn delete_forward(&mut self) {
        if self.has_selection() {
            self.push_undo();
            self.delete_selection();
            self.anchor = None;
            return;
        }
        let line_len = self.lines[self.cursor.row].chars().count();
        if self.cursor.col == line_len {
            if self.cursor.row + 1 < self.lines.len() {
                self.push_undo();
                let next = self.lines.remove(self.cursor.row + 1);
                self.lines[self.cursor.row].push_str(&next);
            }
            return;
        }
        self.push_undo();
        let col = self.cursor.col;
        let mut line = self.lines[self.cursor.row].clone();
        let byte = char_to_byte(&line, col);
        line.remove(byte);
        self.lines[self.cursor.row] = line;
    }

    /// 方向键移动；`extend` 为真时按住 Shift（扩展选区）。
    pub fn move_cursor(&mut self, dx: i32, dy: i32, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else if self.has_selection() {
            // 不扩展的移动先折叠选区到目标端（与常见编辑器一致）
            let (start, end) = ordered(self.anchor.unwrap_or(self.cursor), self.cursor);
            let target = if dx < 0 || dy < 0 { start } else { end };
            self.cursor = target;
            self.anchor = None;
            return;
        } else {
            self.anchor = None;
        }

        if dy != 0 {
            let last = self.lines.len() - 1;
            self.cursor.row = if dy < 0 {
                self.cursor.row.saturating_sub(1)
            } else {
                (self.cursor.row + 1).min(last)
            };
            let line_len = self.lines[self.cursor.row].chars().count();
            self.cursor.col = self.cursor.col.min(line_len);
        }
        if dx != 0 {
            let line_len = self.lines[self.cursor.row].chars().count();
            if dx < 0 {
                if self.cursor.col == 0 {
                    if self.cursor.row > 0 {
                        self.cursor.row -= 1;
                        self.cursor.col = self.lines[self.cursor.row].chars().count();
                    }
                } else {
                    self.cursor.col -= 1;
                }
            } else if self.cursor.col < line_len {
                self.cursor.col += 1;
            } else if self.cursor.row + 1 < self.lines.len() {
                self.cursor.row += 1;
                self.cursor.col = 0;
            }
        }
    }

    /// Home：移到行首；`extend` 为真时扩展选区。
    pub fn home(&mut self, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor.col = 0;
    }

    /// End：移到行尾。
    pub fn end(&mut self, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor.col = self.lines[self.cursor.row].chars().count();
    }

    /// 词左移（`alt+left` / `ctrl+left` / `alt+b`）：按空白断词，与 TUI 的
    /// `move_word_left` 同一套规则（先跳空白，再跳到本词词首）。
    pub fn word_left(&mut self, extend: bool) {
        let (row, mut col) = (self.cursor.row, self.cursor.col);
        let chars: Vec<char> = self.lines[row].chars().collect();
        while col > 0 && chars[col - 1].is_whitespace() {
            col -= 1;
        }
        while col > 0 && !chars[col - 1].is_whitespace() {
            col -= 1;
        }
        if col == 0 && self.cursor.col == 0 && row > 0 {
            // 已在行首：跳到上一行行尾
            if extend {
                self.anchor.get_or_insert(self.cursor);
            }
            self.cursor.row = row - 1;
            self.cursor.col = self.lines[row - 1].chars().count();
            return;
        }
        self.set_cursor(row, col, extend);
    }

    /// 词右移（`alt+right` / `ctrl+right` / `alt+f`）：同样按空白断词。
    pub fn word_right(&mut self, extend: bool) {
        let (row, mut col) = (self.cursor.row, self.cursor.col);
        let chars: Vec<char> = self.lines[row].chars().collect();
        while col < chars.len() && !chars[col].is_whitespace() {
            col += 1;
        }
        while col < chars.len() && chars[col].is_whitespace() {
            col += 1;
        }
        if col == chars.len() && self.cursor.col == chars.len() && row + 1 < self.lines.len() {
            // 已在行尾：跳到下一行行首
            if extend {
                self.anchor.get_or_insert(self.cursor);
            }
            self.cursor.row = row + 1;
            self.cursor.col = 0;
            return;
        }
        self.set_cursor(row, col, extend);
    }

    /// 向前删一个词（`ctrl+w` / `alt+backspace`）：先折叠选区，再按 [`Self::word_left`] 删除。
    pub fn delete_word_backward(&mut self) {
        if self.has_selection() {
            self.push_undo();
            self.delete_selection();
            self.anchor = None;
            return;
        }
        let before = self.cursor;
        self.word_left(false);
        let start = self.cursor;
        self.cursor = before;
        self.delete_range(start, before);
    }

    /// 向后删一个词（`alt+d`）。
    pub fn delete_word_forward(&mut self) {
        if self.has_selection() {
            self.push_undo();
            self.delete_selection();
            self.anchor = None;
            return;
        }
        let start = self.cursor;
        self.word_right(false);
        let end = self.cursor;
        self.cursor = start;
        self.delete_range(start, end);
    }

    /// 删到行首（`ctrl+u`）；已在行首时删掉本行的换行。
    pub fn delete_to_line_start(&mut self) {
        let start = Pos {
            row: self.cursor.row,
            col: 0,
        };
        if self.cursor.col == 0 {
            if self.cursor.row > 0 {
                self.push_undo();
                self.delete_forward();
            }
            return;
        }
        self.delete_range(start, self.cursor);
    }

    /// 删到行尾（`ctrl+shift+k`）。
    pub fn delete_to_line_end(&mut self) {
        let end = Pos {
            row: self.cursor.row,
            col: self.lines[self.cursor.row].chars().count(),
        };
        if self.cursor.col == end.col {
            self.push_undo();
            self.delete_forward();
            return;
        }
        self.delete_range(self.cursor, end);
    }

    /// 删除 `start..end` 之间的内容（保证 `start <= end`），光标停在 `start`。
    fn delete_range(&mut self, start: Pos, end: Pos) {
        if start == end {
            return;
        }
        let (start, end) = if ordered(start, end) == (start, end) {
            (start, end)
        } else {
            (end, start)
        };
        self.push_undo();
        let head: String = self.lines[start.row].chars().take(start.col).collect();
        let tail: String = self.lines[end.row].chars().skip(end.col).collect();
        self.lines.splice(start.row..=end.row, [head + &tail]);
        self.cursor = start;
        self.anchor = None;
    }

    /// 鼠标点选：把光标放到 `row` 行第 `col` 个字符（越界自动夹到行尾）。
    ///
    /// `extend` 为真时保留锚点（拖选/Shift 点选）；为假时清掉选区。
    pub fn set_cursor(&mut self, row: usize, col: usize, extend: bool) {
        let row = row.min(self.lines.len().saturating_sub(1));
        let len = self.lines[row].chars().count();
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = Pos {
            row,
            col: col.min(len),
        };
    }

    /// 全选。
    pub fn select_all(&mut self) {
        let last = self.lines.len() - 1;
        self.anchor = Some(Pos { row: 0, col: 0 });
        self.cursor = Pos {
            row: last,
            col: self.lines[last].chars().count(),
        };
    }

    /// 撤销；没有可撤销内容时返回 false。
    pub fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.snapshot());
        self.restore(snapshot);
        true
    }

    /// 重做；没有可重做内容时返回 false。
    pub fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo.pop() else {
            return false;
        };
        self.undo.push(self.snapshot());
        self.restore(snapshot);
        true
    }

    /// 取走选区文本（没有选区时取当前行）。
    pub fn selected_text(&self) -> String {
        let Some(anchor) = self.anchor else {
            return self.lines[self.cursor.row].clone();
        };
        let (start, end) = ordered(anchor, self.cursor);
        if start == end {
            return String::new();
        }
        if start.row == end.row {
            let line = &self.lines[start.row];
            return slice_chars(line, start.col, end.col);
        }
        let mut out = slice_chars(&self.lines[start.row], start.col, usize::MAX);
        for row in start.row + 1..end.row {
            out.push('\n');
            out.push_str(&self.lines[row]);
        }
        out.push('\n');
        out.push_str(&slice_chars(&self.lines[end.row], 0, end.col));
        out
    }

    /// 光标在全文中的字节偏移。
    pub fn cursor_byte(&self) -> usize {
        self.byte_at(self.cursor)
    }

    /// 光标之前的全部文本（补全触发词识别用，不含选区）。
    pub fn text_before_cursor(&self) -> String {
        let text = self.text();
        let end = self.cursor_byte().min(text.len());
        text[..end].to_string()
    }

    /// 光标处的 UTF-16 偏移（IME 上报选区用）。
    pub fn cursor_utf16(&self) -> usize {
        byte_to_utf16(&self.text(), self.byte_at(self.cursor))
    }

    /// 选区的 UTF-16 区间（起点 ≤ 终点）与光标是否落在起点（`reversed`）。
    pub fn selection_utf16(&self) -> (std::ops::Range<usize>, bool) {
        let text = self.text();
        let Some(anchor) = self.anchor else {
            let cursor = self.cursor_utf16();
            return (cursor..cursor, false);
        };
        let (start, end) = ordered(anchor, self.cursor);
        let from = byte_to_utf16(&text, self.byte_at(start));
        let to = byte_to_utf16(&text, self.byte_at(end));
        (from..to, self.cursor == start && anchor != self.cursor)
    }

    /// 用 `text` 替换全文字节区间 `range`，光标落到替换内容末尾（IME 组装/提交用）。
    /// 区间端点不在字符边界时向内收缩。
    pub fn replace_byte_range(&mut self, range: std::ops::Range<usize>, text: &str) {
        let mut full = self.text();
        let mut start = range.start.min(full.len());
        let mut end = range.end.min(full.len()).max(start);
        while !full.is_char_boundary(start) {
            start -= 1;
        }
        while !full.is_char_boundary(end) {
            end -= 1;
        }
        self.push_undo();
        full.replace_range(start..end, text);
        self.lines = full.split('\n').map(str::to_string).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor = self.pos_at(start + text.len());
        self.anchor = None;
    }

    /// 光标位置 → 全文字节偏移。
    fn byte_at(&self, pos: Pos) -> usize {
        let mut byte = 0;
        for (row, line) in self.lines.iter().enumerate() {
            if row == pos.row {
                return byte + char_to_byte(line, pos.col);
            }
            byte += line.len() + 1;
        }
        byte
    }

    /// 全文字节偏移 → 光标位置（越界取末尾）。
    fn pos_at(&self, byte: usize) -> Pos {
        let mut offset = 0;
        for (row, line) in self.lines.iter().enumerate() {
            if byte <= offset + line.len() {
                let col = line[..(byte - offset).min(line.len())].chars().count();
                return Pos { row, col };
            }
            offset += line.len() + 1;
        }
        let row = self.lines.len() - 1;
        Pos {
            row,
            col: self.lines[row].chars().count(),
        }
    }

    /// 复制用：不动光标。
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        }
    }

    /// 还原一份快照。
    fn restore(&mut self, snapshot: Snapshot) {
        self.lines = snapshot.lines;
        self.cursor = snapshot.cursor;
        self.anchor = snapshot.anchor;
    }

    /// 记录一次编辑前的状态（连续输入会合并：同一行且无换行的成批输入只留一次）
    fn push_undo(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// 在光标处换行。
    fn split_line(&mut self) {
        let col = self.cursor.col;
        let line = self.lines[self.cursor.row].clone();
        let byte = char_to_byte(&line, col);
        let head = line[..byte].to_string();
        let tail = line[byte..].to_string();
        self.lines[self.cursor.row] = head;
        self.lines.insert(self.cursor.row + 1, tail);
        self.cursor = Pos {
            row: self.cursor.row + 1,
            col: 0,
        };
    }

    /// 删除当前选区并把光标放到选区起点。
    fn delete_selection(&mut self) {
        let Some(anchor) = self.anchor else {
            return;
        };
        let (start, end) = ordered(anchor, self.cursor);
        if start == end {
            return;
        }
        let head = slice_chars(&self.lines[start.row], 0, start.col);
        let tail = slice_chars(&self.lines[end.row], end.col, usize::MAX);
        self.lines.drain(start.row..=end.row);
        self.lines.insert(start.row, head + &tail);
        self.cursor = start;
        self.anchor = None;
    }
}

/// 两个位置按先后排序。
fn ordered(a: Pos, b: Pos) -> (Pos, Pos) {
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

/// 全文的字节偏移 → UTF-16 偏移（不在字符边界时向下取整）。
pub fn byte_to_utf16(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    text[..offset].encode_utf16().count()
}

/// 全文的 UTF-16 区间 → 字节区间（越界夹到两端，端点落在字符中间时向下取整）。
pub fn utf16_range_to_bytes(text: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let byte_at = |target: usize| {
        let mut utf16 = 0;
        for (byte, ch) in text.char_indices() {
            if utf16 >= target {
                return byte;
            }
            utf16 += ch.len_utf16();
        }
        text.len()
    };
    let start = byte_at(range.start);
    let end = byte_at(range.end).max(start);
    start..end
}

/// 字符下标 → 字节下标（越界时取行尾）。
fn char_to_byte(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map(|(byte, _)| byte)
        .unwrap_or(line.len())
}

/// 按字符区间取子串（`end` 越界时取到行尾）。
fn slice_chars(line: &str, start: usize, end: usize) -> String {
    line.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 鼠标点选把光标放到 (行, 列)，越界夹到行尾；不扩展时清掉选区。
    #[test]
    fn set_cursor_clamps_to_line_end() {
        let mut editor = Editor::new();
        editor.insert("abc\ndefg");
        editor.set_cursor(1, 99, false);
        assert_eq!(editor.cursor(), Pos { row: 1, col: 4 });
        assert!(!editor.has_selection());
        editor.set_cursor(0, 1, false);
        editor.set_cursor(0, 3, true);
        assert!(editor.has_selection());
        assert_eq!(editor.selected_text(), "bc");
    }

    /// 词移动按空白断词（与 TUI 的 line_input 一致）。
    #[test]
    fn word_moves_split_on_whitespace() {
        let mut editor = Editor::new();
        editor.insert("foo, bar 中文");
        editor.word_left(false);
        assert_eq!(editor.cursor(), Pos { row: 0, col: 9 });
        editor.word_left(false);
        assert_eq!(editor.cursor(), Pos { row: 0, col: 5 });
        editor.word_right(false);
        assert_eq!(editor.cursor(), Pos { row: 0, col: 9 });
        // "foo," 算一个词：下一个词首是 5
        editor.home(false);
        editor.word_right(false);
        assert_eq!(editor.cursor(), Pos { row: 0, col: 5 });
    }

    /// 删词 / 删到行首行尾。
    #[test]
    fn deletes_words_and_line_ranges() {
        let mut editor = Editor::new();
        editor.insert("alpha beta gamma");
        editor.delete_word_backward();
        assert_eq!(editor.text(), "alpha beta ");
        editor.delete_to_line_start();
        assert_eq!(editor.text(), "");
        editor.insert("one two");
        editor.set_cursor(0, 4, false);
        editor.delete_to_line_end();
        assert_eq!(editor.text(), "one ");
    }

    /// IME 组装：先替换空区间插入「nih」，再把同一区域换成「你好」。
    #[test]
    fn ime_replace_byte_range() {
        let mut editor = Editor::new();
        let text = editor.text();
        let range = utf16_range_to_bytes(&text, 0..0);
        editor.replace_byte_range(range, "nih");
        assert_eq!(editor.text(), "nih");

        let text = editor.text();
        let range = utf16_range_to_bytes(&text, 0..3);
        editor.replace_byte_range(range, "你好");
        assert_eq!(editor.text(), "你好");
        assert_eq!(editor.cursor_utf16(), 2);
        assert_eq!(editor.selection_utf16().0, 2..2);
    }

    /// 输入文本后光标在末尾，`\n` 会拆行。
    #[test]
    fn insert_and_split_lines() {
        let mut editor = Editor::new();
        editor.insert("ab\ncd");
        assert_eq!(editor.text(), "ab\ncd");
        assert_eq!(editor.cursor(), Pos { row: 1, col: 2 });
    }

    /// 行首退格与上一行合并；行尾删除与下一行合并。
    #[test]
    fn backspace_and_delete_join_lines() {
        let mut editor = Editor::new();
        editor.insert("ab\ncd");
        editor.home(false);
        editor.backspace();
        assert_eq!(editor.text(), "abcd");
        assert_eq!(editor.cursor(), Pos { row: 0, col: 2 });

        editor.home(false);
        editor.delete_forward();
        assert_eq!(editor.text(), "bcd");
    }

    /// 选区删除与撤销/重做。
    #[test]
    fn selection_delete_and_undo() {
        let mut editor = Editor::new();
        editor.insert("hello");
        for _ in 0..4 {
            editor.move_cursor(-1, 0, true);
        }
        assert!(editor.has_selection());
        editor.insert("X");
        assert_eq!(editor.text(), "hX");

        assert!(editor.undo());
        assert_eq!(editor.text(), "hello");
        assert!(editor.redo());
        assert_eq!(editor.text(), "hX");
    }

    /// 行内选区范围按字符计（中文算一个）。
    #[test]
    fn selection_row_counts_chars() {
        let mut editor = Editor::new();
        editor.insert("中文ab");
        for _ in 0..2 {
            editor.move_cursor(-1, 0, true);
        }
        assert_eq!(editor.selection_row(0), Some((2, 4)));
        assert_eq!(editor.selected_text(), "ab");
    }

    /// 上下移动保持列号并夹到行尾；左右可跨行。
    #[test]
    fn vertical_move_clamps_to_line_length() {
        let mut editor = Editor::new();
        editor.insert("abcdef\nxy");
        editor.move_cursor(0, -1, false);
        assert_eq!(editor.cursor(), Pos { row: 0, col: 2 });
        editor.move_cursor(1, 0, false);
        assert_eq!(editor.cursor(), Pos { row: 0, col: 3 });
        editor.end(false);
        editor.move_cursor(1, 0, false);
        assert_eq!(editor.cursor(), Pos { row: 1, col: 0 });
        editor.move_cursor(-1, 0, false);
        assert_eq!(editor.cursor(), Pos { row: 0, col: 6 });
    }
}
