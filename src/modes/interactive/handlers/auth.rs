//! 登录 / 登出 / 模型面板与会话信息

use crate::{
    core::{auth, login_registry, model_config, model_resolver, model_scope, settings_manager},
    modes::interactive::{
        app::{App, MsgLevel},
        panel::{PanelItem, PanelKind},
    },
    utils::glyphs::DEF_DONE,
};

impl App {
    /// `/login` `/logout` 面板的认证类型标签：`API key` / `account`（OAuth 非订阅）/ `subscription`（OAuth 订阅）。
    /// `auth_type` 为凭据类型（`api_key` / `oauth`）。
    fn auth_type_label(auth_type: &str, subscription: bool) -> &'static str {
        match (auth_type, subscription) {
            ("api_key", _) => "API key",
            (_, true) => "subscription",
            _ => "account",
        }
    }

    /// 面板是否展示 `[类型]` 标签：只在同一份列表里出现多种凭据类型时展示
    /// （同一类型（API key 或 OAuth）下的 `subscription`/`account` 差异不算「多种」）。
    /// 列表为空时返回 false，不计算标签。
    fn show_auth_type_labels(types: &[String]) -> bool {
        match types.first() {
            Some(first) => types.iter().any(|t| t != first),
            None => false,
        }
    }

    /// 打开 /login 第一步面板：选择认证方式（订阅账号 OAuth / API key），
    /// 条目值 `oauth` / `api_key` 决定下一步列出哪类 provider。
    pub(super) fn open_login_auth_panel(&mut self) {
        let items = vec![
            PanelItem::new("Sign in with an account".to_string(), "oauth".to_string()),
            PanelItem::new("Sign in with an API key".to_string(), "api_key".to_string()),
        ];
        self.panel.open(
            PanelKind::LoginAuthType,
            "Select authentication method:".to_string(),
            items,
        );
    }

    /// /login 第二步：选择要配置的 provider
    /// oauth=true 列出订阅登录（account）provider；否则列出 API key providers。
    /// desc 显示状态：`• not configured` / `✓ stored` / `✓ env (VAR)`（success 色由渲染层按 ✓ 前缀判定）。
    /// 一份列表里只有一种凭据类型，不附加 `[account]` / `[subscription]` 标签。
    pub(super) fn open_login_provider_panel(&mut self, oauth: bool) {
        let ids: Vec<&'static str> = if oauth {
            login_registry::oauth_provider_ids()
        } else {
            login_registry::api_key_provider_ids()
        };

        // models.json 显式声明的 apiKey 覆盖 env 探测：此时环境变量不再是该 provider 的来源，
        // 面板不得再标 `✓ env (VAR)`（否则与 /model 的可用性判定矛盾）。一次读盘共用。
        let declared = model_config::read_models_config();

        let items: Vec<PanelItem> = ids
            .iter()
            .map(|id| {
                let stored = auth::has_auth(id);
                let env_var = if declared.get(*id).is_some_and(|c| c.api_key.is_some()) {
                    None
                } else {
                    auth::env_key_source(id)
                };

                // 认证来源三态；stored 与 env 并存时都显示（如 /login 存过 key 又设了环境变量）
                let desc = if let Some(var) = env_var {
                    if stored {
                        format!("{DEF_DONE} stored · env ({var})")
                    } else {
                        format!("{DEF_DONE} env ({var})")
                    }
                } else if stored {
                    format!("{DEF_DONE} stored")
                } else if *id == "anthropic" && auth::anthropic_federation().is_some() {
                    // federation 环境变量已配但本版本不交换 token：明确标出，否则用户会当成完全没配
                    "• not configured · federation env (unsupported)".to_string()
                } else {
                    "• not configured".to_string()
                };
                PanelItem {
                    label: login_registry::provider_name(id),
                    value: (*id).to_string(),
                    desc,
                    name: String::new(),
                    ..Default::default()
                }
            })
            .collect();

        self.panel.push(
            PanelKind::LoginProvider,
            if oauth {
                "Select account provider to configure:".to_string()
            } else {
                "Select provider to configure:".to_string()
            },
            items,
        );
    }

    /// /login 第五步（仅 anthropic）：选择 OAuth 登录方式。
    /// 条目值 `browser` / `copy_code` 决定走本地回调还是复制授权码流程。
    pub(super) fn open_login_method_panel(&mut self) {
        let items = vec![
            PanelItem {
                label: "Browser login".to_string(),
                value: "browser".to_string(),
                desc: "opens a local callback page".to_string(),
                name: String::new(),
                ..Default::default()
            },
            PanelItem {
                label: "Copy code login".to_string(),
                value: "copy_code".to_string(),
                desc: "paste the code shown on the page".to_string(),
                name: String::new(),
                ..Default::default()
            },
        ];
        self.panel.push(
            PanelKind::LoginMethod,
            "Select login method:".to_string(),
            items,
        );
    }

    /// /logout：列出已存凭据的 provider
    pub(super) fn open_logout_panel(&mut self) {
        let stored = auth::list_auth_providers();
        if stored.is_empty() {
            self.push_msg(
                "No stored credentials to remove. /logout only removes credentials saved by /login; \
    environment variables and models.json config are unchanged."
                    .to_string(),
                MsgLevel::Info,
            );
            return;
        }

        let mut stored = stored;
        stored.sort();

        // 已存凭据的类型（auth.json 的 type）：决定 `[API key]` / `[account]` / `[subscription]` 标签
        let kinds: Vec<String> = stored
            .iter()
            .map(|id| {
                auth::read_auth_entry(id)
                    .map(|(t, _)| t)
                    .unwrap_or_default()
            })
            .collect();
        let show_type = Self::show_auth_type_labels(&kinds);

        let items: Vec<PanelItem> = stored
            .into_iter()
            .zip(kinds)
            .map(|(id, kind)| {
                let tag = if show_type {
                    format!(
                        " [{}]",
                        Self::auth_type_label(&kind, login_registry::is_subscription(&id))
                    )
                } else {
                    String::new()
                };
                PanelItem {
                    label: format!("{}{}", login_registry::provider_name(&id), tag),
                    value: id,
                    desc: format!("{DEF_DONE} stored"),
                    name: String::new(),
                    ..Default::default()
                }
            })
            .collect();

        self.panel.open(
            PanelKind::LogoutProvider,
            "/logout — select provider".to_string(),
            items,
        );
    }

    /// 打开模型选择面板
    /// 只列已配置凭据（auth.json /login 或 models.json apiKey）的 provider 模型，
    /// 不局限于当前 provider；无任何配置时面板给出 /login 指引
    pub(super) fn open_model_panel(&mut self) {
        self.open_model_panel_scoped(true)
    }

    /// 按 scope 打开模型面板（Tab 切换 all/scoped）
    pub(super) fn open_model_panel_scoped(&mut self, scope_all: bool) {
        self.model_scope_all = scope_all;
        let providers = auth::list_configured_providers();
        if providers.is_empty() {
            self.panel.open(
                PanelKind::Model,
                "/model".to_string(),
                vec![PanelItem {
                    label:
                        "Only showing models from configured providers. Use /login to add providers."
                            .to_string(),
                    value: String::new(),
                    desc: String::new(),
                    name: String::new(),
                    ..Default::default()
                }],
            );
            return;
        }

        let items = self.build_model_panel_items(&providers, scope_all);

        // 打开面板时显示刷新中状态
        self.refresh_status_message = "Refreshing model catalogs…".to_string();
        self.refresh_status_success = false;
        self.panel.open(
            PanelKind::Model,
            "/model — configured providers".to_string(),
            items,
        );

        // 触发后台刷新每个已配置 provider 的模型目录
        for provider in providers {
            self.pending_refresh.push_back(provider);
        }
    }

    /// 构建模型面板列表项：按 scope 过滤 + 当前使用的模型置顶
    fn build_model_panel_items(&self, providers: &[String], scope_all: bool) -> Vec<PanelItem> {
        let mut items: Vec<PanelItem> = Vec::new();
        let scoped: Vec<String> = self.model_cycle.iter().map(|s| s.canonical()).collect();
        for provider in providers {
            for (id, name) in model_resolver::list_models(provider) {
                if !scope_all && !scoped.is_empty() && !scoped.contains(&format!("{provider}/{id}"))
                {
                    continue;
                }

                // input 支持类型按 provider+model 单独查询；
                // desc 展示为 `[provider] · text,image`，右侧附模型支持的 input 类型，便于选模型
                let capability = model_input_suffix(provider, &id);

                items.push(PanelItem {
                    label: id.clone(),
                    // provider 和 model 一起编码进 value（confirm 时拆分）
                    value: format!("{}\0{}", provider, id),
                    desc: format!("[{}]{}", provider, capability),
                    name,
                    ..Default::default()
                });
            }
        }

        // 将当前使用的模型移到列表顶部（按 provider+model 完整匹配，同名模型不误移）
        if let Some(current_key) = self
            .current_provider
            .as_deref()
            .zip(self.current_model.as_deref())
            .map(|(p, m)| format!("{}\0{}", p, m))
            && let Some(pos) = items.iter().position(|it| it.value == current_key)
        {
            let item = items.remove(pos);
            items.insert(0, item);
        }

        items
    }

    /// 打开 /scoped-models 面板。初值优先级：会话 scope（model_cycle）> settings.enabledModels
    /// 解析结果（no-match pattern 保留为不可用条目）> 全部启用。
    pub(super) fn open_scoped_models_panel(&mut self) {
        let enabled_ids = if !self.model_cycle.is_empty() {
            Some(self.model_cycle.iter().map(|s| s.canonical()).collect())
        } else {
            let patterns = settings_manager::read_enabled_models();
            if patterns.is_empty() {
                None
            } else {
                let available: Vec<model_scope::AvailableModel> = scoped_catalog()
                    .into_iter()
                    .map(|(provider, id, name)| model_scope::AvailableModel { provider, id, name })
                    .collect();
                let resolved = model_scope::resolve_model_scope(&patterns, &available);
                let mut ids: Vec<String> = resolved.scoped.iter().map(|s| s.canonical()).collect();
                for d in resolved.diagnostics {
                    if let model_scope::ScopeDiagnostic::NoMatch { pattern } = d
                        && !ids.contains(&pattern)
                    {
                        ids.push(format!("\0{pattern}"));
                    }
                }
                Some(ids)
            }
        };

        self.model_scope_enabled = enabled_ids;
        self.model_scope_dirty = false;
        self.refresh_status_message = "Refreshing model catalogs…".to_string();
        self.refresh_status_success = false;
        self.panel.open(
            PanelKind::ScopedModels,
            "Model Configuration — Ctrl+P cycling scope".to_string(),
            self.build_scoped_panel_items(),
        );

        // 触发后台刷新每个已配置 provider 的模型目录（复用 /model 刷新管线）
        for provider in auth::list_configured_providers() {
            self.pending_refresh.push_back(provider);
        }
    }

    /// 构建面板列表项：enabled 在前（保持顺序，含不可用 pattern 条目），disabled 在后。
    fn build_scoped_panel_items(&self) -> Vec<PanelItem> {
        let all = scoped_catalog();
        let enabled: Vec<String> = match &self.model_scope_enabled {
            None => all.iter().map(|(p, i, _)| format!("{p}/{i}")).collect(),
            Some(ids) => ids.clone(),
        };

        let mut items: Vec<PanelItem> = Vec::new();
        for key in &enabled {
            if let Some((p, i, name)) = all.iter().find(|(p, i, _)| format!("{p}/{i}") == *key) {
                items.push(PanelItem {
                    label: format!("[x] {i}"),
                    value: scoped_value(key),
                    // desc 展示为 `[provider] · text,image`，与 /model 面板一致
                    desc: format!("[{}]{}", p, model_input_suffix(p, i)),
                    name: name.clone(),
                    ..Default::default()
                });
            } else if key.starts_with('\0') {
                let pattern = key.strip_prefix('\0').unwrap_or(key);
                items.push(PanelItem {
                    label: format!("[x] {pattern}"),
                    value: scoped_value(key),
                    desc: "[unavailable]".to_string(),
                    name: String::new(),
                    ..Default::default()
                });
            }
        }

        for (p, i, name) in &all {
            let canonical = format!("{p}/{i}");
            if !enabled.contains(&canonical) {
                items.push(PanelItem {
                    label: format!("[ ] {i}"),
                    value: scoped_value(&canonical),
                    desc: format!("[{}]{}", p, model_input_suffix(p, i)),
                    name: name.clone(),
                    ..Default::default()
                });
            }
        }
        items
    }

    /// 原地刷新条目的勾选标记（`[x]` / `[ ]`）：条目位置与光标都不动。
    ///
    /// 勾选类操作（Enter/Space/Ctrl+A/Ctrl+X/Ctrl+P）只改启用集合，视图顺序保持不变，
    /// 否则刚切换的条目会立刻跳到 enabled 区顶部、后续条目整体上移；
    /// 重排只在显式重排（Alt+↑/↓）与 Ctrl+S 保存（[`Self::scoped_persist`]）时发生。
    fn refresh_scoped_labels(&mut self) {
        if self.panel.top().map(|l| l.kind) != Some(PanelKind::ScopedModels) {
            return;
        }

        let enabled: Vec<String> = match &self.model_scope_enabled {
            None => scoped_all_ids(),
            Some(ids) => ids.clone(),
        };
        let Some(layer) = self.panel.top_mut() else {
            return;
        };

        for it in layer.items.iter_mut() {
            let key = scoped_key(&it.value);
            let mark = if enabled.contains(&key) { 'x' } else { ' ' };
            let shown = key.strip_prefix('\0').unwrap_or_else(|| {
                key.split_once('/')
                    .map(|(_, id)| id)
                    .unwrap_or(key.as_str())
            });
            it.label = format!("[{mark}] {shown}");
        }
    }

    /// 重建面板 items（打开、保存、重排后），尽量保持选中项
    fn rebuild_scoped_items(&mut self) {
        if self.panel.top().map(|l| l.kind) != Some(PanelKind::ScopedModels) {
            return;
        }
        let selected_value = self
            .panel
            .top()
            .and_then(|l| l.items.get(l.selected))
            .map(|i| i.value.clone());
        let items = self.build_scoped_panel_items();
        let Some(layer) = self.panel.top_mut() else {
            return;
        };
        layer.items = items;
        if let Some(v) = selected_value
            && let Some(pos) = layer.items.iter().position(|i| i.value == v)
        {
            layer.selected = pos;
        } else if layer.selected >= layer.items.len() {
            layer.selected = layer.items.len().saturating_sub(1);
        }
    }

    /// 同步会话 scope（onChange）：model_scope_enabled → model_cycle（pattern-only 条目不进循环）。
    /// 相同 provider/id 的已有条目保留 thinking 级别（面板编辑不改动 thinking）。
    fn sync_scope_cycle(&mut self) {
        let prev = self.model_cycle.clone();
        self.model_cycle = match &self.model_scope_enabled {
            None => Vec::new(),
            Some(ids) => ids
                .iter()
                .filter(|k| !k.starts_with('\0'))
                .filter_map(|k| {
                    k.split_once('/').map(|(p, m)| model_scope::ScopedModel {
                        provider: p.to_string(),
                        id: m.to_string(),
                        thinking: prev
                            .iter()
                            .find(|s| s.provider == p && s.id == m)
                            .and_then(|s| s.thinking.clone()),
                    })
                })
                .collect(),
        };
    }

    /// Enter：切换选中条目启用状态
    pub(super) fn scoped_toggle(&mut self, value: &str) {
        let key = scoped_key(value);
        let all_ids = scoped_all_ids();
        self.model_scope_enabled = match self.model_scope_enabled.clone() {
            // 全部启用 → 移除该项（成为显式名单）
            None => Some(all_ids.iter().filter(|id| *id != &key).cloned().collect()),
            Some(ids) => {
                if ids.contains(&key) {
                    Some(ids.iter().filter(|id| *id != &key).cloned().collect())
                } else {
                    let mut list = ids.clone();
                    list.push(key);
                    scoped_normalize(list, &all_ids)
                }
            }
        };
        self.model_scope_dirty = true;
        self.sync_scope_cycle();
        self.refresh_scoped_labels();
        self.dirty = true;
    }

    /// Ctrl+A：全选（有搜索词时只作用于当前过滤结果；全选覆盖全部可用模型时折叠回 None）
    pub(super) fn scoped_enable_all(&mut self, target_values: &[String]) {
        let all_ids = scoped_all_ids();
        let Some(targets) = (if target_values.is_empty() {
            None
        } else {
            Some(
                target_values
                    .iter()
                    .filter(|v| !v.starts_with('\0'))
                    .map(|v| scoped_key(v))
                    .collect::<Vec<String>>(),
            )
        }) else {
            // 无搜索词：全选全部可用模型 → 折叠回 None
            self.model_scope_enabled = None;
            self.model_scope_dirty = true;
            self.sync_scope_cycle();
            self.rebuild_scoped_items();
            self.dirty = true;
            return;
        };
        let ids = self
            .model_scope_enabled
            .clone()
            .unwrap_or_else(|| all_ids.clone());
        let mut list = ids;
        for t in targets {
            if !list.contains(&t) {
                list.push(t);
            }
        }
        self.model_scope_enabled = scoped_normalize(list, &all_ids);
        self.model_scope_dirty = true;
        self.sync_scope_cycle();
        self.rebuild_scoped_items();
        self.dirty = true;
    }

    /// Ctrl+X：清空（有搜索词时只作用于当前过滤结果）
    pub(super) fn scoped_clear_all(&mut self, target_values: &[String]) {
        let all_ids = scoped_all_ids();
        let targets: Vec<String> = target_values.iter().map(|v| scoped_key(v)).collect();
        self.model_scope_enabled = match self.model_scope_enabled.clone() {
            // 全部启用 → 保留 target 之外
            None => {
                if target_values.is_empty() {
                    Some(Vec::new())
                } else {
                    Some(
                        all_ids
                            .iter()
                            .filter(|id| !targets.contains(id))
                            .cloned()
                            .collect(),
                    )
                }
            }
            Some(ids) => Some(
                ids.iter()
                    .filter(|id| !targets.contains(id))
                    .cloned()
                    .collect(),
            ),
        };
        self.model_scope_dirty = true;
        self.sync_scope_cycle();
        self.rebuild_scoped_items();
        self.dirty = true;
    }

    /// Ctrl+P：切换选中条目所属 provider 的全部模型
    pub(super) fn scoped_toggle_provider(&mut self, value: &str) {
        let key = scoped_key(value);
        if key.starts_with('\0') || !key.contains('/') {
            return; // 不可用条目没有 provider
        }
        let provider = key.split_once('/').map(|(p, _)| p.to_string()).unwrap();
        let all = scoped_catalog();
        let provider_ids: Vec<String> = all
            .iter()
            .filter(|(p, _, _)| p == &provider)
            .map(|(p, i, _)| format!("{p}/{i}"))
            .collect();
        if provider_ids.is_empty() {
            return;
        }
        let enabled: Vec<String> = self
            .model_scope_enabled
            .clone()
            .unwrap_or_else(scoped_all_ids);
        let all_enabled = provider_ids.iter().all(|id| enabled.contains(id));
        let list = if all_enabled {
            enabled
                .iter()
                .filter(|id| !provider_ids.contains(id))
                .cloned()
                .collect()
        } else {
            let mut list = enabled;
            for id in &provider_ids {
                if !list.contains(id) {
                    list.push(id.clone());
                }
            }
            list
        };
        self.model_scope_enabled = scoped_normalize(list, &scoped_all_ids());
        self.model_scope_dirty = true;
        self.sync_scope_cycle();
        self.rebuild_scoped_items();
        self.dirty = true;
    }

    /// Alt+↑/↓：在启用名单内重排（全部启用/不可用条目无操作）
    pub(super) fn scoped_reorder(&mut self, value: &str, delta: i32) {
        let key = scoped_key(value);
        let Some(mut ids) = self.model_scope_enabled.clone() else {
            return;
        };
        let pos = ids.iter().position(|id| *id == key);
        let Some(pos) = pos else { return };
        let new_pos = pos as i32 + delta;
        if new_pos < 0 || new_pos as usize >= ids.len() {
            return;
        }
        ids.swap(pos, new_pos as usize);
        self.model_scope_enabled = Some(ids);
        self.model_scope_dirty = true;
        self.sync_scope_cycle();
        self.rebuild_scoped_items();
        self.dirty = true;
    }

    /// Ctrl+S：持久化到 settings.enabledModels（全选/空名单删除键）。
    /// pattern-only 条目（\0 前缀）还原为原始 pattern 字符串。
    pub(super) fn scoped_persist(&mut self) {
        let patterns: Option<Vec<String>> = self.model_scope_enabled.clone().map(|ids| {
            ids.iter()
                .map(|k| k.strip_prefix('\0').unwrap_or(k).to_string())
                .collect()
        });

        self.model_scope_dirty = false;
        self.sync_scope_cycle();

        match settings_manager::write_enabled_models(patterns.as_deref()) {
            Ok(()) => {
                self.push_msg(
                    "Model selection saved to settings".to_string(),
                    MsgLevel::Success,
                );
            }
            Err(e) => {
                self.push_msg(
                    format!("failed to persist enabled models: {e}"),
                    MsgLevel::Error,
                );
            }
        }
        self.rebuild_scoped_items();
        self.dirty = true;
    }

    /// 面板选中条目的 value（panel.rs 过滤后按 selected 取）
    pub(super) fn scoped_selected_value(&self) -> Option<String> {
        self.panel
            .filtered_items()
            .get(self.panel.top()?.selected)
            .map(|it| it.value.clone())
    }

    /// 对齐 pi `_addPersistedDefaultToNonEmptyScope`：scope 非空且切换的模型不在 scope 内时，
    /// 自动追加 canonical 到会话 scope（model_cycle），并在 settings.enabledModels 存在时同步追加。
    pub(super) fn maybe_append_to_scope(&mut self, provider: &str, model_id: &str) {
        if self.model_cycle.is_empty() {
            return;
        }
        let canonical = format!("{provider}/{model_id}");
        if self.model_cycle.iter().any(|m| m.canonical() == canonical) {
            return;
        }
        self.model_cycle
            .push(crate::core::model_scope::ScopedModel {
                provider: provider.to_string(),
                id: model_id.to_string(),
                thinking: None,
            });
        let mut patterns = settings_manager::read_enabled_models();
        if !patterns.is_empty() && !patterns.iter().any(|p| p.eq_ignore_ascii_case(&canonical)) {
            patterns.push(canonical);
            let _ = settings_manager::write_enabled_models(Some(&patterns));
        }
    }
}

/// 模型 input 支持类型后缀（` · text,image`；无 input 元数据时为空）。
/// /model 与 /scoped-models 面板共用，保持两条展示一致。
fn model_input_suffix(provider: &str, id: &str) -> String {
    let input = model_resolver::find_model(provider, id)
        .map(|m| m.input)
        .unwrap_or_default();
    if input.is_empty() {
        String::new()
    } else {
        format!(" · {}", input.join(","))
    }
}

/// 可用模型目录（已配置 provider，元组 provider/id/name）
fn scoped_catalog() -> Vec<(String, String, String)> {
    auth::list_configured_providers()
        .iter()
        .flat_map(|p| {
            model_resolver::list_models(p)
                .into_iter()
                .map(|(id, name)| (p.clone(), id, name))
        })
        .collect()
}

/// scoped_catalog 中所有模型的 `provider/model` id（测试用）
fn scoped_all_ids() -> Vec<String> {
    scoped_catalog()
        .iter()
        .map(|(p, i, _)| format!("{p}/{i}"))
        .collect()
}

/// 面板条目 value → scope key（provider\0id → provider/id；\0pattern 原样）
fn scoped_key(value: &str) -> String {
    if value.starts_with('\0') {
        value.to_string()
    } else {
        value.replace('\0', "/")
    }
}

/// scope key → 面板条目 value
fn scoped_value(key: &str) -> String {
    if key.starts_with('\0') {
        key.to_string()
    } else {
        key.replacen('/', "\0", 1)
    }
}

/// 全选折叠：显式列表覆盖全部可用模型时回到 None（= 全部启用，对齐 pi normalizeEnabled）
fn scoped_normalize(ids: Vec<String>, all_ids: &[String]) -> Option<Vec<String>> {
    if !all_ids.is_empty() && all_ids.iter().all(|id| ids.contains(id)) {
        None
    } else {
        Some(ids)
    }
}

/// 打开会话选择面板（/resume；当前会话不显示在列表中）
#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::modes::interactive::agent_actor::WorkerHandle;
    use crate::modes::interactive::app::App;
    use crate::modes::interactive::handlers::KeyAction;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    /// 测试 agent 目录守卫：每测试独立临时目录（线程本地 override），
    /// 替代全局锁 + 进程级 env 劫持（并行互不干扰、无死锁）。
    fn test_agent_dir() -> crate::test_support::AgentDirGuard {
        crate::test_support::AgentDirGuard::temp()
    }

    /// 按下单个按键（无修饰符），返回面板动作（测试用）。
    fn press(st: &mut App, code: KeyCode) -> KeyAction {
        st.handle_panel_key(&KeyEvent::new(code, KeyModifiers::NONE))
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
    fn login_flow_reaches_provider_panel_and_stays_open() {
        let mut st = App::new();
        st.open_login_auth_panel();
        assert!(st.panel.active);
        // ↓ 选中 API key → Enter：必须进入 provider 面板且保持打开（含 anthropic）
        press(&mut st, KeyCode::Down);
        press(&mut st, KeyCode::Enter);
        assert!(
            st.panel.active,
            "provider 面板应保持打开（修复 close 覆盖 bug）"
        );
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::LoginProvider);
        assert_eq!(st.panel.top().unwrap().items.len(), 34);
    }

    #[test]
    fn login_selects_default_model_when_unconfigured() {
        let _g = test_agent_dir();
        let mut st = App::new();
        let (agent, mut cmd_rx) = test_agent_cmd();
        st.worker = agent;
        // 模拟无配置启动（无 CLI/env/settings 兑底）：model_is_fallback = true

        let settings_path = crate::core::settings_manager::agent_dir().join("settings.json");
        let _ = std::fs::remove_file(&settings_path);
        crate::core::auth::remove_auth("deepseek").ok();

        st.open_login_auth_panel();
        press(&mut st, KeyCode::Down);
        press(&mut st, KeyCode::Enter);
        press(&mut st, KeyCode::Enter); // DeepSeek（第一项）→ key 输入面板
        for c in "sk-demo-456".chars() {
            press(&mut st, KeyCode::Char(c));
        }
        press(&mut st, KeyCode::Enter);

        // 对齐 pi completeProviderAuthentication：当前模型未配置时自动选择默认模型
        assert!(!st.panel.active);
        // actor：默认模型选择由 worker 的 ApplyKey 执行（回执 applied_key）
        let cmd = cmd_rx.try_recv().expect("应发出 ApplyKey 命令");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::ApplyKey { .. }
            ),
            "登录后应即时应用: {cmd:?}"
        );
        let s = crate::core::settings_manager::read_settings();
        let _ = s.default_provider.as_deref();
        let _ = std::fs::remove_file(&settings_path);
    }

    #[test]
    fn model_panel_open_does_not_block_when_agent_locked() {
        // 回归：模型回复中 prompt future 持有 agent 锁时，打开 /model 面板不得拖锁等待——
        // 拖锁会与 sink 的 shared 锁互相 deadlock。try_lock 拿不到锁时按全量列表打开。
        let _g = test_agent_dir();
        crate::core::auth::write_auth_key("deepseek", "sk-test").unwrap();
        let mut st = App::new();
        st.open_model_panel();
        assert!(st.panel.active, "agent 忙碌时打开面板不得卡死");
        let layer = st.panel.top().unwrap();
        assert_eq!(layer.kind, PanelKind::Model);
        assert!(
            layer
                .items
                .iter()
                .any(|it| it.value.starts_with("deepseek\0")),
            "try_lock 失败应按全量列表展示: {:?}",
            layer
                .items
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>()
        );
        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn model_panel_lists_only_configured_providers() {
        let _g = test_agent_dir();
        let mut st = App::new();
        // 显式确保 deepseek 凭据存在（不依赖 test_agent 的 Once：其他测试可能 remove_auth）
        crate::core::auth::write_auth_key("deepseek", "sk-test").unwrap();
        // 面板只列已认证 provider 的模型
        st.open_model_panel();
        let layer = st.panel.top().unwrap();
        assert_eq!(layer.kind, PanelKind::Model);
        assert_eq!(layer.title, "/model — configured providers");
        assert!(layer.items.len() >= 2, "deepseek 基线两个模型");
        for it in &layer.items {
            // value 编码 provider\0model（confirm 时拆分），desc 为 [provider]
            let (p, m) = it.value.split_once('\0').unwrap_or_else(|| {
                panic!("value 应编码 provider: {:?} (label={})", it.value, it.label)
            });
            assert_eq!(p, "deepseek");
            assert_eq!(it.label, m);
            assert!(
                it.desc.starts_with("[deepseek]") && it.desc.contains(" · "),
                "desc 应含 provider 与 input 类型: {:?}",
                it.desc
            );
        }
        // 登录 opencode-go 后面板包含其模型（跨 provider 可选）
        crate::core::auth::write_auth_key("opencode-go", "sk-x").unwrap();
        st.open_model_panel();
        let layer = st.panel.top().unwrap();
        assert!(
            layer
                .items
                .iter()
                .any(|it| it.value.starts_with("opencode-go\0"))
        );
        crate::core::auth::remove_auth("opencode-go").ok();
    }

    #[test]
    fn login_panel_shows_env_configured_providers() {
        let _g = test_agent_dir();
        let _ = std::fs::remove_file(crate::core::auth::auth_path());
        let _ek = crate::test_support::env_key_lock();
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-env") };
        let mut st = App::new();
        st.open_login_provider_panel(false);
        let items = st.panel.top().unwrap().items.clone();
        // deepseek 由环境变量提供 key → desc 显示 ✓ env (VAR)
        let deepseek = items.iter().find(|i| i.value == "deepseek").unwrap();
        assert_eq!(deepseek.desc, "✓ env (DEEPSEEK_API_KEY)");
        // 无 env 的 provider 仍显示 not configured
        let other = items.iter().find(|i| i.value == "openrouter").unwrap();
        assert_eq!(other.desc, "• not configured");
        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
    }

    #[test]
    fn login_panel_hides_env_when_models_json_declares_key() {
        // models.json 显式声明了 apiKey → 环境变量不再是凭据来源，面板不得再标 `✓ env`
        // （否则与 /model 的可用性判定矛盾）
        let _g = test_agent_dir();
        let _ = std::fs::remove_file(crate::core::auth::auth_path());
        let _ek = crate::test_support::env_key_lock();
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-env") };
        std::fs::write(
            crate::core::settings_manager::agent_dir().join("models.json"),
            r#"{"providers":{"deepseek":{"apiKey":"sk-models-json"}}}"#,
        )
        .unwrap();

        let mut st = App::new();
        st.open_login_provider_panel(false);
        let items = st.panel.top().unwrap().items.clone();
        let deepseek = items.iter().find(|i| i.value == "deepseek").unwrap();
        assert_eq!(deepseek.desc, "• not configured");

        let _ =
            std::fs::remove_file(crate::core::settings_manager::agent_dir().join("models.json"));
        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
    }

    #[test]
    fn login_panel_shows_both_stored_and_env() {
        // stored 与 env 并存：两者都显示（用户 /login 存了 key 又设了环境变量）
        let _g = test_agent_dir();
        let _ = std::fs::remove_file(crate::core::auth::auth_path());
        let _ek = crate::test_support::env_key_lock();
        crate::core::auth::write_auth_key("deepseek", "sk-stored").unwrap();
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-env") };
        let mut st = App::new();
        st.open_login_provider_panel(false);
        let items = st.panel.top().unwrap().items.clone();
        let deepseek = items.iter().find(|i| i.value == "deepseek").unwrap();
        assert_eq!(deepseek.desc, "✓ stored · env (DEEPSEEK_API_KEY)");
        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn model_panel_includes_env_key_provider() {
        // env api-key 命中的 provider 出现在 /model（对齐 pi：environment 来源已配置）
        let _g = test_agent_dir();
        let _ek = crate::test_support::env_key_lock();
        let dir = crate::core::settings_manager::agent_dir();
        let _ = std::fs::remove_file(dir.join("auth.json"));
        let _ = std::fs::remove_file(dir.join("models.json"));
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-env") };
        let mut st = App::new();
        st.open_model_panel();
        let layer = st.panel.top().unwrap();
        assert!(
            layer
                .items
                .iter()
                .any(|it| it.value.starts_with("deepseek\0")),
            "env 命中的 deepseek 应出现在 /model: {:?}",
            layer
                .items
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>()
        );
        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
    }

    #[test]
    fn login_panel_renders_env_desc() {
        // /login API key 列表：env 命中的 provider 渲染出 `✓ env (VAR)`
        let _g = test_agent_dir();
        let _ek = crate::test_support::env_key_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _ = std::fs::remove_file(crate::core::auth::auth_path());
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-env") };
        let mut st = App::new();
        st.open_login_provider_panel(false);
        let out = crate::modes::interactive::render::panel::render_panel_text(&st, 100);
        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
        assert!(
            out.iter().any(|l| l.contains("✓ env (DEEPSEEK_API_KEY)")),
            "login 面板应渲染 env 来源: {:?}",
            out
        );
    }

    #[test]
    fn login_account_path_lists_subscription_provider() {
        let _g = test_agent_dir();
        // 外部进程（如常驻 pi）占用回调端口时跳过 OAuth UI 流程
        if crate::core::oauth::CallbackServer::bind(53692).is_err() {
            eprintln!("skip: callback port occupied by external process");
            return;
        }
        let mut st = App::new();
        st.open_login_auth_panel();
        // Enter（默认选中 account）→ 订阅 provider 列表（anthropic），面板保持打开
        press(&mut st, KeyCode::Enter);
        assert!(st.panel.active);
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::LoginProvider);
        // 6 个订阅 provider（anthropic + github-copilot/kimi-coding/openai/openrouter/xai）
        let items: Vec<String> = st
            .panel
            .top()
            .unwrap()
            .items
            .iter()
            .map(|i| i.value.clone())
            .collect();
        assert_eq!(items.len(), 6);
        assert!(items.contains(&"anthropic".to_string()));
        assert!(items.contains(&"github-copilot".to_string()));
        assert!(items.contains(&"kimi-coding".to_string()));
        assert!(items.contains(&"openai".to_string()));
        assert!(items.contains(&"openrouter".to_string()));
        assert!(items.contains(&"xai".to_string()));
        // 选中 anthropic → 先选登录方式（浏览器 / 复制授权码）
        press(&mut st, KeyCode::Enter);
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::LoginMethod);
        assert!(
            st.oauth_login.is_none(),
            "选登录方式前不应开始 OAuth 会话（否则会白占回调端口）"
        );
        // Enter（默认 Browser login）→ OAuth 登录面板（带授权 URL 说明）
        press(&mut st, KeyCode::Enter);
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::LoginOauth);
        assert!(st.oauth_login.is_some());
        assert!(!st.oauth_login.as_ref().unwrap().is_copy_code());
        // Esc 取消：关闭面板并释放回调服务器
        press(&mut st, KeyCode::Esc);
        assert!(!st.panel.active);
        assert!(st.oauth_login.is_none());
    }

    /// 2.3：`Copy code login` 不绑本地端口（能同时开多个），粘贴 `code#state` 后按 state 校验。
    #[test]
    fn anthropic_copy_code_login_skips_callback_server() {
        let _g = test_agent_dir();
        let mut st = App::new();
        st.open_login_auth_panel();
        press(&mut st, KeyCode::Enter); // account → provider 列表
        press(&mut st, KeyCode::Enter); // anthropic → 登录方式
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::LoginMethod);
        press(&mut st, KeyCode::Down);
        press(&mut st, KeyCode::Enter); // Copy code login

        assert_eq!(st.panel.top().unwrap().kind, PanelKind::LoginOauth);
        let oauth = st.oauth_login.as_ref().expect("应启动登录会话");
        assert!(oauth.is_copy_code());
        assert_eq!(
            oauth.redirect_uri_override(),
            Some(crate::core::oauth::anthropic::COPY_CODE_REDIRECT_URI)
        );
        assert!(
            oauth.auth_url().contains(&crate::utils::http::urlencode(
                crate::core::oauth::anthropic::COPY_CODE_REDIRECT_URI
            )),
            "授权 URL 必须带上 provider 侧回调地址: {}",
            oauth.auth_url()
        );
        assert!(!oauth.auth_url().contains("127.0.0.1"));
        crate::modes::interactive::oauth_flow::cancel(&mut st);
        assert!(st.oauth_login.is_none());
    }

    #[test]
    fn login_escape_returns_to_previous_level() {
        let _g = test_agent_dir();
        let mut st = App::new();
        st.open_login_auth_panel();
        // 进入 provider 面板
        press(&mut st, KeyCode::Down);
        press(&mut st, KeyCode::Enter);
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::LoginProvider);
        // 进入 key 输入面板
        press(&mut st, KeyCode::Enter);
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::LoginKey);
        // Esc：key 输入 → 返回 provider 列表（面板保持打开）
        press(&mut st, KeyCode::Esc);
        assert!(st.panel.active);
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::LoginProvider);
        // Ctrl+C：provider → 返回认证方式选择
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(st.panel.active);
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::LoginAuthType);
        // 再 Esc：关闭整个面板
        press(&mut st, KeyCode::Esc);
        assert!(!st.panel.active);
    }

    /// pi #10256/#10343：`/logout` 列表里多种凭据类型并存时标出 `[API key]` / `[subscription]`；
    /// 只有一种类型时不标（与 pi 的 showAuthTypeLabels 同规则），无凭据文案为 `not configured`。
    #[test]
    fn logout_panel_labels_mixed_credential_types() {
        let _g = test_agent_dir();
        let _ = std::fs::remove_file(crate::core::auth::auth_path());
        crate::core::auth::write_auth_key("deepseek", "sk-mix").unwrap();
        let cred = crate::core::oauth::OAuthCredential {
            access: "oat-mix".into(),
            refresh: "r-mix".into(),
            expires: crate::utils::time::now_ms() + 60_000,
            enterprise_url: None,
            available_model_ids: None,
            client_id: None,
            scopes: None,
        };
        crate::core::auth::write_oauth_credential("anthropic", &cred).unwrap();

        let mut st = App::new();
        st.open_logout_panel();
        let items = st.panel.top().unwrap().items.clone();
        let deepseek = items.iter().find(|i| i.value == "deepseek").unwrap();
        let anthropic = items.iter().find(|i| i.value == "anthropic").unwrap();
        assert_eq!(deepseek.label, "DeepSeek [API key]");
        // anthropic 在登录清单里是订阅型 OAuth
        assert_eq!(anthropic.label, "Anthropic [subscription]");

        // 只剩一种凭据类型：不标类型标签
        crate::core::auth::remove_auth("anthropic").unwrap();
        st.open_logout_panel();
        let items = st.panel.top().unwrap().items.clone();
        let deepseek = items.iter().find(|i| i.value == "deepseek").unwrap();
        assert_eq!(deepseek.label, "DeepSeek");
        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn logout_flow_removes_credential() {
        let _g = test_agent_dir();
        let mut st = App::new();
        crate::core::auth::write_auth_key("opencode", "sk-logout").unwrap();
        st.open_logout_panel();
        assert!(st.panel.active);
        // 面板列出所有有凭据的 provider（并行测试可能同时存在 deepseek 条目）
        let values: Vec<String> = st
            .panel
            .top()
            .unwrap()
            .items
            .iter()
            .map(|i| i.value.clone())
            .collect();
        assert!(
            values.contains(&"opencode".to_string()),
            "opencode 条目缺失: {:?}",
            values
        );
        // 选中 opencode 并确认删除
        let idx = values.iter().position(|v| v == "opencode").unwrap();
        st.panel.top_mut().unwrap().selected = idx;
        press(&mut st, KeyCode::Enter);
        assert!(!st.panel.active);
        assert!(!crate::core::auth::has_auth("opencode"));
    }

    #[test]
    fn model_panel_tab_scope_filters_to_scoped() {
        let _g = test_agent_dir(); // 显式提供 deepseek 凭据，避免依赖并行测试留下的 auth.json 时序
        crate::core::auth::write_auth_key("deepseek", "sk-test").unwrap();
        let mut st = App::new();
        // scoped 列表改为 App 缓存（启动时从 agent 复制）：测试直接设缓存
        st.model_cycle = vec![crate::core::model_scope::ScopedModel {
            provider: "deepseek".to_string(),
            id: "deepseek-flash".to_string(),
            thinking: None,
        }];
        // scoped 模式：只显示 model_cycle 中的模型（deepseek 目录含 v4-pro/flash 等）
        st.open_model_panel_scoped(false);
        let items = st.panel.top().unwrap().items.clone();
        assert!(!items.is_empty());
        for it in &items {
            let id = it.value.split_once('\0').map(|(_, m)| m).unwrap_or("");
            assert_eq!(id, "deepseek-flash", "scoped 模式应只含 cycle 模型: {}", id);
        }
        // all 模式：显示全部
        st.open_model_panel_scoped(true);
        let items_all = st.panel.top().unwrap().items.clone();
        assert!(
            items_all.len() > items.len(),
            "all 模式应比 scoped 多: {} vs {}",
            items_all.len(),
            items.len()
        );
        crate::core::auth::remove_auth("deepseek").ok();
    }

    // --------------------------------------------------------------------
    // /scoped-models 面板（对齐 pi ScopedModelsSelectorComponent）
    // --------------------------------------------------------------------

    /// 构造只配了 deepseek 测试 key 的 App（/scoped-models 面板测试用）。
    fn scoped_test_app() -> App {
        // 每测试独立 agent_dir：无需清理残留 settings.json
        let dir = crate::core::settings_manager::agent_dir();
        let _ = std::fs::remove_file(dir.join("settings.json"));
        crate::core::auth::write_auth_key("deepseek", "sk-test").unwrap();
        App::new()
    }

    #[test]
    fn scoped_panel_rows_show_input_capabilities() {
        // 对齐：条目与 /model 面板一致，右侧附 input 支持类型（`[provider] · text,image`）
        let _g = test_agent_dir();
        let mut st = scoped_test_app();
        st.open_scoped_models_panel();
        let items = st.panel.top().unwrap().items.clone();
        assert!(
            items.iter().any(|i| i.desc.contains(" · ")),
            "应有条目附 input 类型: {:?}",
            items.iter().map(|i| &i.desc).collect::<Vec<_>>()
        );
        let vision = items
            .iter()
            .find(|i| i.label.contains("deepseek-flash"))
            .expect("image 条目");
        assert!(
            vision.desc.contains("text,image"),
            "image 模型应显示 text,image: {:?}",
            vision.desc
        );
        let plain = items
            .iter()
            .find(|i| i.label.contains("deepseek-v4-pro"))
            .unwrap();
        assert!(
            plain.desc.contains(" · text"),
            "文本模型应显示 text: {:?}",
            plain.desc
        );
        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn scoped_models_panel_toggle_cycle_and_persist() {
        let _g = test_agent_dir();
        let mut st = scoped_test_app();
        st.open_scoped_models_panel();
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::ScopedModels);
        // 无 scope：全部启用（None）→ 所有条目 [x]
        assert!(st.model_scope_enabled.is_none());
        assert!(!st.model_scope_dirty);
        let items = st.panel.top().unwrap().items.clone();
        assert!(!items.is_empty());
        assert!(items.iter().all(|i| i.label.starts_with("[x]")));

        // Enter toggle 第一项 → 从全部中移除，成为显式名单
        let first_value = items[0].value.clone();
        st.scoped_toggle(&first_value);
        let enabled = st.model_scope_enabled.clone().unwrap();
        assert!(!enabled.contains(&scoped_key(&first_value)));
        assert!(st.model_scope_dirty);
        // 会话 scope 同步（canonical，无 pattern-only）
        assert_eq!(
            st.model_cycle.len(),
            enabled.iter().filter(|k| !k.starts_with('\0')).count()
        );

        // 再 toggle 回 → 全选折叠回 None（对齐 pi normalizeEnabled）
        st.scoped_toggle(&first_value);
        assert!(st.model_scope_enabled.is_none());
        assert!(st.model_cycle.is_empty());

        // Ctrl+S（None = 全部启用）：删除 settings.enabledModels 键
        st.scoped_persist();
        assert!(crate::core::settings_manager::read_enabled_models().is_empty());
        assert!(!st.model_scope_dirty);

        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn scoped_models_space_toggles_like_enter() {
        let _g = test_agent_dir();
        let mut st = scoped_test_app();
        st.open_scoped_models_panel();
        let first = st.panel.top().unwrap().items[0].value.clone();

        // 空格：切换选中项（不进入过滤框）
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let enabled = st.model_scope_enabled.clone().unwrap();
        assert!(!enabled.contains(&scoped_key(&first)), "空格应切换选中项");
        assert_eq!(
            st.panel.top().unwrap().filter.value,
            "",
            "空格不应进入过滤框"
        );

        // 再按空格切回 → 全部启用
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(st.model_scope_enabled.is_none());

        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn scoped_toggle_keeps_item_order_until_saved() {
        // 切勾选只改 checkbox，条目位置与光标都不动；重排发生在 Ctrl+S 保存时
        let _g = test_agent_dir();
        let mut st = scoped_test_app();
        // 额外挂载 meta：目录多 >1 个模型，才能观察重排
        crate::core::auth::write_auth_key("meta", "sk-test").unwrap();
        st.open_scoped_models_panel();
        let before: Vec<String> = st
            .panel
            .top()
            .unwrap()
            .items
            .iter()
            .map(|i| i.value.clone())
            .collect();
        assert!(before.len() > 2, "需要多项目录: {before:?}");

        st.panel.top_mut().unwrap().selected = 0;
        let target = before[0].clone();
        st.scoped_toggle(&target);

        let after: Vec<String> = st
            .panel
            .top()
            .unwrap()
            .items
            .iter()
            .map(|i| i.value.clone())
            .collect();
        assert_eq!(after, before, "切换勾选不应重排条目");
        assert_eq!(st.panel.top().unwrap().selected, 0, "光标应停在原条目");
        assert!(
            st.panel.top().unwrap().items[0].label.starts_with("[ ]"),
            "被切换的条目应显示为未勾选"
        );

        // Ctrl+S：保存后才重排（禁用的条目沉到末尾）
        st.scoped_persist();
        let saved: Vec<String> = st
            .panel
            .top()
            .unwrap()
            .items
            .iter()
            .map(|i| i.value.clone())
            .collect();
        assert_eq!(saved.last(), Some(&target), "保存后被禁用的条目应排到末尾");

        crate::core::auth::remove_auth("deepseek").ok();
        crate::core::auth::remove_auth("meta").ok();
    }

    #[test]
    fn scoped_models_panel_persist_writes_patterns() {
        let _g = test_agent_dir();
        let mut st = scoped_test_app();
        // 额外挂载 meta（5 个模型），使条目数 >2，便于测试部分选中的持久化
        crate::core::auth::write_auth_key("meta", "sk-test").unwrap();
        st.open_scoped_models_panel();
        st.scoped_clear_all(&[]);
        assert_eq!(st.model_scope_enabled, Some(vec![]));
        assert!(st.model_cycle.is_empty());
        let items = st.panel.top().unwrap().items.clone();
        st.scoped_toggle(&items[0].value);
        st.scoped_toggle(&items[1].value);
        st.scoped_persist();

        let pats = crate::core::settings_manager::read_enabled_models();
        assert_eq!(
            pats.len(),
            2,
            "persist 应写两个 canonical pattern: {pats:?}"
        );
        assert!(pats.iter().all(|p| p.contains('/')));
        assert_eq!(st.model_cycle.len(), 2);

        crate::core::auth::remove_auth("deepseek").ok();
        crate::core::auth::remove_auth("meta").ok();
    }

    #[test]
    fn scoped_panel_bulk_ops_and_provider_toggle() {
        let _g = test_agent_dir();
        let mut st = scoped_test_app();
        st.open_scoped_models_panel();

        st.scoped_enable_all(&[]);
        assert!(st.model_scope_enabled.is_none());

        // toggle_provider：唯一 provider（deepseek）全部启用 → 关闭全部
        let items = st.panel.top().unwrap().items.clone();
        assert!(!scoped_all_ids().is_empty());
        st.scoped_toggle_provider(&items[0].value);
        let enabled = st.model_scope_enabled.clone().unwrap();
        assert!(
            enabled.is_empty(),
            "唯一 provider 全关后应为空名单: {enabled:?}"
        );

        // 再 toggle_provider → 重新全部启用 → 折叠回 None
        st.scoped_toggle_provider(&items[0].value);
        assert!(st.model_scope_enabled.is_none());

        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn scoped_reorder_moves_within_enabled_list() {
        let _g = test_agent_dir();
        let mut st = scoped_test_app();
        // 额外挂载 meta（5 个模型），使条目数 >2，便于测试部分选中的重排
        crate::core::auth::write_auth_key("meta", "sk-test").unwrap();
        st.open_scoped_models_panel();
        st.scoped_clear_all(&[]);
        let items = st.panel.top().unwrap().items.clone();
        st.scoped_toggle(&items[0].value);
        let k0 = scoped_key(&items[0].value);
        st.scoped_toggle(&items[1].value);
        let k1 = scoped_key(&items[1].value);
        let order = st.model_scope_enabled.clone().unwrap();
        assert_eq!(order, vec![k0.clone(), k1.clone()]);

        st.scoped_reorder(&items[1].value, -1);
        assert_eq!(
            st.model_scope_enabled.clone().unwrap(),
            vec![k1.clone(), k0.clone()]
        );
        // 顶部再上移 → 无操作
        st.scoped_reorder(&items[1].value, -1);
        assert_eq!(st.model_scope_enabled.clone().unwrap(), vec![k1, k0]);

        crate::core::auth::remove_auth("deepseek").ok();
        crate::core::auth::remove_auth("meta").ok();
    }

    #[test]
    fn scoped_panel_initial_value_from_settings_with_nomatch() {
        let _g = test_agent_dir();
        let mut st = scoped_test_app();
        crate::core::settings_manager::write_enabled_models(Some(&[
            "deepseek-flash".to_string(),
            "gpt-*".to_string(),
        ]))
        .unwrap();
        st.open_scoped_models_panel();
        let enabled = st.model_scope_enabled.clone().unwrap();
        assert!(enabled.contains(&"deepseek/deepseek-flash".to_string()));
        assert!(enabled.contains(&"\0gpt-*".to_string()));
        let items = st.panel.top().unwrap().items.clone();
        assert!(
            items.iter().any(|i| i.desc == "[unavailable]"),
            "no-match 条目应渲染不可用标注"
        );
        // Ctrl+S：\0pattern 条目还原为原始 pattern 写 settings
        st.scoped_persist();
        let pats = crate::core::settings_manager::read_enabled_models();
        assert!(
            pats.contains(&"gpt-*".to_string()),
            "persist 应写原始 pattern（还原 \\0 前缀）: {pats:?}"
        );
        assert!(
            pats.iter().all(|p| !p.starts_with('\0')),
            "settings 不应含 \\0 前缀: {pats:?}"
        );
        crate::core::auth::remove_auth("deepseek").ok();
    }

    /// 按下 Ctrl+<c>（测试用）。
    fn scoped_test_press_ctrl(st: &mut App, c: char) {
        let ev = KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let _ = st.handle_panel_key(&ev);
    }

    #[test]
    fn scoped_models_panel_keys_route_to_actions() {
        let _g = test_agent_dir();
        let mut st = scoped_test_app();
        st.open_scoped_models_panel();
        let first_value = st.panel.top().unwrap().items[0].value.clone();

        // Enter → toggle（面板保持打开）
        press(&mut st, KeyCode::Enter);
        assert!(st.panel.active, "多选面板 Enter 后应保持打开");
        assert!(st.model_scope_dirty);
        assert!(
            !st.model_scope_enabled
                .clone()
                .unwrap()
                .contains(&scoped_key(&first_value))
        );

        // Ctrl+A（无搜索词）→ 全选折叠回 None
        scoped_test_press_ctrl(&mut st, 'a');
        assert!(st.model_scope_enabled.is_none());

        // Ctrl+X → 清空
        scoped_test_press_ctrl(&mut st, 'x');
        assert_eq!(st.model_scope_enabled, Some(vec![]));

        // Ctrl+S → 持久化（全空 → 删键）
        scoped_test_press_ctrl(&mut st, 's');
        assert!(crate::core::settings_manager::read_enabled_models().is_empty());
        assert!(!st.model_scope_dirty);

        // Esc → 关闭
        press(&mut st, KeyCode::Esc);
        assert!(!st.panel.active);

        crate::core::auth::remove_auth("deepseek").ok();
    }

    /// 回归：启用名单只来自 settings（会话循环名单为空）时，Ctrl+S 后 Ctrl+P 必须按刚保存的名单循环
    #[test]
    fn scoped_persist_syncs_cycle_when_scope_came_from_settings() {
        let _g = test_agent_dir();
        let mut st = scoped_test_app();
        let (agent, mut cmd_rx) = test_agent_cmd();
        st.worker = agent;
        // 模拟：settings 已存启用名单，但会话内未生成循环名单
        crate::core::settings_manager::write_enabled_models(Some(&[
            "deepseek/deepseek-flash".to_string(),
            "deepseek/deepseek-v4-pro".to_string(),
        ]))
        .unwrap();
        st.open_scoped_models_panel();
        assert_eq!(
            st.model_scope_enabled,
            Some(vec![
                "deepseek/deepseek-flash".to_string(),
                "deepseek/deepseek-v4-pro".to_string()
            ])
        );
        assert!(st.model_cycle.is_empty(), "会话循环名单初始为空");

        st.scoped_persist();
        assert_eq!(
            st.model_cycle.len(),
            2,
            "Ctrl+S 后循环名单应同步为保存的启用名单: {:?}",
            st.model_cycle
        );
        assert_eq!(st.model_cycle[0].id, "deepseek-flash");

        // Ctrl+P 应按新名单循环，而不是全部可用模型
        st.current_provider = Some("deepseek".to_string());
        st.current_model = Some("deepseek-flash".to_string());
        st.cycle_model_dir(1);
        let cmd = cmd_rx.try_recv().expect("应发出切模命令");
        let label = format!("{cmd:?}");
        assert!(
            label.contains("deepseek-v4-pro"),
            "Ctrl+P 应切到启用名单内的下一个模型: {label}"
        );
        crate::core::auth::remove_auth("deepseek").ok();
    }

    /// 回归：sync 不得丢失已有条目的 thinking 级别
    #[test]
    fn sync_scope_cycle_preserves_thinking() {
        let mut st = App::new();
        st.model_scope_enabled = Some(vec![
            "deepseek/deepseek-v4-pro".to_string(),
            "deepseek/deepseek-flash".to_string(),
        ]);
        st.model_cycle = vec![model_scope::ScopedModel {
            provider: "deepseek".to_string(),
            id: "deepseek-v4-pro".to_string(),
            thinking: Some("high".to_string()),
        }];
        st.sync_scope_cycle();
        assert_eq!(st.model_cycle.len(), 2);
        assert_eq!(st.model_cycle[0].thinking.as_deref(), Some("high"));
        assert_eq!(st.model_cycle[1].thinking, None);
    }

    #[test]
    fn maybe_append_to_scope_syncs_settings() {
        let _g = test_agent_dir();
        let mut st = scoped_test_app();
        st.model_cycle = vec![crate::core::model_scope::ScopedModel {
            provider: "deepseek".to_string(),
            id: "deepseek-flash".to_string(),
            thinking: None,
        }];
        crate::core::settings_manager::write_enabled_models(Some(&[
            "deepseek/deepseek-flash".to_string()
        ]))
        .unwrap();

        st.maybe_append_to_scope("deepseek", "deepseek-v4-pro");
        assert!(st.model_cycle.iter().any(|m| m.id == "deepseek-v4-pro"));
        let pats = crate::core::settings_manager::read_enabled_models();
        assert!(pats.contains(&"deepseek/deepseek-v4-pro".to_string()));

        st.maybe_append_to_scope("deepseek", "deepseek-v4-pro");
        assert_eq!(st.model_cycle.len(), 2);

        st.model_cycle.clear();
        st.maybe_append_to_scope("deepseek", "deepseek-v4-flash-vision-exp");
        assert!(st.model_cycle.is_empty());

        crate::core::auth::remove_auth("deepseek").ok();
    }
}
