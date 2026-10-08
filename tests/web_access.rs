//! `web-access` 扩展的集成回归：工具开关、注册信息、错误文案与 SSRF 行为。
//!
//! 直接实例化 `WebAccess`（不经全局注册表），避免与 `extensions_registry.rs`
//! 的全局注册状态互相污染。

use prux::core::extensions::{Extension, ToolExecCtx};
use prux::extensions::web_access::WebAccess;
use prux::test_support::AgentDirGuard;
use std::sync::Arc;

fn exec_ctx() -> ToolExecCtx {
    ToolExecCtx {
        execute_tool: prux::core::extensions::unavailable_tool_exec(),
        parent_tool_call_id: None,
        nested_calls: Default::default(),
        session_branch_entries: Default::default(),
        script_tools: Default::default(),
        cwd: ".".to_string(),
        // web_access 的工具不物化子代理，故工厂直接失败
        make_sub_agent: Arc::new(|_| Err("unused".to_string())),
        parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        agent_id: None,
        depth: 0,
        parent_model: None,
        script_call: false,
    }
}

fn tool_names(ext: &WebAccess) -> Vec<String> {
    ext.tools().into_iter().map(|t| t.name).collect()
}

#[test]
fn registers_expected_tools_by_default() {
    let _ad = AgentDirGuard::temp();
    let ext = WebAccess;
    assert_eq!(ext.name(), "web-access");
    assert!(!ext.default_enabled(), "web-access 应默认关闭（按需开启）");
    assert_eq!(
        tool_names(&ext),
        vec![
            "web_search".to_string(),
            "fetch_content".to_string(),
            "get_search_content".to_string()
        ]
    );
    let fork = ext.fork_project().expect("fork info");
    assert_eq!(fork.plugin_name, "pi-web-access");
    assert!(!fork.plugin_version.is_empty());
}

#[test]
fn tool_toggles_disable_individual_tools() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("extensions")).unwrap();
    std::fs::write(
        dir.path().join("extensions/web-search.json"),
        r#"{ "tools": { "fetchContent": { "enabled": false }, "getSearchContent": { "enabled": false } } }"#,
    )
    .unwrap();
    let _ad = AgentDirGuard::set(dir.path());
    let ext = WebAccess;
    assert_eq!(tool_names(&ext), vec!["web_search".to_string()]);

    std::fs::write(
        dir.path().join("extensions/web-search.json"),
        r#"{ "webSearch": { "enabled": false } }"#,
    )
    .unwrap();
    // 顶层 webSearch 关闭同样移除 web_search
    assert!(!tool_names(&ext).contains(&"web_search".to_string()));
}

#[test]
fn corrupted_config_surfaces_error_on_call() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("extensions")).unwrap();
    std::fs::write(dir.path().join("extensions/web-search.json"), "{ not json").unwrap();
    let _ad = AgentDirGuard::set(dir.path());
    let ext = WebAccess;
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime
        .block_on(ext.execute_tool_async(
            "web_search".to_string(),
            serde_json::json!({ "query": "rust" }),
            exec_ctx(),
        ))
        .unwrap_err();
    assert!(err.0.contains("Failed to parse"), "{}", err.0);
}

#[test]
fn web_search_requires_a_query() {
    let _ad = AgentDirGuard::temp();
    let ext = WebAccess;
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime
        .block_on(ext.execute_tool_async(
            "web_search".to_string(),
            serde_json::json!({}),
            exec_ctx(),
        ))
        .unwrap_err();
    assert!(err.0.contains("No query provided"), "{}", err.0);
}

#[test]
fn web_search_reports_when_no_provider_configured() {
    let _ad = AgentDirGuard::temp();
    // provider 可用性会回退读进程环境变量（TAVILY_API_KEY 等），先清空再断言
    let _env = prux::test_support::clear_search_credential_env();
    let ext = WebAccess;
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = runtime
        .block_on(ext.execute_tool_async(
            "web_search".to_string(),
            serde_json::json!({ "query": "rust async", "provider": "tavily" }),
            exec_ctx(),
        ))
        .expect("per-query 错误以文本形式返回（对齐上游）");
    assert!(
        result.text.contains("not configured") && result.text.contains("duckduckgo"),
        "{}",
        result.text
    );
}

#[test]
fn fetch_content_rejects_unsupported_and_missing_url() {
    let _ad = AgentDirGuard::temp();
    let ext = WebAccess;
    let ctx = exec_ctx();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime
        .block_on(ext.execute_tool_async(
            "fetch_content".to_string(),
            serde_json::json!({ "url": "https://example.com", "timestamp": "1:00" }),
            ctx.clone(),
        ))
        .unwrap_err();
    assert!(err.0.contains("timestamp"), "{}", err.0);
    let err = runtime
        .block_on(ext.execute_tool_async("fetch_content".to_string(), serde_json::json!({}), ctx))
        .unwrap_err();
    assert!(err.0.contains("No URL"), "{}", err.0);
}

#[test]
fn fetch_content_blocks_private_addresses() {
    let _ad = AgentDirGuard::temp();
    let ext = WebAccess;
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = runtime
        .block_on(ext.execute_tool_async(
            "fetch_content".to_string(),
            serde_json::json!({ "url": "http://127.0.0.1:8080/secret" }),
            exec_ctx(),
        ))
        .expect("tool returns a tool result with error text");
    assert!(
        result.text.contains("Error") && result.text.contains("SSRF"),
        "{}",
        result.text
    );
}

#[test]
fn get_search_content_errors_are_actionable() {
    let _ad = AgentDirGuard::temp();
    let ext = WebAccess;
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime
        .block_on(ext.execute_tool_async(
            "get_search_content".to_string(),
            serde_json::json!({}),
            exec_ctx(),
        ))
        .unwrap_err();
    assert!(err.0.contains("responseId"), "{}", err.0);
    let err = runtime
        .block_on(ext.execute_tool_async(
            "get_search_content".to_string(),
            serde_json::json!({ "responseId": "abc", "findMode": "fuzzy" }),
            exec_ctx(),
        ))
        .unwrap_err();
    assert!(err.0.contains("requires findText"), "{}", err.0);
    let err = runtime
        .block_on(ext.execute_tool_async(
            "get_search_content".to_string(),
            serde_json::json!({ "responseId": "missing" }),
            exec_ctx(),
        ))
        .unwrap_err();
    assert!(err.0.contains("No stored results"), "{}", err.0);
}

/// 真实网络冒烟（默认 ignored，`cargo test --test web_access -- --ignored` 手动跑）：
/// example.com 正文抓取（严格），以及免密钥 DuckDuckGo 搜索（宽松：
/// 部分网络环境屏蔽 DDG，搜索失败时仅提示不判失败）。
#[test]
#[ignore = "requires network access"]
fn network_smoke_fetch_and_duckduckgo() {
    let _ad = AgentDirGuard::temp();
    let ext = WebAccess;
    let runtime = tokio::runtime::Runtime::new().unwrap();

    let fetched = runtime
        .block_on(ext.execute_tool_async(
            "fetch_content".to_string(),
            serde_json::json!({ "url": "https://example.com", "mode": "readable" }),
            exec_ctx(),
        ))
        .expect("fetch example.com");
    assert!(
        fetched.text.contains("Example Domain"),
        "readable 提取应包含标题: {}",
        fetched.text
    );

    let search = runtime
        .block_on(ext.execute_tool_async(
            "web_search".to_string(),
            serde_json::json!({ "query": "rust programming language", "provider": "duckduckgo", "numResults": 3 }),
            exec_ctx(),
        ))
        .expect("duckduckgo search call");
    if search.text.starts_with("Error:") {
        eprintln!(
            "DuckDuckGo 不可达（已跳过）: {}",
            search.text.lines().next().unwrap_or("")
        );
        return;
    }
    let search_id = search
        .details
        .as_ref()
        .and_then(|d| d.get("searchId"))
        .and_then(|v| v.as_str())
        .expect("searchId")
        .to_string();
    let content = runtime
        .block_on(ext.execute_tool_async(
            "get_search_content".to_string(),
            serde_json::json!({ "responseId": search_id, "queryIndex": 0 }),
            exec_ctx(),
        ))
        .expect("get_search_content");
    assert!(content.text.contains("## Results for"), "{}", content.text);
}
