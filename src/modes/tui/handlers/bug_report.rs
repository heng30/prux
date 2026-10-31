//! `/bug` 交互流程：描述 → 是否附带 transcript → （不附则）是否生成摘要 → 导出/取消。
//!
//! 用模态面板逐级推进（每级一个 `panel.open`，Esc 即取消整个流程）。收集与打包逻辑全在 [`crate::core::bug_report`]，
//! 真正的执行（可能含模型摘要调用）由 worker 侧 [`super::super::agent_actor::AgentCommand::BugReport`] 完成。
//!
//! 无 Radius 网关，故不提供上传，只导出 zip 到当前目录。

use super::super::{
    agent_actor::AgentCommand,
    app::{App, MsgLevel, PendingBugReport, WorkingKind},
    panel::{PanelItem, PanelKind},
};

/// 面板首行的免责说明
const DISCLAIMER: &str = concat!(
    "This report is written to a zip archive in the current directory and is not uploaded anywhere. It includes your ",
    env!("CARGO_PKG_NAME"),
    " version, operating system, the current model and provider configuration (without API keys), loaded extensions, settings, and provider error diagnostics from this session."
);

/// transcript 面板说明
const TRANSCRIPT_NOTE: &str = "The transcript contains your messages, model output, tool calls and their results, including file contents and command output read during this session.";

/// 摘要面板说明
const SUMMARY_NOTE: &str = "The transcript is sent to your current provider with your credentials and tokens. Only the generated summary is attached; the transcript stays on your machine.";

/// Yes/No 面板的命中值（与 `handlers/confirms.rs` 的 YES_NO 同形）
const YES: &str = "yes";
/// 「是否附带 transcript」面板的选项（附带 / 不附带）。
const TRANSCRIPT_ITEMS: &[(&str, &str)] = &[("Yes, include the transcript", "yes"), ("No", "no")];
/// 「是否用当前模型生成摘要」面板的选项（生成 / 不生成）。
const SUMMARY_ITEMS: &[(&str, &str)] = &[("Yes, generate a summary", "yes"), ("No", "no")];
/// 最后一步交付面板的选项（导出 zip / 取消整个流程）。
const DELIVERY_ITEMS: &[(&str, &str)] = &[("Export as Zip", "zip"), ("Cancel", "cancel")];

/// 面板 → 条目（面板族共用的条目构造）
pub(crate) fn choice_items(items: &[(&str, &str)]) -> Vec<PanelItem> {
    items
        .iter()
        .map(|(label, value)| PanelItem::new(*label, *value))
        .collect()
}

impl App {
    /// `/bug [description]`：打开描述输入面板并初始化流程状态。
    pub(super) fn cmd_bug(&mut self, raw: &str) {
        let hint = raw.trim_start_matches("bug").trim().to_string();
        self.pending_bug_report = Some(PendingBugReport {
            hint: hint.clone(),
            include_session: false,
            include_summary: false,
        });
        self.open_bug_hint_panel(&hint);
    }

    /// 描述输入面板（filter 行作为输入框，预填 `/bug` 参数）
    fn open_bug_hint_panel(&mut self, hint: &str) {
        self.panel.close();
        self.panel.open(
            PanelKind::BugReportHint,
            "Report a bug — what went wrong? (optional)".to_string(),
            Vec::new(),
        );
        if let Some(layer) = self.panel.top_mut() {
            layer.filter.set_value(hint);
        }
        self.dirty = true;
    }

    /// 描述面板 Enter：记下描述，进入「是否附带 transcript」。
    pub(super) fn confirm_bug_report_hint(&mut self) {
        let Some(pending) = self.pending_bug_report.as_mut() else {
            self.panel.close();
            self.dirty = true;
            return;
        };
        if let Some(layer) = self.panel.top() {
            pending.hint = layer.filter.value.trim().to_string();
        }
        self.open_bug_choice_panel(
            PanelKind::BugReportTranscript,
            "Include the session transcript?",
            TRANSCRIPT_ITEMS,
        );
    }

    /// transcript 面板 Enter：yes → 导出；no → 询问是否生成摘要。
    pub(super) fn confirm_bug_report_transcript(&mut self, value: &str) {
        let Some(pending) = self.pending_bug_report.as_mut() else {
            self.panel.close();
            self.dirty = true;
            return;
        };
        pending.include_session = value == YES;

        if pending.include_session {
            self.open_bug_choice_panel(PanelKind::BugReportDelivery, "Bug report", DELIVERY_ITEMS);
        } else {
            self.open_bug_choice_panel(
                PanelKind::BugReportSummary,
                "Attach a summary written by the current model instead?",
                SUMMARY_ITEMS,
            );
        }
    }

    /// 摘要面板 Enter → 导出面板。
    pub(super) fn confirm_bug_report_summary(&mut self, value: &str) {
        let Some(pending) = self.pending_bug_report.as_mut() else {
            self.panel.close();
            self.dirty = true;
            return;
        };

        pending.include_summary = value == YES;
        self.open_bug_choice_panel(PanelKind::BugReportDelivery, "Bug report", DELIVERY_ITEMS);
    }

    /// 导出面板 Enter：zip = 交给 worker 构建并写盘；cancel = 取消。
    pub(super) fn confirm_bug_report_delivery(&mut self, value: &str) {
        let Some(pending) = self.pending_bug_report.take() else {
            self.panel.close();
            self.dirty = true;
            return;
        };

        self.panel.close();

        if value != "zip" {
            self.push_msg("Bug report cancelled".to_string(), MsgLevel::Info);
            self.dirty = true;
            return;
        }

        // 忙碌态：摘要生成是模型调用，worker 完成后 on_bug_report_done 复位
        self.busy = true;
        self.working_kind = WorkingKind::BugReport;
        self.clear_status();
        self.worker.send(AgentCommand::BugReport {
            hint: pending.hint,
            include_session: pending.include_session,
            include_summary: pending.include_summary,
        });
        self.dirty = true;
    }

    /// 取消整个 `/bug` 流程（Esc/Ctrl+C 或选择 Cancel）。
    pub(super) fn cancel_bug_report(&mut self) {
        let had = self.pending_bug_report.take().is_some();
        self.cancel_with(had, "Bug report cancelled", MsgLevel::Info);
    }

    /// 打开一个 bug 流程选项面板（条目 + 说明由渲染层从 pending 状态取）。
    fn open_bug_choice_panel(&mut self, kind: PanelKind, title: &str, items: &[(&str, &str)]) {
        self.panel.close();
        self.panel
            .open(kind, title.to_string(), choice_items(items));
        self.dirty = true;
    }

    /// 选项面板的说明文本（渲染层用；按当前 pending 状态合成）。
    pub(crate) fn bug_report_note(&self) -> String {
        let Some(pending) = self.pending_bug_report.as_ref() else {
            return String::new();
        };

        match self.panel.top().map(|l| l.kind) {
            Some(PanelKind::BugReportHint) => DISCLAIMER.to_string(),
            Some(PanelKind::BugReportSummary) => SUMMARY_NOTE.to_string(),
            Some(PanelKind::BugReportDelivery) => format!(
                "Description: {}\nTranscript: {}\nSummary: {}\n\nThe archive is written to the current directory.",
                if pending.hint.is_empty() {
                    "none"
                } else {
                    pending.hint.as_str()
                },
                if pending.include_session {
                    "included"
                } else {
                    "not included"
                },
                if pending.include_summary {
                    "written by the current model"
                } else {
                    "none"
                },
            ),
            _ => TRANSCRIPT_NOTE.to_string(),
        }
    }
}

/// `/bug` 流程用到的面板种类判定（事件处理与渲染共用，避免名单漂移）
pub(crate) fn is_bug_report_panel(kind: PanelKind) -> bool {
    matches!(
        kind,
        PanelKind::BugReportHint
            | PanelKind::BugReportTranscript
            | PanelKind::BugReportSummary
            | PanelKind::BugReportDelivery
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_items_carry_label_and_value() {
        let items = choice_items(DELIVERY_ITEMS);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "Export as Zip");
        assert_eq!(items[0].value, "zip");
        assert_eq!(items[1].value, "cancel");
    }
}
