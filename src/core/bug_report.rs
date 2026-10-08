//! `/bug` 报告收集与导出（本地部分）。
//!
//! 无 Radius 网关，故不做上传；只做「脱敏收集 + 导出 zip」：
//! - `report.json`：脱敏后的环境、会话、模型/provider、扩展、设置元数据；
//! - `diagnostics.json`：失败/中止的 assistant 轮次 + 崩溃日志（不含对话正文）；
//! - `session.jsonl`（可选）：当前分支的会话记录（可被 `/import` 还原）；
//! - `summary.md`（可选）：不附 transcript 时由当前模型生成的报告摘要。
//!
//! 脱敏规则：敏感键名（apiKey/secret/token/password/credential/authorization/cookie）
//! 的值一律替换为 `<redacted>`，URL 里的 userinfo 与敏感查询参数同样处理。

use crate::{
    APP_NAME, PROJECT_SCOPE_NAME,
    core::{
        self, auth,
        changelog::VERSION,
        compaction,
        crash_log::CrashRecord,
        login_registry,
        provider::{AgentMessage, ModelConfig},
        session_manager::Session,
        settings_manager::agent_dir,
    },
    error::Result,
    utils::{
        time::{now_iso, now_ms},
        zip::{ZipEntry, write_zip},
    },
};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{path::Path, sync::OnceLock};
use url::Url;

/// 会话内记录「已导出 bug 报告」的自定义条目类型
pub const BUG_REPORT_CUSTOM_ENTRY_TYPE: &str = concat!(env!("CARGO_PKG_NAME"), ".bug-report");
/// 报告 schema 版本
pub const BUG_REPORT_SCHEMA_VERSION: u32 = 1;
/// 脱敏替换值
pub const REDACTED: &str = "<redacted>";

/// bug 摘要的 system prompt
pub const BUG_SUMMARY_SYSTEM_PROMPT: &str = "You are helping a user file a bug report about prux, the coding agent they are talking to. You will be shown the conversation transcript. Write a report for the prux developers describing what the user was doing and what went wrong.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the report.";

/// bug 摘要的输出要求
pub const BUG_SUMMARY_INSTRUCTIONS: &str = "Write the bug report in Markdown with these sections:

## What the user was doing
One short paragraph.

## What went wrong
Concrete description of the failure: wrong output, errors, hangs, tool failures, unexpected behavior. Quote error messages and tool output verbatim where they exist.

## Steps to reproduce
Numbered list, as specific as the transcript allows.

## Relevant details
Tool calls involved, files touched, model behavior, anything else that helps a developer reproduce or locate the problem.

Do not include file contents, secrets, or credentials from the transcript; refer to files by path only. Keep the report factual and concise.";

/// 敏感键名匹配（先做 camelCase → snake_case 展开，再做整词匹配）
fn sensitive_key_re() -> &'static Regex {
    /// 敏感键名匹配正则单例（apiKey/secret/token/password 等）。
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:^|[-_])(api[-_]?key|secret|token|password|passwd|credential|authorization|cookie)(?:$|[-_])")
            .expect("valid sensitive key regex")
    })
}

/// 键名是否敏感（`apiKey` / `api-key` / `API_KEY` / `sessionToken` 等均命中）
pub fn is_sensitive_key(key: &str) -> bool {
    let camel_split = Regex::new(r"([a-z0-9])([A-Z])")
        .map(|re| re.replace_all(key, "${1}_${2}").to_string())
        .unwrap_or_else(|_| key.to_string());

    for candidate in [key, camel_split.as_str()] {
        // 整词匹配要求键名两端为边界：为短键名补上下划线再做一次匹配
        let padded = format!("_{}_", candidate.trim_matches(['_', '-']).to_lowercase());
        if sensitive_key_re().is_match(&padded) {
            return true;
        }
    }
    false
}

/// 去掉 URL 中的凭据与敏感查询参数；非 URL 原样返回。
pub fn redact_url(value: &str) -> String {
    let Ok(mut url) = Url::parse(value) else {
        return value.to_string();
    };

    let mut changed = false;
    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        changed = true;
    }

    let sensitive: Vec<String> = url
        .query_pairs()
        .filter(|(k, _)| is_sensitive_key(k))
        .map(|(k, _)| k.to_string())
        .collect();

    if !sensitive.is_empty() {
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| {
                if sensitive.iter().any(|s| *s == k) {
                    (k.to_string(), REDACTED.to_string())
                } else {
                    (k.to_string(), v.to_string())
                }
            })
            .collect();
        url.query_pairs_mut().clear().extend_pairs(pairs);
        changed = true;
    }

    if changed {
        url.to_string()
    } else {
        value.to_string()
    }
}

/// 递归脱敏 JSON：敏感键的值替换为 `<redacted>`，字符串值走 [`redact_url`]。
pub fn redact_json_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    if is_sensitive_key(k) {
                        (k.clone(), Value::String(REDACTED.to_string()))
                    } else {
                        (k.clone(), redact_json_value(v))
                    }
                })
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_json_value).collect()),
        Value::String(s) => Value::String(redact_url(s)),
        other => other.clone(),
    }
}

/// 环境信息（版本/运行时/OS/终端/`PRUX_*` 变量名；值一律不出本机）
pub fn collect_environment() -> Value {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let mut prux_vars: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.to_string_lossy().to_string().into())
        .filter(|k| k.starts_with("PRUX_"))
        .collect();

    prux_vars.sort();

    json!({
        "app": APP_NAME,
        "version": VERSION,
        "runtime": "rust",
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "shell": env("SHELL").map(|s| {
            s.rsplit(['/', '\\']).next().unwrap_or(&s).to_string()
        }),
        "terminal": {
            "term": env("TERM"),
            "program": env("TERM_PROGRAM"),
            "programVersion": env("TERM_PROGRAM_VERSION"),
            "colorterm": env("COLORTERM"),
            "tmux": env("TMUX").is_some(),
            "ssh": env("SSH_CONNECTION").is_some() || env("SSH_CLIENT").is_some() || env("SSH_TTY").is_some(),
            "ci": env("CI").is_some(),
        },
        "pruxEnvironmentVariables": prux_vars,
    })
}

/// 模型信息（含脱敏后的 baseUrl / 采样参数）
fn describe_model(model: &ModelConfig) -> Value {
    json!({
        "provider": model.provider,
        "id": model.model_id,
        "api": model.api,
        "baseUrl": redact_url(&model.base_url),
        "reasoning": model.reasoning,
        "input": model.input,
        "contextWindow": model.context_window,
        "maxTokens": model.max_tokens,
        "samplingParams": model.sampling_params.as_ref().map(redact_json_value),
        "providerRouting": model.provider_routing.as_ref().map(redact_json_value),
        "thinkingLevelMap": model.thinking_level_map,
        "allowedFallbackModels": model
            .allowed_fallback_models
            .iter()
            .map(|f| json!({ "provider": f.provider, "model": f.model }))
            .collect::<Vec<Value>>(),
    })
}

/// provider 信息（登录注册表 + 本地凭据类型，绝不含密钥本身）
fn describe_provider(provider: &str) -> Value {
    let info = login_registry::LOGIN_PROVIDERS
        .iter()
        .find(|p| p.id == provider);

    let mut auth_types: Vec<&str> = Vec::new();
    if info.is_some_and(|p| p.supports_api_key) {
        auth_types.push("api_key");
    }

    if info.is_some_and(|p| p.is_subscription) {
        auth_types.push("oauth");
    }

    json!({
        "id": provider,
        "name": info.map(|p| p.name),
        "baseUrl": info.map(|p| redact_url(p.base_url)),
        "authTypes": auth_types,
        "configured": auth::resolve_api_key(provider, None).is_some(),
        "usingOAuth": auth::read_oauth_credential(provider).is_some(),
    })
}

/// 已注册扩展（名称 / 启用状态 / 声明模式）
fn describe_extensions() -> Value {
    Value::Array(
        core::extensions::panel_entries()
            .into_iter()
            .map(|(name, enabled, modes)| {
                json!({
                    "name": name,
                    "enabled": enabled,
                    "modes": modes.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

/// 读取并对 settings.json 脱敏（文件缺失/不可解析 → `null`）。
/// `deviceId` 是本机安装标识：直接从报告里移除。
fn redacted_settings_file(path: &Path) -> Value {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Value::Null;
    };

    match serde_json::from_str::<Value>(&text) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.remove("deviceId");
            }
            redact_json_value(&v)
        }
        Err(_) => Value::String("<unparseable>".to_string()),
    }
}

/// `/bug` 收集元数据所需的上下文（由 worker 侧从 Agent/会话取出）
pub struct BugReportContext<'a> {
    /// `/bug <description>` 的用户描述（trim 后为空则视为未提供）
    pub hint: Option<String>,
    /// 当前会话 id（写入 report/diagnostics 的 `sessionId`）
    pub session_id: String,
    /// 当前工作目录（仅在附带 transcript 时收集进 report.json）
    pub cwd: String,
    /// 会话文件绝对路径（脱敏后记录；无文件时为 `None`）
    pub session_file: Option<String>,
    /// 是否附带当前分支的会话记录（`session.jsonl`）
    pub include_session: bool,
    /// 是否附带由当前模型生成的摘要（`summary.md`）
    pub include_summary: bool,
    /// 当前分支的消息条数
    pub message_count: usize,
    /// 当前模型配置（据此描述 provider/model/api 等）
    pub model: &'a ModelConfig,
    /// 当前思考等级（未启用/未知为 `None`）
    pub thinking_level: Option<String>,
}

/// 收集 `report.json` 内容。
pub fn collect_metadata(ctx: &BugReportContext<'_>) -> Value {
    let hint = ctx
        .hint
        .as_deref()
        .map(|h| h.trim())
        .filter(|h| !h.is_empty())
        .map(|h| h.to_string());
    let mut session = json!({
        "id": ctx.session_id,
        "included": ctx.include_session,
        "summaryIncluded": ctx.include_summary,
        "messageCount": ctx.message_count,
        "file": ctx.session_file.as_deref().map(redact_url),
    });

    if ctx.include_session
        && let Some(obj) = session.as_object_mut()
    {
        // cwd 仅在附带 transcript 时收集
        obj.insert("cwd".to_string(), json!(ctx.cwd));
    }

    json!({
        "schemaVersion": BUG_REPORT_SCHEMA_VERSION,
        "id": new_report_id(),
        "createdAt": now_iso(),
        "hint": hint,
        "environment": collect_environment(),
        "session": session,
        "model": describe_model(ctx.model),
        "provider": describe_provider(&ctx.model.provider),
        "thinkingLevel": ctx.thinking_level,
        "extensions": describe_extensions(),
        "settings": {
            "global": redacted_settings_file(&agent_dir().join("settings.json")),
            "project": redacted_settings_file(&Path::new(&ctx.cwd).join(PROJECT_SCOPE_NAME).join("settings.json")),
        },
    })
}

/// 收集 `diagnostics.json` 内容：失败的 assistant 轮次 + 崩溃记录（不含对话正文）。
pub fn collect_diagnostics(session: Option<&Session>, crashes: &[CrashRecord]) -> Value {
    let session_id = session
        .map(|s| s.session_id.clone())
        .unwrap_or_else(|| "<none>".to_string());
    let entries: Vec<Value> = session.map(|s| s.get_entries()).unwrap_or_default();
    let mut assistant: Vec<Value> = Vec::new();
    let mut assistant_message_count = 0usize;

    for entry in &entries {
        if entry.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }

        let msg = entry.get("message").cloned().unwrap_or(Value::Null);
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }

        assistant_message_count += 1;
        let diagnostics = msg
            .get("diagnostics")
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or(Value::Null);
        let stop_reason = msg.get("stopReason").and_then(|v| v.as_str());
        let error_message = msg.get("errorMessage").and_then(|v| v.as_str());
        let interesting = !diagnostics.is_null()
            || matches!(stop_reason, Some("error") | Some("aborted"))
            || error_message.is_some_and(|m| !m.is_empty());

        if !interesting {
            continue;
        }

        let mut item = json!({
            "entryId": entry.get("id"),
            "timestamp": entry.get("timestamp"),
            "provider": msg.get("provider"),
            "model": msg.get("model"),
            "api": msg.get("api"),
            "stopReason": stop_reason,
            "diagnostics": diagnostics,
        });

        if let Some(obj) = item.as_object_mut() {
            if let Some(raw) = msg.get("rawStopReason")
                && !raw.is_null()
            {
                obj.insert("rawStopReason".to_string(), raw.clone());
            }
            if let Some(em) = error_message {
                obj.insert("errorMessage".to_string(), json!(em));
            }
        }
        assistant.push(item);
    }

    json!({
        "schemaVersion": BUG_REPORT_SCHEMA_VERSION,
        "sessionId": session_id,
        "entryCount": entries.len(),
        "assistantMessageCount": assistant_message_count,
        "assistant": assistant,
        "crashes": crashes
            .iter()
            .map(|c| {
                let mut v = serde_json::to_value(c).unwrap_or(Value::Null);
                if let Some(obj) = v.as_object_mut() {
                    obj.remove("notified"); // `notified` 是本地提示状态，不进报告
                }
                v
            })
            .collect::<Vec<_>>(),
    })
}

/// 当前分支的会话记录（header + 分支 mutations，每行一个 JSON），可被 `/import` 还原。
pub fn serialize_session_branch(session: &Session) -> String {
    session.serialize_branch_jsonl()
}

/// 生成摘要请求：`(system_prompt, user_prompt, max_tokens)`。
///
/// 取上下文窗口 60% 作为 transcript 预算，从最后一条往前挑消息（对齐 pi）。
pub fn build_summary_request(
    messages: &[AgentMessage],
    hint: Option<&str>,
    model: &ModelConfig,
) -> (String, String, Option<u32>) {
    let context_window = if model.context_window > 0 {
        model.context_window
    } else {
        128_000
    };

    let budget = (context_window as f64 * 0.6).floor() as u32;
    let selected = select_recent_messages(messages, budget);
    let conversation = compaction::serialize_conversation(&selected);

    let mut parts: Vec<String> = Vec::new();
    if selected.len() < messages.len() {
        parts.push(format!(
            "Note: only the last {} of {} messages are shown.",
            selected.len(),
            messages.len()
        ));
    }

    parts.push(format!("<conversation>\n{conversation}\n</conversation>"));
    if let Some(hint) = hint.map(|h| h.trim()).filter(|h| !h.is_empty()) {
        parts.push(format!("<user-report>\n{hint}\n</user-report>"));
    }
    parts.push(BUG_SUMMARY_INSTRUCTIONS.to_string());

    let max_tokens = Some(model.max_tokens.unwrap_or(u32::MAX).min(4096));
    (
        BUG_SUMMARY_SYSTEM_PROMPT.to_string(),
        parts.join("\n\n"),
        max_tokens,
    )
}

/// 从尾部往前挑选消息，使估算 token 不超过 `budget`（至少保留最后一条）。
fn select_recent_messages(messages: &[AgentMessage], budget: u32) -> Vec<AgentMessage> {
    let mut selected: Vec<AgentMessage> = Vec::new();
    let mut tokens = 0u32;

    for message in messages.iter().rev() {
        let next = compaction::estimate_tokens(message);
        if !selected.is_empty() && tokens.saturating_add(next) > budget {
            break;
        }
        selected.push(message.clone());
        tokens = tokens.saturating_add(next);
    }
    selected.reverse();
    selected
}

/// 报告 bundle（导出 zip 的输入）
pub struct BugReportBundle {
    /// `report.json` 内容（环境/会话/模型/设置元数据，已脱敏）
    pub metadata: Value,
    /// `diagnostics.json` 内容（失败轮次 + 崩溃记录，不含对话正文）
    pub diagnostics: Value,
    /// 附带 transcript 时的分支 JSONL
    pub session_jsonl: Option<String>,
    /// 不附 transcript 时由模型生成的摘要
    pub summary: Option<String>,
}

/// [`build_bundle`] 的输入：元数据上下文 + 会话 + 崩溃记录 + 已生成的摘要
pub struct BundleInput<'a> {
    /// 元数据上下文（描述、会话、模型、两个开关）
    pub ctx: BugReportContext<'a>,
    /// 当前会话（无会话时为 `None`）
    pub session: Option<&'a Session>,
    /// 待附带的崩溃记录
    pub crashes: &'a [CrashRecord],
    /// 已生成的报告摘要（不生成摘要时为 `None`）
    pub summary: Option<String>,
}

/// 组装完整 bundle（worker 侧唯一入口；zip 内容与 report.json/diagnostics.json 的形状都在这里固定）。
pub fn build_bundle(input: BundleInput<'_>) -> BugReportBundle {
    let BundleInput {
        ctx,
        session,
        crashes,
        summary,
    } = input;

    BugReportBundle {
        metadata: collect_metadata(&ctx),
        diagnostics: collect_diagnostics(session, crashes),
        session_jsonl: if ctx.include_session {
            session.map(serialize_session_branch)
        } else {
            None
        },
        summary,
    }
}

/// 归档文件名：`{APP_NAME}-bug-report-<id>.zip`
pub fn archive_file_name(id: &str) -> String {
    format!("{APP_NAME}-bug-report-{id}.zip")
}

/// 归档条目（report.json / diagnostics.json / 可选 session.jsonl、summary.md）
pub fn bundle_entries(bundle: &BugReportBundle) -> Vec<ZipEntry> {
    let pretty = |v: &Value| format!("{}\n", serde_json::to_string_pretty(v).unwrap_or_default());
    let mut entries = vec![
        ZipEntry::text("report.json", pretty(&bundle.metadata)),
        ZipEntry::text("diagnostics.json", pretty(&bundle.diagnostics)),
    ];

    if let Some(jsonl) = &bundle.session_jsonl {
        entries.push(ZipEntry::text("session.jsonl", jsonl.clone()));
    }

    if let Some(summary) = &bundle.summary {
        let text = if summary.ends_with('\n') {
            summary.clone()
        } else {
            format!("{summary}\n")
        };
        entries.push(ZipEntry::text("summary.md", text));
    }
    entries
}

/// 写出 zip 归档。
pub fn write_bug_report_archive(bundle: &BugReportBundle, path: &Path) -> Result<()> {
    write_zip(path, &bundle_entries(bundle))
}

/// 报告 id：`bug-<ms>-<8 位随机>`
pub fn new_report_id() -> String {
    let mut b = [0u8; 4];
    if getrandom::fill(&mut b).is_err() {
        b.copy_from_slice(&(now_ms() as u32).to_be_bytes());
    }

    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("bug-{}-{}", now_ms(), hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::ContentBlock;

    #[test]
    fn sensitive_keys_match_camel_and_snake_forms() {
        for key in [
            "apiKey",
            "api_key",
            "API-KEY",
            "apikey",
            "sessionToken",
            "access-token",
            "password",
            "passwd",
            "credential",
            "Authorization",
            "cookie",
            "mySecret",
        ] {
            assert!(is_sensitive_key(key), "{key} 应判为敏感");
        }
        for key in [
            "key",
            "theme",
            "model",
            "tokens",
            "inputTokens",
            "cursor",
            "keybindings",
        ] {
            assert!(!is_sensitive_key(key), "{key} 不应判为敏感");
        }
    }

    #[test]
    fn redact_url_strips_userinfo_and_secrets() {
        assert_eq!(
            redact_url("https://user:pw@example.com/v1?api_key=abc&x=1"),
            "https://example.com/v1?api_key=%3Credacted%3E&x=1"
        );
        // 无敏感内容 → 原样（不做无谓的规范化）
        assert_eq!(
            redact_url("https://example.com/v1?x=1"),
            "https://example.com/v1?x=1"
        );
        // 非 URL → 原样
        assert_eq!(redact_url("/tmp/some dir"), "/tmp/some dir");
    }

    #[test]
    fn redact_json_recurses_and_keeps_structure() {
        let v = json!({
            "apiKey": "sk-live-123",
            "nested": { "token": "t", "url": "https://x.example.com/?token=zz", "keep": 1 },
            "list": [ { "cookie": "c" } ],
        });
        let out = redact_json_value(&v);
        assert_eq!(out["apiKey"], REDACTED);
        assert_eq!(out["nested"]["token"], REDACTED);
        assert_eq!(
            out["nested"]["url"],
            "https://x.example.com/?token=%3Credacted%3E"
        );
        assert_eq!(out["nested"]["keep"], 1);
        assert_eq!(out["list"][0]["cookie"], REDACTED);
    }

    #[test]
    fn environment_contains_no_values() {
        // SAFETY: 测试内串行设置进程环境（仅本测试使用该名字空间）
        unsafe { std::env::set_var("PRUX_TEST_SECRET_VALUE", "hunter2") };
        let env = collect_environment();
        let text = env.to_string();
        assert!(text.contains("PRUX_TEST_SECRET_VALUE"), "{text}");
        assert!(!text.contains("hunter2"), "环境变量值不得进入报告");
        assert_eq!(env["app"], APP_NAME);
        assert_eq!(env["version"], VERSION);
        assert!(env["pruxEnvironmentVariables"].is_array());
        unsafe { std::env::remove_var("PRUX_TEST_SECRET_VALUE") };
    }

    #[test]
    fn summary_request_selects_recent_messages_and_notes_truncation() {
        let mut model = test_model();
        // 两条超长消息 + 一条短消息：预算内只装得下最后一条
        let long = |n: u32| AgentMessage {
            role: "user".into(),
            thinking_level: None,
            content: vec![ContentBlock::Text {
                text: format!(
                    "{}marker{n}",
                    "the quick brown fox jumps over the lazy dog ".repeat(500)
                ),
                text_signature: None,
            }],
            ..AgentMessage::user_text("")
        };
        let messages = vec![long(1), long(2), AgentMessage::user_text("tail question")];

        let (system, prompt, max_tokens) =
            build_summary_request(&messages, Some("crash on start"), &model);
        assert_eq!(system, BUG_SUMMARY_SYSTEM_PROMPT);
        assert!(
            prompt.contains("Note: only the last 1 of 3 messages are shown."),
            "{prompt}"
        );
        assert!(prompt.contains("tail question"));
        assert!(!prompt.contains("marker1"));
        assert!(prompt.contains("<user-report>\ncrash on start\n</user-report>"));
        assert!(prompt.contains("## Steps to reproduce"));
        assert_eq!(max_tokens, Some(4096));

        // 无截断时不输出 Note 行；无 hint 时不输出 user-report 段
        model.context_window = 10_000_000;
        let (_, prompt, _) = build_summary_request(&messages, None, &model);
        assert!(!prompt.contains("Note: only the last"));
        assert!(!prompt.contains("<user-report>"));
        assert!(prompt.contains("marker1") && prompt.contains("tail question"));
    }

    #[test]
    fn bundle_entries_shape_and_archive() {
        let metadata = json!({ "id": "bug-1-aaaa", "apiKey": "secret" });
        let bundle = BugReportBundle {
            metadata: redact_json_value(&metadata),
            diagnostics: json!({ "entryCount": 0 }),
            session_jsonl: Some("{\"type\":\"session\"}\n".to_string()),
            summary: Some("no trailing newline".to_string()),
        };
        let entries = bundle_entries(&bundle);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "report.json",
                "diagnostics.json",
                "session.jsonl",
                "summary.md"
            ]
        );
        let report = String::from_utf8(entries[0].data.clone()).unwrap();
        assert!(report.contains(REDACTED));
        assert!(!report.contains("secret"));
        let summary = String::from_utf8(entries[3].data.clone()).unwrap();
        assert!(summary.ends_with('\n'));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(archive_file_name("bug-1-aaaa"));
        write_bug_report_archive(&bundle, &path).unwrap();
        assert!(path.exists());
        assert_eq!(
            archive_file_name("x"),
            format!("{APP_NAME}-bug-report-x.zip")
        );

        // 无可选文件时只有两个条目
        let bare = BugReportBundle {
            metadata: json!({}),
            diagnostics: json!({}),
            session_jsonl: None,
            summary: None,
        };
        assert_eq!(bundle_entries(&bare).len(), 2);
    }

    #[test]
    fn report_id_is_unique_and_prefixed() {
        let a = new_report_id();
        let b = new_report_id();
        assert!(a.starts_with("bug-"));
        assert_ne!(a, b);
    }

    /// 测试用模型配置（anthropic，小上下文窗口便于触发摘要裁剪）
    fn test_model() -> ModelConfig {
        ModelConfig {
            model_type: Default::default(),
            provider: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key: String::new(),
            api: "anthropic-messages".into(),
            output: Vec::new(),
            input: vec!["text".into()],
            reasoning: false,
            max_tokens: Some(64_000),
            temperature: None,
            context_window: 1000,
            thinking_format: String::new(),
            supports_reasoning_effort: false,
            thinking_level_map: None,
            auth_header: false,
            image_resize: Default::default(),
            allowed_fallback_models: Vec::new(),
            supports_developer_role: false,
            requires_reasoning_content_on_assistant_messages: false,
            max_tokens_field: "max_tokens".into(),
            cost: None,
            max_retry_delay_ms: None,
            thinking_budgets: None,
            sampling_params: None,
            sampling_params_by_thinking_level: None,
            provider_routing: None,
            session_id: None,
            supports_usage_in_streaming: true,
            supports_store: true,
            supports_finish_reason: true,
            requires_assistant_after_tool_result: false,
            supports_strict_mode: false,
            send_session_affinity_headers: None,
            session_affinity_format: None,
        }
    }

    /// 元数据从真实 AgentDir/settings.json 收集并脱敏（含 model/provider/thinkingLevel）
    #[test]
    fn metadata_redacts_settings_and_describes_model() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        std::fs::write(
            agent_dir().join("settings.json"),
            r#"{"theme":"dark","apiKey":"sk-live-1","nested":{"token":"t"},"defaultModel":"claude","deviceId":"3f2504e0-4f89-41d3-9a0c-0305e82c3301"}"#,
        )
        .unwrap();
        let model = test_model();

        let ctx = BugReportContext {
            hint: Some("  boom  ".to_string()),
            session_id: "sess-1".to_string(),
            cwd: "/tmp/proj".to_string(),
            session_file: Some("/tmp/proj/s.jsonl".to_string()),
            include_session: true,
            include_summary: false,
            message_count: 3,
            model: &model,
            thinking_level: Some("high".to_string()),
        };
        let meta = collect_metadata(&ctx);

        assert!(meta["id"].as_str().unwrap().starts_with("bug-"));
        assert_eq!(meta["hint"], "boom", "描述应 trim");
        assert_eq!(meta["schemaVersion"], BUG_REPORT_SCHEMA_VERSION);
        assert_eq!(meta["thinkingLevel"], "high");
        assert_eq!(meta["session"]["cwd"], "/tmp/proj");
        assert_eq!(meta["session"]["messageCount"], 3);
        assert!(meta["session"]["summaryIncluded"] == false);
        assert_eq!(meta["model"]["provider"], "anthropic");
        assert_eq!(meta["model"]["id"], "claude-sonnet-4-5");
        assert_eq!(meta["provider"]["id"], "anthropic");
        assert_eq!(meta["provider"]["authTypes"][1], "oauth");
        // 设置项脱敏：密钥类键被替换，其余保留
        assert_eq!(meta["settings"]["global"]["apiKey"], REDACTED);
        assert_eq!(meta["settings"]["global"]["nested"]["token"], REDACTED);
        assert_eq!(meta["settings"]["global"]["theme"], "dark");
        // 本机安装标识不进入报告
        assert!(meta["settings"]["global"].get("deviceId").is_none());
        // 未信任/不存在的项目设置 → null
        assert_eq!(meta["settings"]["project"], Value::Null);
        // 报告整体不含明文密钥
        assert!(!meta.to_string().contains("sk-live-1"));

        // 不附 transcript：不收集 cwd
        let ctx2 = BugReportContext {
            include_session: false,
            ..ctx
        };
        let meta2 = collect_metadata(&ctx2);
        assert!(meta2["session"].get("cwd").is_none());
        assert_eq!(meta2["session"]["included"], false);
    }

    /// 分支序列化可被 `/import` 还原；诊断只收集出错/中止的 assistant 轮次
    #[test]
    fn branch_jsonl_roundtrips_and_diagnostics_only_lists_failures() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let mut session = crate::core::session_manager::Session::create(
            "/tmp/proj",
            Some(dir.path().into()),
            true,
        )
        .unwrap();
        session.append_message(&AgentMessage::user_text("hello"));
        let ok = {
            let mut m = AgentMessage::user_text("");
            m.role = "assistant".into();
            m.content = vec![ContentBlock::Text {
                text: "fine".into(),
                text_signature: None,
            }];
            m.provider = Some("anthropic".into());
            m.model = Some("claude-sonnet-4-5".into());
            m.api = Some("anthropic-messages".into());
            m.stop_reason = Some("endTurn".into());
            m
        };
        session.append_message(&ok);
        let failed = {
            let mut m = AgentMessage::user_text("");
            m.role = "assistant".into();
            m.content = vec![ContentBlock::Text {
                text: "".into(),
                text_signature: None,
            }];
            m.provider = Some("anthropic".into());
            m.model = Some("claude-sonnet-4-5".into());
            m.stop_reason = Some("error".into());
            m.error_message = Some("provider exploded".into());
            m
        };
        session.append_message(&failed);

        // 分支 JSONL：header + 3 条 entry + 尾郎 lane 指针，可被 Session::open 还原
        let jsonl = serialize_session_branch(&session);
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 5, "{jsonl}");
        assert!(lines[0].contains("\"kind\":\"header\""), "{}", lines[0]);
        assert!(lines[4].contains("\"kind\":\"lane\""), "{}", lines[4]);
        for line in &lines {
            serde_json::from_str::<Value>(line).expect("每行都是合法 JSON");
        }
        let path = dir.path().join("branch.jsonl");
        std::fs::write(&path, &jsonl).unwrap();
        let reopened = crate::core::session_manager::Session::open(path.to_str().unwrap()).unwrap();
        assert_eq!(reopened.get_entries().len(), 3, "还原后 entry 数一致");

        // 诊断：只有出错的那条进入 assistant 列表
        let diag = collect_diagnostics(Some(&session), &[]);
        assert_eq!(diag["sessionId"], session.session_id);
        assert_eq!(diag["entryCount"], 3);
        assert_eq!(diag["assistantMessageCount"], 2);
        let assistant = diag["assistant"].as_array().unwrap();
        assert_eq!(assistant.len(), 1, "{diag}");
        assert_eq!(assistant[0]["stopReason"], "error");
        assert_eq!(assistant[0]["errorMessage"], "provider exploded");
        assert_eq!(assistant[0]["provider"], "anthropic");
        assert!(assistant[0]["entryId"].is_string());

        // 无会话 → 空诊断，不 panic
        let empty = collect_diagnostics(None, &[]);
        assert_eq!(empty["sessionId"], "<none>");
        assert_eq!(empty["entryCount"], 0);
    }

    /// 完整 bundle：元数据 + 诊断（含 crash）+ transcript + 摘要，写出后可解压
    #[test]
    fn build_bundle_assembles_all_parts_and_writes_archive() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let mut session = crate::core::session_manager::Session::create(
            "/tmp/proj",
            Some(dir.path().into()),
            true,
        )
        .unwrap();
        session.append_message(&AgentMessage::user_text("hello"));

        let crash = CrashRecord {
            timestamp: crate::utils::time::now_iso(),
            version: VERSION.to_string(),
            kind: "uncaught_exception".to_string(),
            message: "panic!".to_string(),
            stack: Some("src/main.rs:1:1".to_string()),
            session_file: None,
            cwd: "/tmp/proj".to_string(),
            notified: true,
        };
        let model = test_model();
        let ctx = BugReportContext {
            hint: Some("crash".to_string()),
            session_id: session.session_id.clone(),
            cwd: "/tmp/proj".to_string(),
            session_file: None,
            include_session: true,
            include_summary: true,
            message_count: 1,
            model: &model,
            thinking_level: None,
        };

        let bundle = build_bundle(BundleInput {
            ctx,
            session: Some(&session),
            crashes: std::slice::from_ref(&crash),
            summary: Some("# summary".to_string()),
        });

        let names: Vec<String> = bundle_entries(&bundle)
            .iter()
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(
            names,
            vec![
                "report.json".to_string(),
                "diagnostics.json".to_string(),
                "session.jsonl".to_string(),
                "summary.md".to_string()
            ]
        );
        assert!(
            bundle
                .session_jsonl
                .as_deref()
                .unwrap()
                .contains("\"kind\":\"header\"")
        );
        assert_eq!(bundle.diagnostics["crashes"][0]["message"], "panic!");
        // 崩溃记录的本地提示状态不进报告
        assert!(bundle.diagnostics["crashes"][0].get("notified").is_none());
        assert_eq!(bundle.metadata["session"]["summaryIncluded"], true);

        let path = dir
            .path()
            .join(archive_file_name(bundle.metadata["id"].as_str().unwrap()));
        write_bug_report_archive(&bundle, &path).unwrap();
        assert!(path.exists());
        // 归档可被标准 unzip 列出四个条目
        if std::process::Command::new("unzip")
            .arg("-v")
            .output()
            .is_ok()
        {
            let out = std::process::Command::new("unzip")
                .arg("-l")
                .arg(&path)
                .output()
                .unwrap();
            let listing = String::from_utf8_lossy(&out.stdout);
            for name in [
                "report.json",
                "diagnostics.json",
                "session.jsonl",
                "summary.md",
            ] {
                assert!(listing.contains(name), "{listing}");
            }
        }
    }
}
