//! 扩展面板（/extension）：空格 / Ctrl+A / Ctrl+X 只改未保存草稿（面板勾选即时可见，
//! 底栏、主题、工具链不动），Ctrl+S 才应用到注册表并写盘；关闭面板丢弃草稿。

use crate::{
    core::{
        extensions::{self, ExtensionMode},
        settings_manager,
    },
    modes::interactive::{
        self,
        agent_actor::AgentCommand,
        app::{App, MsgLevel},
        panel::{PanelItem, PanelKind},
    },
    utils::glyphs::DEF_INFO,
};

/// 构造扩展面板列表项：label 为 `[x]/[ ] 名称` 的勾选态、value 为扩展名；
/// 只保留当前扩展模式可用的扩展（`All` 模式为兜底，全部保留）。
pub(super) fn extension_panel_items() -> Vec<PanelItem> {
    let mode = extensions::current_extension_mode();
    extensions::panel_entries()
        .into_iter()
        .filter(|(_, _, plugin_modes)| {
            // All 为兜底：所有扩展隐式属于 All；其余模式按声明级别匹配
            mode == ExtensionMode::All || plugin_modes.iter().any(|d| d.usable_in(mode))
        })
        .map(|(name, enabled, _modes)| PanelItem {
            label: format!("[{}] {}", if enabled { "x" } else { " " }, name),
            value: name,
            desc: DEF_INFO.to_string(),
            name: String::new(),
            ..Default::default()
        })
        .collect()
}

impl App {
    /// 打开扩展选择面板（/extension；对齐 /model 面板布局）
    /// 列表项格式：`[x] 名称` + 𝒊 详情图标（checkbox 在 label 内，
    /// 空格切换后原地更新 label，保留 filter 与选中项）。
    /// 极简模式下非本模式扩展不显示（对齐现有禁用灰化的替代方案：
    /// 大量不可交互条目占位 + 禁用色不如直接不显示）。
    pub(super) fn open_extension_panel(&mut self) {
        let entries = extensions::panel_entries();
        if entries.is_empty() {
            self.push_msg(
                "no extensions registered (register via core::extensions::register_extension)"
                    .to_string(),
                MsgLevel::Info,
            );
            return;
        }
        let items = extension_panel_items();
        self.extensions_changed = false;
        self.extension_draft.clear();
        self.panel
            .open(PanelKind::Extension, "/extension".to_string(), items);
    }

    /// /extension 一级面板回车：进入二级详情（描述信息 + fork 来源 URL）
    pub(super) fn open_extension_detail(&mut self) {
        let Some(layer) = self.panel.top() else {
            return;
        };
        let filtered = self.panel.filtered_items();
        let Some(item) = filtered.get(layer.selected) else {
            return;
        };
        let name = item.value.clone();
        self.extension_detail = extensions::extension_detail_lines(&name);

        // 提取 fork URL（fork url: https://... 行）供 Ctrl+click 打开浏览器
        self.extension_detail_url = self
            .extension_detail
            .iter()
            .find_map(|l| l.strip_prefix("  fork url: ").map(|u| u.to_string()));
        self.panel
            .push(PanelKind::ExtensionDetail, name, Vec::new());
    }

    /// 面板项当前显示状态：草稿优先（未保存的切换只影响面板勾选）
    pub(super) fn extension_state(&self, name: &str) -> bool {
        self.extension_draft
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, on)| *on)
            .unwrap_or_else(|| extensions::is_extension_enabled(name))
    }

    /// 写入草稿。与注册表状态一致时不留草稿条目（草稿非空 ⇔ 有未保存改动）
    fn stage_extension_state(&mut self, name: &str, enabled: bool) {
        self.extension_draft.retain(|(n, _)| n != name);
        if extensions::is_extension_enabled(name) != enabled {
            self.extension_draft.push((name.to_string(), enabled));
        }
    }

    /// 草稿层的同组互斥：启用 footer/banner 成员时，把同组其他「显示为启用」的成员标为禁用
    fn stage_group_exclusive(&mut self, name: &str) {
        let group = if extensions::footer_names().iter().any(|n| n == name) {
            extensions::footer_names()
        } else if extensions::banner_names().iter().any(|n| n == name) {
            extensions::banner_names()
        } else {
            return;
        };

        for other in group {
            if other == name || !self.extension_state(&other) {
                continue;
            }
            self.stage_extension_state(&other, false);
        }
    }

    /// /extension 面板空格：切换选中扩展的启用状态。只写草稿——面板勾选即时变化，
    /// 但注册表 / 底栏 / 主题 / 工具链在 Ctrl+S 之前保持不变。
    /// 面板已按当前模式过滤（极简模式仅显示本模式扩展），无需再拦截。
    pub(super) fn toggle_selected_extension(&mut self) {
        let Some(layer) = self.panel.top() else {
            return;
        };
        let filtered = self.panel.filtered_items();
        let Some(item) = filtered.get(layer.selected).cloned() else {
            return;
        };

        let name = item.value.clone();
        let new_state = !self.extension_state(&name);
        if new_state {
            self.stage_group_exclusive(&name);
        }
        self.stage_extension_state(&name, new_state);

        // 原地更新列表项 checkbox（保留 filter 与选中位置）
        self.refresh_extension_labels();
        self.dirty = true;
    }

    /// /extension 面板 Ctrl+A / Ctrl+X：批量启用（全选）或禁用（取消全选）
    /// 当前列表内的扩展（有搜索词时只作用于过滤结果），同样只写草稿。
    ///
    /// footer/banner 组内互斥：全选时每组只取首个可启用项，其余成员草稿标为禁用，
    /// 否则最终留下哪个取决于遍历顺序。
    pub(super) fn set_extensions_enabled_bulk(&mut self, enabled: bool) {
        let names: Vec<String> = self
            .panel
            .filtered_items()
            .iter()
            .map(|it| it.value.clone())
            .collect();
        if names.is_empty() {
            return;
        }

        let footer = extensions::footer_names();
        let banner = extensions::banner_names();
        let (mut footer_picked, mut banner_picked) = (false, false);

        for name in names {
            let target = if !enabled {
                false
            } else if footer.iter().any(|n| n == &name) {
                let picked = !footer_picked;
                footer_picked = true;
                picked
            } else if banner.iter().any(|n| n == &name) {
                let picked = !banner_picked;
                banner_picked = true;
                picked
            } else {
                true
            };
            self.stage_extension_state(&name, target);
        }

        self.refresh_extension_labels();
        self.dirty = true;
    }

    /// /extension 面板 Ctrl+S：把草稿应用到注册表并写回 settings.json
    /// （disabledExtensions / enabledExtensions）。应用后才重建工具列表与主题。
    pub(super) fn save_extension_states(&mut self) {
        if self.extension_draft.is_empty() {
            return;
        }
        let draft = std::mem::take(&mut self.extension_draft);

        for (name, enabled) in &draft {
            extensions::set_extension_enabled(name, *enabled);
        }

        self.extensions_changed = true;
        self.refresh_extension_labels();

        if draft.iter().any(|(name, _)| name == "extra-themes") {
            self.refresh_extra_themes();
        }

        match extensions::persist_extension_states() {
            Ok(()) => self.push_msg(
                "extension states saved to settings".to_string(),
                MsgLevel::Success,
            ),
            Err(e) => self.push_msg(
                format!("failed to save extension state: {}", e),
                MsgLevel::Error,
            ),
        }
        self.dirty = true;
    }

    /// 关闭面板：丢弃未保存草稿（注册表/底栏/主题全程未被改动，无需回滚）
    pub(super) fn discard_extension_draft(&mut self) {
        self.extension_draft.clear();
    }

    /// 按草稿优先的状态重建面板内全部 checkbox 标签（footer/banner 互斥后的同步）
    pub(super) fn refresh_extension_labels(&mut self) {
        let labels: Vec<String> = match self.panel.top() {
            Some(layer) => layer
                .items
                .iter()
                .map(|it| {
                    let on = self.extension_state(&it.value);
                    format!("[{}] {}", if on { "x" } else { " " }, it.value)
                })
                .collect(),
            None => return,
        };

        if let Some(layer) = self.panel.top_mut() {
            for (it, label) in layer.items.iter_mut().zip(labels) {
                it.label = label;
            }
        }
    }

    /// extra-themes 启用状态变化后重载主题：该扩展会写入/移除 agent_dir/themes
    /// 下的主题文件，需立即重载让本次切换即时可见（对齐 /reload 的主题刷新）。
    fn refresh_extra_themes(&mut self) {
        let theme_name = self.theme.name.clone();
        let mut theme = interactive::theme::Theme::load(&theme_name);
        theme.syntax_highlight = settings_manager::read_settings_syntax_highlight();
        theme.mermaid = settings_manager::read_settings_mermaid();
        theme.latex = settings_manager::read_settings_latex();
        self.theme = theme;
        self.invalidate_render(); // 主题文件可能已被写入/移除：颜色变了 → 清消息渲染缓存
    }

    /// 面板关闭（Esc/Ctrl+C）回到主页时应用扩展变更：
    /// 重建当前 Agent 的工具列表与系统提示，使启用状态立即生效（对齐 pi 扩展热更新）。
    pub(super) fn apply_extension_changes(&mut self) {
        if !self.extensions_changed {
            return;
        }

        self.extensions_changed = false;
        self.worker.send(AgentCommand::RebuildTools);

        // 扩展命令随启用状态动态增删：失效当前候选列表，下次输入重建。
        self.suggestion.deselect();
        self.push_msg(
            "extensions updated: tools rebuilt".to_string(),
            MsgLevel::Success,
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::super::commands::slash_commands;
    use super::*;
    use crate::modes::interactive::agent_actor::WorkerHandle;
    use crate::modes::interactive::app::App;
    use crate::modes::interactive::handlers::KeyAction;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    fn lock_auth() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn press(st: &mut App, code: KeyCode) -> KeyAction {
        st.handle_panel_key(&KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(st: &mut App, c: char) {
        st.handle_panel_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    /// 面板内某项的 checkbox 标签（`[x] name` / `[ ] name`）
    fn panel_label(st: &mut App, name: &str) -> String {
        st.panel
            .top_mut()
            .unwrap()
            .items
            .iter_mut()
            .find(|i| i.value == name)
            .unwrap()
            .label
            .clone()
    }

    /// 选中某项（注册表里可能有其他测试注册的扩展，不能假定 selected）
    fn select(st: &mut App, name: &str) {
        let idx = st
            .panel
            .top()
            .unwrap()
            .items
            .iter()
            .position(|i| i.value == name)
            .unwrap();
        st.panel.top_mut().unwrap().selected = idx;
    }

    /// channels 版 worker：可断言 handler 发出的命令（分层测试 C 的 handler 层）
    fn test_agent_cmd() -> (
        WorkerHandle,
        crate::modes::interactive::agent_actor::CommandRx,
    ) {
        // env 类测试（worker 跨线程读 agent_dir）仍走进程级 pin；线程已有 guard 时 pin 自动 no-op
        crate::test_support::pin_test_agent_dir();
        let (cmd_tx, cmd_rx) = crate::modes::interactive::agent_actor::channels();
        (WorkerHandle::new(cmd_tx), cmd_rx)
    }

    fn register_panel_test_ext() {
        use std::sync::Once;
        static REG: Once = Once::new();
        REG.call_once(|| {
            crate::core::extensions::register_extension(PanelTestExt);
        });
    }

    fn register_panel_other_ext() {
        use std::sync::Once;
        static REG: Once = Once::new();
        REG.call_once(|| {
            crate::core::extensions::register_extension(PanelOtherExt);
        });
    }

    struct PanelOtherExt;
    impl crate::core::extensions::Extension for PanelOtherExt {
        fn name(&self) -> &str {
            "panel-other-ext"
        }
        fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
            Vec::new()
        }
    }

    struct PanelTestExt;
    impl crate::core::extensions::Extension for PanelTestExt {
        fn name(&self) -> &str {
            "panel-test-ext"
        }
        fn description(&self) -> &str {
            "panel test description"
        }
        fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
            vec![crate::core::extensions::ExtensionTool::simple(
                "ext-tool-a",
                "tool from panel test ext",
                serde_json::json!({ "type": "object" }),
                "panel test tool",
            )]
        }
    }

    #[test]
    fn apply_extension_changes_does_not_block_while_agent_locked() {
        // 回归：busy 时 Ctrl+L 打开模型面板后 Esc 关闭触发 apply_extension_changes，
        // prompt future 持 agent 锁时不得拖锁等待（拖锁死锁）。
        let _g = lock_auth();
        let (agent, mut cmd_rx) = test_agent_cmd();
        let mut st = App::new();
        st.worker = agent;
        st.extensions_changed = true;
        st.apply_extension_changes();
        // actor：重建工具由 RebuildTools 命令下发 worker（回执 tools_rebuilt 提示）
        let cmd = cmd_rx.try_recv().expect("应发出 RebuildTools 命令");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::RebuildTools
            ),
            "应发 RebuildTools: {cmd:?}"
        );
        assert!(!st.extensions_changed, "发命令后变更标记应复位");
    }

    #[test]
    fn extension_panel_toggle_detail_and_rebuild_flow() {
        let _g = lock_auth();
        register_panel_test_ext();
        // 确保全部模式 + 扩展启用（其他测试可能切换过模式/状态），并清除面板残留状态
        crate::core::extensions::set_extension_mode(crate::core::extensions::ExtensionMode::All);
        crate::core::settings_manager::write_extension_mode("all").ok();
        crate::core::extensions::set_extension_enabled("panel-test-ext", true);
        let _ = crate::core::extensions::persist_extension_states();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        let mut st = App::new();

        // 打开面板：列表项 = checkbox + 名称 + 𝒊
        st.open_extension_panel();
        assert!(st.panel.active);
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::Extension);
        let items = st.panel.top().unwrap().items.clone();
        let item = items
            .iter()
            .find(|i| i.value == "panel-test-ext")
            .expect("扩展条目");
        assert_eq!(item.label, "[x] panel-test-ext");
        assert_eq!(item.desc, "𝒊");
        // 选中 panel-test-ext（注册表可能有其他测试注册的扩展，不能假定 selected=0）
        st.panel.top_mut().unwrap().selected = items
            .iter()
            .position(|i| i.value == "panel-test-ext")
            .unwrap();

        // 空格：只写草稿——面板勾选即时变化，注册表 / 磁盘都不动
        press(&mut st, KeyCode::Char(' '));
        assert_eq!(panel_label(&mut st, "panel-test-ext"), "[ ] panel-test-ext");
        assert!(
            crate::core::extensions::is_extension_enabled("panel-test-ext"),
            "未保存不应改注册表"
        );
        assert!(!st.extension_draft.is_empty(), "空格切换后应记为未保存");
        assert!(!st.extensions_changed, "未保存不应触发工具重建");
        assert!(
            !crate::core::settings_manager::read_disabled_extensions()
                .contains(&"panel-test-ext".to_string()),
            "空格不应写盘"
        );

        // Ctrl+S：应用草稿到注册表 + 写盘
        ctrl(&mut st, 's');
        assert!(!crate::core::extensions::is_extension_enabled(
            "panel-test-ext"
        ));
        assert!(st.extension_draft.is_empty(), "保存后草稿应清空");
        assert!(st.extensions_changed, "保存应触发工具重建");
        assert!(
            crate::core::settings_manager::read_disabled_extensions()
                .contains(&"panel-test-ext".to_string())
        );

        // 回车：进入二级详情
        press(&mut st, KeyCode::Enter);
        assert!(st.panel.active);
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::ExtensionDetail);
        assert_eq!(st.panel.top().unwrap().title, "panel-test-ext");
        assert!(
            st.extension_detail
                .iter()
                .any(|l| l.contains("panel test description")),
            "detail: {:?}",
            st.extension_detail
        );
        // 详情状态徽标数据源：注册表为禁用（已保存）
        assert!(!crate::core::extensions::is_extension_enabled(
            "panel-test-ext"
        ));

        // Esc：详情 → 返回一级列表（面板保持打开，扩展状态不变）
        press(&mut st, KeyCode::Esc);
        assert!(st.panel.active);
        assert_eq!(st.panel.top().unwrap().kind, PanelKind::Extension);
        assert!(!crate::core::extensions::is_extension_enabled(
            "panel-test-ext"
        ));

        // Esc：关闭面板 → 重建 Agent 工具列表（扩展工具移除）
        press(&mut st, KeyCode::Esc);
        assert!(!st.panel.active);
        assert!(!st.extensions_changed, "apply 后复位");

        // 关闭面板丢弃未保存草稿：空格只改勾选，Esc 后注册表仍保持上次保存的状态
        st.open_extension_panel();
        select(&mut st, "panel-test-ext");
        press(&mut st, KeyCode::Char(' '));
        assert_eq!(panel_label(&mut st, "panel-test-ext"), "[x] panel-test-ext");
        assert!(
            !crate::core::extensions::is_extension_enabled("panel-test-ext"),
            "草稿不应落到注册表"
        );
        press(&mut st, KeyCode::Esc);
        assert!(!st.panel.active);
        assert!(st.extension_draft.is_empty(), "关闭应丢弃草稿");
        assert!(!crate::core::extensions::is_extension_enabled(
            "panel-test-ext"
        ));

        // 还原全局状态，避免影响其他测试
        crate::core::extensions::set_extension_enabled("panel-test-ext", true);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }

    #[test]
    fn extension_command_help_and_slash_registered() {
        assert!(slash_commands().iter().any(|(n, _)| n == "extension"));
    }

    /// Ctrl+A 全选 / Ctrl+X 取消全选：只作用于当前列表（有搜索词时为过滤结果），
    /// 且只写草稿（Ctrl+S 才应用到注册表并写盘）。
    #[test]
    fn ctrl_a_x_bulk_toggle_scoped_by_filter_and_saved_only_by_ctrl_s() {
        let _g = lock_auth();
        register_panel_test_ext();
        register_panel_other_ext();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        crate::core::extensions::set_extension_enabled("panel-test-ext", true);
        crate::core::extensions::set_extension_enabled("panel-other-ext", true);

        let mut st = App::new();
        st.open_extension_panel();
        assert!(st.extension_draft.is_empty(), "打开面板应清空草稿");

        // 过滤后 Ctrl+X：只作用过滤命中的扩展
        st.panel.top_mut().unwrap().filter.set_value("panel-test");
        assert_eq!(st.panel.filtered_items().len(), 1);
        ctrl(&mut st, 'x');
        assert_eq!(panel_label(&mut st, "panel-test-ext"), "[ ] panel-test-ext");
        assert!(
            crate::core::extensions::is_extension_enabled("panel-test-ext"),
            "未保存不应改注册表"
        );
        assert!(
            crate::core::extensions::is_extension_enabled("panel-other-ext"),
            "过滤外的扩展不应被批量修改"
        );
        assert!(!st.extension_draft.is_empty(), "批量切换后应记为未保存");
        assert!(
            !crate::core::settings_manager::read_disabled_extensions()
                .contains(&"panel-test-ext".to_string()),
            "Ctrl+X 不应写盘"
        );

        // Ctrl+S：应用草稿 + 写盘
        ctrl(&mut st, 's');
        assert!(st.extension_draft.is_empty(), "保存后草稿应清空");
        assert!(!crate::core::extensions::is_extension_enabled(
            "panel-test-ext"
        ));
        assert!(crate::core::extensions::is_extension_enabled(
            "panel-other-ext"
        ));
        assert!(
            crate::core::settings_manager::read_disabled_extensions()
                .contains(&"panel-test-ext".to_string())
        );

        // Ctrl+A：在过滤结果内重新启用（先只写草稿，盘上仍禁用）
        ctrl(&mut st, 'a');
        assert_eq!(panel_label(&mut st, "panel-test-ext"), "[x] panel-test-ext");
        assert!(
            !crate::core::extensions::is_extension_enabled("panel-test-ext"),
            "Ctrl+A 在 Ctrl+S 之前不应改注册表"
        );
        assert!(
            crate::core::settings_manager::read_disabled_extensions()
                .contains(&"panel-test-ext".to_string()),
            "Ctrl+A 在 Ctrl+S 之前不应写盘"
        );

        ctrl(&mut st, 's');
        assert!(crate::core::extensions::is_extension_enabled(
            "panel-test-ext"
        ));
        assert!(
            !crate::core::settings_manager::read_disabled_extensions()
                .contains(&"panel-test-ext".to_string()),
            "Ctrl+S 应清掉盘上的禁用项"
        );

        // 无草稿时 Ctrl+S 幂等（不发重复保存、不动状态）
        ctrl(&mut st, 's');
        assert!(st.extension_draft.is_empty());

        press(&mut st, KeyCode::Esc);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }

    /// Ctrl+A 全选：footer 同组互斥，每组只保留首个可启用项（否则结果取决于遍历顺序）
    #[test]
    fn ctrl_a_keeps_one_footer_enabled() {
        use crate::core::extensions::{FooterCtx, FooterExtension, FooterLine};

        struct FooterBulkA;
        impl FooterExtension for FooterBulkA {
            fn name(&self) -> &str {
                "panel-footer-bulk-a"
            }
            fn render(&self, _ctx: &FooterCtx) -> Vec<FooterLine> {
                Vec::new()
            }
        }
        struct FooterBulkB;
        impl FooterExtension for FooterBulkB {
            fn name(&self) -> &str {
                "panel-footer-bulk-b"
            }
            fn render(&self, _ctx: &FooterCtx) -> Vec<FooterLine> {
                Vec::new()
            }
        }
        use std::sync::Once;
        static REG: Once = Once::new();
        REG.call_once(|| {
            crate::core::extensions::register_footer_extension(FooterBulkA);
            crate::core::extensions::register_footer_extension(FooterBulkB);
        });

        let _g = lock_auth();
        crate::core::extensions::set_extension_enabled("panel-footer-bulk-a", false);
        crate::core::extensions::set_extension_enabled("panel-footer-bulk-b", false);

        let mut st = App::new();
        st.open_extension_panel();
        st.panel
            .top_mut()
            .unwrap()
            .filter
            .set_value("panel-footer-bulk");
        assert_eq!(st.panel.filtered_items().len(), 2);

        ctrl(&mut st, 'a');
        assert_eq!(
            (
                panel_label(&mut st, "panel-footer-bulk-a").as_str(),
                panel_label(&mut st, "panel-footer-bulk-b").as_str()
            ),
            ("[x] panel-footer-bulk-a", "[ ] panel-footer-bulk-b"),
            "同组全选只应留下首个（a）"
        );
        assert!(
            !crate::core::extensions::is_extension_enabled("panel-footer-bulk-a"),
            "未保存时注册表不应被改动"
        );

        // Ctrl+S：草稿落到注册表，同组互斥生效
        ctrl(&mut st, 's');
        assert!(
            crate::core::extensions::is_extension_enabled("panel-footer-bulk-a")
                && !crate::core::extensions::is_extension_enabled("panel-footer-bulk-b"),
            "保存后同组只应启用 a"
        );

        // 还原：关掉这一组，避免影响其他用例的底部渲染
        crate::core::extensions::set_extension_enabled("panel-footer-bulk-a", false);
        press(&mut st, KeyCode::Esc);
    }

    #[test]
    fn tab_cycles_modes_descending() {
        let _g = lock_auth();
        register_panel_test_ext();
        // 环境还原：全部模式 + 全部扩展启用
        crate::core::extensions::set_extension_mode(crate::core::extensions::ExtensionMode::All);
        crate::core::extensions::set_extension_enabled("panel-test-ext", true);
        crate::core::settings_manager::write_extension_mode("all").ok();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        let mut st = App::new();
        st.open_extension_panel();

        // 默认全部模式：所有扩展可切换（panel-test-ext 仅 All 模式）
        assert_eq!(
            crate::core::extensions::current_extension_mode(),
            crate::core::extensions::ExtensionMode::All
        );
        st.panel.top_mut().unwrap().selected = st
            .panel
            .top()
            .unwrap()
            .items
            .iter()
            .position(|i| i.value == "panel-test-ext")
            .unwrap();
        press(&mut st, KeyCode::Char(' '));
        assert_eq!(panel_label(&mut st, "panel-test-ext"), "[ ] panel-test-ext");
        press(&mut st, KeyCode::Char(' '));
        assert_eq!(panel_label(&mut st, "panel-test-ext"), "[x] panel-test-ext");
        assert!(st.extension_draft.is_empty(), "来回切换后草稿应回到空");

        // Tab → Dev 模式（降序 All→Dev→Creator→Minimal，写盘持久化）
        press(&mut st, KeyCode::Tab);
        assert_eq!(
            crate::core::extensions::current_extension_mode(),
            crate::core::extensions::ExtensionMode::Dev
        );
        assert_eq!(crate::core::settings_manager::read_extension_mode(), "dev");
        assert!(st.extensions_changed);

        // Dev 模式：非本模式扩展（panel-test-ext 仅 All 模式）不再显示
        assert!(
            !st.panel
                .top()
                .unwrap()
                .items
                .iter()
                .any(|i| i.value == "panel-test-ext"),
            "Dev 模式下声明为 All 的扩展应被过滤: {:?}",
            st.panel
                .top()
                .unwrap()
                .items
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>()
        );

        // Tab → Creator（并排同级，同样不含 All 扩展）
        press(&mut st, KeyCode::Tab);
        assert_eq!(
            crate::core::extensions::current_extension_mode(),
            crate::core::extensions::ExtensionMode::Creator
        );
        assert_eq!(
            crate::core::settings_manager::read_extension_mode(),
            "creator"
        );
        assert!(
            !st.panel
                .top()
                .unwrap()
                .items
                .iter()
                .any(|i| i.value == "panel-test-ext"),
            "Creator 模式下声明为 All 的扩展应被过滤"
        );

        // Tab → Minimal（仍不含 All 扩展）
        press(&mut st, KeyCode::Tab);
        assert_eq!(
            crate::core::extensions::current_extension_mode(),
            crate::core::extensions::ExtensionMode::Minimal
        );
        assert_eq!(
            crate::core::settings_manager::read_extension_mode(),
            "minimal"
        );
        assert!(
            !st.panel
                .top()
                .unwrap()
                .items
                .iter()
                .any(|i| i.value == "panel-test-ext"),
            "极简模式下声明为 All 的扩展应被过滤"
        );

        // Tab → 回到全部模式：恢复显示
        press(&mut st, KeyCode::Tab);
        assert_eq!(
            crate::core::extensions::current_extension_mode(),
            crate::core::extensions::ExtensionMode::All
        );
        assert_eq!(crate::core::settings_manager::read_extension_mode(), "all");
        assert!(
            st.panel
                .top()
                .unwrap()
                .items
                .iter()
                .any(|i| i.value == "panel-test-ext"),
            "切回全部模式后扩展恢复显示"
        );
        // 还原清理（关闭面板触发 rebuild 不必要，直接清盘）
        press(&mut st, KeyCode::Esc);
        crate::core::settings_manager::write_extension_mode("all").ok();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }
}
