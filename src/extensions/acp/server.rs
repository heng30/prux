//! ACP 服务端主循环：读 stdin 的 JSON-RPC 消息 → 分发 → 把 agent 事件翻成 `session/update`。
//!
//! # 为什么 stdin 由独立任务读
//!
//! `session/prompt` 会阻塞到整个回合（含工具循环）结束，而 `session/cancel` 是**同一条
//! stdin 流**上的通知。只有一个同步主循环时，取消永远排在一个慢回合后面读不到。
//! 因此 stdin 的读取单独起一个任务，消息经 channel 交给主循环；客户端对我们发起的
//! 请求的答复（无 `method`）由该任务直接分发到 [`protocol::complete`]，不受主循环忙闲影响。
//!
//! # 交互：借客户端的 UI
//!
//! prux 没有 TUI 时扩展的交互一律失效，ACP 里得借用客户端的界面，两条通道：
//! - `elicitation/create`（form 模式）承载 `ask_user_question` 的问卷；
//! - `session/request_permission` 承载 `ask_prompt` 的通用选择面板（如 permissions 门控）。
//!
//! 两者都要求客户端在 `initialize` 里广告了对应能力，否则保持原来的「非交互」行为。

use super::protocol::{self, Incoming};
use crate::core::{
    agent_session::Agent,
    auth::flush_oauth_refreshes,
    extensions::{
        self, ExtensionUiRequest, FormAnswer, FormRequest, FormResponse, OverlayEvent, SelectOption,
    },
    provider::AgentMessage,
    settings_manager::agent_dir,
};
use serde_json::{Map, Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
};

/// 主循环收到的入站消息。
type Messages = mpsc::UnboundedReceiver<Incoming>;

/// 进程没有会话文件时的协议级会话标识（`--no-session`）。
const EPHEMERAL_SESSION_ID: &str = "prux-acp";

/// 轮询扩展 UI 队列的间隔：ACP 没有「UI 请求就绪」的回调，只能定时取。
const UI_POLL: Duration = Duration::from_millis(50);

/// 表单里自由作答字段的标题（与 TUI 覆盖层同一句话）。
const TYPE_SOMETHING: &str = "Type something.";

/// `initialize` 能力日志的大小上限：超过就在下次写入时整份重写。
const LOG_MAX_BYTES: u64 = 64 * 1024;

/// 关停阶段：0 = 正常，1 = 关停已开始（在途工作不再被打断），2 = 强制退出。
///
/// stdin EOF 与 SIGTERM 常常同时到达（Paseo / systemd 就这么干）：每个信号都直接丢下 future
/// 会让 MCP token 刷新、会话落盘这类收尾被截断。所以首个 EOF / 信号只置位，第二个才立即退。
static SHUTDOWN: AtomicU8 = AtomicU8::new(0);

/// 把关停推进到「已开始」；返回是否由本次调用推进（`false` 表示早在关停中）。
fn initiate_shutdown() -> bool {
    SHUTDOWN
        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// 监听 SIGTERM / SIGINT：首个信号不打断收尾，第二个才 `exit`。
fn spawn_signal_watch() {
    tokio::spawn(async {
        loop {
            wait_for_signal().await;
            if !initiate_shutdown() {
                std::process::exit(0);
            }
        }
    });
}

/// 等下一个终止信号（Unix 上含 SIGTERM，各平台都含 Ctrl+C）。
#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// 等下一个终止信号（非 Unix：只有 Ctrl+C）。
#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// 跑 ACP 服务端，直到 stdin 关闭。返回进程退出码。
pub(super) async fn run(agent: Agent) -> i32 {
    spawn_signal_watch();

    let mut agent = agent;
    let session_id = agent
        .session
        .as_ref()
        .map(|s| s.session_id.clone())
        .unwrap_or_else(|| EPHEMERAL_SESSION_ID.to_string());
    let mut messages = spawn_reader();

    loop {
        let Some(msg) = messages.recv().await else {
            // stdin 关闭：等在途 OAuth 刷新落盘再退出，别让轮转后的 token 丢掉
            flush_oauth_refreshes(Duration::from_secs(5)).await;
            return 0;
        };
        let Some(method) = msg.method.clone() else {
            // 客户端答复已由读任务分发，到不了这里。
            continue;
        };

        match method.as_str() {
            "initialize" => {
                log_initialize(msg.params.as_ref());
                // 客户端能渲染什么只有 initialize 说：有 elicitation 就用表单，
                // 只有 `session/request_permission`（ACP 必备方法）就逐题单选。
                install_host(session_id.clone(), supports_forms(msg.params.as_ref()));
                extensions::set_ui_ready(true);
                if let Some(id) = &msg.id {
                    protocol::respond(id, protocol::initialize_result());
                }
            }
            // prux 没有远程认证流程：模型凭据来自本机配置。
            "authenticate" => {
                if let Some(id) = &msg.id {
                    protocol::respond(id, json!({}));
                }
            }
            // 单会话：Agent 在 bootstrap 里已就绪，编辑器开的会话就是它。
            "session/new" => {
                if let Some(id) = &msg.id {
                    protocol::respond(id, json!({"sessionId": session_id}));
                }
            }
            // 空闲期的取消没有回合可停。
            "session/cancel" => {}
            "session/prompt" => {
                let Some(id) = msg.id.clone() else {
                    continue;
                };
                handle_prompt(&mut agent, &mut messages, &session_id, &id, msg.params).await;
            }
            other => {
                if let Some(id) = &msg.id {
                    protocol::respond_error(
                        id,
                        protocol::ERROR_METHOD_NOT_FOUND,
                        &format!("unsupported ACP method `{other}`"),
                    );
                }
            }
        }
    }
}

/// 起一个读 stdin 的任务：请求进 channel，客户端答复直接分发到等待表。
fn spawn_reader() -> Messages {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Some(msg) = Incoming::parse(&line) else {
                        protocol::respond_error(&Value::Null, protocol::ERROR_PARSE, "parse error");
                        continue;
                    };
                    if msg.method.is_none() {
                        protocol::complete(
                            msg.id.as_ref().unwrap_or(&Value::Null),
                            msg.result.clone(),
                        );
                        continue;
                    }
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(e) => {
                    eprintln!(
                        "{APP_NAME}: acp stdin error: {e}",
                        APP_NAME = crate::APP_NAME
                    );
                    return;
                }
            }
        }
    });
    rx
}

/// 客户端在 `initialize` 里广告了 elicitation 的 form 模式吗。
///
/// v1 把能力放在 `clientCapabilities`，v2 改叫 `capabilities`；两处都看。
/// `form` 必须是显式非 null 才算支持（`{}` 表示支持，`null` 表示不支持）。
fn supports_forms(params: Option<&Value>) -> bool {
    let caps = params.and_then(|p| {
        p.get("clientCapabilities")
            .or_else(|| p.get("capabilities"))
    });
    caps.and_then(|c| c.get("elicitation"))
        .and_then(|e| e.get("form"))
        .is_some_and(|form| !form.is_null())
}

/// 跑一个回合：挂事件 sink → 驱动 `prompt_messages` → 期间监听取消与扩展 UI 请求 → 回 `stopReason`。
async fn handle_prompt(
    agent: &mut Agent,
    messages: &mut Messages,
    session_id: &str,
    id: &Value,
    params: Option<Value>,
) {
    let Some(text) = prompt_text(params) else {
        protocol::respond_error(
            id,
            protocol::ERROR_INVALID_PARAMS,
            "prompt must contain at least one non-empty text content block",
        );
        return;
    };

    // 取消信号必须在借用 agent 之前拿到：future 持有 `&mut agent` 的整段时间里，
    // 只有这个 Arc 还能让回合停下来。
    let abort = agent.abort_handle();
    let sink_session = session_id.to_string();
    agent.attach_json_sink(Box::new(move |event| {
        if let Some(update) = map_event(&event) {
            protocol::notify(
                "session/update",
                protocol::session_update(&sink_session, update),
            );
        }
    }));

    let mut turn = Box::pin(agent.prompt_messages(vec![AgentMessage::user_text(&text)]));
    let mut stop_reason = "end_turn";
    // stdin 关闭后不再轮询它：本轮还要跑完，读任务已经结束，继续 select 只会空转。
    let mut messages_closed = false;
    let mut ui_tick = tokio::time::interval(UI_POLL);
    ui_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            result = &mut turn => {
                if let Err(e) = result {
                    protocol::respond_error(id, protocol::ERROR_INTERNAL, &e.to_string());
                    return;
                }
                break;
            }
            msg = messages.recv(), if !messages_closed => {
                let Some(msg) = msg else {
                    // stdin 关闭：首次只置位并让本轮跑完（收尾里还有 MCP token 刷新与会话落盘），
                    // 关停已开始后再次 EOF 才按硬中断丢弃本轮。
                    messages_closed = true;
                    if initiate_shutdown() {
                        continue;
                    }
                    abort.store(true, Ordering::Relaxed);
                    return;
                };
                match msg.method.as_deref() {
                    Some("session/cancel") => {
                        abort.store(true, Ordering::Relaxed);
                        stop_reason = "cancelled";
                    }
                    // 回合内的其它请求不排队：直接回忙，客户端自己重试。
                    Some(other) => {
                        if let Some(pending) = msg.id {
                            protocol::respond_error(
                                &pending,
                                protocol::ERROR_INVALID_REQUEST,
                                &format!("a prompt is already active; `{other}` is unavailable now"),
                            );
                        }
                    }
                    None => {}
                }
            }
            _ = ui_tick.tick() => {
                // 扩展在等用户作答：借客户端的界面。等待期间 stdin 仍由读任务消费，
                // 客户端取消回合时按规范必须当场回 `cancelled`，所以这里不会卡住。
                while let Some(request) = extensions::take_pending_ui() {
                    match request {
                        ExtensionUiRequest::Select { id, title, options } => {
                            let choice = ask_permission(session_id, &title, &options).await;
                            extensions::submit_ui_choice(id, choice);
                        }
                        // 覆盖层没有人画，也没人按键：直接送 Closed 让扩展收尾，否则它的
                        // 工具调用会永远等下去（`ask_user_question` 已改走交互宿主）。
                        ExtensionUiRequest::ShowOverlay { id } => {
                            extensions::overlay_event(id, OverlayEvent::Closed);
                        }
                        // 停靠面板 / toast / 状态栏 / 通知都是 TUI 专属展示，丢弃不会阻塞调用方。
                        _ => {}
                    }
                }
            }
        }
    }

    protocol::respond(id, json!({"stopReason": stop_reason}));
}

/// 把通用选择面板送成 `session/request_permission`，等客户端答复；取消或超时回 `None`。
async fn ask_permission(session_id: &str, title: &str, options: &[SelectOption]) -> Option<String> {
    let choices: Vec<(String, String)> = options
        .iter()
        .map(|option| (option.text.clone(), String::new()))
        .collect();
    let index = pick_one(session_id, title, &choices).await?;
    options.get(index).map(|option| option.text.clone())
}

/// 让客户端弹一次单选（借 `session/request_permission`），返回选中项的下标。
///
/// `options` 是 `(标签, 说明)`；说明会拼进展示文案，返回的下标永远对应原数组。
async fn pick_one(session_id: &str, title: &str, options: &[(String, String)]) -> Option<usize> {
    let choices: Vec<Value> = options
        .iter()
        .enumerate()
        .map(|(index, (label, description))| {
            let name = if description.is_empty() {
                label.clone()
            } else {
                format!("{label} — {description}")
            };
            json!({
                "optionId": format!("o{index}"),
                "name": name,
                "kind": permission_kind(label),
            })
        })
        .collect();

    // 超时只兜「客户端永不回包」：上层门控自己 600s 就放弃了，这里晚一点让它的超时先赢，
    // 否则这个 await 会一直占着主循环，整个回合跟着卡住。
    let response = tokio::time::timeout(
        Duration::from_secs(610),
        protocol::request(
            "session/request_permission",
            json!({
                "sessionId": session_id,
                "toolCall": {
                    "toolCallId": format!("prux-ui-{}", extensions::next_ui_id()),
                    "title": title,
                },
                "options": choices,
            }),
        ),
    )
    .await
    .ok()??;
    let outcome = response.get("outcome")?;
    if outcome.get("outcome").and_then(Value::as_str)? != "selected" {
        return None;
    }
    outcome
        .get("optionId")
        .and_then(Value::as_str)?
        .strip_prefix('o')?
        .parse()
        .ok()
}

/// 选项文案 → ACP 的 `kind`（客户端据此选图标与默认焦点）。
fn permission_kind(label: &str) -> &'static str {
    let lower = label.to_ascii_lowercase();
    if lower.contains("deny")
        || lower.contains("reject")
        || lower.contains("cancel")
        || lower.starts_with("no")
    {
        "reject_once"
    } else if lower.contains("always") || lower.contains("session") {
        "allow_always"
    } else {
        "allow_once"
    }
}

/// 把 `initialize` 的能力声明追写进 `agent_dir()/logs/acp.log`。
///
/// 客户端支不支持交互只有它自己说，排查「问不了」必须先看这一行原文。
/// 只记客户端能力，满 [`LOG_MAX_BYTES`] 就整份重写：这是诊断用的滚动日志， 谁也不会去清它。
fn log_initialize(params: Option<&Value>) {
    let dir = agent_dir().join("logs");
    _ = std::fs::create_dir_all(&dir);
    let path = dir.join("acp.log");
    let stale = std::fs::metadata(&path).is_ok_and(|meta| meta.len() > LOG_MAX_BYTES);

    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true);
    if stale {
        options.truncate(true);
    } else {
        options.append(true);
    }

    let line = format!("initialize {}\n", params.cloned().unwrap_or(Value::Null));
    if let Ok(mut file) = options.open(&path) {
        _ = std::io::Write::write_all(&mut file, line.as_bytes());
    }
}

/// 注册交互宿主：有 `elicitation` 能力的客户端渲染表单，否则逐题借权限对话框。
fn install_host(session_id: String, forms: bool) {
    extensions::set_form_host(Some(Arc::new(move |request: FormRequest| {
        let session_id = session_id.clone();
        Box::pin(async move {
            if forms {
                elicit(&session_id, request).await
            } else {
                pick_answers(&session_id, request).await
            }
        })
    })));
}

/// 无 `elicitation` 时的降级：每道单题弹一次权限对话框。
///
/// `session/request_permission` 只能回一个选择，多选和自由输入都表达不了——多选跳过（
/// 调用方会把它列为未作答），备注里说清是哪几题，模型据此再文本补问。
async fn pick_answers(session_id: &str, request: FormRequest) -> Option<FormResponse> {
    let mut answers = Vec::with_capacity(request.questions.len());
    let mut skipped = Vec::new();

    for question in &request.questions {
        if question.multi {
            skipped.push(question.question.clone());
            answers.push(FormAnswer::default());
            continue;
        }
        let options: Vec<(String, String)> = question
            .options
            .iter()
            .map(|option| (option.label.clone(), option.description.clone()))
            .collect();
        let picked = pick_one(session_id, &question.question, &options).await?;
        answers.push(FormAnswer {
            picked: vec![question.options[picked].label.clone()],
            custom: String::new(),
        });
    }

    Some(FormResponse {
        note: if skipped.is_empty() {
            String::new()
        } else {
            format!("client cannot render multi-select: {}", skipped.join("; "))
        },
        answers,
    })
}

/// 注册表单宿主：问卷 → `elicitation/create`（form），答复翻回结构化作答。
/// 把问卷送成一张 elicitation 表单并等作答；用户取消或客户端不支持时返回 `None`。
async fn elicit(session_id: &str, request: FormRequest) -> Option<FormResponse> {
    let FormRequest { message, questions } = request;
    let mut properties = Map::new();

    for (index, question) in questions.iter().enumerate() {
        let options: Vec<Value> = question
            .options
            .iter()
            .map(|option| {
                let title = if option.description.is_empty() {
                    option.label.clone()
                } else {
                    format!("{} — {}", option.label, option.description)
                };
                json!({"const": option.label, "title": title})
            })
            .collect();
        let key = format!("q{index}");
        let mut field = if question.multi {
            json!({"type": "array", "items": {"anyOf": options}})
        } else {
            json!({"type": "string", "oneOf": options})
        };
        field["title"] = json!(question.header);
        field["description"] = json!(question.question);
        properties.insert(key.clone(), field);
        properties.insert(
            format!("{key}_text"),
            json!({
                "type": "string",
                "title": TYPE_SOMETHING,
                "description": "Free-form answer instead of picking an option",
            }),
        );
    }
    properties.insert(
        "note".to_string(),
        json!({"type": "string", "title": "Additional notes"}),
    );

    let response = protocol::request(
        "elicitation/create",
        json!({
            "sessionId": session_id,
            "mode": "form",
            "message": message,
            "requestedSchema": {"type": "object", "properties": properties},
        }),
    )
    .await?;

    if response.get("action").and_then(Value::as_str)? != "accept" {
        return None;
    }
    let content = response.get("content").and_then(Value::as_object);
    let text = |key: &str| {
        content
            .and_then(|c| c.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string()
    };

    let answers = (0..questions.len())
        .map(|index| {
            let key = format!("q{index}");
            let mut picked = Vec::new();
            match content.and_then(|c| c.get(&key)) {
                Some(Value::String(label)) if !label.is_empty() => picked.push(label.clone()),
                Some(Value::Array(labels)) => {
                    picked.extend(labels.iter().filter_map(Value::as_str).map(str::to_string))
                }
                _ => {}
            }
            FormAnswer {
                picked,
                custom: text(&format!("{key}_text")),
            }
        })
        .collect();

    Some(FormResponse {
        answers,
        note: text("note"),
    })
}

/// 取出 prompt 里的文本块（图片/音频块忽略：`initialize` 已声明不支持）。
fn prompt_text(params: Option<Value>) -> Option<String> {
    let blocks = params?.get("prompt")?.as_array()?.clone();
    let mut text = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(chunk) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(chunk);
    }
    (!text.trim().is_empty()).then_some(text)
}

/// prux 的 agent 事件 → 一条 ACP `session/update` 载荷；不映射的事件返回 `None`。
fn map_event(event: &Value) -> Option<Value> {
    // 流式增量被 kernel 包在 `message_update.assistantMessageEvent` 里，先拆一层。
    if event.get("type").and_then(Value::as_str) == Some("message_update") {
        return map_event(event.get("assistantMessageEvent")?);
    }
    let name = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or_default();
    match event.get("type").and_then(Value::as_str)? {
        "text_delta" => Some(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": name("delta")}
        })),
        "thinking_delta" => Some(json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": name("delta")}
        })),
        "tool_execution_start" => Some(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": name("toolCallId"),
            "title": name("toolName"),
            "status": "in_progress",
            "kind": tool_kind(name("toolName")),
            "rawInput": event.get("args").cloned().unwrap_or(Value::Null),
        })),
        "tool_execution_end" => Some(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": name("toolCallId"),
            "status": if event.get("isError").and_then(Value::as_bool).unwrap_or(false) {
                "failed"
            } else {
                "completed"
            },
        })),
        _ => None,
    }
}

/// 工具名 → ACP 的 `tool kind`（客户端据此选图标与折叠方式）。
fn tool_kind(tool: &str) -> &'static str {
    match tool {
        "read" => "read",
        "edit" | "write" => "edit",
        "bash" | "powershell" => "execute",
        "grep" | "find" | "ls" => "search",
        "web_search" | "web_fetch" | "fetch" => "fetch",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 关停闩锁：首次调用推进状态，之后的 EOF / 信号不再推进（第二个信号才硬退）。
    #[test]
    fn shutdown_latch_is_first_wins() {
        assert!(initiate_shutdown());
        assert!(!initiate_shutdown());
    }

    #[test]
    fn prompt_text_joins_text_blocks_and_skips_others() {
        let params = json!({"prompt": [
            {"type": "text", "text": "hello"},
            {"type": "image", "data": "..."},
            {"type": "text", "text": "world"}
        ]});
        assert_eq!(prompt_text(Some(params)).as_deref(), Some("hello\nworld"));
    }

    #[test]
    fn prompt_text_rejects_empty_and_missing_prompts() {
        assert!(prompt_text(None).is_none());
        assert!(prompt_text(Some(json!({"prompt": []}))).is_none());
        assert!(prompt_text(Some(json!({"prompt": [{"type": "text", "text": "  "}]}))).is_none());
    }

    #[test]
    fn text_delta_becomes_an_agent_message_chunk() {
        let wrapped = json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "text_delta", "delta": "hi"}
        });
        let update = map_event(&wrapped).expect("映射");
        assert_eq!(update["sessionUpdate"], json!("agent_message_chunk"));
        assert_eq!(update["content"]["text"], json!("hi"));
    }

    #[test]
    fn tool_events_carry_id_kind_and_final_status() {
        let start = map_event(&json!({
            "type": "tool_execution_start",
            "toolCallId": "c1",
            "toolName": "bash",
            "args": {"command": "ls"}
        }))
        .expect("映射");
        assert_eq!(start["sessionUpdate"], json!("tool_call"));
        assert_eq!(start["toolCallId"], json!("c1"));
        assert_eq!(start["kind"], json!("execute"));
        assert_eq!(start["status"], json!("in_progress"));

        let end = map_event(&json!({
            "type": "tool_execution_end",
            "toolCallId": "c1",
            "isError": true
        }))
        .expect("映射");
        assert_eq!(end["sessionUpdate"], json!("tool_call_update"));
        assert_eq!(end["status"], json!("failed"));
    }

    #[test]
    fn unmapped_events_are_dropped() {
        assert!(map_event(&json!({"type": "agent_settled"})).is_none());
        assert!(map_event(&json!({})).is_none());
    }

    /// `form` 必须显式非 null 才算支持；v1/v2 两种字段名都要认。
    #[test]
    fn only_explicit_form_capability_counts() {
        assert!(supports_forms(Some(&json!({
            "clientCapabilities": {"elicitation": {"form": {}}}
        }))));
        assert!(supports_forms(Some(&json!({
            "capabilities": {"elicitation": {"form": {}}}
        }))));
        assert!(!supports_forms(Some(&json!({
            "clientCapabilities": {"elicitation": {"form": null}}
        }))));
        assert!(!supports_forms(Some(&json!({"clientCapabilities": {}}))));
        assert!(!supports_forms(None));
    }

    /// 多选表达不了：不发请求、直接列为未答，并在备注里点名是哪几题。
    #[test]
    fn multi_select_questions_are_skipped() {
        let request = extensions::FormRequest {
            message: "pick".to_string(),
            questions: vec![extensions::FormQuestion {
                question: "Which?".to_string(),
                header: "Pick".to_string(),
                multi: true,
                options: vec![
                    extensions::FormOption {
                        label: "a".to_string(),
                        description: String::new(),
                        preview: None,
                    },
                    extensions::FormOption {
                        label: "b".to_string(),
                        description: String::new(),
                        preview: None,
                    },
                ],
            }],
        };

        let response = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(pick_answers("s1", request))
            .expect("仍然要返回一份作答");

        assert_eq!(response.answers.len(), 1);
        assert!(response.answers[0].picked.is_empty(), "多选应保持未作答");
        assert!(response.note.contains("multi-select"));
        assert!(response.note.contains("Which?"));
    }

    /// 拒绝类文案要落到 reject 语义上，否则客户端会把「Deny」画成允许。
    #[test]
    fn permission_kind_maps_refusals_to_reject() {
        assert_eq!(permission_kind("Allow once"), "allow_once");
        assert_eq!(permission_kind("Allow for this session"), "allow_always");
        assert_eq!(permission_kind("Deny"), "reject_once");
        assert_eq!(permission_kind("No"), "reject_once");
    }
}
