//! 子代理扩展的黑盒集成测试：只走公开 API（扩展 trait、工具面、注册表）。
//!
//! 真 LLM 轮次与后台通知的端到端用例在 `src/core/agent_session.rs` 的 lib 测试里
//! （`subagent_background_spawn_notifies_and_persists_child_session` / `subagent_extension_runs_agent_tool`）：
//! 那里能用内置的假 provider 脚本化多连接响应，集成测试拿不到该 helper。

use prux::core::extensions::{self, Extension, ToolExecCtx};
use prux::core::project_trust;
use prux::extensions::subagent::Subagent;
use prux::test_support::AgentDirGuard;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// 失败工厂：本文件的用例都不需要真的物化子代理。
fn dead_ctx() -> ToolExecCtx {
    ToolExecCtx {
        execute_tool: prux::core::extensions::unavailable_tool_exec(),
        parent_tool_call_id: None,
        nested_calls: Default::default(),
        session_branch_entries: Default::default(),
        script_tools: Default::default(),
        cwd: ".".to_string(),
        make_sub_agent: Arc::new(|_| Err("materialization is not expected here".to_string())),
        parent_abort: Arc::new(AtomicBool::new(false)),
        agent_id: None,
        depth: 0,
        parent_model: None,
        script_call: false,
    }
}

fn agent_tool() -> extensions::ExtensionTool {
    Subagent
        .tools()
        .into_iter()
        .find(|t| t.name == "Agent")
        .expect("Agent tool")
}

#[test]
fn identity_tool_surface_and_commands() {
    let ext = Subagent;
    assert_eq!(ext.name(), "subagent");
    assert!(!ext.default_enabled(), "Tier1 保持按需开启");
    assert!(ext.modes().contains(&extensions::ExtensionMode::Dev));
    assert!(ext.modes().contains(&extensions::ExtensionMode::Creator));

    let fork = ext.fork_project().expect("fork provenance");
    assert_eq!(fork.plugin_name, "pi-subagents");
    assert_eq!(fork.plugin_version, "0.19.0");
    assert_eq!(fork.url, "https://github.com/tintinweb/pi-subagents");

    let names: Vec<String> = ext.tools().into_iter().map(|t| t.name).collect();
    assert_eq!(
        names,
        vec![
            "Agent",
            "get_subagent_result",
            "steer_subagent",
            "SubagentWorkflow"
        ]
    );

    let cmds = ext.commands();
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].name, "agents");
    assert!(cmds[0].busy_safe, "/agents handler 不锁 agent");
    let subs: Vec<&str> = cmds[0].subcommands.iter().map(|s| s.name).collect();
    assert_eq!(
        subs,
        vec![
            "types",
            "result",
            "stop",
            "workflows",
            "schedules",
            "settings",
            "eject",
            "enable",
            "disable",
            "delete",
            "reset",
            "edit"
        ]
    );

    // dock / wants_redraw 读的是**进程级**注册表，而这个二进制里的用例并行跑、
    // 有的会派发后台 agent → 在这里断言它们必然是竞态。它们由 `extensions::subagent`
    // 的单测覆盖（`dock_shows_workflow_runs_and_requests_redraw` 等，那些用例持串行锁）。
}

#[test]
fn agent_tool_schema_requires_type_and_defaults_to_background() {
    let tool = agent_tool();
    let required = tool.parameters["required"].as_array().unwrap();
    let req: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(req.contains(&"prompt"));
    assert!(req.contains(&"description"));
    assert!(req.contains(&"subagent_type"), "对齐上游：类型必填");

    // `schedule` 参数只在启用时进 schema（关掉时零上下文成本）
    assert!(
        tool.parameters["properties"].get("schedule").is_some(),
        "缺省启用 → schema 里有 schedule"
    );

    let bg = &tool.parameters["properties"]["run_in_background"];
    assert_eq!(bg["type"], "boolean");
    // Tier2 起默认后台（对齐上游），但是否后台由设置面板决定 → schema 不写死 default
    assert!(
        bg.get("default").is_none(),
        "默认值取决于配置，故 schema 不声明 default"
    );
    assert!(
        tool.description.contains("detaches by default"),
        "工具描述必须说清默认后台: {}",
        tool.description
    );
    assert!(
        tool.description.contains("run_in_background: false"),
        "必须给出改回前台的参数: {}",
        tool.description
    );
    assert!(tool.description.contains("do NOT sleep") || tool.description.contains("never poll"));
    assert!(
        !tool.description.contains("isolation"),
        "Tier1 不暴露 worktree 参数"
    );
    assert!(
        !tool.description.contains("schedule"),
        "Tier1 不暴露 schedule 参数"
    );
}

#[test]
fn agent_tool_description_lists_discovered_types_with_project_override() {
    let _agent_home = AgentDirGuard::temp();
    let cwd = tempfile::tempdir().unwrap();
    let cwd_s = cwd.path().to_string_lossy().to_string();

    // 全局定义（agent_dir 已按线程本地 guard 指向临时目录）
    let global_dir = prux::core::settings_manager::agent_dir()
        .join("extensions")
        .join("agent");
    std::fs::create_dir_all(&global_dir).unwrap();
    std::fs::write(
        global_dir.join("auditor.md"),
        "---\nname: auditor\ndescription: Global auditor\ntools: read, grep\n---\nGLOBAL BODY",
    )
    .unwrap();
    // 项目定义（覆盖同名全局）
    let project_dir = cwd.path().join(".prux").join("agents");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(
        project_dir.join("auditor.md"),
        "---\nname: auditor\ndescription: Project auditor\ntools: read\n---\nPROJECT BODY",
    )
    .unwrap();

    // 工具描述里的类型清单按**会话 cwd**发现（`tools()` 无入参，故取 on_session_start 记录的 cwd）
    Subagent.on_session_start(&cwd_s, &[], None);

    // 未信任项目：只看到全局定义
    let desc = agent_tool().description;
    assert!(desc.contains("Global auditor"), "{desc}");
    assert!(!desc.contains("Project auditor"), "{desc}");
    // 内嵌默认三类型始终在
    for t in ["general-purpose", "Explore", "Plan"] {
        assert!(desc.contains(&format!("- {t}:")), "{t} 缺失: {desc}");
    }

    // 信任项目：项目定义覆盖同名全局定义
    project_trust::set_project_trust(cwd.path(), &prux::core::settings_manager::agent_dir(), true);
    let desc = agent_tool().description;
    assert!(desc.contains("Project auditor"), "{desc}");
    assert!(!desc.contains("Global auditor"), "{desc}");

    // 解析结果对 spawn 也生效：不存在/歧义/禁用的失败路径
    // spawn 路径用 ctx.cwd 重新发现（与工具描述无关地生效）
    let mut ctx = dead_ctx();
    ctx.cwd = cwd_s.clone();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt
        .block_on(Subagent.execute_tool_async(
            "Agent".to_string(),
            json!({
                "prompt": "p",
                "description": "d",
                "subagent_type": "auditor",
                "run_in_background": false
            }),
            ctx,
        ))
        .expect("物化失败不是工具错误（以 status=error 如实回报）");
    let details = out.details.as_ref().unwrap();
    assert_eq!(details["type"], "auditor", "应按项目定义派发: {details}");
    assert_eq!(details["status"], "error");
    assert!(out.text.contains("not expected here"), "{}", out.text);
}

#[test]
fn disabled_type_falls_back_to_general_purpose_with_a_note() {
    let _agent_home = AgentDirGuard::temp();
    let global_dir = prux::core::settings_manager::agent_dir()
        .join("extensions")
        .join("agent");
    std::fs::create_dir_all(&global_dir).unwrap();
    std::fs::write(
        global_dir.join("plan.md"),
        "---\nname: Plan\nenabled: false\n---\n",
    )
    .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt
        .block_on(Subagent.execute_tool_async(
            "Agent".to_string(),
            json!({
                "prompt": "p",
                "description": "d",
                "subagent_type": "Plan"
            }),
            dead_ctx(),
        ))
        .expect("回退不是错误");
    assert!(out.text.contains("is disabled"), "{}", out.text);
    assert!(
        out.text.contains("using general-purpose instead"),
        "{}",
        out.text
    );
    // 物化失败也如实上报为 status=error（不是 panic、不是静默成功）
    assert_eq!(out.details.as_ref().unwrap()["status"], "error");
    assert_eq!(
        out.details.as_ref().unwrap()["type"],
        "general-purpose",
        "details 应反映回退后的类型"
    );
}

#[test]
fn agent_tool_defaults_to_background_when_the_argument_is_omitted() {
    // Tier2 起默认后台（对齐上游）：不写 run_in_background = 脱离回合
    let _agent_home = AgentDirGuard::temp();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt
        .block_on(Subagent.execute_tool_async(
            "Agent".to_string(),
            json!({ "prompt": "p", "description": "d", "subagent_type": "Explore" }),
            dead_ctx(),
        ))
        .expect("物化失败仍以 status=error 回报");
    let details = out.details.as_ref().unwrap();
    assert_eq!(details["background"], true, "缺省应为后台: {details}");
    assert!(
        out.text.contains("you will be notified") || out.text.contains("You will be notified"),
        "后台结果应说明完成时会通知: {}",
        out.text
    );
}

#[test]
fn lookup_failures_are_plain_text_not_tool_errors() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt
        .block_on(Subagent.execute_tool_async(
            "get_subagent_result".to_string(),
            json!({ "agent_id": "deadbeef" }),
            dead_ctx(),
        ))
        .unwrap();
    assert!(
        out.text.contains("unknown agent id: deadbeef"),
        "{}",
        out.text
    );

    let out = rt
        .block_on(Subagent.execute_tool_async(
            "steer_subagent".to_string(),
            json!({ "agent_id": "deadbeef", "message": "hi" }),
            dead_ctx(),
        ))
        .unwrap();
    assert!(
        out.text.contains("unknown agent id: deadbeef"),
        "{}",
        out.text
    );
}

#[test]
fn agent_tool_rejects_invalid_arguments() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let cases = [
        (
            json!({ "description": "d", "subagent_type": "Explore" }),
            "prompt",
        ),
        (
            json!({ "prompt": "p", "subagent_type": "Explore" }),
            "description",
        ),
        (
            json!({ "prompt": "p", "description": "d" }),
            "subagent_type",
        ),
        (
            json!({ "prompt": "p", "description": "d", "subagent_type": "Explore", "max_turns": -3 }),
            "max_turns",
        ),
    ];
    for (args, needle) in cases {
        let err = rt
            .block_on(Subagent.execute_tool_async("Agent".to_string(), args.clone(), dead_ctx()))
            .unwrap_err();
        assert!(err.0.contains(needle), "args={args} err={}", err.0);
    }
}
