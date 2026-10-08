//! 工作流完成卡片。
//!
//! 卡片是 run **唯一持久**的回顾面：run 注册表在会话切换时清空，覆盖层里的树也随之消失，
//! 而卡片走 `PersistSessionEntry` 落盘、`/resume` 后由 `on_session_switched` 重放。
//! 因此它必须带上最终那棵树里最要紧的信息：**每个阶段跑成没跑成**（`✓ 3/3` / `✗ 2/3`），
//! 以及**失败的那几行连同原因**。
//!
//! 卡片仍是纯函数：`card_payload` 把渲染需要的东西序列化成 JSON，`render_card` 只读它——
//! 于是实时投递与会话重放走的是同一条代码路径。

use super::{
    super::super::subagent::notify,
    progress::{self, DisplayState},
    task::WorkflowRun,
};
use crate::{
    core::extensions::RichSpan,
    utils::{
        glyphs::{DEF_DONE, DEF_FAILED, DEF_MIDDOT, DEF_RUNNING, DEF_STOPPED},
        time::format_duration,
    },
};
use serde_json::{Value, json};

/// 卡片的自定义类型
pub const WORKFLOW_CARD_TYPE: &str = "subagent-workflow";

/// 结果预览的字符上限
const PREVIEW_MAX_CHARS: usize = 2000;

/// 组装卡片载荷（纯 JSON，可落盘 / 重放）。
pub(crate) fn card_payload(run: &WorkflowRun) -> Value {
    let stats = run.stats();
    let groups = run.groups();
    let logs = progress::collapse(&progress::parse_entries(&run.log())).logs;
    let preview = run.result_text();
    let preview: String = preview.chars().take(PREVIEW_MAX_CHARS).collect();

    let groups_json: Vec<Value> = groups
        .iter()
        .map(|g| {
            // 只有失败/被切断的行需要出现在卡片里：那是"哪一步没成"的答案。
            let failures: Vec<Value> = g
                .agents
                .iter()
                .filter_map(|a| {
                    let state = progress::display_state(a, false);
                    if !matches!(
                        state,
                        DisplayState::Failed | DisplayState::Blocked | DisplayState::Interrupted
                    ) {
                        return None;
                    }
                    Some(json!({
                        "label": a.label,
                        "state": state.as_str(),
                        "error": a.error,
                    }))
                })
                .collect();

            json!({
                "title": g.title,
                "status": g.status.as_str(),
                "done": g.done,
                "failed": g.failed,
                "total": g.total,
                "tokens": g.tokens,
                "duration": format_duration(g.duration_ms),
                "failures": failures,
            })
        })
        .collect();

    json!({
        "id": run.id,
        "name": run.name,
        "description": run.description,
        "status": run.status.as_str(),
        "agentCount": run.agent_count,
        "done": stats.done,
        "failed": stats.failed,
        "total": stats.total.max(run.agent_count as usize),
        "tokens": run.total_tokens(),
        "duration": format_duration(run.elapsed_ms()),
        "groups": groups_json,
        "logs": logs,
        "preview": preview,
    })
}

/// 渲染卡片（`data` 可能来自实时投递或会话重放）。
pub(crate) fn render_card(data: &Value) -> Vec<RichSpan> {
    let s = |k: &str| data.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let n = |k: &str| data.get(k).and_then(|v| v.as_u64()).unwrap_or(0);

    let status = s("status");
    let (icon, color) = match status {
        "completed" => (DEF_DONE, "success"),
        "killed" => (DEF_STOPPED, "warning"),
        _ => (DEF_FAILED, "error"),
    };

    let mut spans = vec![RichSpan::fg(
        color,
        format!(
            "{icon} workflow {} · {status} · {}/{} agents · {} · {} tok",
            s("name"),
            n("done"),
            n("total"),
            s("duration"),
            n("tokens"),
        ),
    )];

    if !s("description").is_empty() {
        spans.push(RichSpan::plain(format!("\n{}", s("description"))));
    }

    // 阶段树：一行一个阶段（`✓ Scan 3/3 · 1.2k tok · 12s`），失败的行单独列出来。
    if let Some(groups) = data.get("groups").and_then(|v| v.as_array()) {
        for g in groups {
            let gs = |k: &str| g.get(k).and_then(|v| v.as_str()).unwrap_or("");
            let gn = |k: &str| g.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            let (gicon, gcolor) = match gs("status") {
                "done" => (DEF_DONE, "success"),
                "failed" => (DEF_FAILED, "error"),
                "running" => (DEF_RUNNING, "accent"),
                _ => (DEF_MIDDOT, "dim"),
            };

            let mut line = format!("\n  {gicon} {}", gs("title"));
            if gs("status") == "not-started" {
                line.push_str(" · not started");
            } else {
                line.push_str(&format!(" {}/{}", gn("done"), gn("total")));
                if gn("tokens") > 0 {
                    line.push_str(&format!(" · {} tok", notify::format_tokens(gn("tokens"))));
                }

                let d = gs("duration");
                if d != "0ms" {
                    line.push_str(&format!(" · {d}"));
                }
            }

            spans.push(RichSpan::fg(gcolor, line));

            if let Some(failures) = g.get("failures").and_then(|v| v.as_array()) {
                for f in failures {
                    let label = f.get("label").and_then(|v| v.as_str()).unwrap_or("agent");
                    let state = f.get("state").and_then(|v| v.as_str()).unwrap_or("failed");
                    let mut text = format!("\n      {state} {label}");

                    if let Some(err) = f.get("error").and_then(|v| v.as_str())
                        && !err.is_empty()
                    {
                        let err: String = err.chars().take(120).collect();
                        text.push_str(&format!(": {err}"));
                    }

                    spans.push(RichSpan::fg("error", text));
                }
            }
        }
    }

    if let Some(logs) = data.get("logs").and_then(|v| v.as_array()) {
        for log in logs.iter().filter_map(|l| l.as_str()).take(8) {
            spans.push(RichSpan::fg("dim", format!("\n  · {log}")));
        }
    }

    let preview = s("preview");
    if !preview.is_empty() {
        spans.push(RichSpan::plain(format!("\n{preview}")));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::subagent::workflow::task::{self, RunStatus};

    fn text(spans: &[RichSpan]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn payload_and_render_agree_including_failures() {
        let _g = super::super::super::test_lock();
        task::reset_all();
        let id = task::new_id();
        let _abort = task::register(&id, "demo", "find flaky tests", None, None, None);
        task::set_agent_count(&id, 3);
        let log = task::find(&id).unwrap().progress.clone();
        {
            let mut log = log.lock().unwrap();
            log.push(json!({ "type": "workflow_phase", "index": 0, "title": "Scan" }));
            log.push(json!({
                "type": "workflow_agent", "index": 0, "label": "scan a", "state": "done",
                "phaseIndex": 0, "tokens": 500, "startedAt": 1, "lastProgressAt": 11
            }));
            log.push(json!({
                "type": "workflow_agent", "index": 1, "label": "scan b", "state": "error",
                "phaseIndex": 0, "error": "provider error"
            }));
            log.push(json!({ "type": "workflow_log", "message": "audited 2 files" }));
        }
        task::finish(&id, RunStatus::Completed, Some(json!({ "ok": true })), None);
        let run = task::find(&id).unwrap();
        let payload = card_payload(&run);
        assert_eq!(payload["name"], "demo");
        assert_eq!(payload["total"], 3);
        assert_eq!(payload["done"], 1);
        assert_eq!(payload["failed"], 1);
        assert_eq!(payload["groups"][0]["title"], "Scan");
        assert_eq!(payload["groups"][0]["failures"][0]["label"], "scan b");
        assert_eq!(
            payload["groups"][0]["failures"][0]["error"],
            "provider error"
        );

        let rendered = text(&render_card(&payload));
        assert!(rendered.contains("workflow demo"), "{rendered}");
        assert!(rendered.contains("1/3 agents"), "{rendered}");
        assert!(
            rendered.contains("✗ Scan 1/2"),
            "阶段行按 done/total：{rendered}"
        );
        assert!(rendered.contains("Scan"), "{rendered}");
        assert!(
            rendered.contains("failed scan b: provider error"),
            "{rendered}"
        );
        assert!(rendered.contains("audited 2 files"), "{rendered}");
        assert!(rendered.contains("\"ok\": true"), "{rendered}");
        task::reset_all();
    }

    #[test]
    fn killed_run_renders_as_stopped() {
        let _g = super::super::super::test_lock();
        task::reset_all();
        let id = task::new_id();
        let _abort = task::register(&id, "demo", "d", None, None, None);
        task::set_agent_count(&id, 2);
        let log = task::find(&id).unwrap().progress.clone();
        // 一条已终结、一条被切断
        log.lock().unwrap().push(json!({
            "type": "workflow_agent", "index": 0, "label": "a", "state": "done"
        }));
        log.lock().unwrap().push(json!({
            "type": "workflow_agent", "index": 1, "label": "b", "state": "progress",
            "queuedAt": 1, "startedAt": 2
        }));
        task::finish(
            &id,
            RunStatus::Killed,
            None,
            Some("workflow aborted".into()),
        );
        let payload = card_payload(&task::find(&id).unwrap());
        let rendered = text(&render_card(&payload));
        assert!(rendered.contains("killed"), "{rendered}");
        assert!(
            rendered.contains("interrupted b"),
            "被切断的行要标出来：{rendered}"
        );
        assert!(rendered.contains("workflow aborted"), "{rendered}");
        task::reset_all();
    }
}
