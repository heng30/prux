//! ACP 服务端主循环：读 stdin 的 JSON-RPC 消息 → 分发 → 把 agent 事件翻成 `session/update`。
//!
//! # 为什么读和跑要并行
//!
//! `session/prompt` 会阻塞到整个回合（含工具循环）结束，而 `session/cancel` 是**同一条
//! stdin 流**上的通知。只有一个同步主循环时，取消永远排在一个慢回合后面读不到。
//! 因此 `session/prompt` 处理期间用 `select!` 同时听 stdin：取消即时置位 abort 信号，
//! 其它请求直接回「正忙」，不排队。

use super::protocol::{self, Incoming};
use crate::core::{agent_session::Agent, provider::AgentMessage};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};

/// stdin 行流（主循环与「回合内听取消」共用）。
type StdinLines = Lines<BufReader<Stdin>>;

/// 进程没有会话文件时的协议级会话标识（`--no-session`）。
const EPHEMERAL_SESSION_ID: &str = "prux-acp";

/// 跑 ACP 服务端，直到 stdin 关闭。返回进程退出码。
pub(super) async fn run(agent: Agent) -> i32 {
    let mut agent = agent;
    let session_id = agent
        .session
        .as_ref()
        .map(|s| s.session_id.clone())
        .unwrap_or_else(|| EPHEMERAL_SESSION_ID.to_string());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return 0,
            Err(e) => {
                eprintln!(
                    "{APP_NAME}: acp stdin error: {e}",
                    APP_NAME = crate::APP_NAME
                );
                return 1;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let Some(msg) = Incoming::parse(&line) else {
            protocol::respond_error(&Value::Null, protocol::ERROR_PARSE, "parse error");
            continue;
        };
        let Some(method) = msg.method.clone() else {
            // 客户端发来的响应：prux 不发起请求，忽略。
            continue;
        };

        match method.as_str() {
            "initialize" => {
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
                handle_prompt(&mut agent, &mut lines, &session_id, &id, msg.params).await;
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

/// 跑一个回合：挂事件 sink → 驱动 `prompt_messages` → 期间监听取消 → 回 `stopReason`。
async fn handle_prompt(
    agent: &mut Agent,
    lines: &mut StdinLines,
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

    loop {
        tokio::select! {
            result = &mut turn => {
                if let Err(e) = result {
                    protocol::respond_error(id, protocol::ERROR_INTERNAL, &e.to_string());
                    return;
                }
                break;
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        let Some(msg) = Incoming::parse(&line) else { continue };
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
                    // stdin 关闭：客户端进程已走，丢弃本轮 future 后退出（同 TUI 的硬中断路径）。
                    _ => {
                        abort.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
        }
    }

    protocol::respond(id, json!({"stopReason": stop_reason}));
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
}
