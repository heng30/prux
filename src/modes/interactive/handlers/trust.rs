//! 项目信任处理：启动询问面板、`/trust` 对话框、信任决策落盘与未信任警告。
//!
//! 职责边界：
//! - 状态字段 `App::ask_trust` 与 `take_next_action` 的派发门控留在 app.rs；
//! - 面板内容与绘制在 `render::trust`；
//! - 本模块只做状态流转、写盘与提示（启动路径 `maybe_ask_project_trust` 与本模块配对）。
//!
//! 启动选择器含「仅本次」选项（`trust_session`/`distrust_session` 只影响本次运行），
//! `/trust` 对话框只保存持久决策，重启后生效。

use crate::{
    APP_NAME, PROJECT_SCOPE_NAME,
    core::{self, settings_manager},
    modes::interactive::{
        app::{App, MsgLevel},
        panel::PanelKind,
        render::trust as trust_view,
    },
};
use std::path::{Path, PathBuf};

impl App {
    /// 启动期项目信任选择面板：
    /// Trust / Trust parent folder (…) / Trust (this session only) / Do not trust /
    /// Do not trust (this session only)。
    /// 标题与选项由渲染层 `render::trust::project_trust_panel_content` 提供。
    pub(crate) fn open_project_trust_panel(&mut self) {
        let (title, items) = trust_view::project_trust_panel_content(self);

        self.panel.close();
        self.panel.open(PanelKind::ProjectTrust, title, items);
        // 置位 ask_trust：首个用户消息在信任决定前不派发给 agent（见 take_next_action）
        self.ask_trust = true;
        self.dirty = true;
    }

    /// 处理项目信任选择，返回是否已信任（true 时调用方应重载项目资源）。
    /// `trust`/`trust_parent`/`distrust` 写盘持久化；`trust_session`/`distrust_session` 仅本次运行。
    ///
    /// 无论哪个选项都同步**会话信任**（`set_session_trust`）：项目 skills/context/SYSTEM.md
    /// 走 `reload_ctx.trusted`，子代理的 agents/workflows/agent-memory/config 直接读
    /// `is_project_trusted`，两者必须得出同一结论。
    pub(crate) fn resolve_project_trust(&mut self, choice: &str) -> bool {
        let cwd = PathBuf::from(&self.cwd);
        let agent_dir = settings_manager::agent_dir();
        let trusted = match choice {
            "trust" => {
                core::project_trust::set_project_trust(&cwd, &agent_dir, true);
                core::project_trust::set_session_trust(&cwd, true);
                true
            }
            "trust_parent" => {
                let parent = cwd.parent().unwrap_or(&cwd).to_path_buf();
                core::project_trust::set_project_trust(&parent, &agent_dir, true);
                core::project_trust::clear_project_trust(&cwd, &agent_dir);
                // 会话层同样改记到父目录：启动时给 cwd 登记的决策必须先撤掉，
                // 否则它会盖过刚写下的父目录决策（最近祖先里 cwd 更近）。
                core::project_trust::set_session_trust(&parent, true);
                core::project_trust::clear_session_trust(&cwd);
                true
            }
            "trust_session" => {
                core::project_trust::set_session_trust(&cwd, true);
                true
            }
            "distrust" => {
                core::project_trust::set_project_trust(&cwd, &agent_dir, false);
                core::project_trust::set_session_trust(&cwd, false);
                false
            }
            // distrust_session / 取消：本次不加载项目资源，也不写盘
            _ => {
                core::project_trust::set_session_trust(&cwd, false);
                false
            }
        };

        self.ask_trust = false;
        self.panel.close();
        self.dirty = true;

        // 会话信任门控与 reload 同步；/trust 后续也据此加载项目资源
        self.reload_ctx.trusted = trusted;

        if trusted {
            self.push_msg(
                format!("project trusted: {}", cwd.display()),
                MsgLevel::Success,
            );
        } else {
            self.warn_if_project_untrusted();
        }
        trusted
    }

    /// 项目未被信任且存在需信任资源时的启动警告。
    /// 仅在信任决策结束后调用；信任面板打开期间不提示（避免与面板同时出现）。
    pub(crate) fn warn_if_project_untrusted(&mut self) {
        if self.reload_ctx.trusted
            || !core::project_trust::has_trust_requiring_project_resources(Path::new(&self.cwd))
        {
            return;
        }
        self.push_msg(
            format!(
                "This project is not trusted. Project {} resources and packages are ignored. Use /trust to save a trust decision, then restart {}.",
                PROJECT_SCOPE_NAME, APP_NAME,
            ),
            MsgLevel::Warning,
        );
    }

    /// `/trust` 选择面板：展示已保存决策与当前会话状态，
    /// 选项为 Trust / Trust parent folder / Do not trust（无仅本次选项）。
    /// 仅保存决策，重启后生效（不修改当前会话信任）。
    /// 标题/选项/初始选中由渲染层 `render::trust::project_trust_dialog_content` 提供。
    pub(crate) fn open_project_trust_dialog(&mut self) {
        let (title, items, selected) = trust_view::project_trust_dialog_content(self);

        self.panel.close();
        self.panel.open(PanelKind::ProjectTrustDialog, title, items);
        if let Some(layer) = self.panel.top_mut() {
            layer.selected = selected;
        }
        self.dirty = true;
    }

    /// `/trust` 面板确认：持久化决策（不改变当前会话信任，也不动会话决策），并提示重启后生效。
    pub(crate) fn save_project_trust_decision(&mut self, choice: &str) {
        let cwd = PathBuf::from(&self.cwd);
        let agent_dir = settings_manager::agent_dir();
        let trusted = match choice {
            "trust" => {
                core::project_trust::set_project_trust(&cwd, &agent_dir, true);
                true
            }
            "trust_parent" => {
                core::project_trust::set_project_trust(
                    cwd.parent().unwrap_or(&cwd),
                    &agent_dir,
                    true,
                );
                core::project_trust::clear_project_trust(&cwd, &agent_dir);
                true
            }
            "distrust" => {
                core::project_trust::set_project_trust(&cwd, &agent_dir, false);
                false
            }
            _ => return,
        };

        self.panel.close();
        self.dirty = true;
        self.push_msg(
            format!(
                "Saved trust decision: {}. Restart {} for this to take effect.",
                if trusted { "trusted" } else { "untrusted" },
                APP_NAME,
            ),
            MsgLevel::Info,
        );
    }

    /// `/trust` 面板选「Remove all saved trust decisions」：弹 Yes/No 确认面板。
    /// 确认面板替换掉 `/trust` 面板（与其它确认面板族一致），Esc/No 不写盘。
    pub(crate) fn open_trust_clear_confirm(&mut self) {
        self.open_confirm(
            PanelKind::TrustClearConfirm,
            "Remove all saved trust decisions?",
            &[("Yes", "yes"), ("No", "no")],
        );
    }

    /// 清空确认面板选择：`yes` = 清空 trust.json 全部内容；其余 = 取消。
    /// 与 `/trust` 其它选项一样只改持久决策，不动本次运行的会话决策（重启后生效）。
    pub(crate) fn resolve_trust_clear_confirm(&mut self, choice: &str) {
        if choice != "yes" {
            self.push_msg(
                "Clearing trust decisions cancelled".to_string(),
                MsgLevel::Info,
            );
            return;
        }

        let agent_dir = settings_manager::agent_dir();
        core::project_trust::clear_all_trust(&agent_dir);
        self.push_msg(
            format!(
                "Cleared all saved trust decisions in {}. Restart {} for this to take effect.",
                core::project_trust::trust_file_path(&agent_dir).display(),
                APP_NAME,
            ),
            MsgLevel::Info,
        );
    }

    /// 清空确认面板取消（Esc/Ctrl+C）：保留所有信任决策。
    pub(crate) fn cancel_trust_clear_confirm(&mut self) {
        self.push_msg(
            "Clearing trust decisions cancelled".to_string(),
            MsgLevel::Info,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::sys_spans_text;

    #[test]
    fn project_trust_panel_persists_and_gates_reload() {
        // Trust → 写盘 + reload_ctx.trusted；仅本次信任 → 不写盘但会话内生效；
        // Do not trust → 写入 false。每个选项都同步会话信任（is_project_trusted 读它）。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let agent_dir = settings_manager::agent_dir();
        let cwd = "/tmp/prux-trust-panel-cwd";

        let mut app = App {
            cwd: cwd.to_string(),
            ..App::default()
        };
        app.open_project_trust_panel();
        assert!(app.ask_trust);
        assert_eq!(
            app.panel.top().map(|l| l.kind),
            Some(PanelKind::ProjectTrust)
        );

        assert!(app.resolve_project_trust("trust"));
        assert!(!app.ask_trust, "决定后解除门控");
        assert!(app.reload_ctx.trusted, "信任后 reload 应加载项目资源");
        assert!(
            core::project_trust::is_project_trusted(Path::new(cwd), &agent_dir),
            "Trust 应立即对 is_project_trusted 生效"
        );
        assert_eq!(
            core::project_trust::trust_decision(Path::new(cwd), &agent_dir),
            Some((cwd.to_string(), true)),
            "Trust 应写盘"
        );

        // 仅本次信任：不写盘（先清掉上一条持久化记录）
        core::project_trust::clear_project_trust(Path::new(cwd), &agent_dir);
        let mut app = App {
            cwd: cwd.to_string(),
            ..App::default()
        };
        app.open_project_trust_panel();
        assert!(app.resolve_project_trust("trust_session"));
        assert!(app.reload_ctx.trusted);
        assert!(
            core::project_trust::trust_decision(Path::new(cwd), &agent_dir).is_none(),
            "仅本次信任不写盘"
        );
        assert!(
            core::project_trust::is_project_trusted(Path::new(cwd), &agent_dir),
            "仅本次信任应让子代理项目资源（agents/workflows/memory）可读"
        );

        // Do not trust：持久化 false，且不解除 reload 信任
        let mut app = App {
            cwd: cwd.to_string(),
            ..App::default()
        };
        app.open_project_trust_panel();
        assert!(!app.resolve_project_trust("distrust"));
        assert!(!app.reload_ctx.trusted);
        assert!(
            !core::project_trust::is_project_trusted(Path::new(cwd), &agent_dir),
            "Do not trust 应立即失效"
        );
        assert_eq!(
            core::project_trust::trust_decision(Path::new(cwd), &agent_dir),
            Some((cwd.to_string(), false)),
            "Do not trust 应写盘"
        );

        // 收尾：清掉固定路径上的会话决策，避免影响同进程其它用例
        core::project_trust::clear_session_trust(Path::new(cwd));
    }

    #[test]
    fn session_distrust_overrides_persisted_ancestor_trust() {
        // 父目录已持久信任时，面板选「Do not trust (this session only)」必须让当前项目
        // 本次运行不信任（会话决策盖过 trust.json 的祖先条目）。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let agent_dir = settings_manager::agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("repo");
        let cwd = parent.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        core::project_trust::set_project_trust(&parent, &agent_dir, true);
        assert!(core::project_trust::is_project_trusted(&cwd, &agent_dir));

        let mut app = App {
            cwd: cwd.to_string_lossy().to_string(),
            ..App::default()
        };
        app.open_project_trust_panel();
        assert!(!app.resolve_project_trust("distrust_session"));
        assert!(!app.reload_ctx.trusted);
        assert!(
            !core::project_trust::is_project_trusted(&cwd, &agent_dir),
            "会话不信任应盖过父目录的持久信任"
        );

        core::project_trust::clear_session_trust(&cwd);
    }

    #[test]
    fn untrusted_project_warning_line() {
        // 未信任 + 存在需信任资源 → 一行 pi 风格警告；已信任则不提示。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(cwd.join(".prux")).unwrap();
        std::fs::write(cwd.join(".prux").join("settings.json"), "{}").unwrap();

        let mut app = App {
            cwd: cwd.to_string_lossy().to_string(),
            ..App::default()
        };
        app.reload_ctx.trusted = false;
        app.warn_if_project_untrusted();
        let text = app
            .system_messages
            .last()
            .map(|(_, m)| sys_spans_text(&m.spans))
            .unwrap_or_default();
        assert!(text.contains("This project is not trusted"), "{text}");
        assert!(
            text.contains(".prux resources and packages are ignored"),
            "{text}"
        );
        assert!(text.contains("then restart prux."), "{text}");

        // 已信任：不提示
        let n = app.system_messages.len();
        app.reload_ctx.trusted = true;
        app.warn_if_project_untrusted();
        assert_eq!(app.system_messages.len(), n, "已信任不应提示");
    }

    #[test]
    fn trust_dialog_saves_decision_without_session_change() {
        // /trust 面板：仅保存决策 + 提示重启，不改变当前会话信任（对齐 pi TrustSelectorComponent）。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let agent_dir = settings_manager::agent_dir();

        let mut app = App {
            cwd: cwd.to_string_lossy().to_string(),
            ..App::default()
        };
        app.reload_ctx.trusted = false;
        // 模拟启动：`resolve_trusted` 会把当时的结论登记为会话决策；
        // 运行时 is_project_trusted 以它为准，不再重读 trust.json（对齐 pi 的内存值）。
        core::project_trust::set_session_trust(&cwd, false);
        app.open_project_trust_dialog();
        assert_eq!(
            app.panel.top().map(|l| l.kind),
            Some(PanelKind::ProjectTrustDialog)
        );
        // 无已保存决策 → 默认选中 Trust
        assert_eq!(app.panel.top().unwrap().selected, 0);

        app.save_project_trust_decision("trust");
        assert_eq!(
            core::project_trust::trust_decision(&cwd, &agent_dir),
            Some((crate::utils::paths::normalize_cwd(&cwd), true)),
            "应写盘 trusted"
        );
        assert!(
            !app.reload_ctx.trusted,
            "保存不改变当前会话信任（重启后生效）"
        );
        assert!(
            !core::project_trust::is_project_trusted(&cwd, &agent_dir),
            "仅写盘也不能改变会话信任（子代理资源依然不加载）"
        );
        let text = app
            .system_messages
            .last()
            .map(|(_, m)| sys_spans_text(&m.spans))
            .unwrap_or_default();
        assert!(
            text.contains("Saved trust decision: trusted. Restart prux for this to take effect."),
            "{text}"
        );
        core::project_trust::clear_session_trust(&cwd);
    }

    #[test]
    fn trust_parent_writes_only_parent_decision() {
        // 回归：启动面板选「Trust parent folder」后 trust.json 应当只有 `{parent: true}`，
        // 不能多出一条 `cwd: null` 墓碑（pi `setMany(null)` 是 delete）。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let agent_dir = settings_manager::agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("repo");
        let cwd = parent.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();

        let mut app = App {
            cwd: cwd.to_string_lossy().to_string(),
            ..App::default()
        };
        app.open_project_trust_panel();
        assert!(app.resolve_project_trust("trust_parent"));

        let raw = std::fs::read_to_string(agent_dir.join("trust.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let obj = parsed.as_object().unwrap();
        assert_eq!(obj.len(), 1, "不该留下 cwd 的 null 占位：{raw}");
        assert_eq!(
            obj.get(&crate::utils::paths::normalize_cwd(&parent)),
            Some(&serde_json::json!(true))
        );

        // 父目录决策对 cwd 生效（本次会话与持久层一致）
        assert!(app.reload_ctx.trusted);
        assert!(core::project_trust::is_project_trusted(&cwd, &agent_dir));

        core::project_trust::clear_session_trust(&cwd);
        core::project_trust::clear_session_trust(&parent);
    }

    #[test]
    fn trust_dialog_clear_option_confirms_before_clearing() {
        // 「Remove all saved trust decisions」：选中只弹 Yes/No 确认面板（不直接清空），
        // No/Esc 保留决策，Yes 才清空 trust.json。
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let _ad = crate::test_support::AgentDirGuard::temp();
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let agent_dir = settings_manager::agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("repo");
        let cwd = parent.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        core::project_trust::set_project_trust(&parent, &agent_dir, true);
        core::project_trust::set_project_trust(&cwd, &agent_dir, false);

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let select_clear = |app: &mut App| {
            app.open_project_trust_dialog();
            let idx = app
                .panel
                .filtered_items()
                .iter()
                .position(|i| i.value == "clear")
                .expect("面板应有清空选项");
            app.panel.top_mut().unwrap().selected = idx;
            app.handle_panel_key(&enter);
        };

        let mut app = App {
            cwd: cwd.to_string_lossy().to_string(),
            ..App::default()
        };

        // 选中 clear → 弹 Yes/No 确认面板，决策尚未变动
        select_clear(&mut app);
        assert_eq!(
            app.panel.top().map(|l| l.kind),
            Some(PanelKind::TrustClearConfirm),
            "应先弹确认面板"
        );
        assert!(
            core::project_trust::trust_decision(&cwd, &agent_dir).is_some(),
            "确认前不得清空"
        );

        // No → 面板关闭，决策保留
        app.panel.top_mut().unwrap().selected = 1;
        app.handle_panel_key(&enter);
        assert!(!app.panel.active, "确认后面板应关闭");
        assert_eq!(
            core::project_trust::trust_decision(&cwd, &agent_dir),
            Some((crate::utils::paths::normalize_cwd(&cwd), false)),
            "选 No 不得清空"
        );

        // 重走：Yes → 清空全部内容，所有路径回落默认
        select_clear(&mut app);
        assert_eq!(
            app.panel.top().map(|l| l.kind),
            Some(PanelKind::TrustClearConfirm)
        );
        app.panel.top_mut().unwrap().selected = 0;
        app.handle_panel_key(&enter);
        assert!(!app.panel.active);
        assert!(
            core::project_trust::trust_decision(&cwd, &agent_dir).is_none(),
            "Yes 应清空 trust.json"
        );
        assert!(
            core::project_trust::trust_decision(&parent, &agent_dir).is_none(),
            "父目录决策也应被清空"
        );
        let text = app
            .system_messages
            .last()
            .map(|(_, m)| sys_spans_text(&m.spans))
            .unwrap_or_default();
        assert!(
            text.contains("Cleared all saved trust decisions in")
                && text.contains("Restart prux for this to take effect."),
            "应提示已清空并重启生效: {text}"
        );
    }
}
