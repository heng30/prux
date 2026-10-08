//! /settings 设置选择器：键盘处理与值改动动作。
//! 状态在 `settings_selector`，渲染在 `render::settings_selector`，
//! 键盘/动作在本模块（对齐 `sessions.rs` 的会话选择器按键）。

use super::KeyAction;
use crate::{
    core::{
        auth,
        keybindings::{self, KeybindingsManager},
        model_resolver, settings_manager,
    },
    error::Result,
    modes::interactive::{
        agent_actor::AgentCommand,
        app::{App, MsgLevel},
        line_input::InputAction,
        settings_selector::{SettingKind, SubmenuKind, SubmenuState},
        theme::{Theme, available_theme_names},
    },
    utils::wheel::WheelScrollLines,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

impl App {
    /// 激活当前项：Cycle 循环切换；ReadOnly 仅展示不响应
    fn activate_sel(&mut self) {
        let (id, next, _) = {
            let sel = &mut self.settings_selector;
            let Some(&idx) = sel.filtered.get(sel.selected) else {
                return;
            };

            match sel.items[idx].kind.clone() {
                SettingKind::Cycle(values) => {
                    let cur_value = sel.items[idx].current_value.clone();
                    let cur = values.iter().position(|v| *v == cur_value);
                    let next = values[(cur.map(|i| i + 1).unwrap_or(0)) % values.len()].clone();
                    (sel.items[idx].id, Some(next), None)
                }
                SettingKind::Submenu {
                    title,
                    description,
                    kind,
                    options,
                } => {
                    let cur = options
                        .iter()
                        .position(|o| o.value == sel.items[idx].current_value);
                    sel.submenu = Some(SubmenuState {
                        title,
                        description,
                        kind,
                        options,
                        selected: cur.unwrap_or(0),
                        item_index: idx,
                    });
                    (sel.items[idx].id, None, Some(idx))
                }
                SettingKind::ReadOnly => (sel.items[idx].id, None, None),
            }
        };

        if let Some(next) = next {
            self.apply_change(id, &next);
        }
    }

    /// 子菜单确认选择：应用并返回主列表。
    fn confirm_submenu(&mut self) {
        let (id, item_index, opt_value) = {
            let Some(sub) = self.settings_selector.submenu.take() else {
                return;
            };
            let Some(opt) = sub.options.get(sub.selected).cloned() else {
                return;
            };
            let id = self.settings_selector.items[sub.item_index].id;
            let item_index = sub.item_index;
            (id, item_index, opt.value)
        };
        self.settings_selector.selected = item_index;

        match id {
            "theme" => {
                self.theme = Theme::load(&opt_value);
                self.invalidate_render(); // 主题已变：清消息渲染缓存，避免旧主题颜色的块残留
                self.persist("theme", settings_manager::write_theme(&opt_value));
            }
            _ => return,
        }
        if let Some(item) = self.settings_selector.items.get_mut(item_index) {
            item.current_value = opt_value;
        }
    }

    /// 失败提示
    fn persist(&mut self, key: &str, result: Result<()>) {
        if let Err(e) = result {
            self.push_msg(format!("failed to save {}: {}", key, e), MsgLevel::Error);
        }
    }

    /// Ctrl+Shift+T：循环到下一个可用主题。
    /// 用 `App.cwd` 而非锁 agent（busy 时 prompt future 持锁挂起），
    /// 切换逻辑与 `/theme <name>`（cmd_theme_named）一致。
    pub(super) fn cycle_theme(&mut self) {
        let available =
            available_theme_names(&self.cwd, &settings_manager::agent_dir(), !self.no_themes);
        if available.is_empty() {
            self.push_msg("no themes available".to_string(), MsgLevel::Warning);
            return;
        }

        let current = &self.theme.name;
        let idx = available.iter().position(|n| n == current);
        let next = available[idx.map(|i| (i + 1) % available.len()).unwrap_or(0)].clone();

        self.theme = Theme::load(&next);
        self.invalidate_render(); // 主题已变：清消息渲染缓存，避免旧主题颜色的块残留
        self.push_msg_dedup(
            format!("switched theme: {}", self.theme.name),
            "switched theme: ",
            MsgLevel::Info,
        );

        // 成功切换后持久化到 settings.json（重启恢复）
        if let Err(e) = settings_manager::write_theme(&next) {
            self.push_msg(format!("failed to persist theme: {}", e), MsgLevel::Error);
        }
    }

    /// Shift+Tab：循环当前模型支持的 thinking 级别（actor：worker 执行）
    /// UI 侧按 App 镜像的 available levels 计算下一级别，发 SetThinking 命令。
    pub(super) fn cycle_thinking(&mut self) {
        if !self.model_reasoning {
            self.push_msg(
                "model does not support thinking".to_string(),
                MsgLevel::Warning,
            );
            return;
        }

        let levels = self.thinking_levels.clone();
        if levels.is_empty() {
            self.push_msg(
                "model does not support any thinking level".to_string(),
                MsgLevel::Warning,
            );
            return;
        }

        let current = self
            .thinking_level
            .clone()
            .unwrap_or_else(|| "off".to_string());
        let idx = levels.iter().position(|l| *l == current);
        let next = levels[idx.map(|i| (i + 1) % levels.len()).unwrap_or(0)].clone();
        self.worker.send(AgentCommand::SetThinking {
            level: next,
            persist: false,
        });
        self.dirty = true;
    }

    /// Ctrl+P 模型循环（scope 非空时严格按 scope 顺序循环，带 thinking 的条目先 SetThinking 再 SwitchModel）
    pub(super) fn cycle_model_dir(&mut self, dir: i32) {
        // 目标模型列表：scope 内模型（过滤掉目录中不可用的），否则全部已配置模型
        let all: Vec<(String, String, String)> = auth::list_configured_providers()
            .iter()
            .flat_map(|p| {
                model_resolver::list_models(p)
                    .into_iter()
                    .map(|(id, name)| (p.clone(), id, name))
            })
            .collect();

        let scoped = &self.model_cycle;
        let models: Vec<(String, String, String)> = if scoped.is_empty() {
            all
        } else {
            // 保持 scope 顺序；不可用模型跳过
            scoped
                .iter()
                .filter_map(|s| {
                    all.iter()
                        .find(|(p, id, _)| p == &s.provider && id == &s.id)
                        .map(|(p, id, name)| (p.clone(), id.clone(), name.clone()))
                })
                .collect()
        };

        if models.is_empty() {
            self.push_msg("no models to cycle".to_string(), MsgLevel::Info);
        } else if models.len() == 1 {
            let hint = if scoped.is_empty() {
                "only one model available".to_string()
            } else {
                "only one model in scope".to_string()
            };
            self.push_msg_dedup(hint, "only one model", MsgLevel::Info);
        } else {
            let idx = models
                .iter()
                .position(|(p, id, _)| {
                    p == self.current_provider.as_deref().unwrap_or("")
                        && id == self.current_model.as_deref().unwrap_or("")
                })
                .unwrap_or(0);
            let n = models.len();

            // 从当前位置沿 dir 方向找到第一个模型尝试切换（worker 决定是否支持）；
            // scope 条目带 thinking 时先同步思考级别
            let probe = ((idx as i32 + dir).rem_euclid(n as i32)) as usize;
            let (provider, id, _name) = &models[probe];

            if let Some(m) = self
                .model_cycle
                .iter()
                .find(|m| &m.provider == provider && &m.id == id)
                && let Some(level) = &m.thinking
            {
                self.worker.send(AgentCommand::SetThinking {
                    level: level.clone(),
                    persist: false,
                });
            }

            self.worker.send(AgentCommand::SwitchModel {
                provider: provider.clone(),
                model_id: id.clone(),
                persist: false,
            });
        }
        self.dirty = true;
    }

    /// 应用设置项切换：更新列表 current_value、立即生效、写回 settings.json。
    fn apply_change(&mut self, id: &'static str, new_value: &str) {
        match id {
            "autocompact" => self.apply_change_autocompact(new_value),
            "cache-warming-mode" => self.apply_change_cache_warming(new_value),
            "cache-miss-notices" => self.apply_change_cache_miss_notices(new_value),
            "expose-session-env" => self.apply_change_expose_session_env(new_value),
            "steering-mode" => self.apply_change_steering_mode(new_value),
            "follow-up-mode" => self.apply_change_follow_up_mode(new_value),
            "hide-thinking" => self.apply_change_hide_thinking(new_value),
            "show-images" => self.apply_change_show_images(new_value),
            "quiet-startup" => self.apply_change_quiet_startup(new_value),
            "latex" => self.apply_change_latex(new_value),
            "mermaid" => self.apply_change_mermaid(new_value),
            "syntax-highlight" => self.apply_change_syntax_highlight(new_value),
            "autocomplete-max-visible" => self.apply_change_autocomplete_max_visible(new_value),
            "history-max-entries" => self.apply_change_history_max_entries(new_value),
            "wheel-scroll-lines" => self.apply_change_wheel_scroll_lines(new_value),
            "default-project-trust" => self.apply_change_default_project_trust(new_value),
            "images-auto-resize" => self.apply_change_images_auto_resize(new_value),
            "images-block" => self.apply_change_images_block(new_value),
            "retry" => self.persist(
                "retry.enabled",
                settings_manager::write_settings_retry_enabled(new_value == "true"),
            ),
            "retry-max-attempts" => {
                if let Ok(n) = new_value.parse::<u32>() {
                    self.persist(
                        "retry.maxRetries",
                        settings_manager::write_settings_retry_max_retries(n),
                    );
                }
            }
            "compact-reserve" => {
                if let Ok(n) = new_value.parse::<u32>() {
                    self.persist(
                        "compactReserveTokens",
                        settings_manager::write_settings_compact_reserve(n),
                    );
                }
            }
            "compact-keep-recent" => {
                if let Ok(n) = new_value.parse::<u32>() {
                    self.persist(
                        "compactKeepRecentTokens",
                        settings_manager::write_settings_compact_keep_recent(n),
                    );
                }
            }
            "temperature" => self.apply_change_temperature(new_value),
            _ => return,
        }

        if let Some(item) = self
            .settings_selector
            .items
            .iter_mut()
            .find(|it| it.id == id)
        {
            item.current_value = new_value.to_string();
        }
    }

    /// 设置项切换：autoCompact 写盘并持久化（agent 侧字段在 worker 读取设置时生效）
    fn apply_change_autocompact(&mut self, new_value: &str) {
        // actor：persist 到 settings.json；agent 侧字段在 worker 读取设置时生效
        let enabled = new_value == "true";

        self.persist(
            "autoCompact",
            settings_manager::write_settings_auto_compact(enabled),
        );
    }

    /// 设置项切换：cacheWarming 写盘并持久化（每次真实请求开始时重新读取）
    ///
    /// 提示走 [`push_msg_dedup`]：面板里连续切换时原地改写同一条，不刷屏。
    fn apply_change_cache_warming(&mut self, new_value: &str) {
        match settings_manager::write_settings_cache_warming(new_value) {
            Ok(applied) => self.push_msg_dedup(
                format!("Cache warming: {}", applied),
                "Cache warming: ",
                MsgLevel::Info,
            ),
            Err(e) => self.push_msg(
                format!("failed to save cacheWarming: {}", e),
                MsgLevel::Error,
            ),
        }
    }

    /// 设置项切换：showCacheMissNotices 写盘并持久化
    ///
    /// 事件到达时按磁盘值判断（[`events::apply_sink_cache_warm`]），无需内存镜像。
    fn apply_change_cache_miss_notices(&mut self, new_value: &str) {
        self.persist(
            "showCacheMissNotices",
            settings_manager::write_settings_show_cache_miss_notices(new_value == "true"),
        );
    }

    /// 设置项切换：exposeSessionEnvironment 写盘并持久化
    fn apply_change_expose_session_env(&mut self, new_value: &str) {
        let enabled = new_value == "true";

        self.persist(
            "exposeSessionEnvironment",
            settings_manager::write_settings_expose_session_environment(enabled),
        );
    }

    /// 设置项切换：steeringMode 写盘并持久化（回合读取 settings 时生效）
    fn apply_change_steering_mode(&mut self, new_value: &str) {
        // actor：persist；回合读取 settings 时生效（drain_next_batch 每次读）
        self.persist(
            "steeringMode",
            settings_manager::write_settings_steering_mode(new_value),
        );
    }

    /// 设置项切换：followUpMode 写盘并持久化（回合读取 settings 时生效）
    fn apply_change_follow_up_mode(&mut self, new_value: &str) {
        // actor：persist；回合读取 settings 时生效

        self.persist(
            "followUpMode",
            settings_manager::write_settings_follow_up_mode(new_value),
        );
    }

    /// 设置项切换：hideThinkingBlock 写盘并立即更新内存开关
    fn apply_change_hide_thinking(&mut self, new_value: &str) {
        self.set_show_thinking(new_value != "true");

        self.persist(
            "hideThinkingBlock",
            settings_manager::write_settings_hide_thinking(new_value == "true"),
        );
    }

    /// 设置项切换：showImages 写盘、立即生效并清消息渲染缓存
    /// （图片块占用的行数在「预留多行」与「整块不渲染」之间切换）。
    fn apply_change_show_images(&mut self, new_value: &str) {
        let enabled = new_value == "true";
        self.set_show_images(enabled);

        self.persist(
            "showImages",
            settings_manager::write_settings_show_images(enabled),
        );
    }

    /// 设置项切换：quietStartup 写盘并立即改变启动横幅的可见性。
    ///
    /// 资源清单只在启动时输出，此处仅记录档位； `true` 时立刻收起当前还显示的横幅。
    fn apply_change_quiet_startup(&mut self, new_value: &str) {
        let Some(level) = settings_manager::QuietStartup::parse(new_value) else {
            return;
        };

        self.banner_hidden = !level.shows_banner();
        self.persist(
            "quietStartup",
            settings_manager::write_settings_quiet_startup(level),
        );
    }

    /// 设置项切换：syntaxHighlight 写盘、立即生效并废弃在途后台补全
    fn apply_change_syntax_highlight(&mut self, new_value: &str) {
        let enabled = new_value == "true";
        self.theme.syntax_highlight = enabled;
        // 立即生效：停掉在途后台补全（递增快照 ID 丢弃其结果）、清空渲染缓存，
        // 下一帧按新开关重建；补全恢复高亮的逻辑不会覆盖用户刚做的选择。
        self.highlight_backlog = false;
        self.highlight_started = false;
        self.highlight_snap_id += 1;
        self.invalidate_render();

        self.persist(
            "syntaxHighlight",
            settings_manager::write_settings_syntax_highlight(enabled),
        );
    }

    /// 设置项切换：mermaid 写盘、立即生效并清消息渲染缓存
    fn apply_change_mermaid(&mut self, new_value: &str) {
        let enabled = new_value == "true";
        self.theme.mermaid = enabled;
        // 消息缓存可能含已渲染的 mermaid 图：清空后下一帧按新开关重建
        self.invalidate_render();

        self.persist("mermaid", settings_manager::write_settings_mermaid(enabled));
    }

    /// 设置项切换：latex 写盘、立即生效并清消息渲染缓存
    fn apply_change_latex(&mut self, new_value: &str) {
        let enabled = new_value == "true";
        self.theme.latex = enabled;
        // 消息缓存可能含已渲染的公式：清空后下一帧按新开关重建
        self.invalidate_render();
        self.persist("latex", settings_manager::write_settings_latex(enabled));
    }

    /// 设置项切换：autocompleteMaxVisible 写盘并更新内存上限
    fn apply_change_autocomplete_max_visible(&mut self, new_value: &str) {
        if let Ok(n) = new_value.parse::<usize>() {
            self.autocomplete_max_visible = n;

            self.persist(
                "autocompleteMaxVisible",
                settings_manager::write_settings_autocomplete_max_visible(n),
            );
        }
    }

    /// 设置项切换：historyMaxEntries 写盘、更新内存档位，并**立即截断**内存历史。
    ///
    /// 立即截断：否则面板显示 100、`/history` 还能搜出 800 条，自相矛盾。
    /// `max == 0`（不落盘）时 `cap_history` 不截断——那个档位只关持久化，不关功能。
    fn apply_change_history_max_entries(&mut self, new_value: &str) {
        if let Ok(n) = new_value.parse::<usize>() {
            self.history_max_entries = n;
            self.editor.cap_history(n);

            self.persist(
                "historyMaxEntries",
                settings_manager::write_settings_history_max_entries(n),
            );
        }
    }

    /// 设置项切换：fullscreenWheelScrollLines 写盘并同步内存加速器
    fn apply_change_wheel_scroll_lines(&mut self, new_value: &str) {
        let lines = if new_value == "auto" {
            WheelScrollLines::Auto
        } else if let Ok(n) = new_value.parse::<usize>() {
            WheelScrollLines::Lines(n.clamp(1, WheelScrollLines::MAX))
        } else {
            return;
        };

        self.wheel_accel.set_lines(lines);
        self.persist(
            "fullscreenWheelScrollLines",
            settings_manager::write_settings_fullscreen_wheel_scroll_lines(lines),
        );
    }

    /// 设置项切换：images.autoResize 写盘（每次读图时重新读取，立即生效）
    fn apply_change_images_auto_resize(&mut self, new_value: &str) {
        self.persist(
            "images.autoResize",
            settings_manager::write_settings_images_auto_resize(new_value == "true"),
        );
    }

    /// 设置项切换：images.blockImages 写盘（每个图片入口读取时生效）
    fn apply_change_images_block(&mut self, new_value: &str) {
        self.persist(
            "images.blockImages",
            settings_manager::write_settings_images_block_images(new_value == "true"),
        );
    }

    /// 设置项切换：temperature 写盘；`default` 删除该键（回落模型自身采样参数）
    fn apply_change_temperature(&mut self, new_value: &str) {
        let value = if new_value == "default" {
            None
        } else {
            match new_value.parse::<f64>() {
                Ok(t) => Some(t),
                Err(_) => return,
            }
        };

        self.persist(
            "temperature",
            settings_manager::write_settings_temperature(value),
        );
    }

    /// 设置项切换：defaultProjectTrust 归一化后写盘并持久化
    fn apply_change_default_project_trust(&mut self, new_value: &str) {
        let value = match new_value {
            "Always trust" => "always",
            "Never trust" => "never",
            _ => "ask",
        };

        self.persist(
            "defaultProjectTrust",
            settings_manager::write_settings_default_project_trust(value),
        );
    }

    /// 选择器键盘处理（settings_selector.active 时由 handlers/mod 分发）。
    /// 导航/确认/取消键由全局 keybindings 驱动（tui.select.*）； Ctrl+J/K 为扩展导航键（保留）。
    pub(super) fn handle_settings_selector_key(&mut self, key: &KeyEvent) -> KeyAction {
        let kb = keybindings::get_global();

        // 子菜单激活：Up/Down 移动、Enter 确认、Esc 返回（保留备用）
        if self.settings_selector.submenu.is_some() {
            self.handle_settings_submenu_key(key, kb);
            return KeyAction::Continue;
        }

        // 主列表：取消 / 确认 / 翻页 / 导航 / 空格切换 / 搜索输入
        self.handle_settings_main_key(key, kb);
        KeyAction::Continue
    }

    /// 勾选列表子面板：切换当前项的勾选态并**立即写盘**（`+name` / `-name` 增量语法）。
    ///
    /// 写盘后按回读的**有效**工具集刷新勾选：项目级 `defaultTools` 覆盖全局键时，
    /// 切换可能不会改变有效状态（面板不会假装已生效）。
    fn toggle_submenu_option(&mut self) {
        let Some(sub) = self.settings_selector.submenu.as_ref() else {
            return;
        };
        let Some(opt) = sub.options.get(sub.selected) else {
            return;
        };
        let (name, enabled_now, item_index) = (opt.value.clone(), opt.checked, sub.item_index);

        if let Err(e) = settings_manager::toggle_settings_default_tool(&name, enabled_now) {
            self.push_msg(
                format!("failed to save defaultTools: {}", e),
                MsgLevel::Error,
            );
            return;
        }

        let enabled = settings_manager::read_settings_default_tools().unwrap_or_else(|| {
            settings_manager::DEFAULT_TOOL_NAMES
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
        if let Some(sub) = self.settings_selector.submenu.as_mut() {
            for opt in &mut sub.options {
                opt.checked = enabled.contains(&opt.value);
            }
        }
        if let Some(item) = self.settings_selector.items.get_mut(item_index) {
            item.current_value = if enabled.is_empty() {
                "none".to_string()
            } else {
                enabled.join(", ")
            };
        }
    }

    /// 设置子菜单键处理：Up/Down 移动、Enter 确认（勾选列表为切换）、Esc 返回
    fn handle_settings_submenu_key(&mut self, key: &KeyEvent, kb: &'static KeybindingsManager) {
        let toggle = self
            .settings_selector
            .submenu
            .as_ref()
            .is_some_and(|s| s.kind == SubmenuKind::Toggle);

        if kb.matches(key, "tui.select.cancel") {
            self.settings_selector.submenu = None;
            self.dirty = true;
        } else if kb.matches(key, "tui.select.confirm")
            || (toggle && key.code == KeyCode::Char(' ') && key.modifiers.is_empty())
        {
            if toggle {
                self.toggle_submenu_option();
            } else {
                self.confirm_submenu();
            }
            self.dirty = true;
        } else if kb.matches(key, "tui.select.up")
            || (key.code == KeyCode::Char('k') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.settings_selector.move_selection(-1);
            self.dirty = true;
        } else if kb.matches(key, "tui.select.down")
            || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.settings_selector.move_selection(1);
            self.dirty = true;
        }
    }

    /// 设置主列表键处理：取消 / 确认 / 翻页 / 导航 / 空格切换 / 搜索输入
    fn handle_settings_main_key(&mut self, key: &KeyEvent, kb: &'static KeybindingsManager) {
        if kb.matches(key, "tui.select.cancel") {
            self.settings_selector.close();
            self.dirty = true;
        } else if kb.matches(key, "tui.select.confirm") {
            self.activate_sel();
            self.dirty = true;
        } else if kb.matches(key, "tui.select.pageUp") {
            self.settings_selector.move_selection(-10);
            self.dirty = true;
        } else if kb.matches(key, "tui.select.pageDown") {
            self.settings_selector.move_selection(10);
            self.dirty = true;
        } else if kb.matches(key, "tui.select.up")
            || (key.code == KeyCode::Char('k') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.settings_selector.move_selection(-1);
            self.dirty = true;
        } else if kb.matches(key, "tui.select.down")
            || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.settings_selector.move_selection(1);
            self.dirty = true;
        } else if key.code == KeyCode::Char(' ')
            && key.modifiers.is_empty()
            && self.settings_selector.filter.is_empty()
        {
            // 空搜索时 Space 切换（Space 在无输入时是确认键）
            self.activate_sel();
            self.dirty = true;
        } else {
            // type to search：其余键交给搜索输入框（Shift 层映射 / 编辑键统一处理）
            let sel = &mut self.settings_selector;
            match sel.filter.handle_key(key) {
                InputAction::Edited => {
                    sel.recompute();
                    self.dirty = true;
                }
                InputAction::Moved => self.dirty = true,
                InputAction::None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::modes::interactive::agent_actor::WorkerHandle;
    use crate::modes::interactive::app::App;
    use crate::modes::interactive::handlers::handle_event;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use std::{cell::RefCell, rc::Rc};

    /// 测试 agent 目录守卫：每测试独立临时目录（线程本地 override），
    /// 替代全局锁 + 进程级 env 劫持（并行互不干扰、无死锁）。
    fn test_agent_dir() -> crate::test_support::AgentDirGuard {
        crate::test_support::AgentDirGuard::temp()
    }

    fn write_test_auth(provider: &str, key: &str) {
        crate::core::auth::write_auth_key(provider, key).unwrap();
    }

    /// 清空测试目录 auth.json 的残留（并行测试可能互踩留下脏凭据），
    /// 保证每个用例从干净状态开始
    fn reset_test_auth() {
        for p in ["deepseek", "opencode", "opencode-go"] {
            crate::core::auth::remove_auth(p).ok();
        }
    }

    #[test]
    fn cache_warming_toggle_replaces_message_in_place() {
        let _g = test_agent_dir();
        let mut app = App::new();

        for mode in ["streaming", "idle", "off"] {
            app.apply_change_cache_warming(mode);
        }

        let texts: Vec<String> = app
            .system_messages
            .iter()
            .map(|(_, m)| crate::modes::interactive::app::sys_spans_text(&m.spans))
            .collect();
        assert_eq!(
            texts,
            vec!["Cache warming: off"],
            "应原地改写同一条: {texts:?}"
        );
    }

    #[test]
    fn cycle_thinking_does_not_block_while_agent_locked() {
        // 回归：模型回复中 prompt future 持 agent 锁时，切换思考模式（Shift+Tab）
        // 不得拖锁等待——拖锁会与 sink 的 shared 锁互相 deadlock。
        let _g = test_agent_dir();
        let (agent, mut cmd_rx) = test_agent_cmd();
        let mut st = App::new();
        st.worker = agent;
        st.model_reasoning = true;
        st.thinking_levels = vec!["off".to_string(), "high".to_string()];
        st.cycle_thinking();
        // actor：切换经 SetThinking 命令下发 worker，不阻塞不 panic
        let cmd = cmd_rx.try_recv().expect("应发 SetThinking");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SetThinking { .. }
            ),
            "应发 SetThinking: {cmd:?}"
        );
    }

    #[test]
    fn ctrl_p_cycles_between_scoped_models() {
        let _g = test_agent_dir();
        let (agent, _cmd_rx) = test_agent_cmd(); // 必须先设置 PRUX_AGENT_DIR，再动 auth.json
        reset_test_auth();
        write_test_auth("deepseek", "sk-test");
        // scoped 循环列表改由 App.model_cycle 镜像驱动（worker 持 agent）

        let shared = Rc::new(RefCell::new(App::new()));
        shared.borrow_mut().worker = agent;
        // scoped 只剩一个模型：唯一模型没有别的可切，提示原因（对齐 pi）。
        // 用 provider/id 精确形式而非裸 id：裸 id 走前缀匹配，会同时命中目录里的
        // deepseek-v4-flash-vision-exp（id.starts_with），scope 不再是单模型。
        shared.borrow_mut().model_cycle = vec![crate::core::model_scope::ScopedModel {
            provider: "deepseek".to_string(),
            id: "deepseek-flash".to_string(),
            thinking: None,
        }];
        shared.borrow_mut().current_provider = Some("deepseek".to_string());
        shared.borrow_mut().current_model = Some("deepseek-flash".to_string());
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        );
        let st = shared.borrow_mut();
        assert_eq!(st.system_messages.len(), 1, "应有一条提示");
        let text = crate::modes::interactive::app::sys_spans_text(&st.system_messages[0].1.spans);
        assert!(
            text.starts_with("only one model"),
            "提示应说明唯一模型: {}",
            text
        );
        // agent 状态断言由 worker/场景测试覆盖

        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn ctrl_p_cycles_scope_order_with_thinking() {
        // 对齐 pi _cycleScopedModel：Ctrl+P 严格按 scope 顺序循环（而非 provider 目录顺序），
        // 命中带 thinking 的条目先 SetThinking 再 SwitchModel
        let _g = test_agent_dir();
        let (agent, mut cmd_rx) = test_agent_cmd();
        reset_test_auth();
        write_test_auth("deepseek", "sk-test");

        let shared = Rc::new(RefCell::new(App::new()));
        shared.borrow_mut().worker = agent;
        shared.borrow_mut().model_cycle = vec![
            crate::core::model_scope::ScopedModel {
                provider: "deepseek".to_string(),
                id: "deepseek-flash".to_string(),
                thinking: Some("high".to_string()),
            },
            crate::core::model_scope::ScopedModel {
                provider: "deepseek".to_string(),
                id: "deepseek-v4-pro".to_string(),
                thinking: None,
            },
        ];
        // 当前在 pro（scope idx 1）→ forward 到 flash（带 high）
        shared.borrow_mut().current_provider = Some("deepseek".to_string());
        shared.borrow_mut().current_model = Some("deepseek-v4-pro".to_string());
        let ctrl_p = || Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        handle_event(&shared, ctrl_p());
        let cmd = cmd_rx.try_recv().expect("应先发 SetThinking");
        assert!(
            matches!(&cmd, AgentCommand::SetThinking { level, .. } if level == "high"),
            "应发 SetThinking high: {cmd:?}"
        );
        let cmd = cmd_rx.try_recv().expect("再发 SwitchModel");
        assert!(
            matches!(
                &cmd,
                AgentCommand::SwitchModel { provider, model_id, .. }
                    if provider == "deepseek" && model_id == "deepseek-flash"
            ),
            "应发 SwitchModel flash: {cmd:?}"
        );

        // 模拟 worker 回执：当前模型已切到 flash（与 model_cycle 内顺序同步）
        shared.borrow_mut().current_model = Some("deepseek-flash".to_string());

        // 再 forward → pro（无 thinking）→ 仅 SwitchModel，无 SetThinking
        handle_event(&shared, ctrl_p());
        let cmd = cmd_rx.try_recv().expect("应发 SwitchModel");
        assert!(
            matches!(
                &cmd,
                AgentCommand::SwitchModel { provider, model_id, .. }
                    if provider == "deepseek" && model_id == "deepseek-v4-pro"
            ),
            "应发 SwitchModel pro: {cmd:?}"
        );
        assert!(
            cmd_rx.try_recv().is_err(),
            "pro 无 thinking 不应发 SetThinking"
        );

        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn ctrl_p_cycles_across_configured_providers() {
        // 对齐 pi cycleModel / /model 面板：无 --models 时循环所有已配置凭据
        // provider 的模型（跨 provider），而不是只循环当前 provider
        let _g = test_agent_dir();
        let (agent, mut cmd_rx) = test_agent_cmd(); // 必须先设置 PRUX_AGENT_DIR，再动 auth.json
        reset_test_auth();
        write_test_auth("deepseek", "sk-test");
        write_test_auth("opencode", "sk-test-2"); // 当前 provider = deepseek，模型 = deepseek-flash
        let shared = Rc::new(RefCell::new(App::new()));
        shared.borrow_mut().worker = agent;
        shared.borrow_mut().current_provider = Some("deepseek".to_string());
        shared.borrow_mut().current_model = Some("deepseek-flash".to_string());
        let ctrl_p = || Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        // 循环下发 SwitchModel 命令（跨 provider）
        handle_event(&shared, ctrl_p());
        let cmd = cmd_rx.try_recv().expect("应发出 SwitchModel");
        assert!(
            matches!(cmd, crate::modes::interactive::agent_actor::AgentCommand::SwitchModel { ref model_id, .. } if model_id == "deepseek-v4-pro"),
            "flash -> 目录下一项: {cmd:?}"
        );
        handle_event(&shared, ctrl_p());
        // 第二次循环：current_model 仍为首资产镜像（无回执推进），命令仍应发出
        let cmd = cmd_rx.try_recv().expect("第二次应发 SwitchModel");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SwitchModel { .. }
            ),
            "第二次应发 SwitchModel: {cmd:?}"
        );
        crate::core::auth::remove_auth("deepseek").ok();
        crate::core::auth::remove_auth("opencode").ok();
        let _ =
            std::fs::remove_file(crate::core::settings_manager::agent_dir().join("settings.json"));
    }

    #[test]
    fn ctrl_p_switch_does_not_persist_default_model() {
        // 回归：Ctrl+P 只改会话模型，不写 settings 默认值
        // （默认值只由 /model 面板 Ctrl+S 写入）
        let _g = test_agent_dir();
        let (agent, mut cmd_rx) = test_agent_cmd(); // 必须先设置 PRUX_AGENT_DIR，再动 auth.json
        reset_test_auth();
        write_test_auth("deepseek", "sk-test");
        let shared = Rc::new(RefCell::new(App::new()));
        shared.borrow_mut().worker = agent;
        shared.borrow_mut().current_provider = Some("deepseek".to_string());
        shared.borrow_mut().current_model = Some("deepseek-flash".to_string());
        // actor：Ctrl+P 只下发 SwitchModel 命令（worker 不再写盘）
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        );
        let cmd = cmd_rx.try_recv().expect("应发 SwitchModel");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SwitchModel { .. }
            ),
            "切换命令: {cmd:?}"
        );
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SwitchModel {
                    persist: false,
                    ..
                }
            ),
            "Ctrl+P 应以 persist=false 下发: {cmd:?}"
        );
        let s = crate::core::settings_manager::read_settings();
        assert!(
            s.default_provider.is_none() && s.default_model.is_none(),
            "Ctrl+P 不应写默认模型: {:?}/{:?}",
            s.default_provider,
            s.default_model
        );
    }

    #[test]
    fn cycle_thinking_does_not_persist_default_thinking_level() {
        // 回归：Shift+Tab 只改会话 thinking 级别，不写 settings 默认值
        // （默认值只由 /thinking 面板 Ctrl+S 写入）
        let _g = test_agent_dir();
        let (agent, mut cmd_rx) = test_agent_cmd();
        let mut st = App::new();
        st.worker = agent;
        st.model_reasoning = true;
        st.thinking_levels = vec!["off".to_string(), "high".to_string()];
        st.cycle_thinking();
        let cmd = cmd_rx.try_recv().expect("应发 SetThinking");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SetThinking { .. }
            ),
            "切换命令: {cmd:?}"
        );
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SetThinking {
                    persist: false,
                    ..
                }
            ),
            "Shift+Tab 应以 persist=false 下发: {cmd:?}"
        );
        assert!(
            crate::core::settings_manager::read_settings()
                .default_thinking_level
                .is_none(),
            "Shift+Tab 不应写默认 thinking 级别"
        );
    }

    /// channels 版 worker：可断言 handler 发出的命令（分层测试 C 的 handler 层）
    fn test_agent_cmd() -> (
        WorkerHandle,
        crate::modes::interactive::agent_actor::CommandRx,
    ) {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let (cmd_tx, cmd_rx) = crate::modes::interactive::agent_actor::channels();
        (WorkerHandle::new(cmd_tx), cmd_rx)
    }

    #[test]
    fn apply_change_busy_hints_when_agent_field_skipped() {
        // 回归：agent 忙碌（prompt future 持锁）时改动设置，写盘仍应成功，
        // 但 agent 字段未同步必须给出提示——否则用户以为已生效实为静默错过。
        let _ad = crate::test_support::AgentDirGuard::temp();
        // 固定到共享测试目录，避免污染真实 settings.json
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _ = crate::core::settings_manager::write_settings_auto_compact(false);
        let mut st = App::new();
        // actor：apply_change 直接 persist 到 settings.json（agent 字段由 worker 读取设置生效）
        st.apply_change("autocompact", "true");
        assert!(
            crate::core::settings_manager::read_settings_auto_compact(),
            "设置应写盘"
        );
        // 恢复默认状态，避免影响共享测试目录里的后续测试
        crate::core::settings_manager::write_settings_auto_compact(true).ok();
    }

    fn open_sel_on(app: &mut App) {
        let (show_thinking, theme_name, autocomplete_max) = (
            app.show_thinking,
            app.theme.name.clone(),
            app.autocomplete_max_visible,
        );
        app.settings_selector.open(
            show_thinking,
            &theme_name,
            autocomplete_max,
            app.thinking_level.as_deref().unwrap_or("off"),
        );
    }

    #[test]
    fn cycle_toggle_persists_and_updates_value() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        // 固定到共享测试目录，避免污染真实 settings.json；
        // 并显式钉住初始值（不依赖测试目录里的残留状态）
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_settings_auto_compact(true).ok();
        let mut app = App::new();
        app.model_reasoning = true;
        app.thinking_levels = vec!["off".to_string(), "high".to_string()];
        app.thinking_level = Some("off".to_string());
        open_sel_on(&mut app);
        let idx = app
            .settings_selector
            .items
            .iter()
            .position(|i| i.id == "autocompact")
            .unwrap();
        app.activate_sel();
        assert_eq!(
            app.settings_selector.items[idx].current_value, "false",
            "Auto-compact 应循环到 false"
        );
        assert!(
            !crate::core::settings_manager::read_settings_auto_compact(),
            "切换应写盘"
        );
        // 恢复默认，避免影响其他测试
        crate::core::settings_manager::write_settings_auto_compact(true).ok();
    }

    #[test]
    fn builtin_tools_submenu_toggles_and_persists() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let path = settings_manager::agent_dir().join("settings.json");
        let _ = std::fs::remove_file(&path);

        let mut app = App::new();
        open_sel_on(&mut app);

        // 定位 Built-in tools 项并打开勾选子面板
        let idx = app
            .settings_selector
            .items
            .iter()
            .position(|i| i.id == "builtin-tools")
            .expect("面板应包含 builtin-tools");
        app.settings_selector.selected = app
            .settings_selector
            .filtered
            .iter()
            .position(|&i| i == idx)
            .expect("builtin-tools 应在过滤结果中");
        app.activate_sel();
        assert!(app.settings_selector.submenu.is_some(), "应打开子面板");

        let grep = app
            .settings_selector
            .submenu
            .as_ref()
            .unwrap()
            .options
            .iter()
            .position(|o| o.value == "grep")
            .expect("子面板应列出 grep");
        assert!(
            !app.settings_selector.submenu.as_ref().unwrap().options[grep].checked,
            "grep 默认不在默认工具集里"
        );

        app.settings_selector.submenu.as_mut().unwrap().selected = grep;
        app.toggle_submenu_option();
        assert!(app.settings_selector.submenu.as_ref().unwrap().options[grep].checked);
        assert!(
            settings_manager::read_settings_default_tools()
                .unwrap()
                .contains(&"grep".to_string()),
            "切换应写盘"
        );
        assert!(
            app.settings_selector.items[idx]
                .current_value
                .contains("grep"),
            "主列表展示值应同步: {}",
            app.settings_selector.items[idx].current_value
        );

        // 再切回：条目清空 → 删键（回落默认工具集，此时读取返回 None）
        app.toggle_submenu_option();
        assert!(!app.settings_selector.submenu.as_ref().unwrap().options[grep].checked);
        assert!(
            settings_manager::read_settings_default_tools()
                .unwrap_or_default()
                .iter()
                .all(|t| t != "grep"),
            "grep 应回到关闭状态"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn new_selector_items_persist_on_activate() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let path = settings_manager::agent_dir().join("settings.json");
        let _ = std::fs::remove_file(&path);

        let mut app = App::new();
        open_sel_on(&mut app);

        // 逐项循环一次：默认值 → 下一档，并写盘
        for (id, expect) in [
            ("images-auto-resize", "false"),
            ("images-block", "true"),
            ("retry", "false"),
            ("retry-max-attempts", "5"),
            ("compact-reserve", "32768"),
            ("compact-keep-recent", "40000"),
            ("temperature", "0"),
        ] {
            let idx = app
                .settings_selector
                .items
                .iter()
                .position(|i| i.id == id)
                .unwrap_or_else(|| panic!("面板应包含 {id}"));
            app.settings_selector.selected = app
                .settings_selector
                .filtered
                .iter()
                .position(|&i| i == idx)
                .unwrap();
            app.activate_sel();
            assert_eq!(
                app.settings_selector.items[idx].current_value, expect,
                "{id} 应循环到 {expect}"
            );
        }

        assert!(!settings_manager::read_settings_images_auto_resize());
        assert!(settings_manager::read_settings_images_block_images());
        assert!(!settings_manager::read_settings_retry_enabled());
        assert_eq!(settings_manager::read_settings_retry_max_retries(), 5);
        assert_eq!(settings_manager::read_settings_compact_reserve(), 32768);
        assert_eq!(settings_manager::read_settings_compact_keep_recent(), 40000);
        assert_eq!(settings_manager::read_settings_temperature(), Some(0.0));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cache_miss_notices_toggle_persists_and_updates_value() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_settings_show_cache_miss_notices(false).ok();
        let mut app = App::new();
        open_sel_on(&mut app);
        let idx = app
            .settings_selector
            .items
            .iter()
            .position(|i| i.id == "cache-miss-notices")
            .expect("面板应包含 cache-miss-notices");
        assert_eq!(app.settings_selector.items[idx].current_value, "false");

        app.apply_change("cache-miss-notices", "true");
        assert!(
            crate::core::settings_manager::read_settings_show_cache_miss_notices(),
            "切换应写盘"
        );
        assert_eq!(app.settings_selector.items[idx].current_value, "true");

        // 恢复默认，避免影响其他测试
        crate::core::settings_manager::write_settings_show_cache_miss_notices(false).ok();
    }
}
