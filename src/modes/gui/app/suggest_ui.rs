//! 输入补全面板：`/` 与 `@` 触发的候选重建、移动与采纳。
//!
//! 拆自 [`super::app`]：候选数据来自 [`super::suggest`]，这里只管交互状态。

use super::*;

impl GuiApp {
    /// 按输入框当前内容重算补全面板：触发词变了或没有触发词就开关面板。
    pub(super) fn refresh_suggest(&mut self, cx: &mut Context<Self>) {
        // 面板开着时键入的是面板输入框，主输入框不会变，别覆盖面板状态
        if self.suggest.is_some() {
            return;
        }
        // 历史浏览（↑/↓ 载入 `/model` 这类历史）不该弹面板，否则抢走 ↑/↓ 焦点
        if self.editor.browsing_history() {
            self.suggest = None;
            self.attach_images = false;
            return;
        }
        let before = self.editor.text_before_cursor();
        let Some((trigger, start, query)) = suggest::detect(&before) else {
            self.suggest = None;
            self.suggest_dismissed = None;
            self.attach_images = false;
            return;
        };
        // 同一次触发被 Esc / 退格手动关掉后不再自动弹回；触发位置变了才重新生效
        if !suggest_allowed(self.suggest_dismissed, start) {
            self.suggest = None;
            return;
        }
        self.suggest_dismissed = None;
        // 面板输入框显示已输入的前缀，之后继续敲的字符都进它
        self.suggest_input = Editor::new();
        if !query.is_empty() {
            self.suggest_input.insert(&query);
        }
        self.rebuild_suggest(trigger, start, query, cx);
    }
    /// 面板开着时按面板输入框重算候选。
    pub(super) fn refresh_suggest_from_input(&mut self, cx: &mut Context<Self>) {
        let Some((trigger, start)) = self.suggest.as_ref().map(|s| (s.trigger, s.start)) else {
            return;
        };
        let query = self.suggest_input.text();
        self.rebuild_suggest(trigger, start, query, cx);
    }
    /// 文件列表到位/输入变化后重算：面板开着按面板输入，否则按主输入框。
    pub(super) fn refresh_suggest_any(&mut self, cx: &mut Context<Self>) {
        if self.suggest.is_some() {
            self.refresh_suggest_from_input(cx);
        } else {
            self.refresh_suggest(cx);
        }
    }
    /// 重算候选并写回面板状态（`start`/`trigger` 由触发时定下，之后不变）。
    pub(super) fn rebuild_suggest(
        &mut self,
        trigger: Trigger,
        start: usize,
        query: String,
        cx: &mut Context<Self>,
    ) {
        let items = match trigger {
            Trigger::Command => {
                // 命令 + 本会话技能（`/skill:名称`，与 TUI 的技能命令同形）+ 已启用扩展声明的命令，
                // 三类合在一起模糊匹配排序；技能按裸名参与匹配（`/idea` 不会因 `skill:` 前缀误命中）
                let mut extra: Vec<(String, String)> = Vec::new();
                for (name, description, _source) in self
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.skills.clone())
                    .unwrap_or_default()
                {
                    extra.push((format!("skill:{name}"), description));
                }
                // 执行入口还挂在 TUI 层，这里先按现状列出
                for command in crate::core::extensions::registered_commands() {
                    extra.push((
                        command.name.clone(),
                        format!("{} (extension)", command.description),
                    ));
                }
                suggest::commands_with(&extra, &query)
            }
            Trigger::File => {
                self.ensure_files(cx);
                suggest::files(&self.files_cache.paths, &query, self.attach_images)
            }
        };
        // 同一触发词 + 同一前缀：保留选中项，避免每敲一个字符跳回第一行
        let keep = self
            .suggest
            .as_ref()
            .filter(|old| {
                old.trigger == trigger && old.query == query && old.items.len() == items.len()
            })
            .map(|old| old.selected)
            .unwrap_or(0);
        let selected = keep.min(items.len().saturating_sub(1));
        self.suggest = Some(Suggest {
            trigger,
            start,
            query,
            items,
            selected,
        });
    }
    /// 确保 `@` 补全的文件列表已按当前目录加载（第一次用时后台列一次）。
    pub(super) fn ensure_files(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd();
        if self.files_cache.loading
            || (self.files_cache.cwd == cwd && !self.files_cache.paths.is_empty())
        {
            return;
        }
        self.files_cache = FilesCache {
            cwd: cwd.clone(),
            loading: true,
            paths: Vec::new(),
        };
        cx.spawn(async move |this, cx| {
            let paths = cx
                .background_executor()
                .spawn(async move { panel::file_paths(&cwd) })
                .await;
            _ = this.update(cx, |this, cx| {
                this.files_cache.loading = false;
                this.files_cache.paths = paths;
                this.refresh_suggest_any(cx);
                cx.notify();
            });
        })
        .detach();
    }
    /// 上下移动补全选中项（越界回绕）。
    pub(super) fn move_suggestion(&mut self, delta: i32) {
        // 句柄先克隆：`suggest` 借着自己，滚动跟随还要用另一个字段
        let scroll = self.suggest_scroll.clone();
        let Some(suggest) = self.suggest.as_mut() else {
            return;
        };
        let len = suggest.items.len();
        if len == 0 {
            return;
        }
        let next = suggest.selected as i32 + delta;
        suggest.selected = next.clamp(0, len as i32 - 1) as usize;
        // 选中行超出面板时跟着滚，保证它始终在视口里
        overlay::reveal_row(&scroll, suggest.selected);
    }
    /// 把选中候选写进输入框（命令/文件补全）。
    pub(super) fn apply_suggestion(&mut self, cx: &mut Context<Self>) {
        let Some(suggest) = self.suggest.clone() else {
            return;
        };
        let Some(item) = suggest.items.get(suggest.selected) else {
            return;
        };
        let end = self.editor.cursor_byte();
        // `/` 命令是动作不是文本：清掉触发文本直接执行
        if suggest.trigger == Trigger::Command {
            let name = item.label.trim_start_matches('/').to_string();
            self.editor.replace_byte_range(suggest.start..end, "");
            self.suggest = None;
            self.suggest_dismissed = None;
            self.run_slash(&name, cx);
            return;
        }
        let insert = format!("{} ", item.label);
        self.editor.replace_byte_range(suggest.start..end, &insert);
        self.suggest = None;
        self.suggest_dismissed = None;
        self.refresh_suggest(cx);
    }
    /// 点击候选：选中它并写入输入框。
    pub(super) fn pick_suggestion(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(suggest) = self.suggest.as_mut() {
            suggest.selected = index;
        }
        self.apply_suggestion(cx);
        cx.notify();
    }
}
