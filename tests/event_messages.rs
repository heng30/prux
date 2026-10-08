//! core::provider 消息转换集成测试。

use prux::core::provider::completions::{convert_messages, convert_tools};
use prux::core::provider::{AgentMessage, ContentBlock, ModelConfig};

// 角色常量（messages.rs 已移除——生产零使用，测试用字面量）
pub const ROLE_USER: &str = "user";
pub const ROLE_ASSISTANT: &str = "assistant";
pub const ROLE_TOOL_RESULT: &str = "toolResult";
pub const ROLE_BASH_EXECUTION: &str = "bashExecution";
pub const ROLE_BRANCH_SUMMARY: &str = "branchSummary";
pub const ROLE_COMPACTION_SUMMARY: &str = "compactionSummary";

// ---------- provider 消息转换 ----------

fn test_model() -> ModelConfig {
    ModelConfig {
        model_type: Default::default(),
        image_resize: prux::utils::image::ImageResizeLimits::default(),
        allowed_fallback_models: Vec::new(),
        provider: "test".into(),
        model_id: "m".into(),
        base_url: "http://localhost".into(),
        api_key: "k".into(),
        api: "openai-completions".into(),
        output: Vec::new(),
        input: vec!["text".into(), "image".into()],
        reasoning: true,
        max_tokens: Some(100),
        temperature: None,
        context_window: 200_000,
        thinking_format: "default".into(),
        supports_reasoning_effort: false,
        thinking_level_map: None,
        auth_header: false,
        supports_developer_role: false,
        requires_reasoning_content_on_assistant_messages: false,
        max_tokens_field: "max_completion_tokens".into(),
        cost: None,
        session_id: None,
        max_retry_delay_ms: None,
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        provider_routing: None,
        supports_usage_in_streaming: true,
        supports_store: true,
        supports_finish_reason: true,
        requires_assistant_after_tool_result: false,
        supports_strict_mode: false,
        send_session_affinity_headers: None,
        session_affinity_format: None,
        thinking_budgets: None,
    }
}

#[test]
fn convert_user_message_to_openai() {
    let m = test_model();
    let msgs = vec![AgentMessage::user_text("hello")];
    let out = convert_messages(&msgs, "", &m);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["role"], "user");
    assert_eq!(out[0]["content"][0]["text"], "hello");
}

#[test]
fn convert_tool_calls() {
    let m = test_model();
    let mut assistant = AgentMessage::user_text("look at the file");
    assistant.role = "assistant".into();
    assistant.content.push(ContentBlock::ToolCall {
        id: "call_1".into(),
        name: "read".into(),
        arguments: serde_json::json!({ "path": "a.rs" }),
        thought_signature: None,
        namespace: None,
    });
    let out = convert_messages(&[assistant], "", &m);
    // OpenAI 格式：tool_calls 在 assistant 消息顶层
    let calls = out[0]["tool_calls"].as_array().expect("has tool_calls");
    assert_eq!(calls[0]["function"]["name"], "read");
    assert!(
        calls[0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains("a.rs")
    );
}

#[test]
fn convert_compaction_summary_as_user() {
    let m = test_model();
    let mut summary = AgentMessage::user_text("summary");
    summary.role = "compactionSummary".into();
    let out = convert_messages(&[summary], "", &m);
    assert_eq!(out[0]["role"], "user");
    let text = out[0]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("<summary>"));
}

#[test]
fn convert_skips_error_messages() {
    let m = test_model();
    let mut failed = AgentMessage::user_text("");
    failed.role = "assistant".into();
    failed.stop_reason = Some("error".into());
    let out = convert_messages(&[failed], "", &m);
    assert!(out.is_empty());
}

#[test]
fn convert_tools_schema() {
    let defs = vec![(
        "read".to_string(),
        "Read a file".to_string(),
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }),
    )];
    let out = convert_tools(&defs);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["type"], "function");
    assert_eq!(out[0]["function"]["name"], "read");
}

#[test]
fn agent_message_text_and_blocks() {
    let mut msg = AgentMessage::user_text("text");
    msg.content.push(ContentBlock::Thinking {
        thinking: "thinking".into(),
        thinking_signature: None,
        redacted: None,
    });
    assert_eq!(msg.text(), "text");
    assert!(msg.thinking().contains("thinking"));
    assert!(msg.tool_calls().is_empty());
}
