//! 右侧面板与列表选择器：设置 / 扩展 / 技能 / 模型 / 会话选择页的构建与提交。

use super::*;

impl GuiApp {
    /// 数值型设置：弹输入框，Enter 解析后写入（temperature 留空 = 清除）。
    pub(super) fn start_number_setting(&mut self, key: SettingKey, cx: &mut Context<Self>) {
        let current = match key {
            SettingKey::RetryMaxRetries => {
                settings_manager::read_settings_retry_max_retries().to_string()
            }
            SettingKey::Temperature => settings_manager::read_settings_temperature()
                .map(|value| value.to_string())
                .unwrap_or_default(),
            SettingKey::CompactReserve => {
                settings_manager::read_settings_compact_reserve().to_string()
            }
            SettingKey::CompactKeepRecent => {
                settings_manager::read_settings_compact_keep_recent().to_string()
            }
            _ => String::new(),
        };
        let mut editor = Editor::new();
        editor.insert(&current);
        editor.select_all();
        self.dialog = Some(SessionDialog::Number { key, editor });
        cx.notify();
    }
    /// Ctrl-S：把扩展/技能面板的草稿落盘（面板打开时按键先被覆盖层分支吃掉，
    /// 所以要在覆盖层分发之前调用）。
    pub(super) fn save_panel_drafts(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(draft) = self.extensions_draft.take() {
            let mut changed = 0usize;
            for (name, enabled) in draft {
                if crate::core::extensions::is_extension_enabled(&name) != enabled {
                    crate::core::extensions::set_extension_enabled(&name, enabled);
                    changed += 1;
                }
            }
            if changed > 0 {
                if let Err(error) = crate::core::extensions::persist_extension_states() {
                    self.toast(
                        format!("Cannot save extension state: {error}"),
                        ToastKind::Error,
                        cx,
                    );
                }
                _ = self.cmd_tx.send(AgentCommand::RebuildTools);
            }
            self.overlay = None;
            self.toast(
                format!("Applied {changed} extension change(s)"),
                ToastKind::Success,
                cx,
            );
            cx.notify();
            return true;
        }
        if let Some(rules) = self.skills_draft.take() {
            match settings_manager::write_settings_skills(&rules) {
                Ok(()) => {
                    _ = self.cmd_tx.send(AgentCommand::Reload {
                        context_files: Vec::new(),
                        skills: Vec::new(),
                        system_prompt: None,
                        append_system_prompt: None,
                    });
                    self.overlay = None;
                    self.toast("Skill rules saved", ToastKind::Success, cx);
                }
                Err(error) => self.toast(
                    format!("Cannot write settings: {error}"),
                    ToastKind::Error,
                    cx,
                ),
            }
        }
        cx.notify();
        true
    }
    /// 数值设置落盘：解析失败只报错，对话框保留以便改回来。
    pub(super) fn confirm_number_setting(
        &mut self,
        key: SettingKey,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        let value = text.trim();
        let result: Result<(), String> = match key {
            SettingKey::RetryMaxRetries => value
                .parse::<u32>()
                .map_err(|_| "needs a non-negative integer".to_string())
                .and_then(|v| {
                    settings_manager::write_settings_retry_max_retries(v).map_err(|e| e.to_string())
                }),
            SettingKey::Temperature => {
                if value.is_empty() {
                    settings_manager::write_settings_temperature(None).map_err(|e| e.to_string())
                } else {
                    value
                        .parse::<f64>()
                        .map_err(|_| "needs a decimal number".to_string())
                        .and_then(|v| {
                            settings_manager::write_settings_temperature(Some(v))
                                .map_err(|e| e.to_string())
                        })
                }
            }
            SettingKey::CompactReserve => value
                .parse::<u32>()
                .map_err(|_| "needs a non-negative integer".to_string())
                .and_then(|v| {
                    settings_manager::write_settings_compact_reserve(v).map_err(|e| e.to_string())
                }),
            SettingKey::CompactKeepRecent => value
                .parse::<u32>()
                .map_err(|_| "needs a non-negative integer".to_string())
                .and_then(|v| {
                    settings_manager::write_settings_compact_keep_recent(v)
                        .map_err(|e| e.to_string())
                }),
            _ => Ok(()),
        };
        match result {
            Ok(()) => {
                self.dialog = None;
                self.open_settings_panel(cx);
            }
            Err(error) => {
                self.transcript
                    .push_note(Note::Error(format!("Setting not saved: {error}")));
            }
        }
    }
    /// 设置面板：列出常用开关与外观，Enter 直接改并落盘到 settings.json。
    pub(super) fn open_settings_panel(&mut self, _cx: &mut Context<Self>) {
        let on_off = |on: bool| if on { "on" } else { "off" };
        let entries = vec![
            Entry {
                label: format!(
                    "Theme: {}",
                    if self.theme.is_dark { "dark" } else { "light" }
                ),
                detail: "Enter to toggle".into(),
                action: Action::SetDark(!self.theme.is_dark),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Auto-compact: {}",
                    on_off(settings_manager::read_settings_auto_compact())
                ),
                detail: "Compact when near the limit".into(),
                action: Action::ToggleSetting(SettingKey::AutoCompact),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Auto-retry: {}",
                    on_off(settings_manager::read_settings_retry_enabled())
                ),
                detail: "Retry failed requests".into(),
                action: Action::ToggleSetting(SettingKey::Retry),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Hide thinking: {}",
                    on_off(settings_manager::read_settings_hide_thinking())
                ),
                detail: "Hide model reasoning".into(),
                action: Action::ToggleSetting(SettingKey::HideThinking),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Syntax highlighting: {}",
                    on_off(settings_manager::read_settings_syntax_highlight())
                ),
                detail: "Highlight code blocks".into(),
                action: Action::ToggleSetting(SettingKey::SyntaxHighlight),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Auto-resize images: {}",
                    on_off(settings_manager::read_settings_images_auto_resize())
                ),
                detail: "Downscale oversized images".into(),
                action: Action::ToggleSetting(SettingKey::ImagesAutoResize),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Block images: {}",
                    on_off(settings_manager::read_settings_images_block_images())
                ),
                detail: "Refuse image input".into(),
                action: Action::ToggleSetting(SettingKey::ImagesBlockImages),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Expose session env: {}",
                    on_off(settings_manager::read_settings_expose_session_environment())
                ),
                detail: "Pass session info to tools as env vars".into(),
                action: Action::ToggleSetting(SettingKey::ExposeSessionEnvironment),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Cache miss notices: {}",
                    on_off(settings_manager::read_settings_show_cache_miss_notices())
                ),
                detail: String::new(),
                action: Action::ToggleSetting(SettingKey::ShowCacheMissNotices),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Steering mode: {}",
                    settings_manager::read_settings_steering_mode()
                ),
                detail: "Enter to cycle".into(),
                action: Action::ToggleSetting(SettingKey::SteeringMode),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Follow-up mode: {}",
                    settings_manager::read_settings_follow_up_mode()
                ),
                detail: "Enter to cycle".into(),
                action: Action::ToggleSetting(SettingKey::FollowUpMode),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Cache warming: {}",
                    settings_manager::read_settings_cache_warming()
                ),
                detail: "Enter to cycle".into(),
                action: Action::ToggleSetting(SettingKey::CacheWarming),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Quiet startup: {:?}",
                    settings_manager::read_settings_quiet_startup()
                ),
                detail: "Enter to cycle".into(),
                action: Action::ToggleSetting(SettingKey::QuietStartup),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Max retries: {}",
                    settings_manager::read_settings_retry_max_retries()
                ),
                detail: "Enter to edit".into(),
                action: Action::ToggleSetting(SettingKey::RetryMaxRetries),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "temperature: {}",
                    settings_manager::read_settings_temperature()
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "(model default)".into())
                ),
                detail: "Enter to edit, empty restores default".into(),
                action: Action::ToggleSetting(SettingKey::Temperature),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Compact reserve tokens: {}",
                    settings_manager::read_settings_compact_reserve()
                ),
                detail: "Enter to edit".into(),
                action: Action::ToggleSetting(SettingKey::CompactReserve),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Compact keep recent tokens: {}",
                    settings_manager::read_settings_compact_keep_recent()
                ),
                detail: "Enter to edit".into(),
                action: Action::ToggleSetting(SettingKey::CompactKeepRecent),
                ..Entry::default()
            },
            Entry {
                label: format!("UI scale: {}%", (theme::ui_scale() * 100.0).round()),
                detail: "Enter to reset 100%".into(),
                action: Action::Scale(1.0),
                ..Entry::default()
            },
            Entry {
                label: "Default model…".into(),
                detail: String::new(),
                action: Action::Open(ListKind::Model),
                ..Entry::default()
            },
            Entry {
                label: "Thinking level…".into(),
                detail: String::new(),
                action: Action::Open(ListKind::Thinking),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Config directory: {}",
                    settings_manager::agent_dir().display()
                ),
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            },
        ];
        self.overlay = Some(Overlay::List(List::new("Settings", entries)));
    }
    /// 扩展管理面板：`[x] 名称` 勾选态，Enter/点击切换草稿，Ctrl-S 才落盘，Tab 看详情。
    pub(super) fn open_extension_panel(&mut self, cx: &mut Context<Self>) {
        if self.extensions_draft.is_none() {
            self.extensions_draft = Some(
                configurable_extensions()
                    .into_iter()
                    .map(|(name, enabled, _)| (name, enabled))
                    .collect(),
            );
        }
        self.rebuild_extension_panel(cx);
    }
    /// 用草稿重算扩展行：勾选态来自草稿，Ctrl-S 才落到注册表并写盘。
    pub(super) fn rebuild_extension_panel(&mut self, cx: &mut Context<Self>) {
        let draft = self.extensions_draft.clone().unwrap_or_default();
        let modes: std::collections::HashMap<String, String> = configurable_extensions()
            .into_iter()
            .map(|(name, _, modes)| {
                let labels: Vec<String> = modes.iter().map(|m| m.as_str().to_string()).collect();
                (name, labels.join(" / "))
            })
            .collect();
        let mut entries: Vec<Entry> = draft
            .into_iter()
            .map(|(name, enabled)| Entry {
                label: name.clone(),
                detail: modes.get(&name).cloned().unwrap_or_default(),
                action: Action::ToggleExtension(name),
                checked: Some(enabled),
            })
            .collect();
        if entries.is_empty() {
            entries.push(Entry {
                label: "No extensions registered".into(),
                ..Entry::default()
            });
        }
        self.replace_list("Extensions", entries, DetailSource::Extension);
        cx.notify();
    }
    /// `/skills`：按 TUI 的技能管理面板实现——列出全部可发现的技能并显示启用态，
    /// Enter 在草稿里切换启停，Ctrl-S 落盘并重载，Tab 看详情，Esc 丢弃草稿。
    pub(super) fn open_skills_panel(&mut self, cx: &mut Context<Self>) {
        if self.skills_draft.is_none() {
            self.skills_draft = Some(settings_manager::read_settings_skills());
        }
        self.rebuild_skills_panel(cx);
    }
    /// 用当前草稿重算技能行并重新打开面板。
    pub(super) fn rebuild_skills_panel(&mut self, cx: &mut Context<Self>) {
        let rules = self.skills_draft.clone().unwrap_or_default();
        let (all, enabled) = crate::modes::gui::skills_snapshot(&rules);
        let mut entries: Vec<Entry> = all
            .into_iter()
            .map(|(name, description, source, model_off, _path)| {
                let on = enabled.contains(&name);
                let mut detail = description;
                if !source.is_empty() {
                    if !detail.is_empty() {
                        detail.push_str(" · ");
                    }
                    detail.push_str(&source);
                }
                if model_off {
                    detail.push_str(" · [model-off]");
                }
                Entry {
                    label: name.clone(),
                    detail,
                    action: Action::ToggleSkill { name, enable: !on },
                    checked: Some(on),
                }
            })
            .collect();
        if entries.is_empty() {
            entries.push(Entry {
                label: "No skills found".into(),
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            });
        }
        self.replace_list(
            format!("Skills (Ctrl-S saves {} rules)", rules.len()),
            entries,
            DetailSource::Skill,
        );
        cx.notify();
    }
    /// 重建当前列表面板：沿用旧的过滤框与选中行（切换勾选后不跳回第一行）。
    pub(super) fn replace_list(
        &mut self,
        title: impl Into<String>,
        entries: Vec<Entry>,
        detail_source: DetailSource,
    ) {
        match self.overlay.as_mut() {
            Some(Overlay::List(list)) => {
                list.detail = None;
                list.detail_source = detail_source;
                list.refresh(title, entries);
            }
            _ => {
                let mut list = List::new(title, entries);
                list.detail_source = detail_source;
                self.overlay = Some(Overlay::List(list));
            }
        }
    }
    /// Tab：把当前选中项展开成只读详情页（扩展 / 技能），再按 Tab 或 Esc 回列表。
    pub(super) fn open_list_detail(&mut self) {
        let Some(Overlay::List(list)) = &self.overlay else {
            return;
        };
        let Some(name) = list.selected_label().map(|label| label.to_string()) else {
            return;
        };
        let lines = match list.detail_source {
            DetailSource::None => return,
            DetailSource::Extension => crate::core::extensions::extension_detail_lines(&name),
            DetailSource::Skill => crate::modes::gui::skill_detail_lines(
                self.skills_draft.as_deref().unwrap_or_default(),
                &name,
            ),
        };
        if lines.is_empty() {
            return;
        }
        if let Some(Overlay::List(list)) = &mut self.overlay {
            list.detail = Some(Info::new(name, lines));
        }
    }
    /// `/scoped-models`：勾选哪些模型进入 Ctrl-P 循环名单（写 settings 的 enabledModels）。
    pub(super) fn open_scoped_models_panel(&mut self, cx: &mut Context<Self>) {
        self.overlay = Some(Overlay::List(List::new(
            "Scoped models",
            vec![Entry {
                label: "Reading models…".into(),
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            }],
        )));
        cx.spawn(async move |this, cx| {
            let models = cx
                .background_executor()
                .spawn(async {
                    let mut models = Vec::new();
                    for provider in crate::core::auth::list_configured_providers() {
                        for (id, name) in crate::core::model_resolver::list_models(&provider) {
                            models.push((provider.clone(), id, name));
                        }
                    }
                    models
                })
                .await;
            _ = this.update(cx, |app, cx| {
                let enabled = settings_manager::read_enabled_models();
                let entries: Vec<Entry> = models
                    .into_iter()
                    .map(|(provider, id, name)| {
                        let pattern = format!("{provider}/{id}");
                        let on = enabled.is_empty()
                            || enabled.iter().any(|rule| rule == &pattern || rule == &id);
                        Entry {
                            label: format!("[{}] {pattern}", if on { "x" } else { " " }),
                            detail: name,
                            action: Action::ToggleScopedModel(pattern),
                            ..Entry::default()
                        }
                    })
                    .collect();
                app.overlay = Some(Overlay::List(List::new("Scoped models", entries)));
                cx.notify();
            });
        })
        .detach();
    }
    /// 模型选择器。
    pub(super) fn open_model_picker(&mut self, cx: &mut Context<Self>) {
        self.overlay = Some(Overlay::List(List::new(
            "Model",
            vec![Entry {
                label: "Reading models…".into(),
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            }],
        )));
        cx.spawn(async move |this, cx| {
            let models = cx
                .background_executor()
                .spawn(async {
                    let mut models = Vec::new();
                    for provider in crate::core::auth::list_configured_providers() {
                        for (id, name) in crate::core::model_resolver::list_models(&provider) {
                            models.push((provider.clone(), id, name));
                        }
                    }
                    models
                })
                .await;
            _ = this.update(cx, |app, cx| {
                let mut entries: Vec<Entry> = models
                    .into_iter()
                    .map(|(provider, id, name)| Entry {
                        label: format!("{provider} / {id}"),
                        detail: name,
                        action: Action::Command(Box::new(AgentCommand::SwitchModel {
                            provider,
                            model_id: id,
                            persist: true,
                        })),
                        ..Entry::default()
                    })
                    .collect();
                if entries.is_empty() {
                    entries.push(Entry {
                        label: "No configured models".into(),
                        detail: "Run `prux auth` first".into(),
                        action: Action::None,
                        ..Entry::default()
                    });
                }
                app.overlay = Some(Overlay::List(List::new("Model", entries)));
                cx.notify();
            });
        })
        .detach();
    }
    /// 思考级别选择器。
    pub(super) fn open_thinking_picker(&mut self, _cx: &mut Context<Self>) {
        let levels = self
            .snapshot
            .as_ref()
            .map(|s| s.thinking_levels.clone())
            .unwrap_or_default();
        let mut entries: Vec<Entry> = levels
            .into_iter()
            .map(|level| Entry {
                label: level.clone(),
                detail: String::new(),
                action: Action::Command(Box::new(AgentCommand::SetThinking {
                    level,
                    persist: true,
                })),
                ..Entry::default()
            })
            .collect();
        if entries.is_empty() {
            entries.push(Entry {
                label: "This model has no thinking levels".into(),
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            });
        }
        self.overlay = Some(Overlay::List(List::new("Thinking level", entries)));
    }
    /// Shift+Tab：在当前模型支持的 thinking 级别里循环（同 TUI 的 `app.thinking.cycle`）。
    ///
    /// 当前级别不在列表里（或还不知道）时从头算，保证总能切到下一个。
    pub(super) fn cycle_thinking(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let levels = snapshot.thinking_levels.clone();
        if levels.is_empty() {
            self.toast("This model has no thinking levels", ToastKind::Info, cx);
            return;
        }
        let current = snapshot.thinking.clone().unwrap_or_else(|| "off".into());
        let index = levels.iter().position(|level| *level == current);
        let next = levels[cycle_index(index, levels.len(), 1)].clone();
        _ = self.cmd_tx.send(AgentCommand::SetThinking {
            level: next,
            persist: false,
        });
    }
    /// Ctrl+P / Ctrl+Shift+P：在模型 scope 里前后循环（同 TUI 的 `app.model.cycleForward`）。
    ///
    /// scope 来自 `--models` / `settings.enabledModels`（`agent.model_cycle`）；为空时才退回全部已配置模型。
    pub(super) fn cycle_model(&mut self, dir: i32, cx: &mut Context<Self>) {
        let all: Vec<(String, String)> = crate::core::auth::list_configured_providers()
            .iter()
            .flat_map(|provider| {
                crate::core::model_resolver::list_models(provider)
                    .into_iter()
                    .map(|(id, _)| (provider.clone(), id))
            })
            .collect();
        let scoped = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.model_cycle.clone())
            .unwrap_or_default();
        let models = cycle_models(&scoped, &all);
        if models.is_empty() {
            self.toast("No models to cycle", ToastKind::Info, cx);
            return;
        }
        if models.len() == 1 {
            let hint = if scoped.is_empty() {
                "Only one model available"
            } else {
                "Only one model in scope"
            };
            self.toast(hint, ToastKind::Info, cx);
            return;
        }
        let current = self
            .snapshot
            .as_ref()
            .map(|snapshot| (snapshot.provider.clone(), snapshot.model.clone()));
        let index = current
            .as_ref()
            .and_then(|(provider, model)| {
                models
                    .iter()
                    .position(|(p, id)| p == provider && id == model)
            })
            .unwrap_or(0);
        let (provider, model_id) = models[cycle_index(Some(index), models.len(), dir)].clone();
        // scope 条目带了 thinking 时先同步思考级别再切模型（同 TUI）
        if let Some(level) = scoped
            .iter()
            .find(|scoped| scoped.provider == provider && scoped.id == model_id)
            .and_then(|scoped| scoped.thinking.clone())
        {
            _ = self.cmd_tx.send(AgentCommand::SetThinking {
                level,
                persist: false,
            });
        }
        _ = self.cmd_tx.send(AgentCommand::SwitchModel {
            provider,
            model_id,
            persist: false,
        });
    }
    /// 会话选择器。
    pub(super) fn open_session_picker(&mut self, cx: &mut Context<Self>) {
        if self.sessions.is_empty() && !self.sessions_loading {
            self.reload_sessions(cx);
        }
        let entries: Vec<Entry> = self
            .sessions
            .iter()
            .map(|row| Entry {
                label: row.title.clone(),
                detail: format!("{} · {} messages", row.age, row.messages),
                action: Action::LoadSession(row.path.clone()),
                ..Entry::default()
            })
            .collect();
        let mut entries = entries;
        if entries.is_empty() {
            entries.push(Entry {
                label: if self.sessions_loading {
                    "Loading sessions…".into()
                } else {
                    "No sessions yet".to_string()
                },
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            });
        }
        self.overlay = Some(Overlay::List(List::new("Sessions", entries)));
    }
    /// 外观选择器（明/暗）。
    pub(super) fn open_appearance_picker(&mut self, _cx: &mut Context<Self>) {
        let entries = vec![
            Entry {
                label: "dark".into(),
                detail: if self.theme.is_dark { "current" } else { "" }.into(),
                action: Action::SetDark(true),
                ..Entry::default()
            },
            Entry {
                label: "light".into(),
                detail: if self.theme.is_dark { "" } else { "current" }.into(),
                action: Action::SetDark(false),
                ..Entry::default()
            },
        ];
        self.overlay = Some(Overlay::List(List::new("Appearance", entries)));
    }
    /// 信息页。
    pub(super) fn open_info(&mut self, kind: InfoKind, _cx: &mut Context<Self>) {
        let info = match kind {
            InfoKind::Usage => {
                let usage = self.snapshot.as_ref().map(|s| &s.usage);
                let mut lines = Vec::new();
                if let Some(usage) = usage {
                    lines.push(format!("input tokens: {}", usage.input));
                    lines.push(format!("output tokens: {}", usage.output));
                    lines.push(format!("cache read: {}", usage.cache_read));
                    lines.push(format!("cache write: {}", usage.cache_write));
                    lines.push(format!("Context usage: {}", usage.context_tokens));
                    lines.push(format!("Total cost: ${:.4}", usage.cost));
                } else {
                    lines.push("No usage yet".to_string());
                }
                Info::new("Usage", lines)
            }
            // `/session`：当前会话概要（对齐 TUI 的 /session）
            InfoKind::Session => {
                let mut lines = vec![format!("Working directory: {}", self.cwd())];
                if let Some(snapshot) = &self.snapshot {
                    lines.push(format!("Model: {}/{}", snapshot.provider, snapshot.model));
                    if let Some(level) = &snapshot.thinking {
                        lines.push(format!("Thinking level: {level}"));
                    }
                    lines.push(format!("Messages: {}", snapshot.messages));
                    lines.push(format!(
                        "Session file: {}",
                        snapshot.session_path.as_deref().unwrap_or("(in-memory session)")
                    ));
                    lines.push(format!("Active tools: {}", snapshot.tools.join(", ")));
                }
                Info::new("Sessions", lines)
            }
            InfoKind::Help => Info::new(
                "Keybindings",
                vec![
                    "Enter to send · Shift+Enter / Ctrl-J new line".to_string(),
                    "Esc abort turn · Ctrl-C clear input · Ctrl-D quit when empty".to_string(),
                    // 编辑器移动/删除与 TUI 的 tui.editor.* 一一对应
                    "Arrows move · Ctrl-B / Ctrl-F by char · Alt-←/→ or Alt-B/F by word".to_string(),
                    "Home / Ctrl-A line start · End / Ctrl-E line end".to_string(),
                    "Ctrl-W / Alt-Backspace delete word back · Alt-D delete word forward".to_string(),
                    "Ctrl-U delete to line start · Ctrl-Shift-K to line end · Ctrl-- undo".to_string(),
                    "Ctrl-Shift-C copy · Ctrl-V paste".to_string(),
                    "Ctrl-K commands · Ctrl-P / Ctrl-/ model · Shift-Tab / Ctrl-T thinking level".to_string(),
                    "Ctrl-O sessions · Ctrl-Shift-B sidebar · Ctrl-Shift-E right panel".to_string(),
                    "Ctrl-Shift-U usage · Ctrl-I skills · Ctrl-, settings · Ctrl-Shift-A select all".to_string(),
                    "Ctrl-N new session · Ctrl-Z undo · Ctrl-Y redo".to_string(),
                    "Ctrl+= zoom in · Ctrl-Shift+- zoom out · Ctrl+0 reset to 100%".to_string(),
                    "Ctrl-Q quit".to_string(),
                ],
            ),
        };
        self.overlay = Some(Overlay::Info(info));
    }
}
