// 输入编辑器（多行）：光标移动、插入、删除、历史

use regex::Regex;
use std::sync::OnceLock;
use unicode_segmentation::UnicodeSegmentation;

/// 大粘贴判定阈值：超过 10 行或 1000 字符折叠为 marker
const PASTE_MAX_LINES: usize = 10;
/// 大粘贴折叠的字符数阈值：超过则连同行数阈值一起折叠为 marker。
const PASTE_MAX_CHARS: usize = 1000;

/// 大粘贴 marker 匹配：`[paste #1 +14 lines]` 或 `[paste #2 1234 chars]`
static PASTE_MARKER_RE: OnceLock<Regex> = OnceLock::new();

// `[paste #1 +14 lines]` / `[paste #2 1234 chars]`（空格在外层组）
/// 大粘贴 marker 的正则（惰性初始化，全局复用）：匹配 `[paste #N +M lines]`
/// 与 `[paste #N M chars]` 两种形式，捕获组 1 为编号 N。
fn paste_marker_re() -> &'static Regex {
    PASTE_MARKER_RE
        .get_or_init(|| Regex::new(r"\[paste #(\d+)(?: (?:\+\d+ lines|\d+ chars))?\]").unwrap())
}

/// 交互式输入框的多行编辑器：维护文本、光标、历史与撤销栈，并处理大粘贴折叠。
pub struct Editor {
    /// 当前编辑内容，每个元素为一个逻辑行。
    pub lines: Vec<String>,
    /// 光标所在逻辑行下标。
    pub cursor_line: usize,
    pub cursor_col: usize, // 字符（byte 安全用 chars 计数）
    /// 已提交的输入历史，按时间从旧到新排列。
    pub history: Vec<String>,
    /// 历史浏览光标：Some 为当前停留的历史下标，None 表示不在浏览态。
    pub history_index: Option<usize>,
    /// 首次进入历史浏览时保存的草稿（文本 + 光标位置）
    pub history_draft: Option<HistoryDraft>,
    /// 大粘贴原始文本存储：marker `[paste #N ...]` 的 N 对应下标+1
    pastes: Vec<String>,
    /// 撤销栈快照（文本、光标行、光标列、粘贴存储），最多保留 200 条。
    undo_stack: Vec<(Vec<String>, usize, usize, Vec<String>)>,
    /// fish 风格 undo：连续字符输入合并为一个 undo 单元
    last_was_type: bool,
    /// 视觉行宽度（终端 resize 同步；None=未初始化，回退逻辑行移动）
    visual_width: Option<usize>,
    /// sticky column：视觉行垂直移动时光标喜欢的列
    preferred_col: Option<usize>,
    /// 内容版本：任何文本变化（插入/删除/换行/历史载入/撤销/清空）递增。
    /// 仅内容变化才允许刷新候选列表，纯光标移动/翻页不触发。
    pub content_version: u64,
}

/// 进入历史浏览前的草稿快照（文本 + 光标）；Down 越过最新一条时原样恢复。
#[derive(Debug, Clone, Default)]
pub struct HistoryDraft {
    /// 快照时的编辑内容（逐逻辑行）。
    lines: Vec<String>,
    /// 快照时的光标行下标。
    cursor_line: usize,
    /// 快照时的光标列下标（按 char 计数）。
    cursor_col: usize,
}

impl Editor {
    /// 构造空编辑器：单行空文本、光标在 (0,0)，无历史 / 粘贴存储 / 撤销栈。
    pub fn new() -> Self {
        Editor {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            history: Vec::new(),
            history_index: None,
            history_draft: None,
            pastes: Vec::new(),
            undo_stack: Vec::new(),
            content_version: 0,
            last_was_type: false,
            visual_width: None,
            preferred_col: None,
        }
    }

    /// 把当前文本、光标与粘贴存储压入撤销栈（超过 200 条丢弃最旧一条）。
    /// 批量操作前调用一次即可，避免长文本撑爆撤销栈。
    fn mark(&mut self) {
        self.undo_stack.push((
            self.lines.clone(),
            self.cursor_line,
            self.cursor_col,
            self.pastes.clone(),
        ));
        if self.undo_stack.len() > 200 {
            self.undo_stack.remove(0);
        }
    }

    /// 弹出最近一次快照并恢复文本、光标与粘贴存储；栈空时什么也不做。
    /// 恢复后递增 `content_version`，以便候选列表刷新。
    pub fn undo(&mut self) {
        if let Some((lines, line, col, pastes)) = self.undo_stack.pop() {
            self.lines = lines;
            self.cursor_line = line;
            self.cursor_col = col;
            self.pastes = pastes;
            self.content_version += 1;
        }
    }

    /// 以 `\n` 连接各行返回原始文本（不展开 paste marker；提交请用 `expanded_text`）。
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// 展开 `[paste #N ...]` marker 后的完整文本（提交/发送、外部编辑器写出用）。
    /// 存储缺失的 id 保持原样（用户手工输入的同形文本不受影响）。
    pub fn expanded_text(&self) -> String {
        let joined = self.lines.join("\n");

        paste_marker_re()
            .replace_all(&joined, |caps: &regex::Captures| {
                let id: usize = caps[1].parse().unwrap_or(0);
                self.pastes
                    .get(id.wrapping_sub(1))
                    .cloned()
                    .unwrap_or_else(|| caps[0].to_string())
            })
            .to_string()
    }

    /// 所有行均为空时返回 true。
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.is_empty())
    }

    /// 输入行数（含空行）
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// 按 char（而非 byte）统计字符串长度，与 `cursor_col` 的计数口径一致。
    fn char_len(s: &str) -> usize {
        s.chars().count()
    }

    /// 当前逻辑行的 char 长度（光标列的上界）；光标行越界时返回 0。
    pub fn line_char_len(&self) -> usize {
        self.lines
            .get(self.cursor_line)
            .map(|l| Self::char_len(l))
            .unwrap_or(0)
    }

    /// 光标前一个与后一个紧邻字符（同行内，按 char 而非字节）；
    /// 行首的「前一个」与行尾的「后一个」为 `None`。
    pub fn cursor_neighbours(&self) -> (Option<char>, Option<char>) {
        let chars: Vec<char> = self
            .lines
            .get(self.cursor_line)
            .map(|l| l.chars().collect())
            .unwrap_or_default();
        (
            self.cursor_col
                .checked_sub(1)
                .and_then(|i| chars.get(i))
                .copied(),
            chars.get(self.cursor_col).copied(),
        )
    }

    /// 文本内容发生了变化：退出历史浏览并递增内容版本
    /// （开始编辑后恢复候选列表触发；版本号用于区分"输入"与"纯光标移动"）。
    /// 候选补全（`apply_suggestion`）直接改写行文本，同样要经此标记，
    /// 否则随后一次 refresh 会被版本号短路（如 Tab 补全 `/goal ` 后子命令面板不弹）。
    pub(crate) fn content_changed(&mut self) {
        self.history_index = None;
        // 编辑动作结束历史浏览：草稿快照随之作废（否则下次浏览会恢复过期草稿）
        self.history_draft = None;
        self.content_version += 1;
    }

    /// 在光标处插入一个字符并把光标右移；连续字符输入合并为同一个撤销单元。
    pub fn insert_char(&mut self, c: char) {
        self.content_changed();
        // fish 合并：连续打字合并为一个 undo 单元；
        // 中间插入 mark（光标移动/粘贴/删除会重置 last_was_type）
        if !self.last_was_type {
            self.mark();
        }
        self.last_was_type = true;
        self.insert_char_inline(c);
    }

    /// 无 undo 记录的字符插入（供 insert_text/paste_text 批量调用，避免长文本撑爆 undo 栈）
    fn insert_char_inline(&mut self, c: char) {
        let line = &mut self.lines[self.cursor_line];
        let mut chars: Vec<char> = line.chars().collect();
        chars.insert(self.cursor_col.min(chars.len()), c);
        *line = chars.into_iter().collect();
        self.cursor_col += 1;
    }

    /// 无 undo 记录的换行（供 insert_text/paste_text 批量调用）
    fn split_line(&mut self) {
        let line = &mut self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();
        let cut = self.cursor_col.min(chars.len());
        let rest: String = chars[cut..].iter().collect();
        let head: String = chars[..cut].iter().collect();
        *line = head;
        self.lines.insert(self.cursor_line + 1, rest);
        self.cursor_line += 1;
        self.cursor_col = 0;
    }

    /// 插入一段文本：`\n` 拆成新行，`\r` 忽略（\r\n 归一化），其余逐字符插入。
    pub fn insert_text(&mut self, text: &str) {
        self.last_was_type = false;
        self.content_changed();
        self.mark();
        self.insert_raw(text);
    }

    /// 逐字符插入文本但不写撤销栈：`\n` 拆行、`\r` 忽略，供批量插入复用。
    fn insert_raw(&mut self, text: &str) {
        for c in text.chars() {
            if c == '\n' {
                self.split_line();
            } else if c == '\r' {
                // Windows 换行：跳过单独出现的 \r
            } else {
                self.insert_char_inline(c);
            }
        }
    }

    /// 粘贴文本：过滤不可打印字符后，
    /// 超过 10 行或 1000 字符时折叠为 `[paste #N +M lines]` / `[paste #N M chars]`
    /// marker 存入（提交时展开），避免大粘贴占满输入框。
    pub fn paste_text(&mut self, text: &str) {
        // 解码终端 CSI-u 重编码（tmux csi-u 场景：ESC[<n>;5u → 对应字符，如 ESC[106;5u → j），
        // 避免粘贴泄漏 '[' 与尾随可打印字符
        let csi_u = regex::Regex::new(r"\x1b\[([0-9]+);5u").unwrap();
        let text = csi_u.replace_all(text, |caps: &regex::Captures<'_>| {
            let code = caps[1].parse::<u32>().unwrap_or(0);
            char::from_u32(code)
                .map(|c| c.to_string())
                .unwrap_or_default()
        });

        // 过滤控制字符（保留换行），\r或\r\n 由 insert_raw 归一化
        let filtered: String = text
            .chars()
            .filter(|c| *c == '\n' || (*c as u32) >= 32)
            .collect();
        let line_count = filtered.split('\n').count();
        let char_count = filtered.chars().count();

        self.content_changed();
        self.mark();

        if line_count > PASTE_MAX_LINES || char_count > PASTE_MAX_CHARS {
            self.pastes.push(filtered);
            let id = self.pastes.len();
            let marker = if line_count > PASTE_MAX_LINES {
                format!("[paste #{} +{} lines]", id, line_count)
            } else {
                format!("[paste #{} {} chars]", id, char_count)
            };
            for c in marker.chars() {
                self.insert_char_inline(c);
            }
        } else {
            self.insert_raw(&filtered);
        }
    }

    /// 在光标处把当前行拆成两行，并把光标移到新行行首。
    pub fn newline(&mut self) {
        self.content_changed();
        self.mark();
        self.split_line();
    }

    /// 删除光标前的字符：命中 paste marker 时整段删除，否则按字素簇回退（emoji 组合字符）；
    /// 光标在行首时与上一行合并。会改动文本与光标并写撤销栈。
    pub fn backspace(&mut self) {
        self.last_was_type = false;
        self.content_changed();
        self.mark();

        // 光标落在某个完整 paste marker 的字符范围内（含紧贴末尾的缝隙）时整段删除（含存储）。
        // 用整行匹配 + 光标位置判断，而非限定的 prefix：连续粘贴拼接的多个 marker 中，
        // 光标停在哪个 marker 上就删哪个，无需精确停在 marker 末尾或行末。
        if self.cursor_col > 0 {
            let line: String = self.lines[self.cursor_line].clone();
            for m in paste_marker_re().find_iter(&line) {
                let start_chars = line[..m.start()].chars().count();
                let end_chars = line[..m.end()].chars().count();
                if self.cursor_col > start_chars && self.cursor_col <= end_chars {
                    let chars: Vec<char> = line.chars().collect();
                    let mut out: Vec<char> = chars[..start_chars].to_vec();
                    out.extend(chars[end_chars..].iter());
                    self.lines[self.cursor_line] = out.into_iter().collect();
                    self.cursor_col = start_chars;
                    // 存储统一由 renumber_paste_markers 按行内剩余 marker 顺序重建
                    // （不能先 remove(id-1)：数组位移后 renumber 按旧 id 查表会错位）。
                    self.renumber_paste_markers();
                    return;
                }
            }
        }

        if self.cursor_col > 0 {
            // 对齐 pi 字素簇回退：删除光标前整个字素簇（emoji ZWJ/组合字符）
            let grapheme_start = self.grapheme_start_before().unwrap_or(self.cursor_col - 1);
            let line = &mut self.lines[self.cursor_line];
            let mut chars: Vec<char> = line.chars().collect();
            if grapheme_start < chars.len() {
                chars.drain(grapheme_start..self.cursor_col);
            } else {
                chars.remove(self.cursor_col.saturating_sub(1));
            }
            *line = chars.into_iter().collect();
            self.cursor_col = grapheme_start;
        } else if self.cursor_line > 0 {
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            let prev_len = Self::char_len(&self.lines[self.cursor_line]);
            self.lines[self.cursor_line].push_str(&current);
            self.cursor_col = prev_len;
        }
    }

    /// 删除 marker 后按出现顺序重编号（移除 #1 后 #2 → #1），
    /// 存储同步重排，确保 `[paste #N]` 与 pastes 下标始终一致
    fn renumber_paste_markers(&mut self) {
        // 收集行内 marker 的出现顺序（旧 id）
        let mut old_ids: Vec<usize> = Vec::new();
        for line in &self.lines {
            for caps in paste_marker_re().captures_iter(line) {
                if let Ok(id) = caps[1].parse::<usize>() {
                    old_ids.push(id);
                }
            }
        }

        // 按新顺序重建存储
        let mut new_pastes: Vec<String> = Vec::new();
        for old in &old_ids {
            if let Some(t) = self.pastes.get(old.wrapping_sub(1)) {
                new_pastes.push(t.clone());
            }
        }
        self.pastes = new_pastes;

        // 重写 marker 编号与后缀
        let mut next = 0usize;
        for line in &mut self.lines {
            let mut out = String::new();
            let mut last = 0;
            for m in paste_marker_re().find_iter(line) {
                out.push_str(&line[last..m.start()]);
                if let Some(content) = self.pastes.get(next) {
                    next += 1;
                    let lc = content.split('\n').count();
                    let suffix = if lc > PASTE_MAX_LINES {
                        format!("+{} lines", lc)
                    } else {
                        format!("{} chars", content.chars().count())
                    };
                    out.push_str(&format!("[paste #{} {}]", next, suffix));
                }
                last = m.end();
            }
            out.push_str(&line[last..]);
            *line = out;
        }
    }

    /// 删除光标处的字符；光标已在行尾时把下一行并入当前行。写撤销栈。
    pub fn delete(&mut self) {
        self.last_was_type = false;
        self.content_changed();
        self.mark();
        let mut chars: Vec<char> = self.lines[self.cursor_line].chars().collect();
        if self.cursor_col < chars.len() {
            chars.remove(self.cursor_col);
            self.lines[self.cursor_line] = chars.into_iter().collect();
        } else if self.cursor_line + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(&next);
        }
    }

    /// 字符索引集合按字素簇分组（[start_char_idx, end_char_idx) 序列）。
    /// cursor_col 以 char 计数，字素簇移动即跳到相邻字素簇的边界。
    fn grapheme_bounds(&self) -> Vec<(usize, usize)> {
        let line: &str = &self.lines[self.cursor_line];
        let mut bounds = Vec::new();
        let mut start = 0usize;
        for g in line.graphemes(true) {
            let len = g.chars().count();
            bounds.push((start, start + len));
            start += len;
        }
        bounds
    }

    /// 当前位置左侧最近的字素簇起始 char 索引
    fn grapheme_start_before(&self) -> Option<usize> {
        self.grapheme_bounds()
            .into_iter()
            .rev()
            .find(|(_s, e)| *e <= self.cursor_col)
            .map(|(s, _)| s)
    }

    /// 当前位置右侧最近的字素簇结束 char 索引
    fn grapheme_end_after(&self) -> Option<usize> {
        self.grapheme_bounds()
            .into_iter()
            .find(|(s, _)| *s >= self.cursor_col)
            .map(|(_, e)| e)
    }

    /// 光标左移一个字素簇；已在行首时跳到上一行行尾。纯光标移动，不写撤销栈。
    pub fn move_left(&mut self) {
        self.last_was_type = false;
        if self.cursor_col > 0 {
            // emoji ZWJ/组合字符整体移动
            if let Some(start) = self.grapheme_start_before() {
                self.cursor_col = start;
            } else {
                self.cursor_col -= 1;
            }
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.line_char_len();
        }
    }

    /// 移动到前一个词首
    pub fn move_word_left(&mut self) {
        self.mark();
        let chars: Vec<char> = self.lines[self.cursor_line].chars().collect();
        let mut i = self.cursor_col;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor_col = i;
    }

    /// 移动到下一个词尾
    pub fn move_word_right(&mut self) {
        self.mark();
        let chars: Vec<char> = self.lines[self.cursor_line].chars().collect();
        let n = chars.len();
        let mut i = self.cursor_col;
        while i < n && !chars[i].is_whitespace() {
            i += 1;
        }
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        self.cursor_col = i;
    }

    /// 删除光标前一个词；返回被删文本（kill-ring 用）
    pub fn delete_word_backward(&mut self) -> String {
        let before = self.cursor_col;
        let word_start = {
            let chars: Vec<char> = self.lines[self.cursor_line].chars().collect();
            let mut i = self.cursor_col;
            while i > 0 && chars[i - 1].is_whitespace() {
                i -= 1;
            }
            while i > 0 && !chars[i - 1].is_whitespace() {
                i -= 1;
            }
            i
        };
        if word_start == before {
            return String::new();
        }
        self.content_changed();
        self.mark();
        let chars: Vec<char> = self.lines[self.cursor_line].chars().collect();
        let removed: String = chars[word_start..before].iter().collect();
        let mut out: Vec<char> = chars[..word_start].to_vec();
        out.extend(chars[before..].iter());
        self.lines[self.cursor_line] = out.into_iter().collect();
        self.cursor_col = word_start;
        removed
    }

    /// 删除光标后一个词；返回被删文本
    pub fn delete_word_forward(&mut self) -> String {
        let start = self.cursor_col;
        let chars: Vec<char> = self.lines[self.cursor_line].chars().collect();
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
        self.content_changed();
        self.mark();
        let removed: String = chars[start..i].iter().collect();
        let mut out: Vec<char> = chars[..start].to_vec();
        out.extend(chars[i..].iter());
        self.lines[self.cursor_line] = out.into_iter().collect();
        removed
    }

    /// 删除光标前到行首；返回被删文本
    pub fn kill_line_start(&mut self) -> String {
        let col = self.cursor_col;
        if col == 0 {
            return String::new();
        }
        self.content_changed();
        self.mark();
        let chars: Vec<char> = self.lines[self.cursor_line].chars().collect();
        let removed: String = chars[..col].iter().collect();
        let rest: String = chars[col..].iter().collect();
        self.lines[self.cursor_line] = rest;
        self.cursor_col = 0;
        removed
    }

    /// 删除光标后到行尾；返回被删文本
    pub fn kill_line_end(&mut self) -> String {
        let col = self.cursor_col;
        let chars: Vec<char> = self.lines[self.cursor_line].chars().collect();
        if col >= chars.len() {
            return String::new();
        }
        self.content_changed();
        self.mark();
        let removed: String = chars[col..].iter().collect();
        let head: String = chars[..col].iter().collect();
        self.lines[self.cursor_line] = head;
        removed
    }

    /// 光标向上翻一页（10 行 pageUp）
    pub fn move_page_up(&mut self) {
        let target = self.cursor_line.saturating_sub(10);
        self.cursor_line = target;
        self.cursor_col = self.cursor_col.min(self.line_char_len());
    }

    /// 光标向下翻一页（10 行，pageDown）
    pub fn move_page_down(&mut self) {
        self.cursor_line = (self.cursor_line + 10).min(self.lines.len().saturating_sub(1));
        self.cursor_col = self.cursor_col.min(self.line_char_len());
    }

    /// 光标右移一个字素簇；已在行尾时跳到下一行行首。纯光标移动，不写撤销栈。
    pub fn move_right(&mut self) {
        self.last_was_type = false;
        if self.cursor_col < self.line_char_len() {
            // 整体移动
            if let Some(end) = self.grapheme_end_after() {
                self.cursor_col = end;
            } else {
                self.cursor_col += 1;
            }
        } else if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }

    /// 设置视觉行宽度（终端 resize 同步）
    pub fn set_visual_width(&mut self, w: usize) {
        let w = w.max(1);
        if self.visual_width != Some(w) {
            self.visual_width = Some(w);
        }
    }

    /// 将光标所在逻辑行按显示宽度拆成视觉行段，返回 (段起 char 索引, 段宽 char)
    fn visual_segments(&self, line: &str) -> Vec<(usize, usize)> {
        let w = self.visual_width.unwrap_or(usize::MAX);
        let mut segs = Vec::new();
        let mut start = 0usize;
        let mut width = 0usize;
        for c in line.chars() {
            let cw = crate::utils::display::char_width(c);
            if cw == 0 {
                continue;
            }
            if width + cw > w && start < line.chars().count() {
                segs.push((start, width));
                start += width;
                width = 0;
            }
            width += cw;
        }
        segs.push((start, width));
        segs
    }

    /// 视觉坐标 → 光标在当前逻辑行的视觉行号（wrap 段号）
    fn visual_row_of_cursor(&self) -> usize {
        let line = &self.lines[self.cursor_line];
        let segs = self.visual_segments(line);
        let mut row = 0usize;
        for (i, (s, _)) in segs.iter().enumerate() {
            if self.cursor_col >= *s {
                row = i;
            }
        }
        row
    }

    /// 上移光标一格（视觉行优先）。返回是否发生了移动；已在首行首段时返回 false。
    pub fn move_up(&mut self) -> bool {
        self.last_was_type = false;
        let row = self.visual_row_of_cursor();
        if row > 0 {
            // 同一逻辑行的上一视觉行：光标移到该段起点（行首视觉段保留 sticky 列语义简化）
            let segs = self.visual_segments(&self.lines[self.cursor_line]);
            if let Some((start, _)) = segs.get(row - 1).copied() {
                self.cursor_col = start;
            }
            return true;
        }
        if self.cursor_line > 0 {
            self.cursor_line -= 1;
            // sticky column：保持横向意图
            let target = self.preferred_col.unwrap_or(self.cursor_col);
            self.cursor_col = target.min(self.line_char_len());
            return true;
        }
        false
    }

    /// 下移光标一格（视觉行优先）。返回是否发生了移动；已在末行末段时返回 false。
    pub fn move_down(&mut self) -> bool {
        self.last_was_type = false;
        self.preferred_col = Some(self.cursor_col);
        let row = self.visual_row_of_cursor();
        let line_len = self.lines[self.cursor_line].chars().count();
        let segs = self.visual_segments(&self.lines[self.cursor_line]);
        if row + 1 < segs.len() && self.visual_width.is_some() {
            // 同一逻辑行的下一视觉行：移动到该段起点
            if let Some((start, _)) = segs.get(row + 1).copied() {
                self.cursor_col = start;
            }
            return true;
        }
        if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            let target = self.preferred_col.unwrap_or(self.cursor_col);
            self.cursor_col = target.min(self.line_char_len());
            return true;
        }
        if self.visual_width.is_some()
            && row + 1 == segs.len()
            && line_len > 0
            && self.cursor_col < line_len
        {
            // 最后一行最后段：行尾
            self.cursor_col = line_len;
            return true;
        }
        false
    }

    /// ↑：多行内优先移动光标；已在首行行首则切换到上一条历史。
    /// 多行时到达顶部但不在行首会先跳到行首（下一次 ↑ 才切换历史）；
    /// 单行历史保持直接切换，不插入归位步骤。
    pub fn move_up_or_history(&mut self) {
        if self.move_up() {
            return;
        }
        if self.line_count() > 1 && self.cursor_col > 0 {
            self.home();
            return;
        }
        self.history_prev();
    }

    /// ↓：多行内优先移动光标；已在末行行尾则切换到下一条历史。
    /// 多行时到达底部但不在行尾会先跳到行尾（下一次 ↓ 才切换历史）；
    /// 单行历史保持直接切换，不插入归位步骤。
    pub fn move_down_or_history(&mut self) {
        if self.move_down() {
            return;
        }
        if self.line_count() > 1 && self.cursor_col < self.line_char_len() {
            self.end();
            return;
        }
        self.history_next();
    }

    /// 光标移到当前逻辑行行首。
    pub fn home(&mut self) {
        self.cursor_col = 0;
    }

    /// 光标移到当前逻辑行行尾。
    pub fn end(&mut self) {
        self.cursor_col = self.line_char_len();
    }

    /// 清空当前逻辑行的文本并把光标归零；写撤销栈。
    pub fn clear_line(&mut self) {
        self.content_changed();
        self.mark();
        self.lines[self.cursor_line].clear();
        self.cursor_col = 0;
    }

    /// 重置为单个空行，并把光标与粘贴存储一并清空；写撤销栈。
    pub fn clear(&mut self) {
        self.content_changed();
        self.mark();
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.pastes.clear();
    }

    /// 整段替换编辑器内容为 `text`（光标置尾），只留**一步** undo。
    ///
    /// 供 `/history` 候选应用：从历史里选一条就是「整行（整段）替换」，
    /// 在 `clear()` + `insert_text()` 上分两次 `mark()` 会让一次 Ctrl+Z 只退一半。
    pub fn replace_all(&mut self, text: &str) {
        self.content_changed();
        self.mark();
        self.pastes.clear();
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.insert_raw(text);
    }

    /// 删除光标到当前行行尾的内容，光标位置不变；写撤销栈。
    pub fn clear_to_end(&mut self) {
        self.mark();
        // Ctrl+K：删除光标到行尾
        let line = &mut self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();
        *line = chars[..self.cursor_col.min(chars.len())].iter().collect();
    }

    /// 字符前是反斜杠（提示用户用 \ + Enter 换行）
    pub fn should_submit_on_backslash(&self) -> bool {
        let line = self.lines.get(self.cursor_line);
        let Some(line) = line else {
            return false;
        };
        line.chars().nth(self.cursor_col.saturating_sub(1)) == Some('\\')
    }

    /// backslash-Enter：删掉反斜杠并插入换行（shift+enter 回退；不提交）
    pub fn backslash_enter(&mut self) {
        if self.should_submit_on_backslash() {
            self.backspace();
        }
        self.last_was_type = false;
        self.insert_char('\n');
    }

    /// 提交：展开 paste marker 后记录历史并清空
    ///
    /// **历史不做内容过滤**：任何非空输入（含 `/` 斜杠命令与 `!`/`!!` shell 行）都进历史，
    /// 保证用户输入过的东西不会丢。元命令在 ↑/`/history` 里的噪声属于展示层问题，
    /// 不用删数据来解。落盘不在这里做（编辑器不该知道设置与存储路径），由调用方另行处理。
    pub fn submit(&mut self) -> Option<String> {
        let text = self.expanded_text();
        self.clear();
        if text.trim().is_empty() {
            return None;
        }
        self.history.push(text.clone());
        self.history_index = None;
        Some(text)
    }

    /// 把历史截断到最近 `max` 条（`/settings` 改档位时立即生效）。
    ///
    /// `max == 0`（不落盘档位）**不截断**：那个档位只描述持久化策略，
    /// 本次进程内的 ↑/↓ 与 `/history` 照常工作。
    pub fn cap_history(&mut self, max: usize) {
        if max == 0 || self.history.len() <= max {
            return;
        }

        let drop = self.history.len() - max;
        self.history.drain(..drop);
        self.history_index = None; // 截断后下标可能越界；浏览态与草稿一并作废
        self.history_draft = None;
    }

    /// 清空历史列表（`/history @clear` / `@clear-all` 删完文件后同步内存）。
    /// 浏览态与草稿一并以作废：它们指向的条目已经不存在了。
    pub fn clear_history(&mut self) {
        self.history.clear();
        self.history_index = None;
        self.history_draft = None;
    }

    /// 上一条历史：列表为环形（草稿 ↔ 最新…最早 ↔ 草稿）。
    /// 底部草稿 ↑ → 最新一条；顶部最早一条 ↑ → 回绕到底部草稿。
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            // 顶部（最早一条）继续向上 → 回绕到底部草稿
            Some(0) => self.restore_history_draft(),
            Some(i) => {
                let idx = i - 1;
                self.history_index = Some(idx);
                self.load_history(idx);
            }
            None => {
                self.snapshot_history_draft();
                let idx = self.history.len() - 1;
                self.history_index = Some(idx);
                self.load_history(idx);
            }
        }
    }

    /// 下一条历史：最新一条 ↓ → 草稿；底部草稿 ↓ → 回绕到顶部最早一条。
    pub fn history_next(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            // 底部草稿继续向下 → 回绕到顶部最早一条
            None => {
                self.snapshot_history_draft();
                self.history_index = Some(0);
                self.load_history(0);
            }
            Some(i) if i + 1 < self.history.len() => {
                let ni = i + 1;
                self.history_index = Some(ni);
                self.load_history(ni);
            }
            // 最新一条继续向下 → 回到草稿
            Some(_) => self.restore_history_draft(),
        }
    }

    /// 进入历史浏览时保存草稿（仅首次；浏览期间不回写）。
    fn snapshot_history_draft(&mut self) {
        if self.history_draft.is_none() {
            self.history_draft = Some(HistoryDraft {
                lines: self.lines.clone(),
                cursor_line: self.cursor_line,
                cursor_col: self.cursor_col,
            });
        }
    }

    /// 回到草稿：恢复进入历史浏览前的文本与光标；无草稿（如 `clear` 后）则清空。
    fn restore_history_draft(&mut self) {
        self.history_index = None;
        match self.history_draft.take() {
            Some(d) => {
                self.lines = d.lines;

                if self.lines.is_empty() {
                    self.lines = vec![String::new()];
                }

                // 草稿可能因 undo / 外部改写而变短：光标必须落在有效位置
                self.cursor_line = d.cursor_line.min(self.lines.len() - 1);
                self.cursor_col = d
                    .cursor_col
                    .min(Self::char_len(&self.lines[self.cursor_line]));
                self.content_version += 1;
            }
            None => self.clear(),
        }
    }

    /// 把第 `idx` 条历史（含 `\n`）载入编辑器、光标置于末尾；
    /// 递增 `content_version` 但保持 `history_index` 非 None，使刷新逻辑不把它当成激活输入。
    /// `idx` 由调用方保证有效。
    fn load_history(&mut self, idx: usize) {
        let text = self.history[idx].clone();
        self.lines = text.split('\n').map(|s| s.to_string()).collect();
        if self.lines.is_empty() {
            self.lines = vec![String::new()];
        }
        self.cursor_line = self.lines.len() - 1;
        self.cursor_col = Self::char_len(&self.lines[self.cursor_line]);
        // 历史导航也改变文本（保持 history_index 非 None，由 refresh 的浏览检查挡住激活）
        self.content_version += 1;
    }
}

impl Default for Editor {
    /// 等价于 `Editor::new()`（空编辑器）。
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_up_down_moves_cursor_lines() {
        let mut e = Editor::new();
        e.insert_text("a");
        e.newline();
        e.insert_text("b");
        e.newline();
        e.insert_text("c");
        // 光标默认在最后一行（行 2）
        assert_eq!(e.cursor_line, 2);
        e.move_up();
        assert_eq!(e.cursor_line, 1);
        e.move_up();
        assert_eq!(e.cursor_line, 0);
        e.move_up(); // 已在首行：不动
        assert_eq!(e.cursor_line, 0);
        e.move_down();
        assert_eq!(e.cursor_line, 1);
        e.move_down();
        assert_eq!(e.cursor_line, 2);
        e.move_down(); // 已在末行：不动
        assert_eq!(e.cursor_line, 2);
    }

    #[test]
    fn line_count_tracks_newlines() {
        let mut e = Editor::new();
        assert_eq!(e.line_count(), 1);
        e.insert_text("x");
        assert_eq!(e.line_count(), 1);
        e.newline();
        assert_eq!(e.line_count(), 2);
    }

    #[test]
    fn submit_records_every_non_empty_input() {
        let mut e = Editor::new();
        // 元操作也进历史：历史是「用户输入过什么」的无损归档，不做内容过滤
        e.insert_text("/model gpt-5");
        assert!(e.submit().is_some());
        e.insert_text("!ls -la");
        assert!(e.submit().is_some());
        e.insert_text("!!git status");
        assert!(e.submit().is_some());
        e.insert_text("fix the bug");
        assert!(e.submit().is_some());
        assert_eq!(
            e.history,
            vec![
                "/model gpt-5".to_string(),
                "!ls -la".to_string(),
                "!!git status".to_string(),
                "fix the bug".to_string(),
            ],
            "/ 与 ! 开头的行也要进历史"
        );

        // 空/纯空白不记（也没有可提交的内容）
        e.clear();
        assert!(e.submit().is_none());
        e.insert_text("   \n  ");
        assert!(e.submit().is_none());
        assert_eq!(e.history.len(), 4, "空白不入历史: {:?}", e.history);
    }

    #[test]
    fn cap_history_keeps_recent_and_zero_is_unbounded() {
        let mut e = Editor::new();
        for t in ["a", "b", "c", "d", "e"] {
            e.insert_text(t);
            e.submit();
        }
        e.cap_history(2);
        assert_eq!(e.history, vec!["d".to_string(), "e".to_string()]);

        // 0（不落盘档位）不做截断：内存历史照常工作
        e.cap_history(0);
        assert_eq!(e.history.len(), 2);
        e.insert_text("f");
        e.submit();
        e.cap_history(0);
        assert_eq!(e.history.len(), 3, "max=0 不应截断内存历史");
    }

    #[test]
    fn history_navigation_on_single_line() {
        let mut e = Editor::new();
        e.insert_text("hello");
        assert!(e.submit().is_some());
        e.insert_text("world");
        assert!(e.submit().is_some());
        // 单行时空行向上 → 最后一条历史
        e.clear();
        assert!(e.is_empty());
        e.history_prev();
        assert_eq!(e.text(), "world");
        e.history_prev();
        assert_eq!(e.text(), "hello");
        e.history_next();
        assert_eq!(e.text(), "world");
    }

    #[test]
    fn history_wraps_at_both_ends() {
        let mut e = Editor::new();
        e.insert_text("first");
        assert!(e.submit().is_some());
        e.insert_text("second");
        assert!(e.submit().is_some());

        // 空输入框 = 底部草稿：↑ 进最新一条
        e.clear();
        e.history_prev();
        assert_eq!(e.text(), "second", "草稿 ↑ → 最新一条");
        e.history_prev();
        assert_eq!(e.text(), "first", "继续 ↑ → 更早一条");
        // 顶部（最早一条）再按 ↑ → 回绕到底部草稿
        e.history_prev();
        assert_eq!(e.text(), "", "顶部 ↑ 应回绕到草稿");
        assert_eq!(e.history_index, None);
        // 草稿再按 ↑ → 又回到最新一条（环形）
        e.history_prev();
        assert_eq!(e.text(), "second", "草稿再 ↑ 应回到最新一条");

        // ↓ 反向：最新一条 ↓ → 草稿；草稿 ↓ → 回绕到顶部最早一条
        e.history_next();
        assert_eq!(e.text(), "", "最新一条 ↓ 回到草稿");
        e.history_next();
        assert_eq!(e.text(), "first", "草稿 ↓ 应回绕到最早一条");
        e.history_next();
        assert_eq!(e.text(), "second");
        e.history_next();
        assert_eq!(e.text(), "", "走完一圈又回到草稿");
    }

    #[test]
    fn up_down_switch_history_from_multiline_edges() {
        // 先提交多行历史，再提交一条更早的单行历史
        let mut e = Editor::new();
        e.insert_text("older");
        assert!(e.submit().is_some());
        e.insert_text("l1\nl2");
        assert!(e.submit().is_some());

        // 空草稿 ↑ → 最新一条多行历史，光标落在末行行尾
        e.move_up_or_history();
        assert_eq!(e.text(), "l1\nl2");
        assert_eq!((e.cursor_line, e.cursor_col), (1, 2));
        // ↑ → 上移一行（sticky 列，行 0 长度 2）
        e.move_up_or_history();
        assert_eq!((e.cursor_line, e.cursor_col), (0, 2));
        // ↑ → 已在首行但不在行首：先跳到行首，不切历史
        e.move_up_or_history();
        assert_eq!((e.cursor_line, e.cursor_col), (0, 0));
        assert_eq!(e.text(), "l1\nl2", "归位行首不应切换历史");
        // ↑ → 首行行首：切换到上一条历史
        e.move_up_or_history();
        assert_eq!(e.text(), "older");

        // 单行历史直接切换，不插入归位步骤
        e.home();
        e.move_right(); // 非行首
        e.move_up_or_history();
        assert_eq!(e.text(), "", "单行历史光标不在行首也应直接切换");
        assert_eq!(e.history_index, None);

        // ↓ 回到最新多行历史：末行行尾 → 直接归位/切换
        e.move_up_or_history();
        assert_eq!(e.text(), "l1\nl2");
        assert_eq!((e.cursor_line, e.cursor_col), (1, 2));
        // ↓ 已在末行行尾：切换下一条（回到草稿）
        e.move_down_or_history();
        assert_eq!(e.text(), "");
        assert_eq!(e.history_index, None);
    }

    #[test]
    fn down_from_multiline_bottom_goes_to_line_end_first() {
        let mut e = Editor::new();
        e.insert_text("aaa\nbbb");
        assert!(e.submit().is_some());
        e.clear();
        e.move_up_or_history();
        assert_eq!(e.text(), "aaa\nbbb");
        // 手工把光标放到末行中间：↓ 应先跳到行尾，再 ↓ 才切历史
        e.cursor_line = 1;
        e.cursor_col = 1;
        e.move_down_or_history();
        assert_eq!(
            (e.cursor_line, e.cursor_col),
            (1, 3),
            "末行非行尾先跳到行尾"
        );
        assert_eq!(e.text(), "aaa\nbbb", "归位行尾不应切换历史");
        e.move_down_or_history();
        assert_eq!(e.text(), "", "行尾再 ↓ 才切换历史");
    }

    #[test]
    fn history_wrap_is_noop_without_entries() {
        let mut e = Editor::new();
        e.history_prev();
        e.history_next();
        assert_eq!(e.text(), "");
        assert_eq!(e.history_index, None);
    }

    #[test]
    fn history_draft_restores_text_and_cursor() {
        let mut e = Editor::new();
        e.insert_text("old");
        assert!(e.submit().is_some());

        // 草稿：两行，光标停在第二行中间
        e.insert_text("alpha\nbeta");
        e.home();
        e.move_right();
        e.move_right();
        assert_eq!((e.cursor_line, e.cursor_col), (1, 2));

        e.history_prev();
        assert_eq!(e.text(), "old");
        e.history_next();
        assert_eq!(e.text(), "alpha\nbeta", "Down 应原样恢复草稿");
        assert_eq!(
            (e.cursor_line, e.cursor_col),
            (1, 2),
            "草稿光标位置也应恢复"
        );
    }

    #[test]
    fn history_draft_restore_clamps_stale_cursor() {
        // 回归：草稿为空而历史是多行时，回到草稿后光标不得越界（否则下一次输入 panic）
        let mut e = Editor::new();
        e.insert_text("a\nb\nc");
        assert!(e.submit().is_some());
        e.clear();
        e.history_prev();
        assert_eq!(e.text(), "a\nb\nc");
        e.history_next();
        assert_eq!(e.text(), "");
        assert!(e.cursor_line < e.line_count());
        e.insert_char('x');
        assert_eq!(e.text(), "x");
    }

    #[test]
    fn history_draft_snapshot_not_stale_after_edit() {
        let mut e = Editor::new();
        e.insert_text("one");
        assert!(e.submit().is_some());

        // 输入草稿 draft1，↑ 进历史（草稿已快照）
        e.insert_text("draft1");
        e.history_prev();
        assert_eq!(e.text(), "one");

        // 在历史文本上编辑：结束浏览，旧草稿快照作废
        e.insert_char('!');
        assert_eq!(e.text(), "one!");
        assert!(e.history_draft.is_none(), "编辑后应丢弃过期草稿");

        // 再次浏览：快照的是当前内容（one!），而不是过期的 draft1
        e.history_next();
        assert_eq!(e.text(), "one");
        e.history_prev();
        assert_eq!(e.text(), "one!", "应恢复当前草稿而非过期快照");
    }

    #[test]
    fn editing_after_history_prev_exits_browsing() {
        // 对齐 pi：从历史载入 / 开头的文本时不激活候选列表（history_index 非 None），
        // 一旦用户开始编辑则退出历史浏览（history_index = None），恢复候选触发。
        //
        // 直接从列表播种而非 `submit()`：以 `/` 开头的行现在会正常进历史，
        // ↑ 载入后的行为（不激活候选列表）必须正确。
        let mut e = Editor::new();
        e.history.push("/model".to_string());
        e.clear();
        e.history_prev();
        assert_eq!(e.text(), "/model");
        assert!(
            e.history_index.is_some(),
            "历史浏览中 history_index 应为 Some"
        );
        e.insert_char('z');
        assert_eq!(e.history_index, None, "开始编辑后应退出历史浏览");
        e.delete();
        assert_eq!(e.history_index, None);
        e.backspace();
        assert_eq!(e.history_index, None);
    }

    #[test]
    fn insert_text_splits_lines_on_newline() {
        let mut e = Editor::new();
        e.insert_text("line1\nline2\nline3");
        assert_eq!(e.lines, vec!["line1", "line2", "line3"], "\\n 应拆成多行");
        assert_eq!(e.cursor_line, 2);
        assert_eq!(e.cursor_col, 5, "光标在最后一行行末");
        // 光标在末尾 → 右移无效、左移可回退（粘贴后左右键行为正常的回归断言）
        e.move_right();
        assert_eq!(e.cursor_col, 5);
        e.move_left();
        assert_eq!(e.cursor_col, 4);
    }

    #[test]
    fn insert_text_normalizes_crlf_and_keeps_cursor() {
        let mut e = Editor::new();
        e.insert_text("a\r\nb\r");
        assert_eq!(
            e.lines,
            vec!["a", "b"],
            "\\r\\n 归一化为 \\n，独立 \\r 忽略"
        );
        assert_eq!(e.cursor_line, 1);
        assert_eq!(e.cursor_col, 1);
        // 行内不残留零宽控制字符 → 渲染可见字符数与光标 col 一致
        assert_eq!(e.lines[1].chars().count(), 1);
    }

    #[test]
    fn paste_large_multiline_collapses_to_marker() {
        let mut e = Editor::new();
        let big: String = (0..14)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        e.paste_text(&big);
        // 不占满输入框：折叠为单行 marker
        assert_eq!(e.lines.len(), 1, "大粘贴应折叠为单行 marker: {:?}", e.lines);
        assert_eq!(e.lines[0], "[paste #1 +14 lines]");
        // 提交/展开恢复完整文本
        let expanded = e.expanded_text();
        assert_eq!(expanded.lines().count(), 14);
        assert!(expanded.contains("line 13"));
        // submit 返回展开文本并清空存储
        let submitted = e.submit().unwrap();
        assert_eq!(submitted.lines().count(), 14);
        assert!(e.expanded_text().is_empty(), "提交后无残留 marker");
    }

    #[test]
    fn paste_huge_single_line_uses_chars_marker() {
        let mut e = Editor::new();
        e.paste_text(&"x".repeat(1200));
        assert_eq!(e.lines[0], "[paste #1 1200 chars]");
        assert_eq!(e.expanded_text().chars().count(), 1200);
        // 控制字符被过滤（\r 忽略、\n 保留拆行）；小粘贴不折叠
        let mut e2 = Editor::new();
        e2.paste_text("a\r\nb\r\nc");
        assert_eq!(e2.lines, vec!["a", "b", "c"], "小粘贴直接拆行");
        // 40 行明确文本
        let mut e3 = Editor::new();
        e3.paste_text(
            &(0..40)
                .map(|i| format!("x{}y", i))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert_eq!(e3.lines[0], "[paste #1 +40 lines]");
        assert_eq!(e3.expanded_text().lines().count(), 40);
    }

    #[test]
    fn paste_small_text_not_collapsed() {
        let mut e = Editor::new();
        e.paste_text(
            &(0..5)
                .map(|i| format!("line {}", i))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert_eq!(e.lines.len(), 5, "小粘贴不折叠: {:?}", e.lines);
    }

    #[test]
    fn backspace_deletes_marker_atomically_and_renumbers() {
        let mut e = Editor::new();
        let a: String = (0..12)
            .map(|i| format!("a{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let b: String = (0..12)
            .map(|i| format!("b{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        e.paste_text(&a);
        e.newline();
        e.paste_text(&b);
        assert_eq!(
            e.lines,
            vec!["[paste #1 +12 lines]", "[paste #2 +12 lines]"]
        );
        // 光标在第二行 marker 末尾：backspace 整段删除 marker #2
        e.backspace();
        assert_eq!(e.lines, vec!["[paste #1 +12 lines]", ""]);
        assert_eq!(
            e.expanded_text().lines().count(),
            12,
            "#2 存储已移除，只剩 #1"
        );
        // 再删 #1：空行先合并回上一行（光标到 #1 行尾），随后整段删除
        e.backspace();
        assert_eq!(e.lines, vec!["[paste #1 +12 lines]"]);
        e.backspace();
        assert_eq!(e.lines, vec![""]);
        assert!(e.expanded_text().is_empty(), "存储全部清空");
    }

    #[test]
    fn backspace_middle_marker_keeps_following_markers() {
        // 同一行多个 marker：backspace 删除中间的 #1 时，
        // 后续 marker 必须保留并按新顺序重编号，存储同步重排
        let mut e = Editor::new();
        let big1: String = (0..12)
            .map(|i| format!("a{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let big2: String = "x".repeat(1200);
        let big3: String = (0..12)
            .map(|i| format!("c{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        e.paste_text(&big1); // [paste #1 +12 lines]
        e.insert_text(" next "); // 分隔文本（非 marker）
        e.paste_text(&big2); // [paste #2 1200 chars]
        e.insert_text(" next ");
        e.paste_text(&big3); // [paste #3 +12 lines]
        assert_eq!(
            e.lines[0],
            "[paste #1 +12 lines] next [paste #2 1200 chars] next [paste #3 +12 lines]"
        );
        // 光标移到第一个 marker 末尾，backspace 只删除 #1
        e.cursor_col = "[paste #1 +12 lines]".chars().count();
        e.backspace();
        assert_eq!(
            e.lines[0], " next [paste #1 1200 chars] next [paste #2 +12 lines]",
            "删除中间 marker 后其余 marker 应保留并重编号"
        );
        let expanded = e.expanded_text();
        assert!(!expanded.contains("a11"), "被删 marker 的存储应移除");
        assert!(expanded.contains(&"x".repeat(1200)), "#2 存储保留");
        assert!(expanded.contains("c11"), "#3 存储保留并按新编号展开");
    }

    #[test]
    fn backspace_deletes_last_of_multiple_adjacent_markers() {
        // 两次粘贴无分隔拼接：光标在行末 backspace 应整段删除最后一个 marker
        let mut e = Editor::new();
        e.paste_text(&"x".repeat(1200)); // [paste #1 1200 chars]
        assert_eq!(e.lines[0], "[paste #1 1200 chars]");
        e.paste_text(&"y".repeat(1100)); // 直接拼在后面：[paste #2 1100 chars]
        assert_eq!(e.lines[0], "[paste #1 1200 chars][paste #2 1100 chars]");
        e.backspace();
        assert_eq!(
            e.lines[0], "[paste #1 1200 chars]",
            "光标在行末应整段删除第二个 marker，而非只删一个字符"
        );
        let expanded = e.expanded_text();
        assert!(expanded.contains(&"x".repeat(1200)), "#1 存储保留");
        assert!(!expanded.contains("y"), "#2 存储应一并移除");
    }

    #[test]
    fn backspace_targets_marker_by_cursor_position() {
        // 连续粘贴拼接两个 marker，backspace 按光标所在位置删除对应 marker：
        // 光标落在哪个 marker 的字符范围内（含紧贴末尾的缝隙）就删哪个，
        // 无需恰好停在行末。
        let m1 = "[paste #1 1200 chars]";
        let m1_len = m1.chars().count();

        // 光标在第一个 marker 上（末尾缝隙 / 中间 / 开头附近）→ 删第一个
        for col in [m1_len, m1_len - 4, 5] {
            let mut e = Editor::new();
            e.paste_text(&"x".repeat(1200)); // [paste #1 1200 chars]
            e.paste_text(&"y".repeat(1100)); // [paste #2 1100 chars]
            e.cursor_col = col;
            e.backspace();
            assert_eq!(
                e.lines[0], "[paste #1 1100 chars]",
                "光标 col={col} 在第一个 marker 上，应删除第一个 marker"
            );
            assert!(e.expanded_text().contains(&"y".repeat(1100)), "#2 存储保留");
            assert!(!e.expanded_text().contains('x'), "#1 存储应移除");
        }

        // 光标在第二个 marker 上（开头缝隙 / 中间）→ 删第二个
        for col in [m1_len + 1, m1_len + 10, m1_len + 20] {
            let mut e = Editor::new();
            e.paste_text(&"x".repeat(1200));
            e.paste_text(&"y".repeat(1100));
            e.cursor_col = col;
            e.backspace();
            assert_eq!(
                e.lines[0], "[paste #1 1200 chars]",
                "光标 col={col} 在第二个 marker 上，应删除第二个 marker"
            );
            assert!(e.expanded_text().contains(&"x".repeat(1200)), "#1 存储保留");
            assert!(!e.expanded_text().contains('y'), "#2 存储应移除");
        }
    }

    #[test]
    fn renumber_after_paste_at_beginning_keeps_content_order() {
        // 粘贴 A、B 后光标移到行首再粘贴 C：新 marker 拿到递增 id=3，行内 id 顺序为 3,1,2。
        // 删除中间的 id=1 marker 后，剩余 marker 按行内顺序重编号为 1,2，
        // 但内容顺序不变量（C 在前、B 在后），存储同步重排。
        let mut e = Editor::new();
        let a: Vec<String> = (0..12).map(|i| format!("a{}", i)).collect();
        let b: Vec<String> = (0..12).map(|i| format!("b{}", i)).collect();
        let c: Vec<String> = (0..12).map(|i| format!("c{}", i)).collect();
        e.paste_text(&a.join("\n")); // [paste #1 +12 lines]
        e.paste_text(&b.join("\n")); // 拼在后面 [paste #2 +12 lines]
        e.home();
        e.paste_text(&c.join("\n")); // 行首插入 [paste #3 +12 lines]
        assert_eq!(
            e.lines[0], "[paste #3 +12 lines][paste #1 +12 lines][paste #2 +12 lines]",
            "id 按粘贴次数分配，与行内位置无关"
        );
        // 光标落在中间的 id=1 marker 上，backspace 删除它
        let m = "[paste #3 +12 lines]".chars().count();
        e.cursor_col = m + 15;
        e.backspace();
        assert_eq!(e.lines[0], "[paste #1 +12 lines][paste #2 +12 lines]");
        // 内容顺序保持 [C, B]，与删除前的行内相对顺序一致；A 已移除
        let exp: Vec<String> = e
            .expanded_text()
            .split('\n')
            .map(|s| s.to_string())
            .collect();
        assert_eq!(exp.first().map(|s| s.as_str()), Some("c0"), "第一个仍为 C");
        assert_eq!(exp.last().map(|s| s.as_str()), Some("b11"), "第二个仍为 B");
        assert!(!exp.contains(&"a0".to_string()), "A 已移除");
    }

    #[test]
    fn undo_restores_paste_markers() {
        let mut e = Editor::new();
        e.insert_text("head");
        e.newline();
        let big: String = (0..12)
            .map(|i| format!("l{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        e.paste_text(&big);
        assert_eq!(e.lines, vec!["head", "[paste #1 +12 lines]"]);
        assert_eq!(e.expanded_text().lines().count(), 13);
        // undo 一次：回到粘贴前（marker 与存储一并回滚）
        e.undo();
        assert_eq!(e.lines, vec!["head", ""]);
        assert_eq!(e.expanded_text(), "head\n", "无 marker 残留");
    }
}

#[test]
fn undo_merges_consecutive_typing() {
    let mut e = Editor::new();
    // 连续打字 3 个字符 → 一个 undo 单元
    for c in ['a', 'b', 'c'] {
        e.insert_char(c);
    }
    assert_eq!(e.text(), "abc");
    e.undo();
    assert_eq!(e.text(), "", "连续打字应合并为一次 undo");
}

#[test]
fn undo_breaks_on_paste_or_cursor_move() {
    let mut e = Editor::new();
    e.insert_char('a');
    e.insert_text("XYZ"); // paste 重置合并
    e.insert_char('b');
    assert_eq!(e.text(), "aXYZb");
    e.undo();
    assert_eq!(e.text(), "aXYZ", "paste 后输入作为独立 undo 单元");
    e.undo();
    assert_eq!(
        e.text(),
        "a",
        "paste 本身可撤销（clear 后 insert_text 已 mark）"
    );
}

#[test]
fn history_browse_restores_draft() {
    let mut e = Editor::new();
    e.insert_text("my draft");
    e.submit(); // 提交 "my draft" 进历史并清空
    e.insert_text("draft-in-progress");
    e.history_prev();
    assert_eq!(e.text(), "my draft", "进入历史显示旧条目");
    e.history_next();
    assert_eq!(e.text(), "draft-in-progress", "回到尽头恢复草稿");
}

#[test]
fn backslash_enter_replaces_backslash_with_newline() {
    let mut e = Editor::new();
    e.insert_text("line one \\");
    e.move_word_right();
    assert!(e.should_submit_on_backslash(), "光标前反斜杠应触发回退");
    e.backslash_enter();
    let text = e.text();
    assert!(text.contains('\n'), "应插入换行: {text:?}");
    assert!(!text.contains("\\\n"), "反斜杠应被删除: {text:?}");
}

#[test]
fn paste_decodes_csi_u_sequences() {
    let mut e = Editor::new();
    // ESC[106;5u → 'j'（tmux CSI-u 重编码回退，对齐 pi）
    e.paste_text("\u{1b}[106;5u");
    assert_eq!(e.text(), "j", "CSI-u 106;5u 应解码为 j: {:?}", e.text());
}

#[test]
fn cursor_moves_over_grapheme_clusters() {
    let mut e = Editor::new();
    // ZWJ 家庭 emoji（4 个 codepoint 一个字素簇）
    e.insert_text("👨\u{200d}👩\u{200d}👧");
    assert_eq!(e.text().chars().count(), 5, "emoji 由多个 codepoint 组成");
    // 光标在行尾 → 一个 grapheme 移动应回到行首（字素簇整体移动）
    e.move_left();
    assert_eq!(
        e.cursor_col, 0,
        "整个 ZWJ 序列作为一个字素簇移动: {}",
        e.cursor_col
    );
}

#[test]
fn backspace_removes_full_grapheme_cluster() {
    let mut e = Editor::new();
    e.insert_text("a👨\u{200d}👩b");
    e.move_word_left();
    e.move_right();
    // 光标定位到 'b' 后
    for _ in 0..3 {
        e.move_right();
    }
    e.backspace(); // 删 'b'
    assert_eq!(e.text(), "a👨\u{200d}👩", "删除单个字符");
    e.backspace(); // 应删整个 ZWJ 序列
    assert_eq!(e.text(), "a", "backspace 应删除整个字素簇: {:?}", e.text());
}

#[test]
fn visual_line_navigation_moves_across_wrapped_segments() {
    let mut e = Editor::new();
    e.set_visual_width(5);
    e.insert_text("abcdef");
    // 宽度5 → 视觉段 ["abcde", "f"]
    e.home();
    assert_eq!(e.cursor_col, 0);
    // 行尾（第 2 视觉段）
    e.end();
    assert_eq!(e.cursor_col, 6);
    // move_up：从第 2 视觉段回到第 1 段（段起点 0）
    e.move_up();
    assert_eq!(e.cursor_col, 0, "回到上一视觉段起点");
    // move_down：回到第 2 段起点（5）
    e.move_down();
    assert_eq!(e.cursor_col, 5, "前进到下一视觉段起点");
}

#[test]
fn visual_navigation_falls_back_to_logical_without_width() {
    let mut e = Editor::new();
    e.insert_text("ab\ncd");
    e.home();
    e.move_down();
    assert_eq!(e.cursor_line, 1, "无宽度时按逻辑行移动");
    assert_eq!(e.cursor_col, 0);
}
