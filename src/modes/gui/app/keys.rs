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
        self.swallow_char = None;
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

        // 右侧 Files 面板的搜索框聚焦时，按键先喂它（可打印字符由 IME 句柄写进去）
        if self.panel.visible
            && self.panel.search_focus
            && self.dispatch_panel_search_key(&keystroke, primary, shift)
        {
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
                    self.cancel_file_scan();
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
                // 命令面板里敲空格 = 补全命令名 + 空格，让位给子命令面板（同 TUI 的 `/goal `）
                "space" if suggest.trigger == Trigger::Command && !suggest.items.is_empty() => {
                    self.apply_suggestion(cx);
                    // 这个空格已经补进输入框了，别再让 IME 写一遍
                    self.swallow_char = Some(' ');
                    cx.notify();
                    return true;
                }
                // 没有候选（带参数的命令 / 没匹配的路径）：原样补全，命令随即执行
                "enter" if suggest.items.is_empty() => {
                    let command = suggest.trigger.submits_after_apply();
                    self.commit_suggest_input();
                    if command {
                        self.submit(false, cx);
                    }
                    cx.notify();
                    return true;
                }
                // 有候选时 Enter 先补全再提交：`/` 命令随即执行，`@` 文件只补全
                "enter" if !suggest.items.is_empty() => {
                    let command = suggest.trigger.submits_after_apply();
                    self.apply_suggestion(cx);
                    if command {
                        self.submit(false, cx);
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
                        self.cancel_file_scan();
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
        let alt = keystroke.modifiers.alt;
        let key = keystroke.key.as_str();

        // Alt+Up：把排队的 steer / follow-up 全部取回编辑器（同 TUI 的 app.message.dequeue）
        if alt && key == "up" {
            self.dequeue_queued(cx);
            cx.notify();
            return true;
        }
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
        if (primary && !shift && key == "w")
            || (primary && key == "backspace")
            || (alt && key == "backspace")
        {
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
            // 右侧 Files 面板（搜索 + 文件列表 + 预览）：再按一次收起
            "f" if shift => {
                self.toggle_panel_tab(PanelTab::Files, cx);
                return true;
            }
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
                self.resized_bottom();
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
            "w" if shift => {
                self.open_cwd_menu(cx);
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

    /// Files 面板搜索框的按键：只吃编辑键与 Esc/Enter，其余交回正常分发。
    ///
    /// 可打印字符由 IME 句柄写进搜索框（见 `ime_target`），这里不重复插入。
    pub(super) fn dispatch_panel_search_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        primary: bool,
        shift: bool,
    ) -> bool {
        // Ctrl / Cmd 组合留给全局快捷键
        if primary {
            return false;
        }
        match keystroke.key.as_str() {
            // Esc / Enter 先退出搜索（面板本身不关，Esc 再按一次才收起）
            "escape" | "enter" => {
                self.panel.search_focus = false;
                true
            }
            "backspace" => {
                self.panel.search.backspace();
                true
            }
            "delete" => {
                self.panel.search.delete_forward();
                true
            }
            "left" => {
                self.panel.search.move_cursor(-1, 0, shift);
                true
            }
            "right" => {
                self.panel.search.move_cursor(1, 0, shift);
                true
            }
            "home" => {
                self.panel.search.home(shift);
                true
            }
            "end" => {
                self.panel.search.end(shift);
                true
            }
            _ => false,
        }
    }

    /// 无修饰的文字编辑键（Enter / Esc / 方向键 / 退格）。
    pub(super) fn dispatch_text_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        match keystroke.key.as_str() {
            // Alt+Enter：排 follow-up（回合 settle 后才发送，同 TUI 的 app.message.followUp）
            "enter" if keystroke.modifiers.alt => {
                self.submit(true, cx);
                cx.notify();
            }
            "enter" if shift => {
                self.editor.insert("\n");
                cx.notify();
            }
            "enter" => {
                self.submit(false, cx);
                cx.notify();
            }
            "escape" => {
                if self.busy {
                    self.abort_turn(cx);
                } else if self.panel.visible {
                    // 空闲时 Esc 先收右侧面板（giff 等）——之前面板没有关闭途径
                    self.panel.visible = false;
                    self.panel.search_focus = false;
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
        // 扩展覆盖层先拿键：输入行聚焦时 Esc 是「放弃编辑」而不是「关闭整个面板」，
        // 放在下面那条通用 Esc 分支之后会被抢先关掉（问卷曾被整份取消）
        if matches!(self.overlay, Some(Overlay::Extension(_))) {
            self.handle_extension_overlay_key(keystroke, cx);
            return;
        }
        // Ctrl+C 是 tui.select.cancel；Ctrl-J / Ctrl-K 是列表上的上下移动
        if (primary && !shift && key == "c") || key == "escape" {
            // 详情页先退回列表（草稿还在），再按一次才关面板丢草稿
            if let Some(Overlay::List(list)) = self.overlay.as_mut()
                && list.detail.take().is_some()
            {
                return;
            }
            self.dismiss_overlay();
            // 扩展/技能/模型名单面板的改动是草稿：Esc 直接丢弃
            self.skills_draft = None;
            self.extensions_draft = None;
            self.scoped_draft = None;
            return;
        }
        let up = key == "up" || (primary && !shift && key == "k");
        let down = key == "down" || (primary && !shift && key == "j");
        // 详情页只吃 Enter（回列表）与滚动键
        if let Some(Overlay::List(list)) = self.overlay.as_mut()
            && list.detail.is_some()
        {
            if key == "enter" {
                list.detail = None;
            } else if let Some(detail) = list.detail.as_mut() {
                match key {
                    "pageup" => detail.scroll_by(-12.),
                    "pagedown" => detail.scroll_by(12.),
                    "home" => detail.scroll_to(0.),
                    "end" => detail.scroll_to(f32::NEG_INFINITY),
                    _ if up => detail.scroll_by(-1.),
                    _ if down => detail.scroll_by(1.),
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
            // 扩展覆盖层已在上面提前返回
            Overlay::Extension(_) => {}
            Overlay::List(list) => {
                let len = list.filtered().len();
                match key {
                    // 设置 / 扩展 / 技能面板：Space 改值，Enter 看详情
                    "space" if list.space_activates => {
                        let action = list.take_action().cloned();
                        if let Some(action) = action {
                            if !keeps_overlay(&action) {
                                self.overlay = None;
                            }
                            self.run_action(action, cx);
                        }
                        // 同一个空格别再被 IME 写进过滤框
                        self.swallow_char = Some(' ');
                    }
                    "enter" => {
                        if list.detail_source != DetailSource::None {
                            self.open_list_detail();
                        } else if !list.space_activates {
                            let action = list.take_action().cloned();
                            if let Some(action) = action {
                                // 同上：勾选类动作要保留面板
                                if !keeps_overlay(&action) {
                                    self.overlay = None;
                                }
                                self.run_action(action, cx);
                            }
                        }
                    }
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
                "pageup" => info.scroll_by(-12.),
                "pagedown" => info.scroll_by(12.),
                "home" => info.scroll_to(0.),
                "end" => info.scroll_to(f32::NEG_INFINITY),
                _ if up => info.scroll_by(-1.),
                _ if down => info.scroll_by(1.),
                _ => {}
            },
        }
    }

    /// 扩展覆盖层的按键：编辑态交给内联输入行，其余翻译成 [`OverlayKey`] 回传给扩展。
    ///
    /// 与 TUI 的 `handlers/overlay.rs` 同一套顺序：输入行聚焦时字符/退格本地编辑、Enter 提交、
    /// Esc 交出焦点并回传；扩展不认领的导航键由这里滚动内容；Esc 未被认领就关面板。
    pub(super) fn handle_extension_overlay_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) {
        let key = keystroke.key.as_str();
        let primary = keystroke.modifiers.control || keystroke.modifiers.platform;
        // Ctrl+C：与 TUI 一致，直接关掉覆盖层（不是「放弃编辑」）
        if primary && key == "c" {
            self.dismiss_overlay();
            return;
        }
        let editing = matches!(self.overlay, Some(Overlay::Extension(ref ext)) if ext.input_active);
        let has_input =
            matches!(self.overlay, Some(Overlay::Extension(ref ext)) if ext.view.input.is_some());

        // 输入行聚焦：字符交给输入框（IME 写入），退格/方向本地编辑，Enter 提交，Esc 交出焦点
        if editing {
            match key {
                "enter" => {
                    self.submit_overlay_input();
                    return;
                }
                "escape" => {
                    if let Some(Overlay::Extension(ext)) = self.overlay.as_mut() {
                        ext.input_active = false;
                    }
                    self.send_overlay_key(OverlayKey::Esc);
                    return;
                }
                "backspace" | "delete" | "left" | "right" | "home" | "end" => {
                    if let Some(Overlay::Extension(ext)) = self.overlay.as_mut() {
                        match key {
                            "backspace" => ext.input.backspace(),
                            "delete" => ext.input.delete_forward(),
                            "left" => ext.input.move_cursor(-1, 0, false),
                            "right" => ext.input.move_cursor(1, 0, false),
                            "home" => ext.input.home(false),
                            _ => ext.input.end(false),
                        }
                    }
                    return;
                }
                // 其余可打印字符不转发（TUI 同样只本地编辑），避免扩展收到重复输入
                _ if !primary && keystroke.key_char.is_some() => return,
                _ => {}
            }
        }

        // 有输入行但没聚焦时，`i` 聚焦它（TUI 同款；查看器的 steer 输入靠这个）
        if !editing && has_input && key == "i" && !primary {
            if let Some(Overlay::Extension(ext)) = self.overlay.as_mut() {
                ext.input_active = true;
            }
            return;
        }

        let mapped = match key {
            "escape" => OverlayKey::Esc,
            "enter" => OverlayKey::Enter,
            "up" => OverlayKey::Up,
            "down" => OverlayKey::Down,
            "left" => OverlayKey::Left,
            "right" => OverlayKey::Right,
            "tab" => OverlayKey::Tab,
            "pageup" => OverlayKey::PageUp,
            "pagedown" => OverlayKey::PageDown,
            "home" => OverlayKey::Home,
            "end" => OverlayKey::End,
            "backspace" => OverlayKey::Backspace,
            "delete" => OverlayKey::Delete,
            "space" => OverlayKey::Char(' '),
            other if other.chars().count() == 1 => {
                OverlayKey::Char(other.chars().next().unwrap_or(' '))
            }
            _ => OverlayKey::Other,
        };
        let nav = match mapped {
            OverlayKey::Up => Some(-1.),
            OverlayKey::Down => Some(1.),
            OverlayKey::PageUp => Some(-12.),
            OverlayKey::PageDown => Some(12.),
            OverlayKey::Home => Some(f32::MIN),
            OverlayKey::End => Some(f32::MAX),
            _ => None,
        };
        let cancel = mapped == OverlayKey::Esc;
        let char_key = match mapped {
            OverlayKey::Char(char) => Some(char),
            _ => None,
        };
        let consumed = self.send_overlay_key(mapped);
        // 这个字符键让输入行新冒出来（`n` 写备注、`i` 聚焦）：IME 随后要把同一个字符写进输入框，
        // 记下来在写入时吞掉，否则输入框会自带一个多余字符
        if consumed
            && let Some(char) = char_key
            && !editing
            && matches!(self.overlay, Some(Overlay::Extension(ref ext)) if ext.input_active)
        {
            self.swallow_char = Some(char);
        }
        // 扩展不认领的导航键：滚动内容（TUI 的默认语义）
        if !consumed && let Some(rows) = nav {
            self.scroll_extension_overlay(rows);
        }
        if !consumed && cancel {
            self.dismiss_overlay();
        }
        cx.notify();
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
