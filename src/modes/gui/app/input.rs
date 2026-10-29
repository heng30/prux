//! 光标、文本选区与输入法（IME）桥接：把 gpui 的输入协议落到 [`Editor`] 上。
//!
//! 拆自 [`super::app`]：只搬输入相关代码，状态仍在父模块。

use super::*;

impl GuiApp {
    /// 鼠标点选输入框：光标落到 (行, 列)，`extend` 为真时扩展选区（拖选/Shift）。
    pub(super) fn set_cursor_at(
        &mut self,
        row: usize,
        col: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        // 行尾后的点击（占位方块之后）夹到行尾
        self.editor.set_cursor(row, col, extend);
        cx.notify();
    }
}

impl GuiApp {
    /// 按窗口坐标找出落在哪一行文字上，返回（行序号, 行内字符下标）。
    ///
    pub(super) fn line_hit(
        &self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
    ) -> Option<(usize, usize)> {
        let hit = self
            .line_bounds
            .borrow()
            .iter()
            .find(|(_, rect)| rect.contains(&position))
            .map(|(index, rect)| (*index, *rect))?;
        let (index, rect) = hit;
        let line = self
            .line_rows
            .borrow()
            .iter()
            .find(|line| line.index == index)
            .cloned()?;
        // 列号按文字元素自己的盒子算：外层行盒含 padding-left / 列表缩进，
        // 用它的左边界换算会让点选落点整体偏右
        let text = self
            .line_text_bounds
            .borrow()
            .get(&index)
            .copied()
            .unwrap_or(rect);
        // 用 gpui 自己的行布局定位：它会按实际宽度折行，y 落在哪一折行也算进去
        let local = text_local(position, text);
        Some((
            index,
            self.char_index(&line, text.size.width, local, window),
        ))
    }
}

impl GuiApp {
    /// 行内某个位置落在第几个字符（字符下标，不是字节）。
    ///
    /// 直接用 gpui 自己的排版结果：`shape_text` 按行宽折行后，每个视觉行带着
    /// 自己的字形位置，`index_for_position` 再把（含 y 的）鼠标位置换成字节下标，
    /// 折行、中文双宽、混合字体都由排版器算，不做任何估算。
    pub(super) fn char_index(
        &self,
        line: &transcript::LineInfo,
        width: Pixels,
        local: gpui::Point<Pixels>,
        window: &mut Window,
    ) -> usize {
        let total = line.text.chars().count();
        if line.text.is_empty() || width <= px(0.) {
            return 0;
        }
        let system = window.text_system().clone();
        let style = gpui::TextStyle {
            font_family: theme::UI_FONT_FAMILY.into(),
            ..Default::default()
        };
        let size = px(line.font_size);
        let runs = [style.to_run(line.text.len())];
        let Ok(lines) = system.shape_text(line.text.clone().into(), size, &runs, Some(width), None)
        else {
            return 0;
        };
        let Some(wrapped) = lines.first() else {
            return 0;
        };
        let line_height = px((line.font_size * TRANSCRIPT_LINE_HEIGHT).round());
        let byte = match wrapped.index_for_position(local, line_height) {
            Ok(byte) | Err(byte) => byte,
        };
        line.text
            .get(..byte.min(line.text.len()))
            .map(|head| head.chars().count())
            .unwrap_or(total)
    }
}

impl GuiApp {
    /// 鼠标按下：建立选区（`extend` = 按住 Shift 接着扩）。
    pub(super) fn start_text_selection(
        &mut self,
        position: gpui::Point<Pixels>,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((line, col)) = self.line_hit(position, window) else {
            if self.text_sel.is_some() {
                self.text_sel = None;
                cx.notify();
            }
            return;
        };
        let anchor = match (self.text_sel, extend) {
            (Some(selection), true) => selection.anchor,
            _ => (line, col),
        };
        self.text_sel = Some(transcript::TextSel {
            anchor,
            focus: (line, col),
        });
        cx.notify();
    }
}

impl GuiApp {
    /// 拖动：把选区终点挪到鼠标位置。
    pub(super) fn extend_text_selection(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = self.text_sel else {
            return;
        };
        let hit = self.line_hit(position, window);
        let Some((line, col)) = hit else {
            return;
        };
        let next = transcript::TextSel {
            anchor: selection.anchor,
            focus: (line, col),
        };
        if next != selection {
            self.text_sel = Some(next);
            cx.notify();
        }
    }
}

impl GuiApp {
    /// IME 写入的目标：覆盖层列表打开时是它的过滤框，否则是输入卡片。
    pub(super) fn ime_target(&self) -> &Editor {
        if let Some(
            SessionDialog::Rename { editor, .. }
            | SessionDialog::ApiKey { editor, .. }
            | SessionDialog::Import { editor }
            | SessionDialog::Label { editor }
            | SessionDialog::BugHint { editor }
            | SessionDialog::Number { editor, .. },
        ) = &self.dialog
        {
            return editor;
        }
        if self.suggest.is_some() {
            return &self.suggest_input;
        }
        match &self.overlay {
            Some(Overlay::List(list)) => &list.query,
            _ => &self.editor,
        }
    }
}

impl GuiApp {
    /// [`Self::ime_target`] 的可变版：写过滤框时把选中项收回第一行。
    pub(super) fn ime_target_mut(&mut self) -> &mut Editor {
        if let Some(
            SessionDialog::Rename { editor, .. }
            | SessionDialog::ApiKey { editor, .. }
            | SessionDialog::Import { editor }
            | SessionDialog::Label { editor }
            | SessionDialog::BugHint { editor }
            | SessionDialog::Number { editor, .. },
        ) = &mut self.dialog
        {
            return editor;
        }
        if self.suggest.is_some() {
            return &mut self.suggest_input;
        }
        match self.overlay.as_mut() {
            Some(Overlay::List(list)) => {
                list.selected = 0;
                &mut list.query
            }
            _ => &mut self.editor,
        }
    }
}

impl GuiApp {
    /// IME 报上来的 UTF-16 区间 → 当前目标的字节区间（没给区间就用手上的组字区间/选区）。
    pub(super) fn ime_range_bytes(&self, range: Option<Range<usize>>) -> Range<usize> {
        let target = self.ime_target();
        let text = target.text();
        let utf16 = range
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| target.selection_utf16().0);
        text_edit::utf16_range_to_bytes(&text, utf16)
    }
}

impl EntityInputHandler for GuiApp {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.ime_target().text();
        let bytes = text_edit::utf16_range_to_bytes(&text, range.clone());
        let from = text_edit::byte_to_utf16(&text, bytes.start);
        let to = text_edit::byte_to_utf16(&text, bytes.end);
        *adjusted_range = Some(from..to);
        Some(text[bytes].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let (range, reversed) = self.ime_target().selection_utf16();
        Some(UTF16Selection { range, reversed })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 行首的 `/` 只是命令面板的触发符，不进输入框
        if text == "/" && self.suggest.is_none() && self.dialog.is_none() && self.overlay.is_none()
        {
            let before = self.editor.text_before_cursor();
            let probe = format!("{before}/");
            if suggest_engine::detect(&probe)
                .is_some_and(|(trigger, _, _)| trigger == Trigger::Command)
            {
                self.suggest_input = Editor::new();
                let start = self.editor.cursor_byte();
                self.rebuild_suggest(Trigger::Command, start, String::new(), cx);
                cx.notify();
                return;
            }
        }
        let bytes = self.ime_range_bytes(range);
        self.ime_target_mut().replace_byte_range(bytes, text);
        self.marked_range = None;
        self.refresh_suggest_any(cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 组字阶段（拼音/假名）不写进输入框：只记住插入点，等提交时 `replace_text_in_range`
        // 才落到文本里。否则输入框会显示一串「nihao」。
        if new_text.is_empty() {
            self.marked_range = None;
        } else if self.marked_range.is_none() {
            let cursor = self.ime_target().selection_utf16().0.start;
            self.marked_range = Some(cursor..cursor);
        }
        let _ = range;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // 不做逐字定位：候选窗贴着输入卡片左下角即可
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}
