//! /settings 设置选择器：状态、值改动动作与键盘处理。
//! (- 行结构：`  label <padding> value`，label 左对齐（最宽 ≤30），value 靠右截断
//! - 选中行：`→ label <padding> value`（accent 高亮）
//! - 选中项下方显示 description（自动换行，2 空格前缀）
//! - 底部 hint：`Type to search · Enter/Space to change · Esc to cancel`
//! - 页码 `  (x/y)` 仅列表可滚动时显示
//! - 搜索：type to search（fuzzy，对齐 pi fuzzyFilter）
//! - Enter/Space 循环切换 values；thinking / theme 只读展示（改由 /thinking、/theme 修改）
//!
//! 渲染在 `render::settings_selector`，键盘处理在本文件 `handle_settings_selector_key`。

use super::line_input::InputBox;
use crate::{
    core::{
        settings_manager::{self, HISTORY_MAX_ENTRIES_TIERS},
        tools,
    },
    utils::fuzzy::fuzzy_filter,
};

/// 列表最大展示行数
pub const MAX_VISIBLE: usize = 10;

/// 设置项值的类型
#[derive(Debug, Clone)]
pub enum SettingKind {
    /// 只读展示
    ReadOnly,
    /// Enter/Space 循环切换
    Cycle(Vec<String>),
    /// Enter 打开子菜单（单选列表选中写回；勾选列表逐项切换）
    Submenu {
        /// 子菜单顶部标题
        title: String,
        /// 子菜单说明文字，展示在标题下方
        description: String,
        /// 交互方式（单选 / 勾选）
        kind: SubmenuKind,
        /// 可选项列表；单选列表按 `value` 写回，勾选列表按 `value` 切换启用状态
        options: Vec<SubmenuOption>,
    },
}

/// 子面板的交互方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubmenuKind {
    /// 单选：Enter 选中并写回
    #[default]
    Select,
    /// 勾选列表：Space/Enter 切换勾选并**立即写盘**，Esc 返回
    Toggle,
}

/// 子菜单选项
#[derive(Debug, Clone)]
pub struct SubmenuOption {
    /// 选中该项时写入设置的原始值
    pub value: String,
    /// 列表中展示给用户的名称
    pub label: String,
    /// 选项描述
    pub description: String,
    /// 勾选态（仅 [`SubmenuKind::Toggle`] 使用；单选列表恒为 `false`）
    pub checked: bool,
}

/// 设置项
#[derive(Debug, Clone)]
pub struct SettingItem {
    /// 稳定的内部标识，用于回写设置与测试定位
    pub id: &'static str,
    /// 列表中的显示名称
    pub label: String,
    /// 选中时展示的说明文字
    pub description: String,
    /// 当前取值的展示字符串（如 "true"、"off"）
    pub current_value: String,
    /// 取值类型与交互方式（只读 / 循环 / 子菜单）
    pub kind: SettingKind,
}

/// 子菜单状态
#[derive(Debug)]
pub struct SubmenuState {
    /// 子菜单顶部标题
    pub title: String,
    /// 子菜单说明文字
    pub description: String,
    /// 交互方式（单选 / 勾选）
    pub kind: SubmenuKind,
    /// 当前子菜单的可选项
    pub options: Vec<SubmenuOption>,
    /// 高亮选项在 options 中的下标
    pub selected: usize,
    /// 打开子菜单的主列表项索引（关闭后恢复选中并同步 current_value）
    pub item_index: usize,
}

/// /settings 选择器状态
#[derive(Debug, Default)]
pub struct SettingsSelector {
    /// 选择器是否处于打开状态（true 时接管键盘输入）
    pub active: bool,
    /// 搜索输入框
    pub filter: InputBox,
    /// 高亮项在 filtered 中的下标（非 items 下标）
    pub selected: usize,
    /// 全部设置项（未经搜索过滤）
    pub items: Vec<SettingItem>,
    /// 过滤后的 items 索引
    pub filtered: Vec<usize>,
    /// 子菜单状态（当前无 Submenu 项，保留备用）
    pub submenu: Option<SubmenuState>,
}

impl SettingsSelector {
    /// 构造一个未激活的空选择器（各列表与搜索框均为默认值）。
    pub fn new() -> Self {
        SettingsSelector::default()
    }

    /// 打开选择器：构建设置项列表
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &mut self,
        show_thinking: bool,
        theme_name: &str,
        autocomplete_max_visible: usize,
        thinking_current: &str,
    ) {
        self.active = true;
        self.filter.clear();
        self.selected = 0;
        self.submenu = None;

        let settings = settings_manager::read_settings();

        let (
            trust_label,
            auto_compaction,
            steer_mode,
            follow_up_mode,
            thinking_current,
            syntax_highlight,
            mermaid,
            latex,
            history_max_entries,
            show_images,
        ) = {
            let trust_label = match settings.default_project_trust.as_deref() {
                Some("always") => "Always trust",
                Some("never") => "Never trust",
                _ => "Ask",
            };

            let auto_compaction = settings_manager::read_settings_auto_compact();
            let steer_mode = settings_manager::read_settings_steering_mode();
            let follow_up_mode = settings_manager::read_settings_follow_up_mode();
            let syntax_highlight = settings_manager::read_settings_syntax_highlight();
            let mermaid = settings_manager::read_settings_mermaid();
            let latex = settings_manager::read_settings_latex();
            let history_max_entries = settings_manager::read_settings_history_max_entries();
            let show_images = settings_manager::read_settings_show_images();

            (
                trust_label,
                auto_compaction,
                steer_mode,
                follow_up_mode,
                thinking_current.to_string(),
                syntax_highlight,
                mermaid,
                latex,
                history_max_entries,
                show_images,
            )
        };

        self.items = Self::build_setting_items(
            show_thinking,
            autocomplete_max_visible,
            theme_name,
            thinking_current,
            trust_label,
            auto_compaction,
            steer_mode,
            follow_up_mode,
            syntax_highlight,
            mermaid,
            latex,
            history_max_entries,
            show_images,
        );
        self.recompute();
    }

    /// 构建设置项列表（Auto-compact / 主题 / thinking 级别等）
    #[allow(clippy::too_many_arguments)]
    fn build_setting_items(
        show_thinking: bool,
        autocomplete_max_visible: usize,
        theme_name: &str,
        thinking_current: String,
        trust_label: &str,
        auto_compaction: bool,
        steer_mode: String,
        follow_up_mode: String,
        syntax_highlight: bool,
        mermaid: bool,
        latex: bool,
        history_max_entries: usize,
        show_images: bool,
    ) -> Vec<SettingItem> {
        let mut items: Vec<SettingItem> = Vec::new();
        items.extend(Self::build_behavior_setting_items(
            autocomplete_max_visible,
            auto_compaction,
            steer_mode,
            follow_up_mode,
            history_max_entries,
        ));
        items.extend(Self::build_display_trust_setting_items(
            show_thinking,
            syntax_highlight,
            mermaid,
            latex,
            trust_label,
            show_images,
        ));
        items.extend(Self::build_readonly_setting_items(
            theme_name,
            thinking_current,
        ));
        items
    }

    /// 通用行为设置项：自动压缩 / 自动补全可见数 / prompt 历史档位 / 注入模式 / 追问模式
    fn build_behavior_setting_items(
        autocomplete_max_visible: usize,
        auto_compaction: bool,
        steer_mode: String,
        follow_up_mode: String,
        history_max_entries: usize,
    ) -> Vec<SettingItem> {
        let mut items: Vec<SettingItem> = Vec::new();
        items.push(SettingItem {
            id: "autocompact",
            label: "Auto-compact".to_string(),
            description: "Automatically compact context when it gets too large".to_string(),
            current_value: if auto_compaction { "true" } else { "false" }.to_string(),
            kind: SettingKind::Cycle(vec!["true".to_string(), "false".to_string()]),
        });
        // 提示缓存保温：每次刷新都真花钱，故只读全局 settings
        items.push(SettingItem {
            id: "cache-warming-mode",
            label: "Cache warming".to_string(),
            description: "Replay the request before its prompt cache expires: 'streaming' while the agent runs, 'idle' also after settling, 'off' to disable. Refreshes cost real money."
                .to_string(),
            current_value: settings_manager::read_settings_cache_warming(),
            kind: SettingKind::Cycle(vec![
                "off".to_string(),
                "streaming".to_string(),
                "idle".to_string(),
            ]),
        });
        items.push(SettingItem {
            id: "autocomplete-max-visible",
            label: "Autocomplete max items".to_string(),
            description: "Max visible items in autocomplete dropdown (3-20)".to_string(),
            current_value: autocomplete_max_visible.to_string(),
            kind: SettingKind::Cycle(vec![
                "3".to_string(),
                "5".to_string(),
                "7".to_string(),
                "10".to_string(),
                "15".to_string(),
                "20".to_string(),
            ]),
        });
        items.push(SettingItem {
            id: "history-max-entries",
            label: "History max entries".to_string(),
            description: "Prompt history kept on disk for the current project (0 = do not persist)"
                .to_string(),
            current_value: history_max_entries.to_string(),
            kind: SettingKind::Cycle(
                HISTORY_MAX_ENTRIES_TIERS
                    .iter()
                    .map(|n| n.to_string())
                    .collect(),
            ),
        });
        // 滚轮行数：数字 1-100 为固定行数，auto 按滚动速度加速（手改出的非档位值也展示出来）
        let wheel_lines = settings_manager::read_settings_fullscreen_wheel_scroll_lines();
        let mut wheel_values = vec![
            "auto".to_string(),
            "1".to_string(),
            "2".to_string(),
            "3".to_string(),
            "5".to_string(),
            "10".to_string(),
        ];
        let wheel_current = wheel_lines.as_str();
        if !wheel_values.contains(&wheel_current) {
            wheel_values.insert(1, wheel_current.clone());
        }
        items.push(SettingItem {
            id: "wheel-scroll-lines",
            label: "Wheel scroll lines".to_string(),
            description: "Lines moved per mouse-wheel event (1-100). 'auto' accelerates fast wheel spins: an isolated notch moves one line, a fast spin up to ten."
                .to_string(),
            current_value: wheel_current,
            kind: SettingKind::Cycle(wheel_values),
        });
        items.push(SettingItem {
            id: "steering-mode",
            label: "Steering mode".to_string(),
            description: "Enter while streaming queues steering messages. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once."
                .to_string(),
            current_value: steer_mode,
            kind: SettingKind::Cycle(vec![
                "one-at-a-time".to_string(),
                "all".to_string(),
            ]),
        });
        items.push(SettingItem {
            id: "follow-up-mode",
            label: "Follow-up mode".to_string(),
            description: "Enter queues follow-up messages until agent stops. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once."
                .to_string(),
            current_value: follow_up_mode,
            kind: SettingKind::Cycle(vec![
                "one-at-a-time".to_string(),
                "all".to_string(),
            ]),
        });
        items.push(SettingItem {
            id: "retry",
            label: "Retry".to_string(),
            description:
                "Automatically retry transient provider errors (rate limits, 5xx, network)."
                    .to_string(),
            current_value: bool_str(settings_manager::read_settings_retry_enabled()),
            kind: SettingKind::Cycle(vec!["true".to_string(), "false".to_string()]),
        });
        items.push(SettingItem {
            id: "retry-max-attempts",
            label: "Retry max attempts".to_string(),
            description: "Max retries per turn before giving up (0 disables retries). Backoff doubles from retry.baseDelayMs."
                .to_string(),
            current_value: settings_manager::read_settings_retry_max_retries().to_string(),
            kind: SettingKind::Cycle(tier_values(
                &settings_manager::RETRY_MAX_RETRIES_TIERS.map(|n| n.to_string()),
                &settings_manager::read_settings_retry_max_retries().to_string(),
            )),
        });
        items.push(SettingItem {
            id: "compact-reserve",
            label: "Compact reserve tokens".to_string(),
            description: "Tokens held back for the model reply when compacting context".to_string(),
            current_value: settings_manager::read_settings_compact_reserve().to_string(),
            kind: SettingKind::Cycle(tier_values(
                &settings_manager::COMPACT_RESERVE_TIERS.map(|n| n.to_string()),
                &settings_manager::read_settings_compact_reserve().to_string(),
            )),
        });
        items.push(SettingItem {
            id: "compact-keep-recent",
            label: "Compact keep recent tokens".to_string(),
            description: "Recent tokens kept verbatim (not summarised) when compacting context"
                .to_string(),
            current_value: settings_manager::read_settings_compact_keep_recent().to_string(),
            kind: SettingKind::Cycle(tier_values(
                &settings_manager::COMPACT_KEEP_RECENT_TIERS.map(|n| n.to_string()),
                &settings_manager::read_settings_compact_keep_recent().to_string(),
            )),
        });
        // temperature：'default' 删除该键，回落模型自身采样参数
        let temperature_current = settings_manager::read_settings_temperature()
            .map(|t| format!("{t}"))
            .unwrap_or_else(|| "default".to_string());
        items.push(SettingItem {
            id: "temperature",
            label: "Temperature".to_string(),
            description: "Sampling temperature override, applied to every model (beats the model's own samplingParams). 'default' removes the override."
                .to_string(),
            current_value: temperature_current.clone(),
            kind: SettingKind::Cycle(tier_values(
                &settings_manager::TEMPERATURE_TIERS.map(|s| s.to_string()),
                &temperature_current,
            )),
        });
        items
    }

    /// 显示与信任设置项：思考隐藏 / 语法高亮 / mermaid / 缓存未命中提示 / latex / 会话环境暴露 / 默认信任
    fn build_display_trust_setting_items(
        show_thinking: bool,
        syntax_highlight: bool,
        mermaid: bool,
        latex: bool,
        trust_label: &str,
        show_images: bool,
    ) -> Vec<SettingItem> {
        let mut items: Vec<SettingItem> = Vec::new();
        items.push(SettingItem {
            id: "hide-thinking",
            label: "Hide thinking".to_string(),
            description: "Hide thinking blocks in assistant responses".to_string(),
            current_value: if show_thinking { "false" } else { "true" }.to_string(),
            kind: SettingKind::Cycle(vec!["true".to_string(), "false".to_string()]),
        });
        items.push(SettingItem {
            id: "syntax-highlight",
            label: "Syntax highlight".to_string(),
            description: "Highlight syntax in code blocks (syntect)".to_string(),
            current_value: if syntax_highlight { "true" } else { "false" }.to_string(),
            kind: SettingKind::Cycle(vec!["true".to_string(), "false".to_string()]),
        });
        items.push(SettingItem {
            id: "mermaid",
            label: "Mermaid".to_string(),
            description: "Render mermaid code blocks as unicode diagrams".to_string(),
            current_value: if mermaid { "true" } else { "false" }.to_string(),
            kind: SettingKind::Cycle(vec!["true".to_string(), "false".to_string()]),
        });
        // 缓存未命中/保温提示：每次提示都关涉真实花费，故只读全局 settings
        items.push(SettingItem {
            id: "cache-miss-notices",
            label: "Cache miss notices".to_string(),
            description:
                "Show notices for cache costs (warming, misses) and provider recovery diagnostics"
                    .to_string(),
            current_value: if settings_manager::read_settings_show_cache_miss_notices() {
                "true"
            } else {
                "false"
            }
            .to_string(),
            kind: SettingKind::Cycle(vec!["true".to_string(), "false".to_string()]),
        });
        items.push(SettingItem {
            id: "quiet-startup",
            label: "Quiet startup".to_string(),
            description: "Startup banner and resource listing. 'false': show both. 'header': keep the banner (version and key hints), hide the listing. 'true': hide both. --verbose overrides it."
                .to_string(),
            current_value: settings_manager::read_settings_quiet_startup()
                .as_str()
                .to_string(),
            kind: SettingKind::Cycle(vec![
                "false".to_string(),
                "header".to_string(),
                "true".to_string(),
            ]),
        });
        items.push(SettingItem {
            id: "latex",
            label: "LaTeX".to_string(),
            description: "Render $...$ / $$...$$ math as unicode formulas".to_string(),
            current_value: if latex { "true" } else { "false" }.to_string(),
            kind: SettingKind::Cycle(vec!["true".to_string(), "false".to_string()]),
        });
        items.push(SettingItem {
            id: "expose-session-env",
            label: "Expose session env".to_string(),
            description: "Inject PRUX_SESSION_ID/PRUX_SESSION_FILE/PRUX_PROVIDER/PRUX_MODEL/PRUX_REASONING_LEVEL into shell tool environment (exposeSessionEnvironment)".to_string(),
            current_value: if settings_manager::read_settings_expose_session_environment() {
                "true"
            } else {
                "false"
            }.to_string(),
            kind: SettingKind::Cycle(vec!["true".to_string(), "false".to_string()]),
        });
        items.push(SettingItem {
            id: "images-auto-resize",
            label: "Images auto resize".to_string(),
            description: "Downscale images before sending them to the model (@file attachments, read tool, tool results). Turn off to send the original bytes."
                .to_string(),
            current_value: bool_str(settings_manager::read_settings_images_auto_resize()),
            kind: SettingKind::Cycle(vec!["true".to_string(), "false".to_string()]),
        });
        items.push(SettingItem {
            id: "images-block",
            label: "Images block".to_string(),
            description: "Refuse to send any image to the model: @image references, clipboard pastes, the read tool and tool-result images are all dropped."
                .to_string(),
            current_value: bool_str(settings_manager::read_settings_images_block_images()),
            kind: SettingKind::Cycle(vec!["true".to_string(), "false".to_string()]),
        });
        items.push(Self::build_builtin_tools_item());
        items.push(SettingItem {
            id: "default-project-trust",
            label: "Default project trust".to_string(),
            description:
                "Fallback behavior when no extension or saved trust decision decides project trust"
                    .to_string(),
            current_value: trust_label.to_string(),
            kind: SettingKind::Cycle(vec![
                "Ask".to_string(),
                "Always trust".to_string(),
                "Never trust".to_string(),
            ]),
        });
        items.push(SettingItem {
            id: "show-images",
            label: "Show images".to_string(),
            description: "Render images inline (kitty / iTerm2 / sixel / halfblocks) in messages and tool results. Terminal detection runs at startup, so turning it on mid-session falls back to environment detection."
                .to_string(),
            current_value: if show_images { "true" } else { "false" }.to_string(),
            kind: SettingKind::Cycle(vec!["true".to_string(), "false".to_string()]),
        });
        items
    }

    /// 内置工具子面板：勾选列表，Space/Enter 切换某个内置工具的启用状态。
    ///
    /// 只处理内置工具（`all_tool_names`），写入用 `+name` / `-name` 增量语法（见
    /// [`settings_manager::toggle_settings_default_tool`]），不会覆盖手写的纯工具名列表。
    /// 变更在 `/reload` 或重启后生效（会话已选定的工具集不因写盘而变）。
    fn build_builtin_tools_item() -> SettingItem {
        let enabled = settings_manager::read_settings_default_tools().unwrap_or_else(|| {
            settings_manager::DEFAULT_TOOL_NAMES
                .iter()
                .map(|s| s.to_string())
                .collect()
        });

        let options: Vec<SubmenuOption> = tools::all_tool_names()
            .into_iter()
            .map(|name| {
                let on = enabled.contains(&name);
                SubmenuOption {
                    description: tools::index::tool_description(&name).to_string(),
                    value: name.clone(),
                    label: name,
                    checked: on,
                }
            })
            .collect();

        let current_value = if enabled.is_empty() {
            "none".to_string()
        } else {
            enabled.join(", ")
        };

        SettingItem {
            id: "builtin-tools",
            label: "Built-in tools".to_string(),
            description: "Built-in tools enabled at startup (defaultTools). Writes +name/-name deltas to settings.json, so a hand-written plain list is preserved; a project-level defaultTools still overrides it. Takes effect on /reload or restart."
                .to_string(),
            current_value,
            kind: SettingKind::Submenu {
                title: "Built-in tools".to_string(),
                description: "Space/Enter toggles a tool. Deltas are written to settings.json immediately; the session's current tool set changes on /reload or restart."
                    .to_string(),
                kind: SubmenuKind::Toggle,
                options,
            },
        }
    }

    /// 只读设置项：thinking / theme（改由 /thinking、/theme 或快捷键修改）
    fn build_readonly_setting_items(
        theme_name: &str,
        thinking_current: String,
    ) -> Vec<SettingItem> {
        let items: Vec<SettingItem> = vec![
            SettingItem {
                id: "thinking",
                label: "Thinking level".to_string(),
                description: "Current reasoning depth (change via /thinking or Shift+Tab)"
                    .to_string(),
                current_value: thinking_current,
                kind: SettingKind::ReadOnly,
            },
            SettingItem {
                id: "theme",
                label: "Theme".to_string(),
                description: "Current color theme (change via /theme or Ctrl+Shift+T)".to_string(),
                current_value: theme_name.to_string(),
                kind: SettingKind::ReadOnly,
            },
        ];
        items
    }

    /// 关闭选择器：清空设置项与过滤结果，把键盘输入交还给输入框。
    pub fn close(&mut self) {
        self.active = false;
        self.submenu = None;
        self.items.clear();
        self.filtered.clear();
    }

    /// 重算过滤列表
    pub fn recompute(&mut self) {
        let query = self.filter.value.clone();
        let matched = fuzzy_filter(&self.items, &query, |i| i.label.as_str());
        self.filtered = matched
            .into_iter()
            .map(|item| {
                self.items
                    .iter()
                    .position(|i| std::ptr::eq(i, item))
                    .unwrap_or(0)
            })
            .collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    /// 移动选中（主列表按过滤结果循环；子菜单在选项内循环）
    pub fn move_selection(&mut self, delta: i32) {
        if let Some(sub) = &mut self.submenu {
            let n = sub.options.len().max(1);
            sub.selected = (sub.selected as i32 + delta).rem_euclid(n as i32) as usize;
            return;
        }
        let n = self.filtered.len() as i32;
        if n == 0 {
            return;
        }
        self.selected = (self.selected as i32 + delta).rem_euclid(n) as usize;
    }
}

/// 布尔设置项在面板里的展示字符串。
fn bool_str(value: bool) -> String {
    if value { "true" } else { "false" }.to_string()
}

/// 档位列表；当前值不在档位内（手写的非档位值）时插在首项之后，
/// 保证面板能原样展示手改值、且下一次循环不会跳回未知状态。
fn tier_values(tiers: &[String], current: &str) -> Vec<String> {
    let mut values = tiers.to_vec();
    if !values.iter().any(|v| v == current) {
        values.insert(1.min(values.len()), current.to_string());
    }
    values
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::modes::interactive::agent_actor::WorkerHandle;
    use crate::modes::interactive::app::App;

    fn test_agent() -> WorkerHandle {
        let _ad = crate::test_support::AgentDirGuard::temp();
        WorkerHandle::null()
    }

    fn open_sel(sel: &mut SettingsSelector, app: &App, _agent: &WorkerHandle) {
        sel.open(
            app.show_thinking,
            &app.theme.name,
            app.autocomplete_max_visible,
            app.thinking_level.as_deref().unwrap_or("off"),
        );
    }

    #[test]
    fn settings_list_matches_pi_order_and_skips_unsupported() {
        // 每测试独立临时 agent_dir（线程本地 override）：open_sel 只读本测试自己的 settings.json
        let _ad = crate::test_support::AgentDirGuard::temp();
        let st = App::new();
        let agent = test_agent();
        let mut sel = SettingsSelector::new();
        open_sel(&mut sel, &st, &agent);
        assert_eq!(sel.items[0].current_value, "true");
        // 列表项对齐 pi 的相对顺序（prux 自己的项追加在同类组末尾）；
        // 不支持的设置（skill commands / hardware cursor / padding / transport 等）不出现
        let labels: Vec<&str> = sel.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Auto-compact",
                "Cache warming",
                "Autocomplete max items",
                "History max entries",
                "Wheel scroll lines",
                "Steering mode",
                "Follow-up mode",
                "Retry",
                "Retry max attempts",
                "Compact reserve tokens",
                "Compact keep recent tokens",
                "Temperature",
                "Hide thinking",
                "Syntax highlight",
                "Mermaid",
                "Cache miss notices",
                "Quiet startup",
                "LaTeX",
                "Expose session env",
                "Images auto resize",
                "Images block",
                "Built-in tools",
                "Default project trust",
                "Show images",
                "Thinking level",
                "Theme",
            ]
        );
        // thinking / theme 只读（默认 trust 在 expose 后）；历史档位与 mermaid/latex 为 cycle
        assert!(
            matches!(sel.items[3].kind, SettingKind::Cycle { .. }),
            "历史档位为 cycle"
        );
        assert!(
            matches!(sel.items[1].kind, SettingKind::Cycle { .. }),
            "缓存保温为 cycle"
        );
        assert!(matches!(sel.items[6].kind, SettingKind::Cycle { .. }));
        assert!(matches!(sel.items[7].kind, SettingKind::Cycle { .. }));
        assert!(
            matches!(sel.items[15].kind, SettingKind::Cycle { .. }),
            "cache miss notices 为 cycle"
        );
        assert!(
            matches!(sel.items[17].kind, SettingKind::Cycle { .. }),
            "LaTeX 为 cycle"
        );
        assert!(
            matches!(sel.items[18].kind, SettingKind::Cycle { .. }),
            "expose 为 cycle"
        );
        assert!(
            matches!(sel.items[23].kind, SettingKind::Cycle { .. }),
            "show images 为 cycle"
        );
        // 新增的数值/开关项也是 cycle
        for idx in [8usize, 9, 10, 11, 19, 20] {
            assert!(
                matches!(sel.items[idx].kind, SettingKind::Cycle { .. }),
                "{} 为 cycle",
                sel.items[idx].label
            );
        }
        // 内置工具为勾选子面板
        assert!(
            matches!(
                sel.items[21].kind,
                SettingKind::Submenu {
                    kind: SubmenuKind::Toggle,
                    ..
                }
            ),
            "built-in tools 为勾选子面板"
        );
        assert!(matches!(sel.items[24].kind, SettingKind::ReadOnly));
        assert!(matches!(sel.items[25].kind, SettingKind::ReadOnly));
    }

    #[test]
    fn move_selection_wraps_at_both_ends() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let st = App::new();
        let agent = test_agent();
        let mut sel = SettingsSelector::new();
        open_sel(&mut sel, &st, &agent);
        assert!(sel.filtered.len() >= 2);

        // 顶部 ↑ 回绕到末项，末项 ↓ 回绕到首项
        sel.selected = 0;
        sel.move_selection(-1);
        assert_eq!(sel.selected, sel.filtered.len() - 1, "首项 ↑ 应环绕到末项");
        sel.move_selection(1);
        assert_eq!(sel.selected, 0, "末项 ↓ 应环绕到首项");

        // 子菜单选项同样环绕
        sel.submenu = Some(SubmenuState {
            title: "t".into(),
            description: String::new(),
            kind: SubmenuKind::Select,
            options: vec![
                SubmenuOption {
                    value: "a".into(),
                    label: "a".into(),
                    description: String::new(),
                    checked: false,
                },
                SubmenuOption {
                    value: "b".into(),
                    label: "b".into(),
                    description: String::new(),
                    checked: false,
                },
                SubmenuOption {
                    value: "c".into(),
                    label: "c".into(),
                    description: String::new(),
                    checked: false,
                },
            ],
            selected: 0,
            item_index: 0,
        });
        sel.move_selection(-1);
        assert_eq!(sel.submenu.as_ref().unwrap().selected, 2);
        sel.move_selection(1);
        assert_eq!(sel.submenu.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn search_filters_settings() {
        // 与 settings_manager 写盘测试互斥（见 settings_list_matches_pi_order 注释）
        let _ad = crate::test_support::AgentDirGuard::temp();
        let st = App::new();
        let agent = test_agent();
        let mut sel = SettingsSelector::new();
        open_sel(&mut sel, &st, &agent);
        sel.filter.set_value("autocomplete");
        sel.recompute();
        assert_eq!(sel.filtered.len(), 1);
        assert_eq!(sel.items[sel.filtered[0]].label, "Autocomplete max items");
        sel.filter.set_value("zzz-nope");
        sel.recompute();
        assert_eq!(sel.filtered.len(), 0);
    }
}
