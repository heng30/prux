//! 会话文件完整性与崩溃残留处理：修复确认面板、未完成 operation 面板、/import 执行。
//!
//! 三块内容同属「会话文件能否安全恢复」：
//! - `stage_session_repair` / `resolve_session_repair`：文件中段损坏时的截断确认；
//! - `stage_zombie_operations` / `resolve_zombie_operations`：多个未关闭 operation 的恢复方式；
//! - `run_import`：/import 的实际执行（校验 → 必要时弹 cwd 确认 / 修复面板 → 切换会话）。
//!
//! 启动路径在事件循环起来之前用 [`stage_session_repair_global`] /
//! [`stage_zombie_operations_global`] 投递请求，首帧由 `take_staged_*` 取走弹面板。

use super::super::{
    agent_actor::AgentCommand,
    app::{App, MsgLevel, RepairAction, SessionRepairRequest, ZombieOperationsRequest},
    panel::{PanelItem, PanelKind},
};
use crate::{
    core::{
        session_manager::{self, Session},
        settings_manager,
    },
    utils::paths::{copy_no_overwrite, uniquify_import_destination},
};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

/// 启动路径在 TUI 事件循环起来之前 stage 的会话修复请求（main.rs 在 run_interactive 前调用）；
/// 事件循环首帧经 [`take_staged_session_repair`] 取出并弹修复面板。
static STAGED_SESSION_REPAIR: Mutex<Option<SessionRepairRequest>> = Mutex::new(None);

/// 启动路径 stage 的"存在多个未完成 operation"提示请求（事件循环首帧弹出选择面板）。
static STAGED_ZOMBIE_OPERATIONS: Mutex<Option<ZombieOperationsRequest>> = Mutex::new(None);

/// 记录一个待确认的会话修复请求（任何线程可用）。
pub fn stage_session_repair_global(req: SessionRepairRequest) {
    *STAGED_SESSION_REPAIR.lock().unwrap() = Some(req);
}

/// 取走 staged 修复请求（取走即消费，后续由面板确认）。
pub fn take_staged_session_repair() -> Option<SessionRepairRequest> {
    STAGED_SESSION_REPAIR.lock().unwrap().take()
}

/// 记录一个待确认的"未完成 operation"提示请求（任何线程可用）。
pub fn stage_zombie_operations_global(req: ZombieOperationsRequest) {
    *STAGED_ZOMBIE_OPERATIONS.lock().unwrap() = Some(req);
}

/// 取走 staged 的"未完成 operation"请求（取走即消费，后续由面板确认）。
pub fn take_staged_zombie_operations() -> Option<ZombieOperationsRequest> {
    STAGED_ZOMBIE_OPERATIONS.lock().unwrap().take()
}

impl App {
    /// Pop the session-repair confirmation panel（按档位展示两种修复路径）:
    /// 可抢救时给出「抢救（保留后缀）/ 截断（只留前缀）/ 取消」三项；
    /// 不可抢救（引用断裂等）只给「截断 / 取消」。
    pub(crate) fn stage_session_repair(&mut self, req: SessionRepairRequest) {
        let plan = &req.plan;
        let (title, items) = if plan.salvage_available() {
            let kept = plan
                .salvage_content
                .as_ref()
                .map(|c| c.lines().count())
                .unwrap_or(0);
            let title = format!(
                "Session file corrupted: salvage keeps {} lines (skip {} broken) / truncate keeps {} lines",
                kept, plan.salvaged_skipped, plan.valid_lines
            );
            let items = vec![
                PanelItem::new(
                    "Salvage: skip broken lines, keep suffix (recommended)".to_string(),
                    "salvage".to_string(),
                ),
                PanelItem::new(
                    format!("Truncate: keep prefix only ({} lines)", plan.valid_lines),
                    "truncate".to_string(),
                ),
                PanelItem::new("Cancel (no repair)".to_string(), "cancel".to_string()),
            ];
            (title, items)
        } else {
            let dropped = plan.dropped_lines();
            let title = format!(
                "Session file corrupted (unrecoverable suffix): keep {} lines / drop {} lines",
                plan.valid_lines, dropped
            );
            let items = vec![
                PanelItem::new(
                    format!("Truncate: keep prefix only ({} lines)", plan.valid_lines),
                    "truncate".to_string(),
                ),
                PanelItem::new("Cancel (no repair)".to_string(), "cancel".to_string()),
            ];
            (title, items)
        };
        self.panel.close();
        self.panel.open(PanelKind::SessionRepair, title, items);
        self.pending_session_repair = Some(req);
        self.dirty = true;
    }

    /// 修复面板确认：`salvage` = 抢救写回（跳过损坏行、保留后缀）；`truncate` = 截断写回；其余 = 取消。
    pub(crate) fn resolve_session_repair(&mut self, choice: &str) {
        let Some(req) = self.pending_session_repair.clone() else {
            return;
        };
        self.pending_session_repair = None;

        let use_salvage = match choice {
            "salvage" => true,
            "truncate" => false,
            _ => {
                self.push_msg("Session repair cancelled".to_string(), MsgLevel::Info);
                self.dirty = true;
                return;
            }
        };

        match Session::repair_write(&req.path.to_string_lossy(), &req.plan, use_salvage) {
            Ok(()) => {
                // 状态栏提示恢复中（不把文件路径打到消息流；回执 on_session_switched 会覆盖为 Resumed session）
                self.status = "Restoring session...".to_string();
                self.dirty = true;

                // 按修复后动作分发：Resume 直接打开；Fork 重新 fork 到指定目录
                match req.action {
                    RepairAction::Resume => {
                        self.worker.send(AgentCommand::ResumeSession {
                            path: req.path.to_string_lossy().to_string(),
                        });
                    }
                    RepairAction::Fork { dir } => {
                        self.worker.send(AgentCommand::ForkFrom {
                            path: req.path.to_string_lossy().to_string(),
                            dir,
                        });
                    }
                }
            }
            Err(e) => self.push_msg(format!("Session repair failed: {}", e), MsgLevel::Error),
        }
        self.dirty = true;
    }

    /// 修复面板取消（Esc）：放弃修复，保留文件原样。
    pub(crate) fn cancel_session_repair(&mut self) {
        let had_pending = self.pending_session_repair.take().is_some();
        self.cancel_with(
            had_pending,
            "Session repair cancelled, file left unchanged",
            MsgLevel::Info,
        );
    }

    /// 弹"未完成 operation"提示面板：告知会话里存在多个崩溃残留的未关闭操作，
    /// 并让用户选择恢复方式（Continue 默认只读容忍 / Rewrite 压平 / Cancel 不改动）。
    /// 面板**不阻塞恢复**——会话在检测时已按"取最新 operation"正常装配。
    pub(crate) fn stage_zombie_operations(&mut self, req: ZombieOperationsRequest) {
        let title = format!(
            "Session has {} unfinished operation(s) (from interrupted runs)",
            req.count
        );
        let stale = req.count.saturating_sub(1);
        let items = vec![
            PanelItem::new(
                "Continue: resume latest run, leave log as-is (recommended)".to_string(),
                "continue".to_string(),
            ),
            PanelItem::new(
                format!(
                    "Rewrite: mark {} older run(s) as finished (backup + append)",
                    stale
                ),
                "rewrite".to_string(),
            ),
            PanelItem::new(
                "Cancel: leave file unchanged".to_string(),
                "cancel".to_string(),
            ),
        ];
        self.panel.close();
        self.panel.open(PanelKind::ZombieOperations, title, items);
        self.pending_zombie_operations = Some(req);
        self.dirty = true;
    }

    /// 僵尸操作面板确认：`continue` = 只读容忍（默认，无动作）；
    /// `rewrite` = 下发压平命令（为旧 operation 补 operation_finished）；其余 = 取消（不改动）。
    pub(crate) fn resolve_zombie_operations(&mut self, choice: &str) {
        let Some(req) = self.pending_zombie_operations.clone() else {
            return;
        };

        self.pending_zombie_operations = None;
        let path = req.path.to_string_lossy().to_string();
        match choice {
            "continue" => {
                self.worker.send(AgentCommand::ResumeZombie {
                    path: path.clone(),
                    action: "continue".to_string(),
                });
                self.push_msg(
                    "Opening session with latest run; older interrupted runs left as-is"
                        .to_string(),
                    MsgLevel::Info,
                );
            }
            "rewrite" => {
                self.worker.send(AgentCommand::ResumeZombie {
                    path: path.clone(),
                    action: "rewrite".to_string(),
                });
                self.push_msg(
                    "Marking older interrupted runs as finished...".to_string(),
                    MsgLevel::Info,
                );
            }
            _ => {
                self.push_msg("Session left unopened".to_string(), MsgLevel::Info);
            }
        }
        self.dirty = true;
    }

    /// 僵尸操作面板取消（Esc）：不改动，不打开该会话。
    pub(crate) fn cancel_zombie_operations(&mut self) {
        let had_pending = self.pending_zombie_operations.take().is_some();
        self.cancel_with(had_pending, "Session left unopened", MsgLevel::Info);
    }

    /// 执行 /import：打开会话文件（open_checked 校验完整性），写回 agent 由 worker 完成。
    /// 中间行损坏 → 弹修复面板（确认后截断并 ResumeSession，语义同 import）。
    /// 源文件不在会话目录时复制进当前会话目录（basename），从副本恢复。
    /// 会话存储的 cwd 不存在 → 弹“Session cwd not found”确认框（继续在当前 cwd 恢复）。
    /// 返回是否已打开替换面板（cwd 确认 / 修复），调用方应跳过末尾 close。
    pub(crate) fn run_import(&mut self, source: PathBuf) -> bool {
        let opened_panel = match session_manager::Session::open_checked(&source.to_string_lossy()) {
            session_manager::OpenSessionResult::Ok(session) => {
                let agent_dir = settings_manager::agent_dir();
                let dir = session_manager::default_session_dir(&self.cwd, &agent_dir);
                let mut destination = source.clone();
                if let Some(name) = source.file_name() {
                    let candidate = dir.join(name);
                    if candidate != source {
                        if let Err(e) = std::fs::create_dir_all(&dir) {
                            self.push_msg(
                                format!("import failed: Failed to create session dir: {}", e),
                                MsgLevel::Error,
                            );
                            return false;
                        }

                        // 目标存在时递增编号 name-1.jsonl、name-2.jsonl…
                        // 避免静默覆盖同名已有会话；复制使用 O_EXCL 不覆盖语义作最后一道保险。
                        let dest = uniquify_import_destination(&dir, name);
                        if let Err(e) = copy_no_overwrite(&source, &dest) {
                            self.push_msg(
                                format!("import failed: Failed to copy session: {}", e),
                                MsgLevel::Error,
                            );
                            return false;
                        }
                        destination = dest;
                    }
                }
                // 会话存储的 cwd 与当前 cwd 不一致（含缺失为空）时弹确认框：
                // 缺失 → “Session cwd not found”文案沿用 pi；
                // 存在但不同 → “Session cwd mismatch”更准确的表述。
                let cwd_missing = session.cwd.is_empty() || !Path::new(&session.cwd).exists();
                let cwd_changed = cwd_missing || session.cwd != self.cwd;
                if cwd_changed {
                    self.stage_import_cwd_confirm(
                        destination,
                        if session.cwd.is_empty() {
                            "<unknown>".to_string()
                        } else {
                            session.cwd.clone()
                        },
                        cwd_missing,
                    );
                    true
                } else {
                    self.worker.send(AgentCommand::ResumeSession {
                        path: destination.to_string_lossy().into_owned(),
                    });
                    self.set_status_msg("Importing session...");
                    false
                }
            }
            session_manager::OpenSessionResult::Invalid(e) => {
                self.push_msg(format!("import failed: {}", e), MsgLevel::Error);
                false
            }
            session_manager::OpenSessionResult::Repairable { path, plan } => {
                self.stage_session_repair(SessionRepairRequest {
                    path,
                    plan,
                    action: RepairAction::Resume,
                });
                true
            }
        };
        self.dirty = true;
        opened_panel
    }

    /// 退出 TUI 时清理：当前会话为空（无用户消息）则删除会话文件。
    pub(crate) fn delete_current_session_if_empty(&mut self) {
        let Some(path) = self.current_session_path.clone() else {
            return;
        };
        match session_manager::Session::open_checked(&path) {
            session_manager::OpenSessionResult::Ok(s) if !s.has_user_messages() => {
                let _ = std::fs::remove_file(&path);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::provider::AgentMessage,
        modes::interactive::{
            app::{App, sys_spans_text},
            panel::Panel,
        },
    };

    #[test]
    fn run_import_never_overwrites_same_name_and_increments() {
        // /import 对齐 pi：复制不覆盖，同名冲突时递增 name-1.jsonl…，已有会话内容保持原样。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = crate::core::settings_manager::agent_dir();
        let cwd = dir.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let sess_dir = session_manager::default_session_dir(&cwd.to_string_lossy(), &agent_dir);
        std::fs::create_dir_all(&sess_dir).unwrap();

        // 目标目录已有一个同名的已有会话（内容标记为“旧”），覆盖写在 src_file 确定后。

        // 源会话（在另一个 cwd 下，cwd 不同会弹确认框，但复制已发生）。
        let orig = dir.path().join("orig");
        std::fs::create_dir_all(&orig).unwrap();
        let mut s = Session::create(
            orig.to_str().unwrap(),
            Some(dir.path().join("other-sessions")),
            true,
        )
        .unwrap();
        s.append_message(&AgentMessage::user_text("hello"));
        let src_file = s.get_session_file().unwrap().to_path_buf();
        drop(s);

        // 目标目录已有一个同名的已有会话（内容标记为“旧”）。
        let source_name = src_file.file_name().unwrap().to_os_string();
        let existing = sess_dir.join(&source_name);
        std::fs::write(&existing, "old-content").unwrap();

        let spilled_path = src_file.clone();

        let mut app = App {
            cwd: cwd.to_string_lossy().to_string(),
            ..App::default()
        };
        app.run_import(spilled_path.clone());

        // 同名文件未被覆盖，新副本递增为 -1。
        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            "old-content",
            "同名已有会话不能被覆盖"
        );
        let first = sess_dir.join(format!(
            "{}-1{}",
            source_name.to_string_lossy().trim_end_matches(".jsonl"),
            ".jsonl"
        ));
        assert!(
            first.exists(),
            "冲突时应生成 {}-1.jsonl",
            source_name.to_string_lossy()
        );
        // pending_import_cwd 指向的恢复副本是 -1。
        assert_eq!(
            app.pending_import_cwd
                .as_ref()
                .map(|p| p.session_cwd.as_str())
                .unwrap_or(""),
            orig.to_str().unwrap()
        );
        assert_eq!(
            app.pending_import_cwd
                .as_ref()
                .map(|p| p.destination.clone()),
            Some(first.clone())
        );

        // 再次导入同一源 → 递增到 -2。
        app.pending_import_cwd = None;
        app.panel = Panel::default();
        app.run_import(src_file);
        let second = sess_dir.join(format!(
            "{}-2{}",
            source_name.to_string_lossy().trim_end_matches(".jsonl"),
            ".jsonl"
        ));
        assert!(
            second.exists(),
            "再冲突时应生成 {}-2.jsonl",
            source_name.to_string_lossy()
        );
        assert_eq!(
            app.pending_import_cwd
                .as_ref()
                .map(|p| p.destination.clone()),
            Some(second)
        );
    }

    #[test]
    fn run_import_missing_file_reports_error() {
        // 确认 Yes 后源文件缺失 → 直接报错，不触碰 worker（open_checked 返回 Invalid）
        let mut app = App::default();
        app.run_import(std::path::PathBuf::from(
            "/tmp/definitely-missing-session.jsonl",
        ));
        let has_error = app.system_messages.iter().any(|(_, m)| {
            m.level == MsgLevel::Error
                && sys_spans_text(&m.spans).contains("Failed to read session file")
        });
        assert!(has_error, "缺失文件应报 Failed to read session file");
    }

    #[test]
    fn delete_current_session_if_empty_removes_empty_keeps_nonempty() {
        // 退出 TUI：当前会话为空（无用户消息）则删除；有消息保留
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        let empty = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        let empty_path = empty
            .get_session_file()
            .unwrap()
            .to_path_buf()
            .to_string_lossy()
            .to_string();
        drop(empty);
        let mut full = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        full.append_message(&AgentMessage::user_text("hi"));
        let full_path = full
            .get_session_file()
            .unwrap()
            .to_path_buf()
            .to_string_lossy()
            .to_string();
        drop(full);

        let mut app = App {
            current_session_path: Some(empty_path.clone()),
            ..App::default()
        };
        app.delete_current_session_if_empty();
        assert!(
            !std::path::Path::new(&empty_path).exists(),
            "空会话退出时应被删除"
        );

        app.current_session_path = Some(full_path.clone());
        app.delete_current_session_if_empty();
        assert!(
            std::path::Path::new(&full_path).exists(),
            "有用户消息的会话应保留"
        );
    }
}
