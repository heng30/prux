//! 钩子命令的执行与结果解释。
//!
//! 一条钩子 = 一个子进程：JSON 信封写进 stdin，结果从 stdout / 退出码读。
//! 输出必须走管道（绝不继承终端），否则钩子里的横幅、警告会糊在 TUI 上。

use super::config::HookCommand;
use crate::utils::process::{run_timed, shell_command};
use serde_json::{Value, json};

/// 钩子结果要表达的意向；按 [`Verdict`] 的序做 most-severe-wins 合并。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(super) enum Verdict {
    /// 没有表态。
    #[default]
    None,
    /// 明确放行（只用于抵消同批次里的 `Ask`）。
    Allow,
    /// 需要用户确认。
    Ask,
    /// 拒绝 / 阻止。
    Deny,
}

/// 一条钩子的解释结果。
#[derive(Debug, Clone, Default)]
pub(super) struct Decision {
    /// 意向。
    pub verdict: Verdict,
    /// 拒绝或请求确认的理由。
    pub reason: Option<String>,
    /// 追加给模型或工具结果的文本（多条按顺序拼接）。
    pub context: Option<String>,
    /// 改写的工具入参。
    pub updated_input: Option<Value>,
}

impl Decision {
    /// 把 `other` 合并进来：意向取更严的，理由跟着更严的那条走。
    pub(super) fn merge(&mut self, other: Decision) {
        if other.verdict > self.verdict {
            self.verdict = other.verdict;
            self.reason = other.reason;
        } else if self.reason.is_none() {
            self.reason = other.reason;
        }
        if let Some(ctx) = other.context {
            self.context = Some(match self.context.take() {
                Some(prev) => format!("{prev}\n{ctx}"),
                None => ctx,
            });
        }
        if other.updated_input.is_some() {
            self.updated_input = other.updated_input;
        }
    }
}

/// 一次子进程执行的原始结果。
#[derive(Debug, Clone, Default)]
struct Run {
    /// 退出码；进程被信号杀掉或起不来时为 `None`。
    code: Option<i32>,
    /// 截留的 stdout。
    stdout: String,
    /// 截留的 stderr。
    stderr: String,
    /// 是否超时被杀。
    timed_out: bool,
}

/// 跑一条钩子并解释结果。`cwd` 是钩子的工作目录，`envelope` 是写进 stdin 的 JSON。
pub(super) fn run(hook: &HookCommand, cwd: &str, envelope: &Value) -> Decision {
    let mut cmd = shell_command(&hook.command);
    cmd.current_dir(cwd).env("PRUX_PROJECT_DIR", cwd);
    let payload = format!("{envelope}\n");

    match run_timed(cmd, Some(payload), hook.timeout) {
        Ok(output) => interpret(
            hook,
            &Run {
                code: output.code,
                stdout: output.stdout,
                stderr: output.stderr,
                timed_out: output.timed_out,
            },
        ),
        Err(e) => Decision {
            verdict: Verdict::None,
            reason: Some(format!("cannot start hook: {e}")),
            ..Default::default()
        },
    }
}

/// 把一次执行的原始结果翻译成意向。
///
/// 约定（与 Claude Code 同形）：
/// - 退出码 `0`：stdout 是可选 JSON，见 [`interpret_json`]；
///   `UserPromptSubmit` 下非 JSON 的 stdout 整段当作追加上下文；
/// - 退出码 `2`：阻止，理由取 stderr（空则取 stdout）；
/// - 其它退出码 / 超时 / 起不来：没有表态，只留 stderr 备查。
fn interpret(hook: &HookCommand, run: &Run) -> Decision {
    if run.timed_out {
        return Decision {
            verdict: Verdict::None,
            reason: Some(format!(
                "hook timed out after {}s: {}",
                hook.timeout.as_secs(),
                hook.command
            )),
            ..Default::default()
        };
    }

    match run.code {
        Some(0) => interpret_stdout(&run.stdout),
        Some(2) if !run.stderr.trim().is_empty() => Decision {
            verdict: Verdict::Deny,
            reason: Some(run.stderr.trim().to_string()),
            ..Default::default()
        },
        Some(2) => Decision {
            verdict: Verdict::Deny,
            reason: Some(run.stdout.trim().to_string()),
            ..Default::default()
        },
        Some(_) => Decision {
            verdict: Verdict::None,
            reason: Some(run.stderr.trim().to_string()),
            ..Default::default()
        },
        None => Decision {
            verdict: Verdict::None,
            reason: Some(run.stderr.trim().to_string()),
            ..Default::default()
        },
    }
}

/// 解释退出码 0 的 stdout：JSON 决定 `decision` / `hookSpecificOutput`，
/// 非 JSON 时整段文本作为追加上下文（事件本身要不要用由调用方决定）。
fn interpret_stdout(stdout: &str) -> Decision {
    let text = stdout.trim();
    if text.is_empty() {
        return Decision::default();
    }

    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Decision {
            context: Some(text.to_string()),
            ..Default::default()
        };
    };

    let mut decision = Decision::default();

    match value.get("decision").and_then(|v| v.as_str()) {
        Some("block") => decision.verdict = Verdict::Deny,
        Some("approve") => decision.verdict = Verdict::Allow,
        _ => {}
    }
    if let Some(reason) = value.get("reason").and_then(|v| v.as_str()) {
        decision.reason = Some(reason.to_string());
    }

    if let Some(specific) = value.get("hookSpecificOutput") {
        match specific.get("permissionDecision").and_then(|v| v.as_str()) {
            Some("deny" | "block") => decision.verdict = Verdict::Deny,
            Some("ask") => decision.verdict = Verdict::Ask,
            Some("allow") if decision.verdict == Verdict::None => {
                decision.verdict = Verdict::Allow;
            }
            _ => {}
        }
        if let Some(reason) = specific
            .get("permissionDecisionReason")
            .and_then(|v| v.as_str())
            .or_else(|| specific.get("additionalContext").and_then(|v| v.as_str()))
        {
            match decision.verdict {
                Verdict::Deny | Verdict::Ask => decision.reason = Some(reason.to_string()),
                _ => {}
            }
        }
        if let Some(ctx) = specific.get("additionalContext").and_then(|v| v.as_str()) {
            decision.context = Some(ctx.to_string());
        }
        if let Some(input) = specific.get("updatedInput").filter(|v| v.is_object()) {
            decision.updated_input = Some(input.clone());
        }
    }

    decision
}

/// 构造一个信封对象：公共字段 + 事件专属字段。
pub(super) fn envelope(event: &str, session_id: &str, cwd: &str, extra: Value) -> Value {
    let mut value = json!({
        "hook_event_name": event,
        "session_id": session_id,
        "cwd": cwd,
    });
    if let (Some(base), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        for (key, item) in extra {
            base.insert(key.clone(), item.clone());
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// `decision: block` + reason 变成拒绝；`approve` 变成放行。
    #[test]
    fn interprets_block_and_approve() {
        let deny = interpret_stdout(r#"{"decision":"block","reason":"no"}"#);
        assert_eq!(deny.verdict, Verdict::Deny);
        assert_eq!(deny.reason.as_deref(), Some("no"));

        let allow = interpret_stdout(r#"{"decision":"approve"}"#);
        assert_eq!(allow.verdict, Verdict::Allow);
    }

    /// `hookSpecificOutput` 能给出权限判定、追加上下文与改写的入参。
    #[test]
    fn interprets_hook_specific_output() {
        let d = interpret_stdout(
            r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"sure?",
                "additionalContext":"ctx","updatedInput":{"command":"ls"}}}"#,
        );
        assert_eq!(d.verdict, Verdict::Ask);
        assert_eq!(d.reason.as_deref(), Some("sure?"));
        assert_eq!(d.context.as_deref(), Some("ctx"));
        assert_eq!(d.updated_input.unwrap()["command"], "ls");
    }

    /// 非 JSON 的 stdout 整段当作上下文；空输出什么都不表态。
    #[test]
    fn interprets_plain_stdout_as_context() {
        assert_eq!(
            interpret_stdout(" remember the tests\n").context.as_deref(),
            Some("remember the tests")
        );
        assert_eq!(interpret_stdout("   ").verdict, Verdict::None);
    }

    /// 合并取最严：Allow 不覆盖 Ask，Ask 不覆盖 Deny，理由跟着更严的那条走。
    #[test]
    fn merge_keeps_the_most_severe_verdict() {
        let mut merged = Decision::default();
        merged.merge(Decision {
            verdict: Verdict::Allow,
            ..Default::default()
        });
        merged.merge(Decision {
            verdict: Verdict::Ask,
            reason: Some("confirm".to_string()),
            ..Default::default()
        });
        merged.merge(Decision {
            verdict: Verdict::Allow,
            ..Default::default()
        });
        assert_eq!(merged.verdict, Verdict::Ask);
        assert_eq!(merged.reason.as_deref(), Some("confirm"));
    }

    /// 真跑一次 `sh -c`：stdout 被收进结果，退出码 2 按阻止处理。
    #[test]
    fn runs_a_real_command_and_captures_stdout() {
        #[cfg(windows)]
        let command = "echo {\"decision\":\"block\",\"reason\":\"x\"}";
        #[cfg(not(windows))]
        let command = "printf '%s' '{\"decision\":\"block\",\"reason\":\"x\"}'";

        let hook = HookCommand {
            command: command.to_string(),
            timeout: Duration::from_secs(10),
        };
        let decision = run(&hook, ".", &json!({"hook_event_name": "PreToolUse"}));
        assert_eq!(decision.verdict, Verdict::Deny);
        assert_eq!(decision.reason.as_deref(), Some("x"));
    }

    /// 超时的钩子按「没有表态」处理，不当成阻止。
    #[test]
    fn timeout_is_not_a_verdict() {
        #[cfg(windows)]
        let command = "ping -n 30 127.0.0.1 > nul";
        #[cfg(not(windows))]
        let command = "sleep 30";

        let hook = HookCommand {
            command: command.to_string(),
            timeout: Duration::from_millis(1),
        };
        let decision = run(&hook, ".", &Value::Null);
        assert_eq!(decision.verdict, Verdict::None);
        assert!(decision.reason.unwrap_or_default().contains("timed out"));
    }
}
