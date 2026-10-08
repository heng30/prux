//! 确认面板族：/import、import 的 cwd 确认、settings.json 覆写、/history 删除。
//!
//! 四族同构：`stage_/open_*` 建面板并存 pending 载荷 → `resolve_*(choice)` 按选项执行
//! → `cancel_*` 复原（不写盘/不改数据）。公共部分收敛为 [`App::open_confirm`]（纯选项面板）
//! 与 [`App::cancel_with`]（清 pending + 一句提示 + 置脏）。
//!
//! 载荷类型（`PendingImport` / `PendingImportCwd` / `PendingSettingsOverwrite` /
//! `PendingHistoryClear`）与 `App` 字段同处 app.rs；真正的 /import 执行在
//! [`App::run_import`]。

use super::super::{
    agent_actor::AgentCommand,
    app::{
        App, MsgLevel, PendingHistoryClear, PendingImport, PendingImportCwd,
        PendingSettingsOverwrite,
    },
    panel::{PanelItem, PanelKind},
};
use crate::core::{prompt_history, settings_manager};
use std::path::PathBuf;

/// Yes/No 确认项的公共定义（/import、import cwd、/history 三族共用）
const YES_NO: &[(&str, &str)] = &[("Yes", "yes"), ("No", "no")];

impl App {
    /// 打开一个「纯选项」确认面板（条目只含 label/value）：关旧面板 → 开新面板 → 置脏。
    pub(crate) fn open_confirm(
        &mut self,
        kind: PanelKind,
        title: impl Into<String>,
        items: &[(&str, &str)],
    ) {
        let items = items
            .iter()
            .map(|(label, value)| PanelItem::new(*label, *value))
            .collect();
        self.panel.close();
        self.panel.open(kind, title.into(), items);
        self.dirty = true;
    }

    /// 取消确认面板的公共尾巴：确有 pending 才提示并置脏。
    pub(crate) fn cancel_with(&mut self, had_pending: bool, msg: &str, level: MsgLevel) {
        if !had_pending {
            return;
        }
        self.push_msg(msg.to_string(), level);
        self.dirty = true;
    }

    /// /import 弹确认面板（替换当前会话前 Yes/No 确认）。
    pub(crate) fn stage_import_confirm(&mut self, source: PathBuf) {
        self.open_confirm(PanelKind::ImportConfirm, "Import session", YES_NO);
        self.pending_import = Some(PendingImport { source });
    }
    /// /import 确认面板选择：`yes` = 执行导入；其余 = 取消。
    /// 返回是否已打开替换面板（cwd 不一致确认 / 修复面板），调用方应跳过末尾 close。
    pub(crate) fn resolve_import_confirm(&mut self, choice: &str) -> bool {
        let Some(pending) = self.pending_import.take() else {
            return false;
        };
        let PendingImport { source } = pending;
        if choice != "yes" {
            self.push_msg("Import cancelled".to_string(), MsgLevel::Info);
            self.dirty = true;
            return false;
        }
        self.run_import(source)
    }

    /// /import 确认面板取消（Esc/Ctrl+C）：放弃导入，保持当前会话。
    pub(crate) fn cancel_import_confirm(&mut self) {
        let had_pending = self.pending_import.take().is_some();
        self.cancel_with(had_pending, "Import cancelled", MsgLevel::Info);
    }

    /// /import 会话 cwd 不一致：弹确认框。
    /// 原 cwd 缺失 → 标题 “Session cwd not found”（沿用 pi 文案）；
    /// 存在但不同 → 标题 “Session cwd mismatch”。
    /// Yes = 继续在当前 cwd 恢复；No = 取消导入。
    pub(crate) fn stage_import_cwd_confirm(
        &mut self,
        destination: PathBuf,
        session_cwd: String,
        cwd_missing: bool,
    ) {
        let title = if cwd_missing {
            "Session cwd not found"
        } else {
            "Session cwd mismatch"
        };
        self.open_confirm(PanelKind::ImportCwdConfirm, title, YES_NO);
        self.pending_import_cwd = Some(PendingImportCwd {
            destination,
            session_cwd,
            cwd_missing,
        });
        self.dirty = true;
    }

    /// /import 会话 cwd 缺失确认：`yes` = 继续在当前 cwd 恢复；其余 = 取消。
    pub(crate) fn resolve_import_cwd_confirm(&mut self, choice: &str) {
        let Some(pending) = self.pending_import_cwd.take() else {
            return;
        };
        let PendingImportCwd {
            destination,
            session_cwd: _,
            cwd_missing: _,
        } = pending;
        if choice != "yes" {
            self.push_msg("Import cancelled".to_string(), MsgLevel::Info);
            self.dirty = true;
            return;
        }
        self.worker.send(AgentCommand::ResumeSession {
            path: destination.to_string_lossy().into_owned(),
        });
        self.set_status_msg("Importing session...");
        self.dirty = true;
    }

    /// /import 会话 cwd 缺失确认取消（Esc/Ctrl+C）：放弃导入。
    pub(crate) fn cancel_import_cwd_confirm(&mut self) {
        let had_pending = self.pending_import_cwd.take().is_some();
        self.cancel_with(had_pending, "Import cancelled", MsgLevel::Info);
    }

    /// settings.json 损坏时弹覆写确认面板（Overwrite / Cancel，无输入框）。
    pub(crate) fn stage_settings_overwrite(&mut self, path: String, detail: String) {
        self.open_confirm(
            PanelKind::SettingsOverwrite,
            "settings.json is invalid",
            &[("Overwrite", "overwrite"), ("Cancel", "cancel")],
        );
        self.pending_settings_overwrite = Some(PendingSettingsOverwrite { path, detail });
    }

    /// 覆写确认面板选择：`overwrite` = 强制写入暂存内容；其余 = 取消（保留原文件）。
    pub(crate) fn resolve_settings_overwrite(&mut self, choice: &str) {
        let Some(pending) = self.pending_settings_overwrite.take() else {
            return;
        };
        if choice == "overwrite" {
            match settings_manager::confirm_settings_overwrite() {
                Ok(()) => self.push_msg(
                    format!("Overwrote invalid settings file {}", pending.path),
                    MsgLevel::Info,
                ),
                Err(e) => self.push_msg(format!("failed to save settings: {}", e), MsgLevel::Error),
            }
        } else {
            settings_manager::cancel_settings_overwrite();
            self.push_msg(
                format!("Settings not saved: {} is not valid JSON", pending.path),
                MsgLevel::Warning,
            );
        }
        self.dirty = true;
    }

    /// 覆写确认面板取消（Esc/Ctrl+C）：保留损坏文件，不写入。
    pub(crate) fn cancel_settings_overwrite(&mut self) {
        let had_pending = self.pending_settings_overwrite.take().is_some();
        if had_pending {
            settings_manager::cancel_settings_overwrite();
        }
        self.cancel_with(had_pending, "Settings not saved", MsgLevel::Warning);
    }

    /// `/history @clear` / `@clear-all`：打开 Yes/No 确认面板。
    ///
    /// 无文件可删时不弹窗，只给一句友好提示（否则用户会对着一个空确认框发楞）。
    /// `detail` 预先算好真实计数：这是「不可恢复」操作唯一能给的预警。
    pub(crate) fn open_history_clear_confirm(&mut self, all: bool) {
        let (title, detail) = if all {
            let (files, entries) = prompt_history::all_summary();
            if files == 0 {
                self.push_msg(
                    "No prompt history files to clear".to_string(),
                    MsgLevel::Info,
                );
                self.dirty = true;
                return;
            }

            (
                "Clear ALL prompt history".to_string(),
                format!(
                    "{} files · {} entries (every project)\n{}",
                    files,
                    entries,
                    prompt_history::history_dir().display()
                ),
            )
        } else {
            match prompt_history::project_summary(&self.cwd) {
                Some((name, entries)) => (
                    "Clear prompt history for this project".to_string(),
                    format!(
                        "{} · {} entries\n{}",
                        name,
                        entries,
                        prompt_history::history_path(&self.cwd).display()
                    ),
                ),
                None => {
                    self.push_msg(
                        "No prompt history for this project".to_string(),
                        MsgLevel::Info,
                    );
                    self.dirty = true;
                    return;
                }
            }
        };

        self.open_confirm(PanelKind::HistoryClearConfirm, title, YES_NO);
        self.pending_history_clear = Some(PendingHistoryClear { all, detail });
    }

    /// 确认面板选择：`yes` = 执行删除；其余 = 取消。
    ///
    /// 删完同步清空内存列表（`clear_history`）：已不存在的条目不能在 `/history`
    /// 与 ↑/↓ 里继续出现（与「改档位即时截断」同一个「不自相矛盾」原则）。
    pub(crate) fn resolve_history_clear_confirm(&mut self, choice: &str) {
        let Some(pending) = self.pending_history_clear.take() else {
            return;
        };

        if choice != "yes" {
            self.push_msg(
                "Clearing prompt history cancelled".to_string(),
                MsgLevel::Info,
            );
            self.dirty = true;
            return;
        }

        let done = if pending.all {
            prompt_history::clear_all()
        } else {
            prompt_history::clear_project(&self.cwd)
        };
        self.editor.clear_history();

        let mut msg = if pending.all {
            format!(
                "Cleared all prompt history ({} files, {} entries)",
                done.files, done.entries
            )
        } else {
            format!(
                "Cleared prompt history for this project ({} entries)",
                done.entries
            )
        };

        if done.removed_dir {
            msg.push_str(" · removed empty history directory");
        }

        self.push_msg(msg, MsgLevel::Info);
        self.dirty = true;
    }

    /// 确认面板取消（Esc/Ctrl+C）：保留历史，什么都不删。
    pub(crate) fn cancel_history_clear_confirm(&mut self) {
        let had_pending = self.pending_history_clear.take().is_some();
        self.cancel_with(
            had_pending,
            "Clearing prompt history cancelled",
            MsgLevel::Info,
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::modes::interactive::panel::PanelKind;
    use crate::{
        core::{provider::AgentMessage, session_manager::Session},
        modes::interactive::app::App,
    };

    #[test]
    fn import_confirm_stage_no_and_cancel() {
        // /import 确认面板：stage 打开 Yes/No 面板，No/Esc 取消并保留 pending 清理
        let mut app = App::default();
        let src = std::path::PathBuf::from("/tmp/fake-session.jsonl");
        app.stage_import_confirm(src.clone());
        assert!(app.panel.active);
        assert_eq!(
            app.panel.top().map(|l| l.kind),
            Some(PanelKind::ImportConfirm)
        );
        assert!(app.pending_import.is_some());
        let items = app.panel.filtered_items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "Yes");
        assert_eq!(items[1].label, "No");
        // 选 No → 取消，pending 清空
        app.resolve_import_confirm("no");
        assert!(app.pending_import.is_none());
        // 重新 stage 后 Esc 取消 → 同样清空
        app.stage_import_confirm(src);
        app.cancel_import_confirm();
        assert!(app.pending_import.is_none());
    }

    #[test]
    fn settings_overwrite_panel_stage_confirm_and_cancel() {
        // settings.json 损坏覆写确认面板：stage 打开 Overwrite/Cancel（无输入框），
        // Overwrite 经 core 强制写盘，Cancel/Esc 保留原文件。
        // 进程级 pending 槽需串行（生产跨 worker/UI 线程，测试并行会互踩）。
        let _serial = crate::test_support::SETTINGS_OVERWRITE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _ui = crate::test_support::SettingsUiGuard::set(true);
        let mut app = App::default();

        // Cancel：面板清空、不写盘
        app.stage_settings_overwrite(
            "/tmp/settings.json".to_string(),
            "/tmp/settings.json is not valid JSON".to_string(),
        );
        assert!(app.panel.active);
        assert_eq!(
            app.panel.top().map(|l| l.kind),
            Some(PanelKind::SettingsOverwrite)
        );
        assert!(app.pending_settings_overwrite.is_some());
        let items = app.panel.filtered_items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "Overwrite");
        assert_eq!(items[1].label, "Cancel");
        app.cancel_settings_overwrite();
        assert!(app.pending_settings_overwrite.is_none());

        // Overwrite：core 暂存 → App 确认 → 写盘（覆写损坏文件）
        let path = crate::core::settings_manager::agent_dir().join("settings.json");
        std::fs::write(&path, "{invalid").unwrap();
        crate::core::settings_manager::write_theme("dark").unwrap();
        let (p, d) = crate::core::settings_manager::pending_settings_overwrite().unwrap();
        app.stage_settings_overwrite(p, d);
        app.resolve_settings_overwrite("overwrite");
        assert_eq!(
            crate::core::settings_manager::read_settings()
                .theme
                .as_deref(),
            Some("dark")
        );
        assert!(app.pending_settings_overwrite.is_none());
        assert!(crate::core::settings_manager::pending_settings_overwrite().is_none());
    }

    #[test]
    fn import_cwd_missing_stages_confirm_and_cancel() {
        // 对齐 pi assertSessionCwdExists：会话存储的 cwd 与当前 cwd 不一致（含缺失）→ 弹
        // “Session cwd not found”确认框；No/Esc 取消，Yes 后从副本恢复。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let ghost = dir.path().join("ghost-cwd"); // 不存在的 cwd
        let sess_dir = dir.path().join("sessions");
        let mut s = Session::create(ghost.to_str().unwrap(), Some(sess_dir.clone()), true).unwrap();
        s.append_message(&AgentMessage::user_text("hello"));
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);

        let mut app = App::default();
        std::fs::create_dir_all(dir.path().join("other")).unwrap(); // 存在但与 app.cwd 不同
        app.cwd = dir.path().join("current").to_string_lossy().to_string();
        app.run_import(path.clone());
        assert!(
            app.panel.active
                && app.panel.top().map(|l| l.kind) == Some(PanelKind::ImportCwdConfirm),
            "cwd 缺失应弹确认框"
        );
        assert!(app.pending_import_cwd.is_some());
        let items = app.panel.filtered_items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "Yes");
        assert_eq!(items[1].label, "No");
        // 缺失场景：标题沿用 pi，cwd_missing = true
        assert_eq!(
            app.panel.top().map(|l| l.title.as_str()),
            Some("Session cwd not found")
        );
        assert!(app.pending_import_cwd.as_ref().unwrap().cwd_missing);
        // No → 取消
        app.resolve_import_cwd_confirm("no");
        assert!(app.pending_import_cwd.is_none());
        // 重新触发后 Esc 取消 → 同样清空
        app.run_import(path);
        app.cancel_import_cwd_confirm();
        assert!(app.pending_import_cwd.is_none());
    }

    #[test]
    fn import_cwd_exists_but_differs_stages_confirm() {
        // cwd 存在但与会话原目录不同 → 同样弹确认框（目录不同也弹框）
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let orig = dir.path().join("original");
        std::fs::create_dir_all(&orig).unwrap();
        let sess_dir = dir.path().join("sessions");
        let mut s = Session::create(orig.to_str().unwrap(), Some(sess_dir.clone()), true).unwrap();
        s.append_message(&AgentMessage::user_text("hi"));
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);

        let mut app = App::default();
        let current = dir.path().join("current");
        std::fs::create_dir_all(&current).unwrap();
        app.cwd = current.to_string_lossy().to_string();
        app.run_import(path);
        assert!(
            app.panel.active
                && app.panel.top().map(|l| l.kind) == Some(PanelKind::ImportCwdConfirm),
            "cwd 不同也应弹确认框"
        );
        assert!(app.pending_import_cwd.is_some());
        let expected = app
            .pending_import_cwd
            .as_ref()
            .map(|p| p.session_cwd.as_str())
            .unwrap_or("");
        assert_eq!(expected, orig.to_str().unwrap());
        // 不同场景：标题改用 Session cwd mismatch，cwd_missing = false
        assert_eq!(
            app.panel.top().map(|l| l.title.as_str()),
            Some("Session cwd mismatch")
        );
        assert!(!app.pending_import_cwd.as_ref().unwrap().cwd_missing);
        // Yes → 清空 pending（worker 无接收者，send 静默失败，不 panic）
        app.resolve_import_cwd_confirm("yes");
        assert!(app.pending_import_cwd.is_none());
    }

    #[test]
    fn import_cwd_same_skips_confirm() {
        // cwd 与当前目录一致 → 不弹确认框，直接恢复
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap().to_string();
        let sess_dir = dir.path().join("sessions");
        let mut s = Session::create(&cwd, Some(sess_dir.clone()), true).unwrap();
        s.append_message(&AgentMessage::user_text("hi"));
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);

        let mut app = App {
            cwd,
            ..App::default()
        };
        app.run_import(path);
        assert!(!app.panel.active, "cwd 一致不应弹确认框");
        assert!(app.pending_import_cwd.is_none());
    }
}
