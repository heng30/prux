//! `/debug receipts`：会话回执 —— 这个会话到底做了什么。
//!
//! 数据源是 prux 自己**已经落盘**的两份记录，不新增任何状态：
//!
//! - 会话文件（`host.session_path()`）：header 给 cwd / 创建时间，`tool_started` 记录给
//!   每次工具调用（工具名 + 生效参数 + 结果条目 id），`message` 条目给工具结果
//!   （`isError`、正文、`details.patch` 的增删行数），`operation_finished` 给回合级失败；
//! - permissions 的 review log：**没有跑起来**的那些调用（`deny` / `ask`）只在这里有痕迹。
//!
//! 逐行解析而不是 `Session::open`：命令是 busy-safe 的，可能在流式输出中途执行，
//! 尾部半行必须跳过而不是让整份回执失败。
//!
//! review log 里**没有会话 id**，只按时间过滤（条目 `ts` ≥ header `createdAt`）。
//! 同一个 agent_dir 下并行跑多个 prux 时，别家会话的判定可能混进来——回执照抄不猜。

use crate::{
    core::extensions::{CommandHost, UiNotifyLevel},
    extensions::{permissions::review_log_path, util::truncate_chars},
    utils::time::{format_clock, parse_iso_ms},
};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path};

/// 明细行的默认上限（超出只留计数）。
const DEFAULT_MAX_ITEMS: usize = 40;
/// 命令 / 路径等单行文本的截断长度。
const TEXT_MAX: usize = 120;
/// 拦截明细最多列出几条（其余只进计数）。
const MAX_REFUSALS: usize = 10;
/// 失败原因取结果正文首行的截断长度。
const REASON_MAX: usize = 160;
/// bash 工具在退出码非零时追加的状态行前缀。
const EXIT_MARKER: &str = "Command exited with code ";

/// `/debug receipts [n]`：渲染当前会话的回执，`n` 是明细行上限。
pub(super) fn command(host: &mut dyn CommandHost, rest: &str) -> bool {
    let max_items = rest
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_ITEMS);

    let Some(path) = host.session_path() else {
        host.push_message(
            "Receipt: this session has no file (in-memory session), so there is nothing to read.",
            UiNotifyLevel::Warning,
        );
        return false;
    };

    match render(Path::new(&path), max_items) {
        Ok(text) => host.push_message(&text, UiNotifyLevel::Info),
        Err(e) => host.push_message(&format!("Receipt unavailable: {e}"), UiNotifyLevel::Warning),
    }
    false
}

/// 一次工具调用（来自会话文件的 `tool_started` 记录）。
struct Call {
    /// 调用发起时刻（ms epoch），用于排序与时间窗。
    ts: u64,
    /// 工具名（`bash` / `edit` / `mcp` …）。
    tool: String,
    /// 实际生效的参数。
    args: Value,
    /// 结果条目的 id；指向 [`Ledger::outcomes`]。
    result_id: Option<String>,
}

/// 一条工具结果（来自会话文件的 `toolResult` 消息条目）。
struct Outcome {
    /// 工具是否返回了错误。
    is_error: bool,
    /// 结果正文（用于取失败原因、识别 bash 的退出码后缀）。
    text: String,
    /// 工具自带的结构化详情（`edit` 的 `patch` 在里面）。
    details: Option<Value>,
}

/// 会话回执的原始素材。
#[derive(Default)]
struct Ledger {
    /// 会话创建时刻（ms epoch）；review log 的时间窗下界。
    created_at: u64,
    /// 会话 id（回执首行只显示前 8 位）。
    id: String,
    /// 会话工作目录。
    cwd: String,
    /// 最近一次 assistant 消息用的 `provider/model`。
    model: Option<String>,
    /// 最后一条记录的时刻（回执时间窗上界）。
    last_ts: u64,
    /// 按出现顺序的工具调用。
    calls: Vec<Call>,
    /// 结果条目 id → 结果。
    outcomes: std::collections::HashMap<String, Outcome>,
    /// 回合级结束（`operation_finished` 的 outcome 不是 completed）：(outcome, 原因)。
    failures: Vec<(String, String)>,
}

/// 读取并解析会话文件；读不到文件时才失败（坏行一律跳过）。
fn read_ledger(path: &Path) -> Result<Ledger, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut ledger = Ledger::default();

    for line in text.lines() {
        // 尾行可能是本进程刚写了一半：跳过而不是让整份回执失败。
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts = value.get("timestamp").and_then(Value::as_u64).unwrap_or(0);
        ledger.last_ts = ledger.last_ts.max(ts);

        match value.get("kind").and_then(Value::as_str) {
            Some("header") => {
                ledger.created_at = value.get("createdAt").and_then(Value::as_u64).unwrap_or(0);
                ledger.id = text_field(&value, "id");
                ledger.cwd = text_field(&value, "cwd");
            }
            Some("record") => match value.get("type").and_then(Value::as_str) {
                Some("tool_started") => ledger.calls.push(Call {
                    ts,
                    tool: text_field(&value, "toolName"),
                    args: value.get("effectiveArgs").cloned().unwrap_or(Value::Null),
                    result_id: value
                        .get("resultEntryId")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }),
                Some("operation_finished") => {
                    let outcome = text_field(&value, "outcome");
                    if outcome != "completed" && !outcome.is_empty() {
                        let reason = value
                            .get("error")
                            .map(error_text)
                            .filter(|s| !s.is_empty())
                            .unwrap_or_default();
                        ledger.failures.push((outcome, reason));
                    }
                }
                _ => {}
            },
            Some("entry") => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                match message.get("role").and_then(Value::as_str) {
                    Some("toolResult") => {
                        if let Some(id) = value.get("id").and_then(Value::as_str) {
                            ledger.outcomes.insert(
                                id.to_string(),
                                Outcome {
                                    is_error: message
                                        .get("isError")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false),
                                    text: message_text(message),
                                    details: message.get("details").cloned(),
                                },
                            );
                        }
                    }
                    Some("assistant") => {
                        let provider = text_field(message, "provider");
                        let model = text_field(message, "model");
                        if !model.is_empty() {
                            ledger.model = Some(if provider.is_empty() {
                                model
                            } else {
                                format!("{provider}/{model}")
                            });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    Ok(ledger)
}

/// 取一个字符串字段；缺失或不是字符串时给空串。
fn text_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 把 `error` 字段压成一行（可能是字符串，也可能是对象里的 `message`）。
fn error_text(error: &Value) -> String {
    match error {
        Value::String(s) => s.lines().next().unwrap_or("").trim().to_string(),
        Value::Object(map) => map
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        // `null` / 布尔等不是原因，别把它当文案输出（旧版会打出 `turn failed — null`）。
        Value::Null | Value::Bool(_) => String::new(),
        other => other.to_string(),
    }
}

/// 拼接消息里所有文本块（图片块忽略）。
fn message_text(message: &Value) -> String {
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    blocks
        .iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 从统一 patch 里数增删行（`+++` / `---` 文件头不算）。
fn count_patch(patch: &str) -> (u32, u32) {
    let mut added = 0;
    let mut removed = 0;
    for line in patch.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

/// bash 结果正文里的退出码后缀（`append_status` 只在非零时写）。
fn exit_code_suffix(text: &str) -> Option<i32> {
    // 没有状态行时不能拿正文当退出码。
    let (_, tail) = text.rsplit_once(EXIT_MARKER)?;
    tail.trim_start()
        .split(|c: char| !c.is_ascii_digit() && c != '-')
        .next()
        .and_then(|n| n.parse().ok())
}

/// 一个文件在本会话里的累计增删行数。
#[derive(Default)]
struct FileChange {
    /// 新增行数。
    added: u32,
    /// 删除行数。
    removed: u32,
}

/// 渲染整份回执。
fn render(path: &Path, max_items: usize) -> Result<String, String> {
    let ledger = read_ledger(path)?;
    if ledger.id.is_empty() {
        return Err(format!("{} is not a session file", path.display()));
    }

    let mut files: BTreeMap<String, FileChange> = BTreeMap::new();
    let mut items: Vec<String> = Vec::new();
    let mut commands = 0usize;
    let mut mcp_calls = 0usize;
    let mut agents = 0usize;
    let mut failures = ledger.failures.len();

    let mut calls: Vec<&Call> = ledger.calls.iter().collect();
    calls.sort_by_key(|c| c.ts);

    for call in calls {
        let outcome = call
            .result_id
            .as_deref()
            .and_then(|id| ledger.outcomes.get(id));
        let failed = outcome.map(|o| o.is_error).unwrap_or(false);
        let reason = outcome
            .map(|o| truncate_chars(o.text.lines().next().unwrap_or("").trim(), REASON_MAX))
            .unwrap_or_default();

        let path_arg = call.args.get("path").and_then(Value::as_str).unwrap_or("");
        match call.tool.as_str() {
            "edit" => {
                let (added, removed) = outcome
                    .and_then(|o| o.details.as_ref())
                    .and_then(|d| d.get("patch"))
                    .and_then(Value::as_str)
                    .map(count_patch)
                    .unwrap_or((0, 0));
                if !failed {
                    let entry = files.entry(path_arg.to_string()).or_default();
                    entry.added += added;
                    entry.removed += removed;
                }
                items.push(if failed {
                    format!(
                        "failed: edited `{}` — {}",
                        truncate_chars(path_arg, TEXT_MAX),
                        reason
                    )
                } else {
                    format!(
                        "edited `{}` (+{added} −{removed})",
                        truncate_chars(path_arg, TEXT_MAX)
                    )
                });
            }
            "write" => {
                let added = call
                    .args
                    .get("content")
                    .and_then(Value::as_str)
                    .map(|c| c.lines().count() as u32)
                    .unwrap_or(0);
                if !failed {
                    let entry = files.entry(path_arg.to_string()).or_default();
                    entry.added += added;
                }
                items.push(if failed {
                    format!(
                        "failed: wrote `{}` — {}",
                        truncate_chars(path_arg, TEXT_MAX),
                        reason
                    )
                } else {
                    format!("wrote `{}` (+{added})", truncate_chars(path_arg, TEXT_MAX))
                });
            }
            "bash" | "powershell" => {
                commands += 1;
                let command = call
                    .args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .replace('\n', " ");
                let command = truncate_chars(&command, TEXT_MAX);
                items.push(if failed {
                    failures += 1;
                    format!("failed: `{command}` — {reason}")
                } else {
                    match outcome.and_then(|o| exit_code_suffix(&o.text)) {
                        Some(0) | None => format!("ran `{command}` — exit 0"),
                        Some(code) => format!("ran `{command}` — exit {code}"),
                    }
                });
            }
            "mcp" => {
                mcp_calls += 1;
                let action = text_field(&call.args, "action");
                let server = text_field(&call.args, "server");
                let name = text_field(&call.args, "name");
                let target = match (server.is_empty(), name.is_empty()) {
                    (false, false) => format!(" {server}/{name}"),
                    (false, true) => format!(" {server}"),
                    (true, false) => format!(" {name}"),
                    (true, true) => String::new(),
                };
                let action = if action.is_empty() { "call" } else { &action };
                let line = format!("called mcp · {action}{target}");
                items.push(if failed {
                    failures += 1;
                    format!("failed: {line} — {reason}")
                } else {
                    line
                });
            }
            "Agent" | "SubagentWorkflow" => {
                agents += 1;
                let verb = if call.tool == "Agent" {
                    "started agent"
                } else {
                    "started workflow"
                };
                items.push(if failed {
                    failures += 1;
                    format!("failed: {verb} — {reason}")
                } else {
                    verb.to_string()
                });
            }
            other => {
                let line = format!("called `{}`", truncate_chars(other, TEXT_MAX));
                items.push(if failed {
                    failures += 1;
                    format!("failed: {line} — {reason}")
                } else {
                    line
                });
            }
        }
    }

    let refusals = read_refusals(ledger.created_at);
    let files_added: u32 = files.values().map(|f| f.added).sum();
    let files_removed: u32 = files.values().map(|f| f.removed).sum();

    let mut out = String::new();
    out.push_str(&format!(
        "Receipt: session {}\n",
        &ledger.id[..ledger.id.len().min(8)]
    ));
    let mut meta = vec![
        if ledger.cwd.is_empty() {
            "?".to_string()
        } else {
            ledger.cwd.clone()
        },
        ledger
            .model
            .clone()
            .unwrap_or_else(|| "unknown model".to_string()),
    ];
    if ledger.created_at > 0 {
        meta.push(format!(
            "{} → {}",
            format_clock(ledger.created_at),
            format_clock(ledger.last_ts.max(ledger.created_at))
        ));
    }
    out.push_str(&meta.join(" · "));
    out.push_str("\n\n");

    let mut totals = vec![
        format!(
            "Changed {} file(s) (+{files_added} −{files_removed})",
            files.len()
        ),
        format!("ran {commands} command(s)"),
    ];
    if mcp_calls > 0 {
        totals.push(format!("{mcp_calls} MCP call(s)"));
    }
    if agents > 0 {
        totals.push(format!("{agents} agent(s)"));
    }
    if failures > 0 {
        totals.push(format!("{failures} failure(s)"));
    }
    out.push_str(&totals.join(" · "));
    out.push('\n');
    out.push_str(&refusals.summary);
    out.push_str("\n\n");

    for (i, item) in items.iter().take(max_items).enumerate() {
        out.push_str(&format!("{}. {item}\n", i + 1));
    }
    if items.len() > max_items {
        out.push_str(&format!(
            "… {} more (run /debug receipts {} for the rest)\n",
            items.len() - max_items,
            items.len()
        ));
    }
    if items.is_empty() {
        out.push_str("(no tool calls in this session)\n");
    }

    for line in &refusals.lines {
        out.push_str(line);
        out.push('\n');
    }
    if refusals.hidden > 0 {
        out.push_str(&format!("… {} more refusals\n", refusals.hidden));
    }

    if !ledger.failures.is_empty() {
        out.push('\n');
        for (outcome, reason) in &ledger.failures {
            if reason.is_empty() {
                out.push_str(&format!("turn {outcome}\n"));
            } else {
                out.push_str(&format!(
                    "turn {outcome} — {}\n",
                    truncate_chars(reason, REASON_MAX)
                ));
            }
        }
    }

    Ok(out.trim_end().to_string())
}

/// review log 里本会话时间窗内的拦截明细。
struct Refusals {
    /// 汇总一行（「Approvals: …」）。
    summary: String,
    /// 逐条拦截明细（`did not run …`）。
    lines: Vec<String>,
    /// 因 [`MAX_REFUSALS`] 未列出的条数。
    hidden: usize,
}

/// 读 review log：统计 allow / ask / deny，并列出没跑起来的那些调用。
fn read_refusals(since_ms: u64) -> Refusals {
    let path = review_log_path();
    let text = fs::read_to_string(&path).unwrap_or_default();

    let mut allowed = 0usize;
    let mut asked = 0usize;
    let mut denied = 0usize;
    let mut approved_by_user = 0usize;
    let mut lines = Vec::new();
    let mut hidden = 0usize;

    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts = value
            .get("ts")
            .and_then(Value::as_str)
            .and_then(parse_iso_ms)
            .unwrap_or(0);
        if ts < since_ms {
            continue;
        }

        let action = text_field(&value, "action");
        let user_approved = value
            .get("sessionApproved")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if user_approved {
            approved_by_user += 1;
        }

        match action.as_str() {
            "allow" => allowed += 1,
            "ask" => asked += 1,
            "deny" => denied += 1,
            _ => continue,
        }
        if action == "allow" {
            continue;
        }

        if lines.len() >= MAX_REFUSALS {
            hidden += 1;
            continue;
        }
        let tool = text_field(&value, "tool");
        let pattern = text_field(&value, "pattern");
        let reason = text_field(&value, "reason");
        let subject = truncate_chars(&text_field(&value, "value"), TEXT_MAX);
        let verb = if action == "deny" {
            "denied"
        } else {
            "needed approval"
        };
        let mut line = format!("- did not run `{subject}` — {verb} ({tool})");
        if !pattern.is_empty() {
            line.push_str(&format!(
                ", pattern `{}`",
                truncate_chars(&pattern, TEXT_MAX)
            ));
        }
        if !reason.is_empty() {
            line.push_str(&format!(": {}", truncate_chars(&reason, TEXT_MAX)));
        }
        lines.push(line);
    }

    let summary = if allowed + asked + denied == 0 {
        "Approvals: none recorded since this session started (permissions-review.jsonl is empty)"
            .to_string()
    } else {
        let mut parts = vec![format!("{allowed} allowed")];
        if asked > 0 {
            parts.push(format!("{asked} needed approval"));
        }
        if denied > 0 {
            parts.push(format!("{denied} denied"));
        }
        if approved_by_user > 0 {
            parts.push(format!("{approved_by_user} approved by session grant"));
        }
        format!("Approvals: {}", parts.join(" · "))
    };

    Refusals {
        summary,
        lines,
        hidden,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;
    use std::io::Write;

    /// 写一份最小会话文件：header + edit 调用/结果 + 失败的 bash + 失败的 op。
    fn write_session(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("session.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        let lines = [
            r#"{"kind":"header","version":4,"id":"0123456789abcdef","createdAt":1700000000000,"cwd":"/work/repo"}"#,
            r#"{"kind":"record","type":"tool_started","id":"t1","seq":1,"lane":"main","timestamp":1700000001000,"toolName":"edit","effectiveArgs":{"path":"src/a.rs"},"resultEntryId":"r1"}"#,
            r#"{"kind":"entry","lane":"main","type":"message","id":"r1","seq":2,"timestamp":1700000001000,"message":{"role":"toolResult","content":[{"type":"text","text":"Successfully replaced 1 block(s) in src/a.rs."}],"toolName":"edit","isError":false,"details":{"patch":"--- a\n+++ b\n@@ -1 +1,2 @@\n-old\n+new\n+more\n"}}}"#,
            r#"{"kind":"record","type":"tool_started","id":"t2","seq":3,"lane":"main","timestamp":1700000002000,"toolName":"bash","effectiveArgs":{"command":"cargo test"},"resultEntryId":"r2"}"#,
            r#"{"kind":"entry","lane":"main","type":"message","id":"r2","seq":4,"timestamp":1700000002000,"message":{"role":"toolResult","content":[{"type":"text","text":"boom\nCommand exited with code 101"}],"toolName":"bash","isError":true}}"#,
            r#"{"kind":"record","type":"operation_finished","id":"o1","seq":5,"lane":"main","timestamp":1700000003000,"runId":"run-1","outcome":"failed","error":{"message":"provider returned 500"}}"#,
        ];
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn renders_files_commands_and_failures() {
        let _ad = AgentDirGuard::temp();
        let dir = std::env::temp_dir().join(format!("prux-receipts-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = write_session(&dir);

        let text = render(&path, DEFAULT_MAX_ITEMS).expect("receipt");
        assert!(text.starts_with("Receipt: session 01234567"), "{text}");
        assert!(text.contains("Changed 1 file(s) (+2 −1)"), "{text}");
        assert!(text.contains("edited `src/a.rs` (+2 −1)"), "{text}");
        assert!(text.contains("failed: `cargo test` — boom"), "{text}");
        assert!(
            text.contains("turn failed — provider returned 500"),
            "{text}"
        );
        assert!(text.contains("2 failure(s)"), "{text}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exit_code_only_comes_from_the_status_suffix() {
        assert_eq!(
            exit_code_suffix("ok\n\nCommand exited with code 101"),
            Some(101)
        );
        assert_eq!(
            exit_code_suffix("(no output)\n\nCommand exited with code 2"),
            Some(2)
        );
        // 正文以数字开头但没有状态行时不能当成退出码。
        assert_eq!(exit_code_suffix("123 files changed"), None);
    }

    #[test]
    fn lists_refusals_from_the_review_log() {
        let _ad = AgentDirGuard::temp();
        let dir = std::env::temp_dir().join(format!("prux-receipts-deny-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = write_session(&dir);

        let log = review_log_path();
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(
            &log,
            concat!(
                r#"{"ts":"2023-11-14T22:13:22Z","tool":"bash","surface":"bash","pattern":"rm*","action":"deny","value":"rm -rf build","reason":"no rm here","sessionApproved":false}"#,
                "\n",
                r#"{"ts":"2000-01-01T00:00:00Z","tool":"bash","surface":"bash","pattern":"*","action":"allow","value":"ls","reason":null,"sessionApproved":false}"#,
                "\n",
            ),
        )
        .unwrap();

        let text = render(&path, DEFAULT_MAX_ITEMS).expect("receipt");
        assert!(text.contains("1 denied"), "{text}");
        assert!(
            text.contains("- did not run `rm -rf build` — denied (bash)"),
            "{text}"
        );
        assert!(text.contains("pattern `rm*`"), "{text}");
        // 会话开始之前的记录不进本次回执（那条 `allow` 不计入统计）。
        assert!(text.contains("Approvals: 0 allowed"), "{text}");

        fs::remove_dir_all(&dir).ok();
    }
}
