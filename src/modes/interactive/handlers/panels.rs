//! 通用面板键盘处理与主题面板

use super::{KeyAction, bug_report, extensions::extension_panel_items};
use crate::{
    cli::args::VALID_THINKING_LEVELS,
    core::{
        self, auth,
        keybindings::{self, KeybindingsManager},
        model_refresh, oauth,
        provider::AgentMessage,
        settings_manager,
    },
    modes::interactive::{
        self,
        agent_actor::AgentCommand,
        app::{App, MsgLevel},
        line_input::InputAction,
        oauth_flow,
        panel::{PanelItem, PanelKind},
        settings_manager::agent_dir,
        theme::available_theme_names,
    },
    utils::clipboard::read_clipboard,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// /thinking 级别描述
const THINKING_LEVEL_DESCRIPTIONS: &[(&str, &str)] = &[
    ("off", "No reasoning"),
    ("minimal", "Very brief reasoning (~1k tokens)"),
    ("low", "Light reasoning (~2k tokens)"),
    ("medium", "Moderate reasoning (~8k tokens)"),
    ("high", "Deep reasoning (~16k tokens)"),
    ("xhigh", "Extra-high reasoning (~32k tokens)"),
    ("max", "Maximum reasoning"),
];

impl App {
    /// 打开 /theme 主题选择面板；把当前主题移到列表顶部，便于直接回车确认。
    pub(super) fn open_theme_panel(&mut self) {
        let cwd = self.cwd.clone();
        let names = available_theme_names(&cwd, &agent_dir(), !self.no_themes);
        let mut items: Vec<PanelItem> = names
            .into_iter()
            .map(|n| PanelItem::new(n.clone(), n.clone()))
            .collect();

        // 将当前使用的主题移到列表顶部
        if let Some(pos) = items.iter().position(|it| it.value == self.theme.name) {
            let item = items.remove(pos);
            items.insert(0, item);
        }

        self.panel
            .open(PanelKind::Theme, "/theme".to_string(), items);
    }

    /// /thinking：打开思考级别选择面板（可搜索列表 + 当前级别高亮，Enter 选择）。可用级别缺失时回退全部合法级别。
    pub(super) fn open_thinking_panel(&mut self) {
        let levels = if self.thinking_levels.is_empty() {
            VALID_THINKING_LEVELS
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            self.thinking_levels.clone()
        };

        let items: Vec<PanelItem> = levels
            .iter()
            .map(|l| PanelItem {
                label: l.clone(),
                value: l.clone(),
                desc: THINKING_LEVEL_DESCRIPTIONS
                    .iter()
                    .find(|(k, _)| *k == l)
                    .map(|(_, d)| d.to_string())
                    .unwrap_or_default(),
                name: String::new(),
                ..Default::default()
            })
            .collect();

        // 当前级别默认高亮
        let current = self
            .thinking_level
            .clone()
            .unwrap_or_else(|| "off".to_string());
        let current_idx = items.iter().position(|it| it.value == current).unwrap_or(0);

        self.panel
            .open(PanelKind::Thinking, "/thinking".to_string(), items);
        if let Some(layer) = self.panel.top_mut() {
            layer.selected = current_idx;
        }
    }

    /// 面板键盘处理（模态，键盘独占）。导航/确认/取消键由全局 keybindings 驱动
    /// tui.select.*；Ctrl+J/K/G/D/U 为扩展导航键（保留）。
    pub(super) fn handle_panel_key(&mut self, key: &KeyEvent) -> KeyAction {
        let kb = keybindings::get_global();

        // 取消：Esc / Ctrl+C（tui.select.cancel）
        if kb.matches(key, "tui.select.cancel") {
            return self.handle_panel_cancel();
        }

        // OAuth 登录面板：app.message.copy 复制授权 URL
        // （URL 折行后无法整段选中，只能靠这键或面板里的粘贴行）
        if kb.matches(key, "app.message.copy")
            && self
                .panel
                .top()
                .is_some_and(|l| l.kind == PanelKind::LoginOauth)
        {
            self.copy_oauth_authorization_url();
            return KeyAction::Continue;
        }

        // 空格 / Tab / Ctrl+V 特殊键：返回 true 表示已处理完成（跳过下方导航/输入）
        if self.handle_panel_special_key(key) {
            return KeyAction::Continue;
        }

        // 分页 / 确认 / 导航（tui.select.*；Ctrl+J/K/G/D/U 为扩展导航）
        self.handle_panel_nav_key(key, kb);
        KeyAction::Continue
    }

    /// 面板取消处理（Esc / Ctrl+C）：扩展面板回传 None、OAuth 释放回调、关闭面板
    fn handle_panel_cancel(&mut self) -> KeyAction {
        // 扩展 UI 选择面板：取消 → 按请求 id 回传 None（扩展返回 follow-up 则触发下一轮）
        if let Some(layer) = self.panel.top()
            && let PanelKind::Custom(id) = layer.kind
        {
            self.trigger_extension_ui_followup(id, None);
        }

        // 取消 OAuth 登录时同步释放回调服务器
        if let Some(layer) = self.panel.top()
            && layer.kind == PanelKind::LoginOauth
        {
            oauth_flow::cancel(self);
            self.dirty = true;
            return KeyAction::Continue;
        }

        // 取消会话修复面板：放弃修复，保留文件原样
        if let Some(layer) = self.panel.top()
            && layer.kind == PanelKind::SessionRepair
        {
            self.cancel_session_repair();
        }

        // 取消“未完成 operation”面板：不改动，保持只读容忍状态
        if let Some(layer) = self.panel.top()
            && layer.kind == PanelKind::ZombieOperations
        {
            self.cancel_zombie_operations();
        }

        // 取消 /import 确认面板：放弃导入，保持当前会话
        if let Some(layer) = self.panel.top()
            && layer.kind == PanelKind::ImportConfirm
        {
            self.cancel_import_confirm();
        }

        // 取消 /import 会话 cwd 缺失确认面板：放弃导入
        if let Some(layer) = self.panel.top()
            && layer.kind == PanelKind::ImportCwdConfirm
        {
            self.cancel_import_cwd_confirm();
        }

        // 取消 settings.json 覆写确认面板：保留损坏文件，不写入
        if let Some(layer) = self.panel.top()
            && layer.kind == PanelKind::SettingsOverwrite
        {
            self.cancel_settings_overwrite();
        }

        // 取消 /history 历史删除确认面板：什么都不删
        if let Some(layer) = self.panel.top()
            && layer.kind == PanelKind::HistoryClearConfirm
        {
            self.cancel_history_clear_confirm();
        }

        // 取消「清空 trust.json」确认面板：保留所有信任决策
        if let Some(layer) = self.panel.top()
            && layer.kind == PanelKind::TrustClearConfirm
        {
            self.cancel_trust_clear_confirm();
        }

        // 取消 /bug 流程（任意一步）：清除待处理状态，等同“Bug report cancelled”
        if let Some(layer) = self.panel.top()
            && bug_report::is_bug_report_panel(layer.kind)
        {
            self.cancel_bug_report();
            self.panel.cancel();
            self.dirty = true;
            return KeyAction::Continue;
        }

        // 取消项目信任面板（Esc/Ctrl+C）：等同「仅本次不信任」，不写盘
        if let Some(layer) = self.panel.top()
            && layer.kind == PanelKind::ProjectTrust
        {
            self.resolve_project_trust("distrust_session");
            return KeyAction::Continue;
        }

        // /extension 面板：关闭时丢弃未保存草稿（底栏/主题/工具链全程未被打动，无需回滚）
        if self.panel.top().map(|l| l.kind) == Some(PanelKind::Extension) {
            self.discard_extension_draft();
        }

        // /skills 面板：丢弃未保存草稿（settings.json 与磁盘全程未被打动）
        if self.panel.top().map(|l| l.kind) == Some(PanelKind::Skill) {
            self.discard_skill_draft();
        }

        self.panel.cancel();

        if !self.panel.active {
            self.apply_extension_changes();
        }

        self.dirty = true;
        KeyAction::Continue
    }

    /// 面板特殊字符键处理（空格 / Tab / Ctrl+V 粘贴 / scoped-models 快捷键）；
    /// 返回 true 表示已处理（跳过导航）
    fn handle_panel_special_key(&mut self, key: &KeyEvent) -> bool {
        // /scoped-models 面板
        if self.handle_scoped_models_key(key) {
            return true;
        }

        // /model /thinking 面板：Ctrl+S 应用选中项并保存为默认值（命令下传 persist=true，
        // 由 worker 在切换/设置成功后写 settings；硬编码，与 /scoped-models 的 Ctrl+S 一致，
        // 避免与全局表 app.session.toggleSort 冲突）
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
            && matches!(
                self.panel.top().map(|l| l.kind),
                Some(PanelKind::Model) | Some(PanelKind::Thinking)
            )
        {
            self.confirm_panel_selection_with_default();
            return true;
        }

        // /extension 面板：Ctrl+A 全选（启用）/ Ctrl+X 取消全选（禁用）/ Ctrl+S 保存
        // （硬编码，避免与全局表已占用的 ctrl+a/x/s 冲突告警）
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && self.panel.top().map(|l| l.kind) == Some(PanelKind::Extension)
        {
            match key.code {
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    self.set_extensions_enabled_bulk(true);
                    return true;
                }
                KeyCode::Char('x') | KeyCode::Char('X') => {
                    self.set_extensions_enabled_bulk(false);
                    return true;
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    self.save_extension_states();
                    return true;
                }
                _ => {}
            }
        }

        // /skills 面板：Ctrl+S 应用草稿（写 settings.json + 落盘扩展技能 + 重建 skill 工具）。
        // 硬编码，避免与全局表已占用的 ctrl+s 冲突告警
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && self.panel.top().map(|l| l.kind) == Some(PanelKind::Skill)
            && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
        {
            self.save_skill_states();
            return true;
        }

        match key.code {
            KeyCode::Char(' ') => {
                // /extension 面板：空格切换选中扩展的启用状态（其他面板空格进入过滤框）
                if self.panel.top().map(|l| l.kind) == Some(PanelKind::Extension) {
                    self.toggle_selected_extension();
                    self.dirty = true;
                    return true;
                }
                // /skills 面板：空格切换选中技能的草稿启用状态（不进入过滤框）
                if self.panel.top().map(|l| l.kind) == Some(PanelKind::Skill) {
                    self.toggle_selected_skill();
                    return true;
                }
                // /scoped-models 面板：空格与回车一致，切换选中项的启用状态（不进入过滤框）
                if self.panel.top().map(|l| l.kind) == Some(PanelKind::ScopedModels)
                    && let Some(v) = self.scoped_selected_value()
                {
                    self.scoped_toggle(&v);
                    self.dirty = true;
                    return true;
                }
            }
            KeyCode::Tab => {
                // 模型面板：Tab 切换 all/scoped
                if self.panel.top().map(|l| l.kind) == Some(PanelKind::Model) {
                    let next = !self.model_scope_all;
                    self.open_model_panel_scoped(next);
                    self.dirty = true;
                    return true;
                }

                // /extension 一级面板：Tab 切换扩展模式（降序 All→Dev→Creator→Minimal，写盘持久化）
                if self.panel.top().map(|l| l.kind) == Some(PanelKind::Extension) {
                    let next = core::extensions::current_extension_mode().next();
                    core::extensions::set_extension_mode(next);
                    self.extensions_changed = true;

                    // 模式切换后重建列表：非当前模式条目不再显示（保留面板状态标记）
                    if let Some(layer) = self.panel.top_mut() {
                        layer.items = extension_panel_items();
                        layer.filter.clear();
                        layer.selected = 0;
                    }
                    // 重建的勾选来自注册表，需重新盖回未保存草稿
                    self.refresh_extension_labels();
                    self.dirty = true;
                }
            }
            KeyCode::Char('v') | KeyCode::Char('V')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // 粘贴剪贴板到面板 filter（LoginKey 的 API key 输入）；
                // 无输入框面板（确认框 / Custom 等）不落入不可见 filter
                let has_filter = self.panel.top().is_some_and(|l| l.kind.has_filter_input());
                if has_filter
                    && let Some(text) = read_clipboard()
                    && let Some(layer) = self.panel.top_mut()
                {
                    layer.filter.insert_text(&text);
                    layer.selected = 0;
                    self.dirty = true;
                }
            }
            _ => {}
        }
        false
    }

    /// /scoped-models 面板快捷键（面板键盘独占，硬编码以避免
    /// 与全局表中已占用的 ctrl+a/x/p/s/alt+↑↓ 冲突告警）；返回 true 表示已处理
    fn handle_scoped_models_key(&mut self, key: &KeyEvent) -> bool {
        if self.panel.top().map(|l| l.kind) != Some(PanelKind::ScopedModels) {
            return false;
        }

        let filtered_targets = || {
            self.panel
                .filtered_items()
                .iter()
                .map(|it| it.value.clone())
                .collect::<Vec<String>>()
        };

        // 有搜索词：只作用于过滤结果；否则全部可用模型（折叠回全部启用）
        let bulk_targets = || {
            if self.panel.top().is_some_and(|l| !l.filter.value.is_empty()) {
                filtered_targets()
            } else {
                Vec::new()
            }
        };
        let ctrl_mod = KeyModifiers::CONTROL;

        match (key.code, key.modifiers) {
            (KeyCode::Char('a'), m) if m.contains(ctrl_mod) => {
                self.scoped_enable_all(&bulk_targets());
            }
            (KeyCode::Char('x'), m) if m.contains(ctrl_mod) => {
                self.scoped_clear_all(&bulk_targets());
            }
            (KeyCode::Char('p'), m) if m.contains(ctrl_mod) => {
                if let Some(v) = self.scoped_selected_value() {
                    self.scoped_toggle_provider(&v);
                }
            }
            (KeyCode::Char('s'), m) if m.contains(ctrl_mod) => {
                self.scoped_persist();
            }
            (KeyCode::Up, m) if m.contains(KeyModifiers::ALT) => {
                if let Some(v) = self.scoped_selected_value() {
                    self.scoped_reorder(&v, -1);
                }
            }
            (KeyCode::Down, m) if m.contains(KeyModifiers::ALT) => {
                if let Some(v) = self.scoped_selected_value() {
                    self.scoped_reorder(&v, 1);
                }
            }
            _ => return false,
        }
        true
    }

    /// 面板分页/确认/导航键处理（tui.select.*；Ctrl+J/K/G/D/U 为扩展导航）
    fn handle_panel_nav_key(&mut self, key: &KeyEvent, kb: &'static KeybindingsManager) {
        if kb.matches(key, "tui.select.pageUp") {
            self.panel_move(-10);
        } else if kb.matches(key, "tui.select.pageDown") {
            self.panel_move(10);
        } else if kb.matches(key, "tui.select.confirm") {
            // /extension 一级面板：回车进入二级详情（不走 confirm 执行）
            if self.panel.top().map(|l| l.kind) == Some(PanelKind::Extension) {
                self.open_extension_detail();
                self.dirty = true;
            } else if self.panel.top().map(|l| l.kind) == Some(PanelKind::Skill) {
                // /skills 一级面板：回车进入二级详情（SKILL.md 头信息 + 来源）
                self.open_skill_detail();
                self.dirty = true;
            } else {
                self.confirm_panel_selection();
            }
        } else if kb.matches(key, "tui.select.down")
            || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL))
            || (key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL))
            || (key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            // Ctrl+J/G/D：扩展导航（向下）
            self.panel_move(1);
        } else if kb.matches(key, "tui.select.up")
            || (key.code == KeyCode::Char('k') && key.modifiers.contains(KeyModifiers::CONTROL))
            || (key.code == KeyCode::Char('u') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            // Ctrl+K/U：扩展导航（向上）
            self.panel_move(-1);
        } else {
            // 面板过滤框：统一输入框处理（Shift 层映射 / 编辑键 / 光标移动）；
            // 无输入框面板（确认框 / Custom 等）文本键一律不落入不可见 filter
            let has_filter = self.panel.top().is_some_and(|l| l.kind.has_filter_input());
            if has_filter && let Some(layer) = self.panel.top_mut() {
                match layer.filter.handle_key(key) {
                    InputAction::None => {}
                    InputAction::Edited => {
                        layer.selected = 0;
                        self.dirty = true;
                    }
                    InputAction::Moved => self.dirty = true,
                }
            }
        }
    }

    /// 面板选择移动（基于过滤后的列表长度约束）
    fn panel_move(&mut self, delta: i32) {
        let n = self.panel.filtered_items().len();
        if n > 0
            && let Some(layer) = self.panel.top_mut()
        {
            layer.selected = (layer.selected as i32 + delta).rem_euclid(n as i32) as usize;
            self.dirty = true;
        }
    }

    /// 确认当前选中的面板项（单级直接执行）
    fn confirm_panel_selection(&mut self) {
        self.confirm_panel_selection_inner(false);
    }

    /// /model /thinking 面板 Ctrl+S：应用选中项并按请求写为默认值（persist=true 下传 worker）
    fn confirm_panel_selection_with_default(&mut self) {
        self.confirm_panel_selection_inner(true);
    }

    /// 确认当前选中的面板项。`persist_default` 为 true 时（Ctrl+S）额外请求把选中项
    /// 写为默认模型/思考级别（切换成功后才写盘）；普通 Enter 只改会话状态，不写盘。
    fn confirm_panel_selection_inner(&mut self, persist_default: bool) {
        let Some(layer) = self.panel.top() else {
            return;
        };
        // OAuth 登录面板：filter 行即手动粘贴授权码/URL 的输入框，Enter 提交后
        // 由事件循环 tick（oauth_flow::poll）处理；空输入 Enter：device AwaitInput 步骤，
        // 提交空串（如 copilot 默认 github.com），否则取消登录
        if layer.kind == PanelKind::LoginOauth {
            self.confirm_login_oauth();
            return;
        }

        // LoginKey 面板没有列表项：filter 行即 API key 输入框，必须在
        // filtered_items 检查之前处理（否则空列表会直接 cancel）
        if layer.kind == PanelKind::LoginKey {
            self.confirm_login_key();
            return;
        }

        // /bug 描述面板同样无列表项：filter 行即描述输入框
        if layer.kind == PanelKind::BugReportHint {
            self.confirm_bug_report_hint();
            return;
        }

        let selected = layer.selected;
        let kind = layer.kind;
        let item = self.panel.filtered_items().get(selected).cloned().cloned();
        let Some(item) = item else {
            self.panel.cancel();
            return;
        };

        // LoginKey 已在函数开头处理（无列表项）；这里不可能再到达
        match kind {
            PanelKind::LoginKey => {}
            PanelKind::LoginOauth => {}
            PanelKind::Model => {
                // 无凭据提示项/畸形项已由内部提前返回（跳过末尾 close）
                if self.confirm_model_selection(&item, persist_default) {
                    return;
                }
            }
            // /scoped-models：Enter 切换启用状态（多选，面板保持打开）
            PanelKind::ScopedModels => {
                self.scoped_toggle(&item.value);
                self.dirty = true;
                return;
            }
            PanelKind::Session => {
                self.worker.send(AgentCommand::ResumeSession {
                    path: item.value.clone(),
                });
            }
            PanelKind::Theme => self.confirm_theme_selection(&item),
            PanelKind::Thinking => {
                // persist_default（Ctrl+S）由 worker 在设置成功后写盘默认级别并回执状态栏提示。
                self.worker.send(AgentCommand::SetThinking {
                    level: item.value,
                    persist: persist_default,
                });
            }
            PanelKind::LoginAuthType => {
                // 按认证类型过滤 provider 列表（oauth → 订阅 provider；api_key → 注册表清单）。
                let is_oauth = item.value == "oauth";
                self.login_auth_type = if is_oauth {
                    "oauth".to_string()
                } else {
                    "api_key".to_string()
                };
                self.open_login_provider_panel(is_oauth);
                self.dirty = true;
                return;
            }
            PanelKind::LoginProvider => {
                self.login_provider_id = item.value.clone();
                self.login_provider_name = item.label.clone();

                if self.login_auth_type == "oauth" {
                    // account 登录：需要选方式的 provider（anthropic）先问浏览器 / 复制授权码
                    if oauth::supports_copy_code_login(&item.value) {
                        self.open_login_method_panel();
                    } else {
                        // OAuth 流程（URL + 手动输入 + 回调等待），面板由 oauth_flow 接管
                        oauth_flow::start(self);
                    }
                } else {
                    // `Login to {name}` + 输入框（filter 行承载）。推入下一级（Esc 逐级返回），结尾的 close 必须跳过。
                    self.panel.push(
                        PanelKind::LoginKey,
                        format!("Login to {}", item.label),
                        Vec::new(),
                    );
                }
                self.dirty = true;
                return;
            }
            PanelKind::LogoutProvider => self.confirm_logout_provider(&item),

            // /login：登录方式选择（anthropic）——浏览器登录 / 复制授权码
            PanelKind::LoginMethod => {
                if item.value == "copy_code" {
                    oauth_flow::start_copy_code(self);
                } else {
                    oauth_flow::start(self);
                }
                self.dirty = true;
                return;
            }

            // /extension 两级面板不走 confirm（一级 Enter 进详情、二级无操作）
            PanelKind::Extension | PanelKind::ExtensionDetail => {}
            PanelKind::Skill | PanelKind::SkillDetail => {}

            // 扩展 UI 选择面板：选中项按请求 id 回传（Execute → 触发下一轮；其余停留）
            PanelKind::Custom(id) => {
                self.trigger_extension_ui_followup(id, Some(item.value.clone()))
            }

            // 会话损坏修复确认：Enter 确认（truncate → 截断重开 / cancel → 取消）
            PanelKind::SessionRepair => self.resolve_session_repair(&item.value),

            // “未完成 operation”恢复方式选择：continue / rewrite / cancel
            PanelKind::ZombieOperations => self.resolve_zombie_operations(&item.value),

            // /import 确认：Enter 确认（yes → 执行导入 / no → 取消）
            // 内部可能已打开替换面板（cwd 不一致 / 会话修复确认），需跳过末尾 close
            PanelKind::ImportConfirm => {
                if self.resolve_import_confirm(&item.value) {
                    return;
                }
            }

            // /import 会话 cwd 缺失确认：Enter 确认（yes → 当前 cwd 恢复 / no → 取消）
            PanelKind::ImportCwdConfirm => self.resolve_import_cwd_confirm(&item.value),

            // settings.json 覆写确认：Enter 确认（overwrite → 强制写入 / cancel → 保留原文件）
            PanelKind::SettingsOverwrite => self.resolve_settings_overwrite(&item.value),

            // /history 历史删除确认：Enter 确认（yes → 执行删除 / no → 取消）
            PanelKind::HistoryClearConfirm => self.resolve_history_clear_confirm(&item.value),

            // /bug：transcript / 摘要 / 导出三步都是选项面板（选择后打开下一级，均需提前返回）
            PanelKind::BugReportHint => {}
            PanelKind::BugReportTranscript => {
                self.confirm_bug_report_transcript(&item.value);
                return;
            }
            PanelKind::BugReportSummary => {
                self.confirm_bug_report_summary(&item.value);
                return;
            }
            PanelKind::BugReportDelivery => {
                self.confirm_bug_report_delivery(&item.value);
                return;
            }

            // 启动期项目信任选择：信任时重载项目 context/skills/SYSTEM.md 等资源
            PanelKind::ProjectTrust => {
                if self.resolve_project_trust(&item.value) {
                    self.reload_project_resources();
                }
                return;
            }

            // `/trust` 选择面板：仅保存决策（重启后生效），不改变当前会话信任；
            // 「Remove all saved trust decisions」另弹 Yes/No 确认（内部已开新面板，跳过末尾 close）
            PanelKind::ProjectTrustDialog => {
                if item.value == "clear" {
                    self.open_trust_clear_confirm();
                    return;
                }
                self.save_project_trust_decision(&item.value);
                return;
            }

            // 清空 trust.json 确认：Enter 确认（yes → 清空全部内容 / no → 取消）
            PanelKind::TrustClearConfirm => self.resolve_trust_clear_confirm(&item.value),
        }

        self.panel.close();
        self.dirty = true;
    }

    /// 确认模型项：切换模型（value 空为无凭据提示项，不做任何事）；返回 true 表示已提前返回（跳过关闭）。
    /// `persist_default` 为 true（Ctrl+S）时透传给 worker：切换成功后写 settings 默认模型。
    fn confirm_model_selection(&mut self, item: &PanelItem, persist_default: bool) -> bool {
        // 无凭据时的提示项（value 为空）：不做任何事
        if item.value.is_empty() {
            self.panel.cancel();
            self.dirty = true;
            return true;
        }
        let Some((provider, id)) = item.value.split_once('\0') else {
            self.push_msg(
                "switch failed: malformed model item".to_string(),
                MsgLevel::Error,
            );
            self.panel.cancel();
            self.dirty = true;
            return true;
        };

        self.worker.send(AgentCommand::SwitchModel {
            provider: provider.to_string(),
            model_id: id.to_string(),
            persist: persist_default,
        });

        // scope 非空且切到 scope 外模型 → 自动并入 scope 并同步 enabledModels
        self.maybe_append_to_scope(provider, id);
        false
    }

    /// 确认 OAuth 登录面板：filter 行即授权码/URL 输入框，Enter 提交交由 tick 处理
    fn confirm_login_oauth(&mut self) {
        let input = self
            .panel
            .top()
            .map(|l| l.filter.value.trim().to_string())
            .unwrap_or_default();
        let awaiting = self
            .oauth_login
            .as_ref()
            .is_some_and(|o| o.awaiting_input());

        if input.is_empty() && !awaiting {
            oauth_flow::cancel(self);
            self.dirty = true;
            return;
        }

        if let Some(oauth) = self.oauth_login.as_mut() {
            oauth.manual_input = Some(input);
        }

        // 清除输入缓冲，保持面板等待回调/tick 处理
        if let Some(layer) = self.panel.top_mut() {
            layer.filter.value.clear();
            layer.filter.cursor = 0;
        }

        self.dirty = true;
    }

    /// 确认 LoginKey 面板：filter 行即 API key 输入框，写盘并下发 ApplyKey 命令
    fn confirm_login_key(&mut self) {
        let provider = self.login_provider_id.clone();
        let name = self.login_provider_name.clone();
        let key = self
            .panel
            .top()
            .map(|l| l.filter.value.trim().to_string())
            .unwrap_or_default();

        if key.is_empty() {
            self.panel.cancel();
            self.dirty = true;
            return;
        }

        match auth::write_auth_key(&provider, &key) {
            Ok(_) => {
                self.push_msg(
                    format!(
                        "Saved API key for {}. Credentials saved to {}",
                        name,
                        auth::auth_path().display()
                    ),
                    MsgLevel::Success,
                );

                self.worker.send(AgentCommand::ApplyKey {
                    provider: provider.clone(),
                    key: key.clone(),
                });

                // 登录成功后刷新该 provider 的模型目录
                self.pending_refresh.push_back(provider);
            }
            Err(e) => self.push_msg(
                format!("Failed to save API key for {}: {}", name, e),
                MsgLevel::Error,
            ),
        }

        self.panel.close();
        self.dirty = true;
    }

    /// 确认主题项：加载主题并持久化到 settings.json
    fn confirm_theme_selection(&mut self, item: &PanelItem) {
        let name = item.value.clone();
        self.theme = interactive::theme::Theme::load(&name);
        self.invalidate_render(); // 主题已变：清消息渲染缓存，避免旧主题颜色的块残留
        self.push_msg_dedup(
            format!("switched theme: {}", self.theme.name),
            "switched theme: ",
            MsgLevel::Info,
        );

        // 成功切换后持久化到 settings.json（重启恢复）
        if let Err(e) = settings_manager::write_theme(&name) {
            self.push_msg(format!("failed to persist theme: {}", e), MsgLevel::Error);
        }
    }

    /// 确认登出项：移除存储的 API key 并清除远程目录条目
    fn confirm_logout_provider(&mut self, item: &PanelItem) {
        let value = item.value.clone();
        let name = item.label.clone();
        match auth::remove_auth(&value) {
            Ok(_) => {
                self.push_msg(format!(
                    "Removed stored API key for {}. Environment variables and models.json config are unchanged.",
                    name
                ), MsgLevel::Success);

                // 保持 store 洁净：该 provider 已无凭据，清除其远程目录条目
                _ = model_refresh::remove_stored_entry(&value);
            }
            Err(e) => self.push_msg(format!("Logout failed: {}", e), MsgLevel::Error),
        }
    }

    /// 扩展 UI 面板选择结果回传：core 按请求 id 分发到扩展（[`Extension::on_ui_choice`]）。
    /// 扩展返回 `Some(text)`（follow-up prompt）时推入消息并置 status_start，
    /// 由事件循环 `take_next_action` 触发下一轮；`None` 无动作（停留）。
    fn trigger_extension_ui_followup(&mut self, id: u64, choice: Option<String>) {
        if let Some(text) = core::extensions::submit_ui_choice(id, choice) {
            self.messages.push(AgentMessage::user_text(&text));
            self.status_start = Some(text);
        }
    }
}

/// 构建扩展面板列表项：按当前模式过滤（极简模式仅显示 modes 含 Minimal 的扩展）
#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::modes::interactive::WorkingKind;
    use crate::modes::interactive::agent_actor::WorkerHandle;
    use crate::modes::interactive::app::{App, sys_spans_text};
    use crossterm::event::KeyEvent;
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
    fn bug_flow_walks_hint_transcript_delivery_and_dispatches() {
        // /bug <desc>：描述面板（预填）→ transcript 面板 → 导出面板 → BugReport 命令
        let _ad = crate::test_support::AgentDirGuard::temp();
        let (agent, mut cmd_rx) = test_agent_cmd();
        let mut st = App::new();
        st.worker = agent;

        st.cmd_bug("bug crash on start");
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::BugReportHint);
        assert_eq!(st.panel.top().unwrap().filter.value, "crash on start");
        // 免责说明来自 core 的常量（渲染层用）
        assert!(
            st.bug_report_note().contains("not uploaded anywhere"),
            "{}",
            st.bug_report_note()
        );

        st.confirm_panel_selection();
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::BugReportTranscript);
        // 「附带 transcript」的说明
        assert!(
            st.bug_report_note()
                .contains("transcript contains your messages")
        );

        // Yes, include the transcript（默认选中项）
        st.confirm_panel_selection();
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::BugReportDelivery);
        let note = st.bug_report_note();
        assert!(note.contains("Description: crash on start"), "{note}");
        assert!(note.contains("Transcript: included"), "{note}");
        assert!(note.contains("Summary: none"), "{note}");

        // Export as Zip（默认选中项）
        st.confirm_panel_selection();
        assert!(!st.panel.active, "提交后面板应关闭");
        assert!(st.pending_bug_report.is_none());
        assert!(st.busy, "导出期间应为忙碌态");
        assert_eq!(st.working_kind, WorkingKind::BugReport);
        let cmd = cmd_rx.try_recv().expect("应发出 BugReport 命令");
        match cmd {
            crate::modes::interactive::agent_actor::AgentCommand::BugReport {
                hint,
                include_session,
                include_summary,
            } => {
                assert_eq!(hint, "crash on start");
                assert!(include_session);
                assert!(!include_summary, "附 transcript 时不再生成摘要");
            }
            other => panic!("应发 BugReport: {other:?}"),
        }
    }

    #[test]
    fn bug_flow_without_transcript_asks_for_summary() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let (agent, mut cmd_rx) = test_agent_cmd();
        let mut st = App::new();
        st.worker = agent;

        st.cmd_bug("");
        st.panel.top_mut().unwrap().filter.set_value("typed hint");
        st.confirm_panel_selection();
        // 选 No（index 1）→ 询问是否生成摘要
        st.panel.top_mut().unwrap().selected = 1;
        st.confirm_panel_selection();
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::BugReportSummary);
        assert!(
            st.bug_report_note()
                .contains("sent to your current provider")
        );

        // Yes, generate a summary（默认选中项）→ 导出面板
        st.confirm_panel_selection();
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::BugReportDelivery);
        assert!(st.bug_report_note().contains("Transcript: not included"));
        assert!(
            st.bug_report_note()
                .contains("Summary: written by the current model")
        );

        st.confirm_panel_selection();
        match cmd_rx.try_recv().expect("应发出 BugReport 命令") {
            crate::modes::interactive::agent_actor::AgentCommand::BugReport {
                hint,
                include_session,
                include_summary,
            } => {
                assert_eq!(hint, "typed hint", "输入框里的描述应被采纳");
                assert!(!include_session);
                assert!(include_summary);
            }
            other => panic!("应发 BugReport: {other:?}"),
        }
    }

    #[test]
    fn bug_flow_cancel_clears_pending_and_sends_no_command() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let (agent, mut cmd_rx) = test_agent_cmd();
        let mut st = App::new();
        st.worker = agent;

        // 导出面板选 Cancel（index 1）
        st.cmd_bug("x");
        st.confirm_panel_selection(); // → transcript
        st.confirm_panel_selection(); // → delivery
        st.panel.top_mut().unwrap().selected = 1;
        st.confirm_panel_selection();
        assert!(!st.panel.active);
        assert!(st.pending_bug_report.is_none());
        assert!(!st.busy, "取消不应进入忙碌态");
        assert!(cmd_rx.try_recv().is_err(), "取消不应下发命令");
        assert!(
            st.system_messages
                .iter()
                .any(|(_, m)| sys_spans_text(&m.spans).contains("Bug report cancelled")),
            "应提示已取消"
        );

        // Esc 路径：任意一步取消都清空状态
        st.cmd_bug("y");
        assert!(st.pending_bug_report.is_some());
        st.cancel_bug_report();
        assert!(st.pending_bug_report.is_none());
        assert!(!st.busy);
    }

    #[test]
    fn bug_panel_kinds_are_recognised_and_not_filter_panels_for_choices() {
        use crate::modes::interactive::handlers::bug_report::is_bug_report_panel;
        for kind in [
            PanelKind::BugReportHint,
            PanelKind::BugReportTranscript,
            PanelKind::BugReportSummary,
            PanelKind::BugReportDelivery,
        ] {
            assert!(is_bug_report_panel(kind), "{kind:?} 应属于 /bug 流程");
        }
        assert!(
            PanelKind::BugReportHint.has_filter_input(),
            "描述面板需输入行"
        );
        for kind in [
            PanelKind::BugReportTranscript,
            PanelKind::BugReportSummary,
            PanelKind::BugReportDelivery,
        ] {
            assert!(!kind.has_filter_input(), "{kind:?} 是纯选项面板");
        }
    }

    #[test]
    fn session_confirm_does_not_block_while_agent_locked() {
        // 回归：确认切换会话经 ResumeSession 命令下发（actor），UI 不持锁不阻塞
        let _ad = crate::test_support::AgentDirGuard::temp();
        let (agent, mut cmd_rx) = test_agent_cmd();
        let mut st = App::new();
        st.worker = agent;
        st.panel.open(
            PanelKind::Session,
            "resume".to_string(),
            vec![PanelItem::new(
                "some-session".to_string(),
                "/tmp/prux-session.jsonl".to_string(),
            )],
        );
        st.confirm_panel_selection();
        let cmd = cmd_rx.try_recv().expect("应发出 ResumeSession 命令");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::ResumeSession { .. }
            ),
            "应发 ResumeSession: {cmd:?}"
        );
    }
    #[test]
    fn thinking_panel_confirms_setthinking_command() {
        // 回归：/thinking 无参打开选择面板，Enter 确认经 SetThinking 命令下发（actor），不阻塞
        let _ad = crate::test_support::AgentDirGuard::temp();
        let (agent, mut cmd_rx) = test_agent_cmd();
        let mut st = App::new();
        st.worker = agent;
        st.open_thinking_panel();
        assert!(st.panel.active, "/thinking 应打开面板");
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::Thinking);
        // 默认列出全部合法思考级别（thinking_levels 未就绪时回退）
        let labels: Vec<&str> = st
            .panel
            .filtered_items()
            .iter()
            .map(|it| it.label.as_str())
            .collect();
        assert_eq!(
            labels,
            crate::cli::args::VALID_THINKING_LEVELS,
            "思考级别面板应列出全部合法级别: {labels:?}"
        );
        // 当前级别默认高亮：/thinking 无参应选中 off
        assert_eq!(st.panel.top().unwrap().selected, 0);

        // 选择 high（index 4）并确认 → 下发 SetThinking
        st.panel.top_mut().unwrap().selected = 4;
        st.confirm_panel_selection();
        let cmd = cmd_rx.try_recv().expect("应发出 SetThinking 命令");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SetThinking { ref level, .. }
                    if level == "high"
            ),
            "应发 SetThinking high: {cmd:?}"
        );
        assert!(!st.panel.active, "确认后面板应关闭");
        // Enter（非 Ctrl+S）只改会话级别：命令 persist=false，不写默认值
        assert!(
            !matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SetThinking {
                    persist: true,
                    ..
                }
            ),
            "Enter 不应以 persist=true 下发: {cmd:?}"
        );
    }

    #[test]
    fn thinking_panel_ctrl_s_requests_persist() {
        // Ctrl+S：下发 SetThinking { persist: true }，由 worker 在设置成功后写默认值
        let _ad = crate::test_support::AgentDirGuard::temp();
        let (agent, mut cmd_rx) = test_agent_cmd();
        let mut st = App::new();
        st.worker = agent;
        st.open_thinking_panel();
        st.panel.top_mut().unwrap().selected = 4; // VALID_THINKING_LEVELS[4] = high
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let cmd = cmd_rx.try_recv().expect("应发 SetThinking");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SetThinking { ref level, persist: true }
                    if level == "high"
            ),
            "Ctrl+S 应发 SetThinking high persist=true: {cmd:?}"
        );
        assert!(!st.panel.active, "Ctrl+S 后面板应关闭");
    }

    #[test]
    fn thinking_panel_defaults_highlight_to_current_level() {
        // 当前思考级别为 high 时，/thinking 面板应默认高亮 high
        let mut st = App::new();
        st.thinking_level = Some("high".to_string());
        st.thinking_levels = vec!["off".to_string(), "low".to_string(), "high".to_string()];
        st.open_thinking_panel();
        let layer = st.panel.top().unwrap();
        assert_eq!(layer.kind, PanelKind::Thinking);
        assert_eq!(layer.selected, 2, "当前级别 high 应默认高亮");
    }

    #[test]
    fn login_key_busy_hints_immediate_apply_skipped() {
        // 回归：agent 忙碌时保存 API key，写盘仍成功；但即时应用到运行中 agent
        // 被跳过时必须提示，否则用户以为已生效实为静默错过。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let (agent, mut cmd_rx) = test_agent_cmd();
        crate::core::auth::write_auth_key("deepseek", "sk-old").unwrap();
        let mut st = App::new();
        st.worker = agent;
        st.panel.open(
            PanelKind::LoginKey,
            "Login to deepseek".to_string(),
            Vec::new(),
        );
        st.login_provider_id = "deepseek".to_string();
        st.login_provider_name = "DeepSeek".to_string();
        st.panel.top_mut().unwrap().filter.set_value("sk-new");
        st.confirm_panel_selection();
        assert_eq!(
            crate::core::auth::read_auth_key("deepseek").unwrap(),
            "sk-new",
            "key 应写盘"
        );
        // actor：即时应用由 ApplyKey 命令下发 worker 执行
        let cmd = cmd_rx.try_recv().expect("应发出 ApplyKey 命令");
        assert!(
            matches!(cmd, crate::modes::interactive::agent_actor::AgentCommand::ApplyKey { ref provider, .. } if provider == "deepseek"),
            "应发 ApplyKey: {cmd:?}"
        );
        let text = st
            .system_messages
            .iter()
            .map(|(_, s)| crate::modes::interactive::app::sys_spans_text(&s.spans))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Saved API key"), "保存提示应存在: {text:?}");
        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn model_panel_confirm_switches_and_reports_name() {
        // 回归：confirm 切换模型后不得死锁（旧实现二次 lock agent），
        // 消息对齐 pi 格式 Switched to <name> (<provider>)
        let _ad = crate::test_support::AgentDirGuard::temp();
        let (agent, mut cmd_rx) = test_agent_cmd(); // 先设置 PRUX_AGENT_DIR，再动 auth.json
        crate::core::auth::write_auth_key("deepseek", "sk-test").unwrap();
        let mut st = App::new();
        st.worker = agent;
        st.panel.open(
            PanelKind::Model,
            "model".to_string(),
            vec![PanelItem {
                label: "deepseek-v4-pro".to_string(),
                value: "deepseek\0deepseek-v4-pro".to_string(),
                desc: "[deepseek]".to_string(),
                name: "DeepSeek V4 Pro".to_string(),
                ..Default::default()
            }],
        );
        st.confirm_panel_selection();
        // actor：切模型由 SwitchModel 命令下发 worker；st.current_model 由回执更新（场景测试覆盖）
        let cmd = cmd_rx.try_recv().expect("应发出 SwitchModel 命令");
        assert!(
            matches!(cmd, crate::modes::interactive::agent_actor::AgentCommand::SwitchModel { ref model_id, .. } if model_id == "deepseek-v4-pro"),
            "应发 SwitchModel: {cmd:?}"
        );
        // Enter（非 Ctrl+S）只切会话模型：命令 persist=false，不写默认值
        assert!(
            !matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SwitchModel {
                    persist: true,
                    ..
                }
            ),
            "Enter 不应以 persist=true 下发: {cmd:?}"
        );
        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn model_panel_ctrl_s_requests_persist() {
        // Ctrl+S：下发 SwitchModel { persist: true }，由 worker 在切换成功后写默认值
        let _ad = crate::test_support::AgentDirGuard::temp();
        let (agent, mut cmd_rx) = test_agent_cmd();
        crate::core::auth::write_auth_key("deepseek", "sk-test").unwrap();
        let mut st = App::new();
        st.worker = agent;
        st.panel.open(
            PanelKind::Model,
            "model".to_string(),
            vec![PanelItem {
                label: "deepseek-v4-pro".to_string(),
                value: "deepseek\0deepseek-v4-pro".to_string(),
                desc: "[deepseek]".to_string(),
                name: "DeepSeek V4 Pro".to_string(),
                ..Default::default()
            }],
        );
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let cmd = cmd_rx.try_recv().expect("应发 SwitchModel");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SwitchModel {
                    ref model_id,
                    persist: true,
                    ..
                } if model_id == "deepseek-v4-pro"
            ),
            "Ctrl+S 应发 SwitchModel persist=true: {cmd:?}"
        );
        assert!(!st.panel.active, "Ctrl+S 后面板应关闭");
        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn model_panel_confirm_does_not_block_when_agent_locked() {
        // 回归：确认切模型走命令通道，UI 不持锁不阻塞（actor），
        // SwitchModel 命令被下发即返回。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let (agent, mut cmd_rx) = test_agent_cmd();
        crate::core::auth::write_auth_key("deepseek", "sk-test").unwrap();
        let mut st = App::new();
        st.worker = agent;
        st.panel.open(
            PanelKind::Model,
            "model".to_string(),
            vec![PanelItem {
                label: "deepseek-v4-pro".to_string(),
                value: "deepseek\0deepseek-v4-pro".to_string(),
                desc: "[deepseek]".to_string(),
                name: "DeepSeek V4 Pro".to_string(),
                ..Default::default()
            }],
        );
        st.confirm_panel_selection();
        // 不阻塞：命令在下发通道中
        let cmd = cmd_rx.try_recv().expect("应发出 SwitchModel 命令");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SwitchModel { .. }
            ),
            "应发 SwitchModel: {cmd:?}"
        );
        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn panel_filter_uses_line_input_shift_layer() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let mut st = App::new();
        st.panel.open(
            PanelKind::Model,
            "model".to_string(),
            vec![PanelItem {
                label: "model-a".to_string(),
                value: "m1".to_string(),
                desc: "[p]".to_string(),
                name: String::new(),
                ..Default::default()
            }],
        );
        // shift+; → ':'（kitty 协议上报 Char(';')+SHIFT）
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char(';'), KeyModifiers::SHIFT));
        assert_eq!(
            st.panel.top().unwrap().filter.value,
            ":",
            "面板过滤框 Shift+; 应输入 :"
        );
        // 普通字符输入 + 退格
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(st.panel.top().unwrap().filter.value, ":x");
        st.handle_panel_key(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(st.panel.top().unwrap().filter.value, ":");
        // 编辑键（Ctrl+A 行首）走统一输入框；Ctrl+U 在面板中是导航键（不删文本）
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(st.panel.top().unwrap().filter.value, "x:");
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(
            st.panel.top().unwrap().filter.value,
            "x:",
            "Ctrl+U 在面板中是导航键，不清空输入"
        );
    }

    /// 2.18：OAuth 登录面板里 `app.message.copy` 复制授权 URL（走剪贴板路径），
    /// 不把 URL 当普通文本输入到粘贴框。
    #[test]
    fn login_panel_app_message_copy_copies_the_url() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let mut st = App::new();
        st.oauth_login = Some(crate::core::oauth::openrouter::start_authorize_flow().unwrap());
        st.panel.push(
            PanelKind::LoginOauth,
            "Login to OpenRouter".to_string(),
            vec![],
        );

        st.handle_panel_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));

        // 剪贴板可用性依赖运行环境，因此断言「走到了复制」，而不是「复制成功」
        let copied = st.status.contains("Copied sign-in URL")
            || st
                .system_messages
                .iter()
                .any(|(_, m)| sys_spans_text(&m.spans).contains("copy failed"));
        assert!(copied, "Ctrl+X 应触发复制授权 URL");
        assert!(
            st.panel.top().unwrap().filter.value.is_empty(),
            "复制不应把字符输入到粘贴框"
        );
    }

    #[test]
    fn custom_panel_ignores_text_input_and_confirms_selection() {
        // 回归：扩展 UI 选择面板（plan-mode Select）无输入框——文本键不落入隐藏 filter，
        // Enter 确认的是选中项（而非把输入文本当内容发送），提示文案不误导。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let mut st = App::new();
        st.panel.open(
            PanelKind::Custom(7),
            "Plan mode - what next?".to_string(),
            vec![PanelItem::new(
                "Execute the plan".to_string(),
                "v-exe".to_string(),
            )],
        );

        st.handle_panel_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(
            st.panel.top().unwrap().filter.value,
            "",
            "Custom 面板文本键不应写入隐藏 filter"
        );

        st.handle_panel_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!st.panel.active, "Enter 应关闭面板并确认选中项");
    }

    #[test]
    fn extension_panel_filter_line_input() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let mut st = App::new();
        st.panel.open(
            PanelKind::Extension,
            "/extension".to_string(),
            vec![PanelItem {
                label: "[x] footer(normal)".to_string(),
                value: "footer(normal)".to_string(),
                desc: "ℹ".to_string(),
                name: String::new(),
                ..Default::default()
            }],
        );
        // /extension 过滤框同样走统一输入框：Shift+1 → '!'
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char('1'), KeyModifiers::SHIFT));
        assert_eq!(
            st.panel.top().unwrap().filter.value,
            "!",
            "/extension 过滤框 Shift+1 应输入 !"
        );
        // 空格键在 extension 面板是切换启用（不进入过滤框，保持原行为）
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(st.panel.top().unwrap().filter.value, "!");
    }

    #[test]
    fn panel_filter_move_and_cancel() {
        use crate::modes::interactive::panel::PanelItem;
        let mut a = App::new();
        a.panel.open(
            PanelKind::Model,
            "test".to_string(),
            vec![
                PanelItem {
                    label: "claude-3".into(),
                    value: "c1".into(),
                    desc: "a".into(),
                    name: String::new(),
                    ..Default::default()
                },
                PanelItem {
                    label: "gpt-4".into(),
                    value: "g2".into(),
                    desc: "b".into(),
                    name: String::new(),
                    ..Default::default()
                },
                PanelItem {
                    label: "claude-3-5".into(),
                    value: "c3".into(),
                    desc: "c".into(),
                    name: String::new(),
                    ..Default::default()
                },
            ],
        );
        assert_eq!(a.panel.filtered_items().len(), 3);
        if let Some(l) = a.panel.top_mut() {
            l.filter.set_value("claude");
        }
        assert_eq!(a.panel.filtered_items().len(), 2);
        a.panel_move(1);
        assert_eq!(a.panel.top().unwrap().selected, 1);
        a.panel_move(1);
        assert_eq!(a.panel.top().unwrap().selected, 0, "wrap from end to start");
        a.panel_move(-1);
        assert_eq!(a.panel.top().unwrap().selected, 1, "wrap from start to end");
        a.panel.cancel();
        assert!(!a.panel.active, "top-level cancel deactivates");
    }

    #[test]
    fn import_confirm_keeps_cwd_mismatch_panel_open() {
        // 回归：/import 确认 Yes 后若会话 cwd 与当前 cwd 不一致，应接着弹
        // “Session cwd mismatch”确认框；confirm_panel_selection 不得把新面板立即
        // 关闭（否则后续确认框消失，会话既不弹框也不导入，静默中断）。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let orig = dir.path().join("original");
        std::fs::create_dir_all(&orig).unwrap();
        let sess_dir = dir.path().join("sessions");
        let mut s = crate::core::session_manager::Session::create(
            orig.to_str().unwrap(),
            Some(sess_dir.clone()),
            true,
        )
        .unwrap();
        s.append_message(&crate::core::provider::AgentMessage::user_text("hi"));
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);

        let mut st = App::new();
        let current = dir.path().join("current");
        std::fs::create_dir_all(&current).unwrap();
        st.cwd = current.to_string_lossy().to_string();
        st.stage_import_confirm(path);
        assert!(
            st.panel.active && st.panel.top().map(|l| l.kind) == Some(PanelKind::ImportConfirm),
            "第一步：/import 应打开导入确认框"
        );
        // 默认选中 Yes（index 0），Enter 确认
        st.confirm_panel_selection();
        assert!(
            st.panel.active && st.panel.top().map(|l| l.kind) == Some(PanelKind::ImportCwdConfirm),
            "第二步：cwd 不一致应保留 cwd 确认框（不得被 confirm_panel_selection 关闭）"
        );
        assert!(st.pending_import_cwd.is_some());
        // 再确认 Yes → cwd 确认框关闭，下发 ResumeSession（worker 无接收者，send 静默失败）
        st.confirm_panel_selection();
        assert!(!st.panel.active, "确认后 cwd 框应关闭");
        assert!(st.pending_import_cwd.is_none());
    }
}
