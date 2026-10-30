//! 右侧面板与列表选择器：设置 / 扩展 / 技能 / 模型 / 会话选择页的构建与提交。

use super::*;

impl GuiApp {
    /// 数值型设置：弹输入框，Space 触发后 Enter 解析写入（temperature 留空 = 清除）。
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
        if let Some(patterns) = self.scoped_draft.take() {
            match settings_manager::write_enabled_models(Some(&patterns)) {
                Ok(()) => {
                    self.overlay = None;
                    self.toast(
                        format!("Model scope saved ({} enabled)", patterns.len()),
                        ToastKind::Success,
                        cx,
                    );
                }
                Err(error) => self.toast(
                    format!("Cannot write settings: {error}"),
                    ToastKind::Error,
                    cx,
                ),
            }
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
    /// 设置面板：`Setting / Value / Notes` 三列表格，Space 改值并落盘到 settings.json。
    pub(super) fn open_settings_panel(&mut self, _cx: &mut Context<Self>) {
        let mut list = List::new("Settings", settings_entries(self.theme.is_dark));
        list.space_activates = true;
        list.columns = vec![
            Column::fixed("Setting", TABLE_SETTING_W),
            Column::fixed("Value", TABLE_VALUE_W),
            Column::flex("Notes"),
        ];
        self.overlay = Some(Overlay::List(list));
    }
    /// 扩展管理面板：`[x] 名称` 勾选态，Space/点击切换草稿，Ctrl-S 才落盘，Enter 看详情。
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
            .map(|(name, enabled)| {
                let modes_cell = modes.get(&name).cloned().unwrap_or_default();
                Entry {
                    label: name.clone(),
                    detail: String::new(),
                    action: Action::ToggleExtension(name),
                    checked: Some(enabled),
                    // 表格第二列：扩展可用的模式
                    cells: vec![modes_cell],
                }
            })
            .collect();
        if entries.is_empty() {
            entries.push(Entry {
                label: "No extensions registered".into(),
                ..Entry::default()
            });
        }
        self.replace_list(
            "Extensions (Ctrl-S saves)",
            entries,
            DetailSource::Extension,
            vec![
                Column::fixed("Extension", TABLE_NAME_W),
                Column::fixed("Modes", TABLE_MODES_W),
            ],
        );
        self.mark_space_activates();
        cx.notify();
    }
    /// `/skills`：按 TUI 的技能管理面板实现——列出全部可发现的技能并显示启用态，
    /// Space 在草稿里切换启停，Ctrl-S 落盘并重载，Enter 看详情，Esc 丢弃草稿。
    pub(super) fn open_skills_panel(&mut self, cx: &mut Context<Self>) {
        if self.skills_draft.is_none() {
            self.skills_draft = Some(settings_manager::read_settings_skills());
        }
        self.rebuild_skills_panel(cx);
    }
    /// 用当前草稿重算技能行并重新打开面板。
    pub(super) fn rebuild_skills_panel(&mut self, cx: &mut Context<Self>) {
        let rules = self.skills_draft.clone().unwrap_or_default();
        let (all, enabled) = skills_snapshot(&rules);
        let mut entries: Vec<Entry> = all
            .into_iter()
            .map(|(name, description, source, model_off, _path)| {
                let on = enabled.contains(&name);
                // model-off 罕见，标在描述前；完整说明在 Tab 详情页里
                let description = if model_off {
                    format!("[model-off] {description}")
                } else {
                    description
                };
                Entry {
                    label: name.clone(),
                    detail: String::new(),
                    action: Action::ToggleSkill { name, enable: !on },
                    checked: Some(on),
                    // 表格第 2、3 列：来源根目录（超宽截断）+ 描述
                    cells: vec![source, description],
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
            vec![
                Column::fixed("Skill", TABLE_NAME_W),
                Column::fixed("Source", TABLE_SOURCE_W),
                Column::flex("Description"),
            ],
        );
        self.mark_space_activates();
        cx.notify();
    }
    /// 当前列表面板改由 Space 触发条目动作（扩展 / 技能面板，详情走 Enter）。
    pub(super) fn mark_space_activates(&mut self) {
        if let Some(Overlay::List(list)) = self.overlay.as_mut() {
            list.space_activates = true;
        }
    }
    /// 重建当前列表面板：沿用旧的过滤框与选中行（切换勾选后不跳回第一行）。
    pub(super) fn replace_list(
        &mut self,
        title: impl Into<String>,
        entries: Vec<Entry>,
        detail_source: DetailSource,
        columns: Vec<Column>,
    ) {
        match self.overlay.as_mut() {
            Some(Overlay::List(list)) => {
                list.detail = None;
                list.detail_source = detail_source;
                list.columns = columns;
                list.refresh(title, entries);
            }
            _ => {
                let mut list = List::new(title, entries);
                list.detail_source = detail_source;
                list.columns = columns;
                self.overlay = Some(Overlay::List(list));
            }
        }
    }
    /// Enter：把当前选中项展开成只读详情页（扩展 / 技能），再按 Enter 或 Esc 回列表。
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
            DetailSource::Skill => {
                skill_detail_lines(self.skills_draft.as_deref().unwrap_or_default(), &name)
            }
        };
        if lines.is_empty() {
            return;
        }
        if let Some(Overlay::List(list)) = &mut self.overlay {
            list.detail = Some(Info::with_rows(name, info_rows(lines)));
        }
    }
    /// `/scoped-models`：勾选哪些模型进入 Ctrl-P 循环名单。
    /// 与扩展/技能面板一样只改草稿，Ctrl-S 才写回 settings 的 enabledModels。
    pub(super) fn open_scoped_models_panel(&mut self, cx: &mut Context<Self>) {
        if self.scoped_draft.is_none() {
            // settings 里没写等于全选：展开成完整名单，取消勾选才有对象可减
            let stored = settings_manager::read_enabled_models();
            self.scoped_draft = Some(if stored.is_empty() {
                scoped_catalog()
                    .into_iter()
                    .map(|(provider, id, _)| format!("{provider}/{id}"))
                    .collect()
            } else {
                stored
            });
        }
        self.rebuild_scoped_models_panel(cx);
    }
    /// 用草稿重算模型名单（勾选态来自草稿，Ctrl-S 才写 settings）。
    pub(super) fn rebuild_scoped_models_panel(&mut self, cx: &mut Context<Self>) {
        let enabled = self.scoped_draft.clone().unwrap_or_default();
        let mut on_count = 0usize;
        let mut entries: Vec<Entry> = scoped_catalog()
            .into_iter()
            .map(|(provider, id, name)| {
                let pattern = format!("{provider}/{id}");
                let on = enabled.iter().any(|rule| rule == &pattern || rule == &id);
                if on {
                    on_count += 1;
                }
                Entry {
                    label: id.clone(),
                    detail: String::new(),
                    action: Action::ToggleScopedModel(pattern),
                    checked: Some(on),
                    // 表格第 2、3 列：provider + 模型显示名
                    cells: vec![provider, name],
                }
            })
            .collect();
        if entries.is_empty() {
            entries.push(Entry {
                label: "No models configured".into(),
                action: Action::None,
                ..Entry::default()
            });
        }
        self.replace_list(
            format!("Scoped models — Ctrl+P cycling (Ctrl-S saves {on_count})"),
            entries,
            DetailSource::None,
            vec![
                Column::fixed("Model", TABLE_NAME_W),
                Column::fixed("Provider", TABLE_PROVIDER_W),
                Column::flex("Name"),
            ],
        );
        cx.notify();
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
                    "Ctrl-U delete to line start · Ctrl-K delete to line end · Ctrl-- undo".to_string(),
                    "Ctrl-Shift-C copy selection · Alt-C copy input · Ctrl-V paste".to_string(),
                    "Ctrl-M commands · Ctrl-L model · Ctrl-P / Ctrl-Shift-P cycle model".to_string(),
                    "Ctrl-T show/hide thinking · Shift-Tab cycle thinking level".to_string(),
                    "Ctrl-O expand tools · Ctrl-Shift-T toggle light/dark · Ctrl-Shift-G appearance".to_string(),
                    "Ctrl-Shift-O sessions · Ctrl-Shift-B sidebar · Ctrl-Shift-E right panel".to_string(),
                    "Ctrl-Shift-U usage · Ctrl-I skills · Ctrl-, settings · Ctrl-Shift-A select all".to_string(),
                    "Ctrl-N new session · Ctrl-Z undo · Ctrl-Y redo · Ctrl-X copy last reply".to_string(),
                    "Lists: ↑/↓ or Ctrl-K/Ctrl-J move · Home/End first/last · PgUp/PgDn · Ctrl-C close".to_string(),
                    "Session list: Ctrl-R rename · Ctrl-D delete · Ctrl-G next session".to_string(),
                    "Ctrl+= zoom in · Ctrl-Shift+- zoom out · Ctrl+0 reset to 100%".to_string(),
                    "Ctrl-Q quit".to_string(),
                ],
            ),
        };
        self.overlay = Some(Overlay::Info(info));
    }
}

/// 扫描全部技能并算出给定过滤规则下的启用集合（`/skills` 面板用）。
///
/// 返回全部技能行与启用集合；两条路径都用
/// `skills::load_skills`，所以「勾选框说开着」和「实际加载到」必然一致。
/// 技能行：`(名称, 描述, 来源, 是否禁模型调用, SKILL.md 路径)`。
type SkillRow = (String, String, String, bool, String);

/// `/skills` 面板需要的数据：全部技能行 + 当前规则下启用的技能名。
type SkillsSnapshot = (Vec<SkillRow>, std::collections::HashSet<String>);

/// 扫描技能并返回快照（`rules` 是未落盘的草稿规则，启用态按它实算）。
fn skills_snapshot(rules: &[String]) -> SkillsSnapshot {
    let home_dir = crate::utils::paths::home_dir();
    let cwd = crate::utils::paths::cwd().to_string_lossy().to_string();
    let agent_dir = settings_manager::agent_dir();
    let bundled = crate::extensions::skills::bundled_skill_names();

    let load = |filter: &[String]| {
        crate::core::skills::load_skills(
            &cwd,
            &agent_dir,
            &home_dir,
            &[],
            &bundled,
            true,
            true,
            filter,
        )
    };

    let all = load(&[]);
    let enabled: std::collections::HashSet<String> =
        load(rules).into_iter().map(|skill| skill.name).collect();
    let rows = all
        .into_iter()
        .map(|skill| {
            (
                skill.name,
                skill.description,
                skill.source,
                skill.disable_model_invocation,
                skill.path.to_string_lossy().to_string(),
            )
        })
        .collect();
    (rows, enabled)
}

/// 设置面板的行：名称进第一列，当前值与说明进后两个单元格
/// （`theme_dark` 决定 `Theme` 行显示的当前值）。
pub(super) fn settings_entries(theme_dark: bool) -> Vec<Entry> {
    let on_off = |on: bool| if on { "on" } else { "off" };
    let config_dir = settings_manager::agent_dir().display().to_string();
    let row = |name: &str, value: String, notes: &str, action: Action| Entry {
        label: name.to_string(),
        cells: vec![value, notes.to_string()],
        action,
        ..Entry::default()
    };

    vec![
        row(
            "Theme",
            if theme_dark { "dark" } else { "light" }.into(),
            "Space to toggle",
            Action::ToggleSetting(SettingKey::GuiTheme),
        ),
        row(
            "Auto-compact",
            on_off(settings_manager::read_settings_auto_compact()).into(),
            "Compact when near the limit",
            Action::ToggleSetting(SettingKey::AutoCompact),
        ),
        row(
            "Auto-retry",
            on_off(settings_manager::read_settings_retry_enabled()).into(),
            "Retry failed requests",
            Action::ToggleSetting(SettingKey::Retry),
        ),
        row(
            "Hide thinking",
            on_off(settings_manager::read_settings_hide_thinking()).into(),
            "Hide model reasoning",
            Action::ToggleSetting(SettingKey::HideThinking),
        ),
        row(
            "Syntax highlighting",
            on_off(settings_manager::read_settings_syntax_highlight()).into(),
            "Highlight code blocks",
            Action::ToggleSetting(SettingKey::SyntaxHighlight),
        ),
        row(
            "Auto-resize images",
            on_off(settings_manager::read_settings_images_auto_resize()).into(),
            "Downscale oversized images",
            Action::ToggleSetting(SettingKey::ImagesAutoResize),
        ),
        row(
            "Block images",
            on_off(settings_manager::read_settings_images_block_images()).into(),
            "Refuse image input",
            Action::ToggleSetting(SettingKey::ImagesBlockImages),
        ),
        row(
            "Expose session env",
            on_off(settings_manager::read_settings_expose_session_environment()).into(),
            "Pass session info to tools as env vars",
            Action::ToggleSetting(SettingKey::ExposeSessionEnvironment),
        ),
        row(
            "Cache miss notices",
            on_off(settings_manager::read_settings_show_cache_miss_notices()).into(),
            "",
            Action::ToggleSetting(SettingKey::ShowCacheMissNotices),
        ),
        row(
            "Steering mode",
            settings_manager::read_settings_steering_mode(),
            "Space to cycle",
            Action::ToggleSetting(SettingKey::SteeringMode),
        ),
        row(
            "Follow-up mode",
            settings_manager::read_settings_follow_up_mode(),
            "Space to cycle",
            Action::ToggleSetting(SettingKey::FollowUpMode),
        ),
        row(
            "Cache warming",
            settings_manager::read_settings_cache_warming(),
            "Space to cycle",
            Action::ToggleSetting(SettingKey::CacheWarming),
        ),
        row(
            "Quiet startup",
            format!("{:?}", settings_manager::read_settings_quiet_startup()),
            "Space to cycle",
            Action::ToggleSetting(SettingKey::QuietStartup),
        ),
        row(
            "Max retries",
            settings_manager::read_settings_retry_max_retries().to_string(),
            "Space to edit",
            Action::ToggleSetting(SettingKey::RetryMaxRetries),
        ),
        row(
            "temperature",
            settings_manager::read_settings_temperature()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "(model default)".into()),
            "Space to edit, empty restores default",
            Action::ToggleSetting(SettingKey::Temperature),
        ),
        row(
            "Compact reserve tokens",
            settings_manager::read_settings_compact_reserve().to_string(),
            "Space to edit",
            Action::ToggleSetting(SettingKey::CompactReserve),
        ),
        row(
            "Compact keep recent tokens",
            settings_manager::read_settings_compact_keep_recent().to_string(),
            "Space to edit",
            Action::ToggleSetting(SettingKey::CompactKeepRecent),
        ),
        row(
            "UI scale",
            format!("{}%", (theme::ui_scale() * 100.0).round()),
            "Space to reset 100%",
            Action::Scale(1.0),
        ),
        row(
            "Default model",
            String::new(),
            "Space to pick",
            Action::Open(ListKind::Model),
        ),
        row(
            "Thinking level",
            String::new(),
            "Space to pick",
            Action::Open(ListKind::Thinking),
        ),
        row("Config directory", String::new(), &config_dir, Action::None),
    ]
}

/// 详情行（`"  key: value"` / 纯描述）→ 表格行：`key: value` 拆成两列，
/// 首个不是键值对的行作描述；空行是分组分隔，丢掉。
pub(super) fn info_rows(lines: Vec<String>) -> Vec<InfoRow> {
    let mut rows = Vec::new();
    for line in lines {
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        match text.split_once(": ") {
            Some((key, value)) => rows.push(InfoRow {
                key: key.to_string(),
                value: value.to_string(),
            }),
            None => rows.push(InfoRow {
                key: "Description".to_string(),
                value: text.to_string(),
            }),
        }
    }
    rows
}

/// 已配置 provider 的模型目录：`(provider, id, 显示名)`（本地 JSON，同步读足够快）。
fn scoped_catalog() -> Vec<(String, String, String)> {
    let mut models = Vec::new();
    for provider in crate::core::auth::list_configured_providers() {
        for (id, name) in crate::core::model_resolver::list_models(&provider) {
            models.push((provider.clone(), id, name));
        }
    }
    models
}

/// `/skills` 详情页（Tab）的行：启用态 + SKILL.md 头信息 + 来源 + 路径。
///
/// 技能不存在时返回空（调用方据此不打开详情页）。
fn skill_detail_lines(rules: &[String], name: &str) -> Vec<String> {
    let (all, enabled) = skills_snapshot(rules);
    let Some(row) = all.iter().find(|row| row.0 == name) else {
        return Vec::new();
    };
    vec![
        format!("  name: {name}"),
        format!("  enabled: {}", enabled.contains(name)),
        format!("  disable-model-invocation: {}", row.3),
        format!("  source: {}", row.2),
        format!("  path: {}", row.4),
        String::new(),
        format!("  description: {}", row.1),
    ]
}
