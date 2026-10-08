//! 工作流运行完成的**模型向**通知。
//!
//! 与子代理完成通知同一个批处理通道（`manager::BatchItem::Rendered`）。
//! 正文里带上 `result`（脚本 `return` 的值，超长从尾部截断）与 `usage`：
//! 模型需要的正是这两样——它要靠结果决定下一步，靠用量决定还要不要再 fan-out。

use crate::utils::time::format_duration;

use super::{
    super::super::util::truncate_chars_with_postfix,
    progress::{self, EntryState},
    task::WorkflowRun,
};

/// 正文上限（上游 4000 字符）。
const RESULT_MAX_CHARS: usize = 4000;

/// 一个 run 的 `<workflow_result …>` 信封。
pub(crate) fn envelope(run: &WorkflowRun) -> String {
    let stats = run.stats();
    let replayed = replayed_count(run);
    let result = run.result_text();
    let result = truncate_chars_with_postfix(&result, RESULT_MAX_CHARS, "\n...(truncated)");
    let (tool_calls, output_tokens) = totals(run);

    format!(
        "<workflow_result id=\"{}\" name=\"{}\" status=\"{}\" agents=\"{}/{}\" tokens=\"{}\" tool_uses=\"{}\" duration=\"{}\"{}>\n{}{}\n</workflow_result>",
        attr(&run.id),
        attr(&run.name),
        run.status.as_str(),
        stats.done,
        stats.total.max(run.agent_count as usize),
        output_tokens,
        tool_calls,
        format_duration(run.elapsed_ms()),
        match (replayed, run.resumed_from.as_deref()) {
            (n, Some(from)) if n > 0 => {
                format!(
                    "\n<summary>{n} of {} agents replayed from {from}</summary>",
                    stats.total.max(run.agent_count as usize)
                )
            }
            _ => String::new(),
        },
        match run.script_path.as_deref() {
            Some(p) => format!("\n<script>{}</script>", attr(p)),
            None => String::new(),
        },
        result,
    )
}

/// 本次运行里从 journal 重放回来的 agent 数（进度条目上的 `cached` 标记）。
///
/// 按 `index` 去重：日志是 append-only 的，一个 agent 可能有多条。
pub(crate) fn replayed_count(run: &WorkflowRun) -> usize {
    let entries = progress::parse_entries(&run.log());
    progress::collapse(&entries)
        .agents
        .iter()
        .filter(|a| a.cached)
        .count()
}

/// 各 agent 的 toolCalls / tokens 合计（从进度日志折叠而来——权威来源是日志本身）。
pub(crate) fn totals(run: &WorkflowRun) -> (u64, u64) {
    let entries = progress::parse_entries(&run.log());
    let collapsed = progress::collapse(&entries);
    let mut tokens = 0u64;
    let mut tool_calls = 0u64;

    for a in &collapsed.agents {
        // 只统计已结算的条目：运行中被切断的那些没有用量可报
        if matches!(a.state, Some(EntryState::Done) | Some(EntryState::Error)) {
            tool_calls += a.tool_calls.unwrap_or(0);
            tokens += a.tokens.unwrap_or(0);
        }
    }

    (tool_calls, tokens)
}

/// XML 属性转义（与 `super::super::notify` 同一口径的极简版）。
fn attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::subagent::workflow::task;
    use serde_json::json;

    #[test]
    fn envelope_carries_status_usage_and_result() {
        let _g = super::super::super::test_lock();
        task::reset_all();
        let id = task::new_id();
        let _abort = task::register(&id, "demo", "d", None, None, None);
        task::set_agent_count(&id, 2);
        let log = task::find(&id).unwrap().progress.clone();
        log.lock().unwrap().push(json!({
            "type": "workflow_agent", "index": 0, "label": "a", "state": "done",
            "tokens": 120, "toolCalls": 3
        }));
        log.lock().unwrap().push(json!({
            "type": "workflow_agent", "index": 1, "label": "b", "state": "error", "error": "boom"
        }));
        task::finish(
            &id,
            task::RunStatus::Completed,
            Some(json!({"ok": true})),
            None,
        );
        let run = task::find(&id).unwrap();
        let text = envelope(&run);
        assert!(text.starts_with("<workflow_result "), "{text}");
        assert!(text.contains(&format!("id=\"{id}\"")), "{text}");
        assert!(text.contains("status=\"completed\""), "{text}");
        // done/total（上游口径：失败不计入分子，靠 status 与失败行体现）
        assert!(text.contains("agents=\"1/2\""), "{text}");
        assert!(text.contains("tokens=\"120\""), "{text}");
        assert!(text.contains("tool_uses=\"3\""), "{text}");
        assert!(text.contains("\"ok\": true"), "{text}");
        assert!(text.ends_with("</workflow_result>"), "{text}");

        // 失败：正文是错误文本（照上游 `workflowResultText`）
        task::finish(
            &id,
            task::RunStatus::Failed,
            None,
            Some("script blew up".into()),
        );
        let text = envelope(&task::find(&id).unwrap());
        assert!(text.contains("status=\"failed\""), "{text}");
        assert!(text.contains("script blew up"), "{text}");
        task::reset_all();
    }

    #[test]
    fn long_results_are_truncated() {
        let _g = super::super::super::test_lock();
        task::reset_all();
        let id = task::new_id();
        let _abort = task::register(&id, "demo", "d", None, None, None);
        let big = "x".repeat(RESULT_MAX_CHARS + 100);
        task::finish(&id, task::RunStatus::Completed, Some(json!(big)), None);
        let text = envelope(&task::find(&id).unwrap());
        assert!(text.contains("...(truncated)"), "超长必须截断");
        assert!(text.len() < RESULT_MAX_CHARS + 400, "截断后不该仍带着全文");
        task::reset_all();
    }

    #[test]
    fn killed_run_reports_the_abort_reason() {
        let _g = super::super::super::test_lock();
        task::reset_all();
        let id = task::new_id();
        let _abort = task::register(&id, "demo", "d", None, None, None);
        task::finish(
            &id,
            task::RunStatus::Killed,
            None,
            Some("workflow aborted".into()),
        );
        let text = envelope(&task::find(&id).unwrap());
        assert!(text.contains("status=\"killed\""), "{text}");
        assert!(text.contains("workflow aborted"), "{text}");
        task::reset_all();
    }
}
