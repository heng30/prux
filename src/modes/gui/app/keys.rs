//! 键盘分发：全局快捷键、覆盖层与对话框的按键处理。
//!
//! 拆自 [`super::app`]：只搬按键分派，动作执行见 [`super::actions`]。

use super::*;

impl GuiApp {
    /// 键盘入口：分发完统一重绘一次。
    ///
    /// 打开覆盖层 / 切面板的分支原本各自 `notify`，漏一处就表现为「按了没反应、
    /// 要再敲一个字符才出来」，所以重绘收在这一层。
    pub(super) fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_key(event, cx);
        self.refresh_suggest(cx);
        cx.notify();
    }
    /// 键盘分发：覆盖层打开时只喂覆盖层，否则走输入框。
    pub(super) fn dispatch_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = event.keystroke.clone();
        let primary = keystroke.modifiers.control || keystroke.modifiers.platform;
        let shift = keystroke.modifiers.shift;

        if primary && keystroke.key == "q" {
            cx.quit();
            return;
        }

        if self.dialog.is_some() {
            if keystroke.key == "enter" {
                self.confirm_dialog(cx);
            } else {
                self.handle_dialog_key(&keystroke.key, shift);
            }
            cx.notify();
            return;
        }

        if self.session_menu.is_some() {
            // 菜单只认 Esc，其余键先收菜单再照常处理
            self.session_menu = None;
            if keystroke.key == "escape" {
                cx.notify();
                return;
            }
        }

        // Shift+Tab：循环 thinking 级别（同 TUI 的 `app.thinking.cycle`）；
        // 放在补全面板之前，面板开着时也能切
        if shift && keystroke.key == "tab" {
            self.cycle_thinking(cx);
            cx.notify();
            return;
        }

        if self.suggest.is_some() && self.dispatch_suggest_key(&keystroke, shift, cx) {
            cx.notify();
            return;
        }

        // Ctrl-S 先于覆盖层分发：技能/扩展面板是覆盖层，否则按键被它吃掉
        if primary && !shift && keystroke.key == "s" && self.save_panel_drafts(cx) {
            cx.notify();
            return;
        }

        if self.overlay.is_some() {
            self.handle_overlay_key(&keystroke.key, shift, cx);
            cx.notify();
            return;
        }

        if self.dispatch_editor_key(&keystroke, primary, shift, cx) {
            cx.notify();
            return;
        }

        if primary && self.dispatch_panel_shortcut(&keystroke, shift, cx) {
            return;
        }

        self.dispatch_text_key(&keystroke, shift, cx);
    }

    /// 补全面板开着时的按键：面板输入框自己吃键，`true` 表示已消费。
    pub(super) fn dispatch_suggest_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        shift: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(suggest) = self.suggest.clone() {
            match keystroke.key.as_str() {
                "escape" => {
                    self.suggest_dismissed = Some(suggest.start);
                    self.suggest = None;
                    cx.notify();
                    return true;
                }
                "up" => {
                    self.move_suggestion(-1);
                    cx.notify();
                    return true;
                }
                "down" => {
                    self.move_suggestion(1);
                    cx.notify();
                    return true;
                }
                "tab" if !suggest.items.is_empty() => {
                    self.apply_suggestion(cx);
                    cx.notify();
                    return true;
                }
                // 有候选时 Enter 先采纳；shell 面板没有候选，Enter 直接执行
                "enter" if suggest.trigger != Trigger::Shell && !suggest.items.is_empty() => {
                    self.apply_suggestion(cx);
                    cx.notify();
                    return true;
                }
                // 退格/方向键改的是面板输入框，不动主输入框
                "backspace" if suggest.trigger != Trigger::Shell => {
                    if self.suggest_input.text().is_empty() {
                        // 面板输入已空：收起面板，触发符留在输入框里由用户自己删
                        self.suggest_dismissed = Some(suggest.start);
                        self.suggest = None;
                    } else {
                        self.suggest_input.backspace();
                        self.refresh_suggest_from_input(cx);
                    }
                    cx.notify();
                    return true;
                }
                "delete" if suggest.trigger != Trigger::Shell => {
                    self.suggest_input.delete_forward();
                    self.refresh_suggest_from_input(cx);
                    cx.notify();
                    return true;
                }
                "left" | "right" | "home" | "end" if suggest.trigger != Trigger::Shell => {
                    match keystroke.key.as_str() {
                        "left" => self.suggest_input.move_cursor(-1, 0, shift),
                        "right" => self.suggest_input.move_cursor(1, 0, shift),
                        "home" => self.suggest_input.home(shift),
                        _ => self.suggest_input.end(shift),
                    }
                    cx.notify();
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// 编辑器键位（与 TUI 的 `tui.editor.*` 对齐）：`true` 表示已消费。
    pub(super) fn dispatch_editor_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        primary: bool,
        shift: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        // ── 编辑器键位：与 TUI 的 tui.editor.* 对齐 ─────────────
        let alt = keystroke.modifiers.alt;
        let key = keystroke.key.as_str();
        if (alt && (key == "left" || key == "b")) || (primary && key == "left") {
            self.editor.word_left(shift);
            cx.notify();
            return true;
        }
        if (alt && (key == "right" || key == "f")) || (primary && key == "right") {
            self.editor.word_right(shift);
            cx.notify();
            return true;
        }
        if primary && !shift && key == "b" {
            self.editor.move_cursor(-1, 0, false);
            cx.notify();
            return true;
        }
        if primary && !shift && key == "a" {
            self.editor.home(false);
            cx.notify();
            return true;
        }
        if primary && !shift && key == "e" {
            self.editor.end(false);
            cx.notify();
            return true;
        }
        if primary && !shift && key == "u" {
            self.editor.delete_to_line_start();
            cx.notify();
            return true;
        }
        if primary && shift && key == "k" {
            self.editor.delete_to_line_end();
            cx.notify();
            return true;
        }
        if (primary && (key == "w" || key == "backspace")) || (alt && key == "backspace") {
            self.editor.delete_word_backward();
            cx.notify();
            return true;
        }
        if alt && key == "d" {
            self.editor.delete_word_forward();
            cx.notify();
            return true;
        }
        if primary && key == "j" {
            self.editor.insert("\n");
            cx.notify();
            return true;
        }
        if primary && shift && key == "c" {
            // 对话流里有选区就先复制它，否则复制输入框里的选中文字
            let picked = self
                .text_sel
                .map(|selection| transcript::selected_text(selection, &self.line_rows.borrow()))
                .filter(|text| !text.is_empty());
            let text = picked.unwrap_or_else(|| self.editor.selected_text());
            if !text.is_empty() {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                self.toast("Copied selection", ToastKind::Info, cx);
            }
            return true;
        }
        if primary && !shift && key == "c" {
            // 对话流里有选区时 Ctrl-C 复制它，否则沿用「清空输入框」
            if let Some(text) = self
                .text_sel
                .map(|selection| transcript::selected_text(selection, &self.line_rows.borrow()))
                .filter(|text| !text.is_empty())
            {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                self.toast("Copied selection", ToastKind::Info, cx);
                return true;
            }
            self.editor.clear();
            cx.notify();
            return true;
        }
        if primary && !shift && key == "d" && self.editor.is_empty() {
            cx.quit();
            return true;
        }
        false
    }

    /// Ctrl 组合键：打开面板 / 切换窗口部件。`true` 表示已消费。
    pub(super) fn dispatch_panel_shortcut(
        &mut self,
        keystroke: &gpui::Keystroke,
        shift: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        match keystroke.key.as_str() {
            "k" => {
                self.open_command_palette(cx);
                return true;
            }
            // Ctrl+P / Ctrl+Shift+P 直接切上一个/下一个模型（同 TUI 的模型循环）
            "p" => {
                self.cycle_model(if shift { -1 } else { 1 }, cx);
                return true;
            }
            "/" => {
                self.open_model_picker(cx);
                return true;
            }
            "t" => {
                self.open_thinking_picker(cx);
                return true;
            }
            "o" => {
                self.open_session_picker(cx);
                return true;
            }
            "g" => {
                self.open_appearance_picker(cx);
                return true;
            }
            "u" if shift => {
                self.open_info(InfoKind::Usage, cx);
                return true;
            }
            "i" => {
                self.open_skills_panel(cx);
                return true;
            }
            "," => {
                self.open_settings_panel(cx);
                return true;
            }
            "b" if shift => {
                self.show_sidebar = !self.show_sidebar;
                cx.notify();
                return true;
            }
            "e" if shift => {
                self.toggle_panel(None, cx);
                cx.notify();
                return true;
            }
            "l" => {
                self.expand_tools = !self.expand_tools;
                cx.notify();
                return true;
            }
            "n" => {
                _ = self.cmd_tx.send(AgentCommand::NewSession);
                self.transcript.clear();
                self.flow_height.set(None);
                self.reload_sessions(cx);
                cx.notify();
                return true;
            }
            "=" | "+" | "-" | "0" => {
                let current = theme::ui_scale();
                let scale = match keystroke.key.as_str() {
                    "0" => 1.0,
                    "-" => current - theme::UI_SCALE_STEP,
                    _ => current + theme::UI_SCALE_STEP,
                };
                _ = settings_manager::write_settings_ui_scale(Some(theme::set_ui_scale(scale)));
                cx.notify();
                return true;
            }
            "s" if !shift => {
                self.save_panel_drafts(cx);
                return true;
            }
            "a" if shift => {
                self.editor.select_all();
                cx.notify();
                return true;
            }
            "z" => {
                _ = if shift {
                    self.editor.redo()
                } else {
                    self.editor.undo()
                };
                cx.notify();
                return true;
            }
            "y" => {
                self.editor.redo();
                cx.notify();
                return true;
            }
            "v" => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.editor.insert(&text);
                }
                cx.notify();
                return true;
            }
            _ => {}
        }
        false
    }

    /// 无修饰的文字编辑键（Enter / Esc / 方向键 / 退格）。
    pub(super) fn dispatch_text_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        match keystroke.key.as_str() {
            "enter" if shift => {
                self.editor.insert("\n");
                cx.notify();
            }
            "enter" => {
                self.submit(cx);
                cx.notify();
            }
            "escape" => {
                if self.busy {
                    // worker 直接 drop 回合 future，agent 不会再发 `agent_settled`，
                    // 忙状态只能在本地复位，中断提示也只能自己补（同 TUI 的硬中止）
                    _ = self.cmd_tx.send(AgentCommand::Abort);
                    self.queued = 0;
                    self.busy = false;
                    self.transcript.abort();
                    self.scroll_to_bottom();
                } else if self.panel.visible {
                    // 空闲时 Esc 先收右侧面板（giff 等）——之前面板没有关闭途径
                    self.panel.visible = false;
                }
                cx.notify();
            }
            "backspace" => {
                self.editor.backspace();
                cx.notify();
            }
            "delete" => {
                self.editor.delete_forward();
                cx.notify();
            }
            "left" => {
                self.editor.move_cursor(-1, 0, shift);
                cx.notify();
            }
            "right" => {
                self.editor.move_cursor(1, 0, shift);
                cx.notify();
            }
            "up" => {
                self.editor.move_cursor(0, -1, shift);
                cx.notify();
            }
            "down" => {
                self.editor.move_cursor(0, 1, shift);
                cx.notify();
            }
            "home" => {
                self.editor.home(shift);
                cx.notify();
            }
            "end" => {
                self.editor.end(shift);
                cx.notify();
            }
            "tab" => {
                self.editor.insert("  ");
                cx.notify();
            }
            // 可打印字符由平台经 IME 句柄（`replace_text_in_range`）送进来，
            // 这里再插一次就会变成「敲一个 a 出现 aa」。
            _ => {}
        }
    }

    /// 对话框按键：重命名时打字进输入框，Enter 确认、Esc 取消。
    pub(super) fn handle_dialog_key(&mut self, key: &str, shift: bool) {
        if key == "escape" {
            self.dialog = None;
            return;
        }
        let Some(dialog) = self.dialog.as_mut() else {
            return;
        };
        // 只有带输入框的对话框（重命名 / 切目录）吃按键
        let editor = match dialog {
            SessionDialog::Rename { editor, .. }
            | SessionDialog::ApiKey { editor, .. }
            | SessionDialog::Import { editor }
            | SessionDialog::Label { editor }
            | SessionDialog::BugHint { editor }
            | SessionDialog::Number { editor, .. } => editor,
            SessionDialog::Delete { .. } => return,
        };
        match key {
            "backspace" => editor.backspace(),
            "delete" => editor.delete_forward(),
            "left" | "right" | "home" | "end" => match key {
                "left" => editor.move_cursor(-1, 0, shift),
                "right" => editor.move_cursor(1, 0, shift),
                "home" => editor.home(shift),
                _ => editor.end(shift),
            },
            _ => {}
        }
    }
    /// 覆盖层按键。
    pub(super) fn handle_overlay_key(&mut self, key: &str, _shift: bool, cx: &mut Context<Self>) {
        if key == "escape" {
            // 详情页先退回列表（草稿还在），再按一次才关面板丢草稿
            if let Some(Overlay::List(list)) = self.overlay.as_mut()
                && list.detail.take().is_some()
            {
                return;
            }
            self.overlay = None;
            // 扩展/技能面板的改动是草稿：Esc 直接丢弃
            self.skills_draft = None;
            self.extensions_draft = None;
            return;
        }
        // 详情页只吃 Tab（回列表）与滚动键
        if let Some(Overlay::List(list)) = self.overlay.as_mut()
            && list.detail.is_some()
        {
            if key == "tab" || key == "enter" {
                list.detail = None;
            } else if let Some(detail) = list.detail.as_mut() {
                match key {
                    "up" => detail.scroll_by(-1, 18),
                    "down" => detail.scroll_by(1, 18),
                    "pageup" => detail.scroll_by(-12, 18),
                    "pagedown" => detail.scroll_by(12, 18),
                    _ => {}
                }
            }
            cx.notify();
            return;
        }
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        match overlay {
            Overlay::List(list) => match key {
                "enter" => {
                    let action = list.take_action().map(clone_action);
                    if let Some(action) = action {
                        // 同上：勾选类动作要保留面板
                        if !keeps_overlay(&action) {
                            self.overlay = None;
                        }
                        self.run_action(action, cx);
                    }
                }
                "tab" => self.open_list_detail(),
                "up" => list.move_selection(-1),
                "down" => list.move_selection(1),
                "backspace" => {
                    list.query.backspace();
                    list.selected = 0;
                }
                // 可打印字符由 IME 句柄写进过滤框（见 `ime_target`），
                // 这里再插一次就会「敲一个 a 出现 aa」。
                _ => {}
            },
            Overlay::Info(info) => match key {
                "up" => info.scroll_by(-1, 18),
                "down" => info.scroll_by(1, 18),
                "pageup" => info.scroll_by(-12, 18),
                "pagedown" => info.scroll_by(12, 18),
                _ => {}
            },
        }
    }
}
