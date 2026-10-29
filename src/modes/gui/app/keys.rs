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
            // Ctrl-C 等同 Esc 取消（同 TUI 的 tui.select.cancel）
            let key = if primary && keystroke.key == "c" {
                "escape"
            } else {
                keystroke.key.as_str()
            };
            if key == "enter" {
                self.confirm_dialog(cx);
            } else {
                self.handle_dialog_key(key, shift);
            }
            cx.notify();
            return;
        }

        if self.session_menu.is_some() {
            if self.dispatch_session_menu_key(&keystroke, cx) {
                cx.notify();
                return;
            }
            // 其余键：先收菜单再照常处理
            self.session_menu = None;
            if keystroke.key == "escape" {
                cx.notify();
                return;
            }
        }

        if self.dropdown.is_some() {
            if self.dispatch_dropdown_key(&keystroke, cx) {
                cx.notify();
                return;
            }
            // 同上：其它键先收菜单
            self.dropdown = None;
        }

        // Shift+Tab：循环 thinking 级别（同 TUI 的 `app.thinking.cycle`）；
        // 放在补全面板之前，面板开着时也能切
        if shift && keystroke.key == "tab" {
            self.cycle_thinking(cx);
            cx.notify();
            return;
        }

        if self.suggest.is_some() && self.dispatch_suggest_key(&keystroke, primary, shift, cx) {
            cx.notify();
            return;
        }

        // Ctrl-S 先于覆盖层分发：技能/扩展面板是覆盖层，否则按键被它吃掉
        if primary && !shift && keystroke.key == "s" && self.save_panel_drafts(cx) {
            cx.notify();
            return;
        }

        if self.overlay.is_some() {
            if primary
                && !shift
                && matches!(keystroke.key.as_str(), "r" | "d" | "g")
                && self.dispatch_session_list_key(&keystroke.key, cx)
            {
                cx.notify();
                return;
            }
            self.handle_overlay_key(&keystroke, shift, cx);
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
        primary: bool,
        shift: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(suggest) = self.suggest.clone() {
            // Ctrl-J / Ctrl-K 也是列表上下（`tui.select.up/down` 的别名）；
            // 无修饰的 j / k 是普通字符，必须留给面板输入框。
            let ctrl_j = primary && !shift && keystroke.key == "j";
            let ctrl_k = primary && !shift && keystroke.key == "k";
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
                _ if ctrl_k => {
                    self.move_suggestion(-1);
                    cx.notify();
                    return true;
                }
                _ if ctrl_j => {
                    self.move_suggestion(1);
                    cx.notify();
                    return true;
                }
                "tab" if !suggest.items.is_empty() => {
                    // Tab 只补全：命令文本落进输入框，等 Enter 再执行
                    self.apply_suggestion(cx);
                    cx.notify();
                    return true;
                }
                // 有候选时 Enter 先补全再提交：`/` 命令随即执行，`@` 文件只补全
                "enter" if !suggest.items.is_empty() => {
                    let command = suggest.trigger == Trigger::Command;
                    self.apply_suggestion(cx);
                    if command {
                        self.submit(cx);
                    }
                    cx.notify();
                    return true;
                }
                // 退格/方向键改的是面板输入框，不动主输入框
                "backspace" => {
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
                "delete" => {
                    self.suggest_input.delete_forward();
                    self.refresh_suggest_from_input(cx);
                    cx.notify();
                    return true;
                }
                "left" | "right" | "home" | "end" => {
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
        if primary && !shift && key == "k" {
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
        if alt && key == "c" {
            // TUI 的 app.editor.copy：复制输入框全文
            let text = self.editor.text();
            if !text.is_empty() {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                self.toast("Copied input", ToastKind::Info, cx);
            }
            return true;
        }
        if primary && !shift && key == "x" {
            // TUI 的 app.message.copy：复制最后一条助手回复
            self.copy_last_reply(cx);
            return true;
        }
        if primary && key == "home" {
            self.scroll.set_offset(gpui::point(px(0.), px(0.)));
            self.pinned_bottom.set(None);
            cx.notify();
            return true;
        }
        if primary && key == "end" {
            self.scroll_to_bottom();
            cx.notify();
            return true;
        }
        if shift && (key == "pageup" || key == "pagedown") {
            self.page_transcript(if key == "pageup" { -1. } else { 1. });
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
    ///
    /// TUI 已有含义的键一律跟随 TUI（`app.model.select` = Ctrl-L、`app.tools.expand` = Ctrl-O、
    /// `app.thinking.toggle` = Ctrl-T、`app.theme.cycle` = Ctrl-Shift-T）；GUI 独有的面板挤在
    /// TUI 没绑定的键上，Shift 组合是它们的后缀。
    pub(super) fn dispatch_panel_shortcut(
        &mut self,
        keystroke: &gpui::Keystroke,
        shift: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        match keystroke.key.as_str() {
            // GUI 独有的命令面板：Ctrl-K 已经归 TUI 的 deleteToLineEnd
            "m" => {
                self.open_command_palette(cx);
                return true;
            }
            // Ctrl+P / Ctrl+Shift+P 直接切上一个/下一个模型（同 TUI 的模型循环）
            "p" => {
                self.cycle_model(if shift { -1 } else { 1 }, cx);
                return true;
            }
            // TUI 的 app.model.select
            "l" => {
                self.open_model_picker(cx);
                return true;
            }
            // GUI 旧键位，TUI 未绑定，留作别名
            "/" => {
                self.open_model_picker(cx);
                return true;
            }
            // TUI 的 app.tools.expand
            "o" if !shift => {
                self.expand_tools = !self.expand_tools;
                cx.notify();
                return true;
            }
            "o" if shift => {
                self.open_session_picker(cx);
                return true;
            }
            // TUI 的 app.thinking.toggle
            "t" if !shift => {
                self.show_thinking = !self.show_thinking;
                cx.notify();
                return true;
            }
            // TUI 的 app.theme.cycle：GUI 只有明/暗两套，切一次就是换主题
            "t" if shift => {
                let is_dark = !self.theme.is_dark;
                self.set_dark(is_dark);
                cx.notify();
                return true;
            }
            "g" if shift => {
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
                self.editor.move_up_or_history(shift);
                cx.notify();
            }
            "down" => {
                self.editor.move_down_or_history(shift);
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
    /// 覆盖层按键。列表类面板统一支持 `tui.select.*`：
    /// 上下（含 Ctrl-J / Ctrl-K）、首页/末页、翻页、Ctrl-C 取消。
    pub(super) fn handle_overlay_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        let key = keystroke.key.as_str();
        let primary = keystroke.modifiers.control || keystroke.modifiers.platform;
        // Ctrl+C 是 tui.select.cancel；Ctrl-J / Ctrl-K 是列表上的上下移动
        if (primary && !shift && key == "c") || key == "escape" {
            // 详情页先退回列表（草稿还在），再按一次才关面板丢草稿
            if let Some(Overlay::List(list)) = self.overlay.as_mut()
                && list.detail.take().is_some()
            {
                return;
            }
            self.overlay = None;
            // 扩展/技能/模型名单面板的改动是草稿：Esc 直接丢弃
            self.skills_draft = None;
            self.extensions_draft = None;
            self.scoped_draft = None;
            return;
        }
        let up = key == "up" || (primary && !shift && key == "k");
        let down = key == "down" || (primary && !shift && key == "j");
        // 详情页只吃 Tab（回列表）与滚动键
        if let Some(Overlay::List(list)) = self.overlay.as_mut()
            && list.detail.is_some()
        {
            if key == "tab" || key == "enter" {
                list.detail = None;
            } else if let Some(detail) = list.detail.as_mut() {
                match key {
                    "pageup" => detail.scroll_by(-12, 18),
                    "pagedown" => detail.scroll_by(12, 18),
                    "home" => detail.scroll_by(-(detail.lines.len() as i32), 18),
                    "end" => detail.scroll_by(detail.lines.len() as i32, 18),
                    _ if up => detail.scroll_by(-1, 18),
                    _ if down => detail.scroll_by(1, 18),
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
            Overlay::List(list) => {
                let len = list.filtered().len();
                match key {
                    "enter" => {
                        let action = list.take_action().cloned();
                        if let Some(action) = action {
                            // 同上：勾选类动作要保留面板
                            if !keeps_overlay(&action) {
                                self.overlay = None;
                            }
                            self.run_action(action, cx);
                        }
                    }
                    "tab" => self.open_list_detail(),
                    "pageup" => list.move_selection(-12),
                    "pagedown" => list.move_selection(12),
                    "home" => list.select_index(0),
                    "end" => list.select_index(len.saturating_sub(1)),
                    "backspace" => {
                        list.query.backspace();
                        list.selected = 0;
                    }
                    _ if up => list.move_selection(-1),
                    _ if down => list.move_selection(1),
                    // 可打印字符由 IME 句柄写进过滤框（见 `ime_target`），
                    // 这里再插一次就会「敲一个 a 出现 aa」。
                    _ => {}
                }
            }
            Overlay::Info(info) => match key {
                "pageup" => info.scroll_by(-12, 18),
                "pagedown" => info.scroll_by(12, 18),
                "home" => info.scroll_by(-(info.lines.len() as i32), 18),
                "end" => info.scroll_by(info.lines.len() as i32, 18),
                _ if up => info.scroll_by(-1, 18),
                _ if down => info.scroll_by(1, 18),
                _ => {}
            },
        }
    }

    /// 会话列表面板里的 TUI 专用键（`app.session.rename` / `delete` / `next`）：
    /// 只有选中项是会话时才吃键，`true` 表示已消费。
    pub(super) fn dispatch_session_list_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let path = match self.overlay.as_ref() {
            Some(Overlay::List(list)) => match list.take_action() {
                Some(Action::LoadSession(path)) => path.clone(),
                _ => return false,
            },
            _ => return false,
        };
        match key {
            "r" => {
                self.overlay = None;
                self.start_rename(&path, cx);
            }
            "d" => {
                self.overlay = None;
                self.ask_delete(&path, cx);
            }
            // app.session.next：按列表顺序跳到当前会话的下一个
            _ => {
                if self.sessions.is_empty() {
                    return true;
                }
                let current = self.sessions.iter().position(|row| row.path == path);
                let next = self.sessions[cycle_index(current, self.sessions.len(), 1)]
                    .path
                    .clone();
                self.overlay = None;
                _ = self.cmd_tx.send(AgentCommand::ResumeSession { path: next });
            }
        }
        true
    }

    /// 下拉菜单按键：`true` 表示已消费（未消费时调用方会收起菜单）。
    pub(super) fn dispatch_dropdown_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) -> bool {
        // 选完后执行的动作（先取出来，避免下拉菜单占着 &mut self）
        let mut run: Option<Action> = None;
        let key = keystroke.key.as_str();
        let primary = keystroke.modifiers.control || keystroke.modifiers.platform;
        let Some(dropdown) = self.dropdown.as_mut() else {
            return false;
        };
        let len = dropdown.items.len();
        match key {
            "escape" => {}
            "enter" => {
                if let Some(item) = dropdown.items.get(dropdown.selected) {
                    run = Some(item.action.clone());
                }
            }
            "pageup" => dropdown.selected = dropdown.selected.saturating_sub(12),
            "pagedown" => {
                if len > 0 {
                    dropdown.selected = (dropdown.selected + 12).min(len - 1);
                }
            }
            "home" => dropdown.selected = 0,
            "end" => dropdown.selected = len.saturating_sub(1),
            "up" => dropdown.selected = dropdown.selected.saturating_sub(1),
            "down" if len > 0 => dropdown.selected = (dropdown.selected + 1).min(len - 1),
            _ if primary && !keystroke.modifiers.shift && key == "k" => {
                dropdown.selected = dropdown.selected.saturating_sub(1);
            }
            _ if primary && !keystroke.modifiers.shift && key == "j" && len > 0 => {
                dropdown.selected = (dropdown.selected + 1).min(len - 1);
            }
            _ => return false,
        }
        // 选中行超出菜单高度时跟着滚
        overlay::reveal_row(&dropdown.handle, dropdown.selected);
        if run.is_some() || key == "escape" {
            self.dropdown = None;
        }
        if let Some(action) = run {
            self.run_action(action, cx);
        }
        true
    }

    /// 会话行右键菜单按键：`true` 表示已消费（未消费时调用方会收起菜单）。
    pub(super) fn dispatch_session_menu_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = keystroke.key.as_str();
        let primary = keystroke.modifiers.control || keystroke.modifiers.platform;
        let Some(menu) = self.session_menu.as_mut() else {
            return false;
        };
        let path = menu.path.clone();
        match key {
            "escape" => {
                self.close_session_menu(cx);
            }
            "enter" => {
                if menu.selected == 0 {
                    self.start_rename(&path, cx);
                } else {
                    self.ask_delete(&path, cx);
                }
            }
            "up" | "home" => menu.selected = 0,
            "down" | "end" => menu.selected = 1,
            _ if primary && !keystroke.modifiers.shift && key == "k" => menu.selected = 0,
            _ if primary && !keystroke.modifiers.shift && key == "j" => menu.selected = 1,
            _ => return false,
        }
        true
    }
}
