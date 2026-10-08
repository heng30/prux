//! 面向模型与人的输出整形：状态头、`Continuation` 信封、转写与截断。

use super::types::AgentRecord;
use crate::{
    core::{extensions::RichSpan, provider::AgentMessage},
    extensions::util::truncate_chars,
    utils::{
        glyphs::{DEF_DONE, DEF_FAILED, DEF_QUEUED, DEF_SPINNER_BRAILLE, DEF_STOPPED},
        time::format_duration_ms,
    },
};
use serde_json::Value;

pub use super::super::util::format_tokens;

/// `get_subagent_result(verbose)` 的转写上限（先到者为准）。
pub const VERBOSE_MAX_LINES: usize = 200;

/// 字节上限（先到者为准）。
pub const VERBOSE_MAX_BYTES: usize = 8 * 1024;

/// 完成卡片的持久化/渲染载荷的 `custom_type`（同时是重放时的分派键）。
pub const CARD_TYPE: &str = "subagent-completion";

/// 卡片预览行数上限
pub const CARD_PREVIEW_LINES: usize = 12;

/// 后台完成时注入对话的 envelope（完整结果，不截断）。
pub fn result_envelope(rec: &AgentRecord) -> String {
    let body = rec
        .result
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("(no output)");
    let name_attr = rec
        .name
        .as_deref()
        .map(|n| format!(" name=\"{}\"", attr(n)))
        .unwrap_or_default();

    format!(
        "<subagent_result id=\"{}\" type=\"{}\"{} status=\"{}\" turns=\"{}\" tokens=\"{}\" duration=\"{}\">\n{}\n</subagent_result>",
        attr(&rec.id),
        attr(&rec.agent_type),
        name_attr,
        rec.status.as_str(),
        rec.turns,
        rec.usage.display_total(),
        format_duration_ms(rec.duration_ms()),
        body,
    )
}

/// 后台完成时投给模型的 `Continuation` 消息。
pub fn continuation_message(rec: &AgentRecord) -> AgentMessage {
    AgentMessage::user_text(&result_envelope(rec))
}

/// 分组完成的合并信封（join `smart`/`group`）：外层一个 `<subagent_results>`，
/// 内层是各条通知**已经渲染好的**信封。外层带 batch 级的 `tokens` / `duration` 合计，
/// 让人不用逐条相加就能看出这一批花了多少。
///
/// 接受纯文本而不是记录列表，是为了让批次不限定于子代理：工作流运行的完成通知
/// （`<workflow_result …>`）也走同一条合并路径（见 `manager::BatchItem`）。
pub fn batch_envelope(envelopes: &[String], tokens: u64, duration_ms: u64) -> String {
    let mut out = format!(
        "<subagent_results count=\"{}\" tokens=\"{}\" duration=\"{}\">",
        envelopes.len(),
        tokens,
        format_duration_ms(duration_ms),
    );
    for e in envelopes {
        out.push('\n');
        out.push_str(e);
    }
    out.push_str("\n</subagent_results>");
    out
}

/// 转写行：(主题键, 文本)。`verbose_transcript` 与会话查看器共用同一渲染口径。
pub fn transcript_rows(rec: &AgentRecord) -> Vec<(&'static str, String)> {
    let mut rows: Vec<(&'static str, String)> = Vec::new();
    for msg in &rec.transcript {
        render_message_rows(msg, &mut rows);
    }
    rows
}

/// 渲染 `verbose` 转写：分轮列出 role 与工具调用摘要，超出上限时保留尾部。
pub fn verbose_transcript(rec: &AgentRecord) -> String {
    let mut lines: Vec<String> = transcript_rows(rec).into_iter().map(|(_, t)| t).collect();
    if lines.is_empty() {
        return "(no transcript)".to_string();
    }

    let truncated_lines = lines.len() > VERBOSE_MAX_LINES;
    if truncated_lines {
        lines.drain(..lines.len() - VERBOSE_MAX_LINES);
    }

    // 字节上限：从尾部保留
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in lines.iter().rev() {
        let add = line.len() + 1;
        if bytes + add > VERBOSE_MAX_BYTES && !kept.is_empty() {
            break;
        }
        bytes += add;
        kept.push(line);
    }
    kept.reverse();
    let truncated_bytes = kept.len() < lines.len();

    let mut out = String::new();
    if truncated_lines || truncated_bytes {
        out.push_str(&format!(
            "… truncated (showing last {} lines / {} bytes)\n",
            kept.len(),
            bytes
        ));
    }
    out.push_str(&kept.join("\n"));
    out
}

/// 单条 message → 转写行（键用于查看器着色）。
fn render_message_rows(msg: &Value, lines: &mut Vec<(&'static str, String)>) {
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("?");
    let empty: Vec<Value> = Vec::new();
    let content = msg
        .get("content")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    match role {
        "toolResult" => {
            let name = msg
                .get("toolName")
                .and_then(|v| v.as_str())
                .unwrap_or("tool");
            let text = content
                .iter()
                .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                .next()
                .unwrap_or("");
            lines.push(("dim", format!("tool {name}: {}", truncate_chars(text, 160))));
        }
        _ => {
            for block in content {
                match block.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        let t = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if !t.trim().is_empty() {
                            let key = if role == "user" { "muted" } else { "text" };
                            lines.push((key, format!("{role}: {}", truncate_chars(t, 400))));
                        }
                    }
                    Some("toolCall") => {
                        let n = block.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let args = block
                            .get("arguments")
                            .map(|v| truncate_chars(&v.to_string(), 120))
                            .unwrap_or_default();
                        lines.push(("accent", format!("{role} → {n} {args}")));
                    }
                    Some("thinking") => {
                        lines.push(("dim", format!("{role} (thinking)"))); // 不展示思考内容，只留标记
                    }
                    _ => {}
                }
            }
        }
    }
}

/// 结果卡片（`/agents` 菜单与人向通知）：状态头着色 + 结果正文 + 可选转写。
pub fn record_card(rec: &AgentRecord, verbose: bool) -> Vec<RichSpan> {
    let (key, _) = rec.status.color();
    let mut spans = vec![RichSpan::fg(
        key,
        format!("{} {}", rec.status.icon(), rec.status_header()),
    )];

    if let Some(result) = rec
        .result
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        spans.push(RichSpan::plain(format!("\n{result}")));
    } else if !rec.status.is_terminal() {
        spans.push(RichSpan::plain(format!(
            "\n(still {})",
            rec.status.as_str()
        )));
    }

    if verbose {
        spans.push(RichSpan::fg("dim", "\n--- transcript ---"));
        spans.push(RichSpan::plain(format!("\n{}", verbose_transcript(rec))));
    }
    spans
}

/// 卡片载荷：**纯数据**，实时投递与会话恢复重放共用同一份（见 `render_card`）。
///
/// 刻意只放"结果预览"而不是全文：全文由 `Continuation` 交给模型（对话区不再重复长文），
/// 卡片负责让人一眼看到状态、用量、耗时与结果开头，并给出 session 路径以便深挖。
pub fn card_payload(rec: &AgentRecord) -> Value {
    let result = rec.result.as_deref().map(str::trim).unwrap_or("");
    let preview_lines: Vec<&str> = result.lines().take(CARD_PREVIEW_LINES).collect();
    let truncated = result.lines().count() > preview_lines.len();
    let mut preview = preview_lines.join("\n");

    if truncated {
        preview.push_str("\n…");
    }

    serde_json::json!({
        "id": rec.id,
        "name": rec.name,
        "type": rec.agent_type,
        "displayName": rec.display_name,
        "status": rec.status.as_str(),
        "turns": rec.turns,
        "toolUses": rec.tool_uses,
        "tokens": rec.usage.display_total(),
        "cost": rec.usage.cost,
        "durationMs": rec.duration_ms(),
        "sessionPath": rec.session_path,
        "outputPath": rec.output_path,
        "preview": preview,
        "previewTruncated": truncated,
    })
}

/// 卡片渲染（实时与重放共用；只读 payload，不碰进程内状态）。
pub fn render_card(data: &Value) -> Vec<RichSpan> {
    let s = |k: &str| data.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let n = |k: &str| data.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let status = s("status");
    let icon = match status {
        "completed" | "steered" => DEF_DONE,
        "running" => DEF_SPINNER_BRAILLE[0],
        "queued" => DEF_QUEUED,
        "stopped" => DEF_STOPPED,
        _ => DEF_FAILED,
    };
    let status_key = match status {
        "completed" | "steered" => "success",
        "running" => "accent",
        "queued" => "muted",
        "stopped" => "warning",
        _ => "error",
    };

    let label = if s("name").is_empty() {
        format!("{} {}", s("displayName"), s("id"))
    } else {
        format!("{} ({}) {}", s("displayName"), s("name"), s("id"))
    };

    let mut spans = vec![
        RichSpan::fg(
            status_key,
            format!(
                "{icon} {label} · {status} · {}t · {} tok · {:.1}s",
                n("turns"),
                format_tokens(n("tokens")),
                n("durationMs") as f64 / 1000.0
            ),
        ),
        RichSpan::plain(format!(
            "\n{} — {}",
            s("type"),
            truncate_chars(s("preview"), 2000)
        )),
    ];

    if let Some(path) = data.get("sessionPath").and_then(|v| v.as_str()) {
        spans.push(RichSpan::fg("dim", format!("\nsession: {path}")));
    }

    if let Some(path) = data.get("outputPath").and_then(|v| v.as_str()) {
        spans.push(RichSpan::fg("dim", format!("\noutput: {path}")));
    }

    spans
}

/// 从会话文件的 JSONL 里取回本扩展的卡片载荷（`/resume` 后重放卡片用）。
///
/// 会话条目形状（`session_v4`）：`{"kind":"custom","customType":"...","data":{...}}`。
/// 只做浅层扫描：解析失败的行走跳过（会话文件里可能有其它插件写的东西）。
pub fn cards_from_session(path: &str) -> Vec<(String, Value)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|v| {
            let custom_type = v.get("customType").and_then(|c| c.as_str())?;

            // 本扩展的全部卡片（代理完成 + 工作流完成），各自带自己的 `custom_type`
            if !custom_type.starts_with("subagent-") {
                return None;
            }

            Some((
                custom_type.to_string(),
                v.get("data").cloned().unwrap_or(Value::Null),
            ))
        })
        .collect()
}

/// XML 属性值转义。
fn attr(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            other => other.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::subagent::types::{AgentStatus, UsageTotals};
    use serde_json::json;

    fn rec(status: AgentStatus) -> AgentRecord {
        AgentRecord {
            id: "a1b2c3d4".to_string(),
            name: Some("auth-audit".to_string()),
            handle: Some("explore".to_string()),
            alias: Some("auth-audit".to_string()),
            agent_type: "Explore".to_string(),
            display_name: "Explore".to_string(),
            description: "inspect code".to_string(),
            status,
            background: true,
            turns: 7,
            tool_uses: 5,
            usage: UsageTotals {
                input: 1_000,
                output: 200,
                cache_read: 9_999,
                cache_write: 0,
                cost: 0.0,
            },
            started_ms: 0,
            ended_ms: Some(42_100),
            result: Some("first line\nsecond line".to_string()),
            transcript: Vec::new(),
            session_path: None,
            output_path: None,
            max_turns: None,
            color: None,
            activity: None,
            steer: None,
            abort: None,
            stop_requested: false,
            worktree: None,
            worktree_result: None,
            parent_agent_id: None,
            depth: 0,
            workflow_owned: false,
            model: None,
            compaction_count: 0,
            consumed: false,
            done_rx: None,
        }
    }

    #[test]
    fn tokens_exclude_cache_read_and_are_humanized() {
        let r = rec(AgentStatus::Completed);
        assert_eq!(r.usage.display_total(), 1_200);
        assert_eq!(format_tokens(1_200), "1.2K");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn envelope_carries_full_body_and_attributes() {
        let r = rec(AgentStatus::Completed);
        let e = result_envelope(&r);
        assert!(
            e.starts_with("<subagent_result id=\"a1b2c3d4\" type=\"Explore\" name=\"auth-audit\"")
        );
        assert!(e.contains("status=\"completed\""));
        assert!(e.contains("turns=\"7\""));
        assert!(e.contains("first line\nsecond line"));
        assert!(e.ends_with("</subagent_result>"));
        assert_eq!(format_duration_ms(42_100), "42.1s");
    }

    #[test]
    fn card_payload_previews_and_render_agrees() {
        let mut r = rec(AgentStatus::Completed);
        r.result = Some(
            (0..CARD_PREVIEW_LINES + 3)
                .map(|i| format!("line{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let payload = card_payload(&r);
        assert_eq!(payload["status"], "completed");
        assert_eq!(payload["tokens"], 1_200);
        assert_eq!(payload["previewTruncated"], true);
        let preview = payload["preview"].as_str().unwrap();
        assert_eq!(
            preview.lines().count(),
            CARD_PREVIEW_LINES + 1,
            "预览上限 + 省略号行"
        );
        assert!(preview.ends_with('…'));

        let spans = render_card(&payload);
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("✓"), "{text}");
        assert!(text.contains("Explore (auth-audit) a1b2c3d4"), "{text}");
        assert!(text.contains("completed"), "{text}");
        assert!(text.contains("1.2K tok"), "{text}");
        assert!(text.contains("line0"), "{text}");
        assert_eq!(
            spans[0].fg.as_deref(),
            Some("success"),
            "完成态用 success 色"
        );

        // 失败态：error 色 + ✗
        let mut bad = card_payload(&rec(AgentStatus::Error));
        bad["status"] = json!("error");
        let spans = render_card(&bad);
        assert_eq!(spans[0].fg.as_deref(), Some("error"));
        assert!(spans[0].text.starts_with('✗'), "{}", spans[0].text);
    }

    #[test]
    fn card_payloads_from_session_scans_custom_lines_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let payload = card_payload(&rec(AgentStatus::Completed));
        let lines = [
            json!({"kind":"entry","seq":1,"message":{"role":"user"}}),
            json!({"kind":"custom","customType":CARD_TYPE,"data":payload}),
            json!({"kind":"custom","customType":"other-ext-thing","data":{"x":1}}),
            json!("not an object"),
            json!({"kind":"custom","customType":CARD_TYPE,"data":{"status":"error"}}),
        ];
        let text = lines
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, text).unwrap();

        let found = cards_from_session(&path.to_string_lossy());
        assert_eq!(found.len(), 2, "只取本扩展的卡片条目: {found:?}");
        assert!(found.iter().all(|(t, _)| t == CARD_TYPE));
        assert_eq!(found[0].1["status"], "completed");
        assert_eq!(found[1].1["status"], "error");
        // 工作流卡片也重放（同一个前缀、各自的 custom_type）
        let wf = json!({"kind":"custom","customType":"subagent-workflow","data":{"name":"x"}});
        std::fs::write(&path, wf.to_string()).unwrap();
        let found = cards_from_session(&path.to_string_lossy());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "subagent-workflow");
        assert_eq!(found[0].1["name"], "x");
        // 文件不存在/不可读 → 空（不 panic）
        assert!(cards_from_session("/nonexistent/nope.jsonl").is_empty());
    }

    #[test]
    fn batch_envelope_nests_each_result() {
        let a = rec(AgentStatus::Completed);
        let mut b = rec(AgentStatus::Error);
        b.result = None;
        let e = batch_envelope(&[result_envelope(&a), result_envelope(&b)], 1_200, 1_500);
        assert!(
            e.starts_with("<subagent_results count=\"2\" tokens=\"1200\" duration=\"1.5s\">"),
            "{e}"
        );
        assert!(e.ends_with("</subagent_results>"), "{e}");
        assert_eq!(e.matches("<subagent_result id=").count(), 2, "{e}");
        assert!(e.contains("(no output)"), "失败的成员也要占一条: {e}");
    }

    #[test]
    fn envelope_handles_missing_result() {
        let mut r = rec(AgentStatus::Error);
        r.result = None;
        assert!(result_envelope(&r).contains("(no output)"));
    }

    #[test]
    fn transcript_renders_roles_and_tools() {
        let mut r = rec(AgentStatus::Completed);
        r.transcript = vec![
            json!({"role":"user","content":[{"type":"text","text":"do it"}]}),
            json!({"role":"assistant","content":[{"type":"text","text":"ok"},{"type":"toolCall","name":"read","arguments":{"path":"a"}}]}),
            json!({"role":"toolResult","toolName":"read","content":[{"type":"text","text":"line1\nline2"}]}),
        ];
        let t = verbose_transcript(&r);
        assert!(t.contains("user: do it"), "{t}");
        assert!(t.contains("assistant: ok"), "{t}");
        assert!(t.contains("assistant → read"), "{t}");
        assert!(t.contains("tool read: line1"), "{t}");
    }

    #[test]
    fn transcript_rows_carry_theme_keys_for_the_viewer() {
        let mut r = rec(AgentStatus::Completed);
        r.transcript = vec![
            json!({"role":"user","content":[{"type":"text","text":"do it"}]}),
            json!({"role":"assistant","content":[{"type":"toolCall","name":"read","arguments":{"path":"a"}}]}),
            json!({"role":"toolResult","toolName":"read","content":[{"type":"text","text":"line1"}]}),
            json!({"role":"assistant","content":[{"type":"thinking","text":"hmm"}]}),
        ];
        let rows = transcript_rows(&r);
        let by_key = |k: &str| rows.iter().filter(|(key, _)| *key == k).count();
        assert_eq!(by_key("muted"), 1, "user 行用 muted");
        assert_eq!(by_key("accent"), 1, "工具调用行用 accent");
        assert_eq!(by_key("dim"), 2, "工具结果 + thinking 用 dim");
        assert_eq!(by_key("text"), 0);
        // 与 verbose_transcript 文本口径一致
        assert_eq!(
            rows.iter()
                .map(|(_, t)| t.clone())
                .collect::<Vec<_>>()
                .join("\n"),
            verbose_transcript(&r)
        );
    }

    #[test]
    fn transcript_is_capped_by_lines() {
        let mut r = rec(AgentStatus::Completed);
        r.transcript = (0..(VERBOSE_MAX_LINES + 50))
            .map(|i| json!({"role":"user","content":[{"type":"text","text":format!("m{i}")}]}))
            .collect();
        let t = verbose_transcript(&r);
        assert!(t.starts_with("… truncated"), "{t}");
        assert_eq!(t.lines().count(), VERBOSE_MAX_LINES + 1);
        assert!(t.contains(&format!("m{}", VERBOSE_MAX_LINES + 49)));
        assert!(!t.contains("m0\n"));
    }
}
