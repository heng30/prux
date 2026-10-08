//! `SubagentWorkflow` 工具：解析入参 → `meta` 校验 → **登记后台运行并立刻返回 task id**。
//!
//! 后台语义：工具立刻返回，运行继续在没有它的地方跑，完成时经 join 批次发一条 `Continuation` 通知模型。
//! - 运行状态活在 [`super::task`] 的注册表里（不在工具调用的闭包里）；
//! - 进度日志由 run 拥有，kernel 只往它追加（覆盖层/dock 每帧读）；
//! - 子代理绑的是 **run 自己的取消标志**（自建 [`ToolExecCtx`]），而不是启动回合的
//!   `parent_abort`——否则用户在同回合一按 Esc 就会把一个后台 fan-out 的子代理全部掐死，
//!   而脚本会带着一串 `null` 跑到终点交回一个假结果。

use super::{
    super::{
        agent_types, discovery_opts, manager, mention,
        model::ModelSource,
        output_file, session,
        types::{self, AgentRecord, AgentStatus, AgentType, SpawnRequest},
    },
    bridge::{
        self, AgentMeta, AgentReport, AgentRequest, AgentRunner, SchemaHooks, WorkflowInputs,
        WorkflowLimits, WorkflowOutcome,
    },
    card::{self, WORKFLOW_CARD_TYPE},
    journal::{self, Journal},
    meta::{self, WorkflowMeta},
    notify,
    saved::{self, Resolved},
    structured_output::{self, CaptureSlot, CompiledSchema},
    task::{self, ProgressLog, RunStatus},
};
use crate::{
    PROJECT_SCOPE_NAME,
    core::{
        extensions::{ExtensionTool, ToolExecCtx, request_show_dock},
        settings_manager::agent_dir,
        tools::{ToolError, ToolResult},
    },
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64},
    },
    time::Duration,
};

/// `agent({ resume })` 用的 label → 管理器记录 id 表：
/// `resume` 续的是"先前在这个 label 下跑过的 child"，
/// 而 label 是脚本能看见的唯一定位（记录 id 是内部 id，脚本拿不到）。
type LabelIndex = Mutex<HashMap<String, String>>;

/// 工作流工具对模型暴露的名称，也是让位判定与工具表过滤所用的键。
pub(crate) const WORKFLOW_TOOL_NAME: &str = "SubagentWorkflow";

/// gate 命令的墙钟上限：gate 经常就是一整套测试，所以给得宽，但**不能没有**。
/// 一个挂死的 gate 会一直占着它那个并发槽位。
const GATE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// 构造 SubagentWorkflow 工具的定义：名称、描述、JSON Schema（script/scriptPath/name/args/
/// resumeFromRunId）与 prompt guidelines；脚本优先级由 schema 描述与运行时一致地声明。
pub(crate) fn tool_def() -> ExtensionTool {
    let mut tool = ExtensionTool::simple(
        WORKFLOW_TOOL_NAME,
        TOOL_DESCRIPTION,
        json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "The workflow script source. Must begin with `export const meta = {...}`. Pass it inline; do not write it to a file first."
                },
                "scriptPath": {
                    "type": "string",
                    "description": "Path to a workflow script file to read. Takes precedence over `script` — this is how you re-run an edited workflow."
                },
                "name": {
                    "type": "string",
                    "description": format!("Name of a saved workflow: `<name>.js` in {}/workflows/ (project) or `{}/extensions/workflows/` (global). Lowest precedence — `scriptPath` and `script` both win over it.", PROJECT_SCOPE_NAME, agent_dir().display())
                },
                "args": {
                    "description": "Arbitrary JSON value exposed to the script as the `args` global. Pass actual JSON values, not a stringified blob."
                },
                "resumeFromRunId": {
                    "type": "string",
                    "description": "Resume a previous run of this session: the unchanged prefix comes back from its journal and only the edit runs. With no script/scriptPath/name of its own, it re-runs that run's script."
                }
            }
        }),
        "Run a workflow script that orchestrates multiple subagents deterministically.",
    );

    tool.label = Some("SubagentWorkflow".to_string());
    tool.prompt_guidelines = vec![
        "Use SubagentWorkflow when the number of agents depends on something discovered at \
         runtime, when work flows through stages, or when findings should be independently \
         verified. Use Agent for one delegated task or a handful you can name up front."
            .to_string(),
        "Prefer `pipeline` over `parallel` — a barrier costs wall-clock whenever the stages \
         are unevenly sized."
            .to_string(),
        "A workflow runs in the background and notifies you when it finishes — do not poll or \
         sleep waiting for it."
            .to_string(),
    ];
    tool
}

/// CLI flag（`--subagents-workflow-file <path>`）的入口：与工具调用**同一条路径**
/// （解析 → `meta` 校验 → 落盘 → 登记 → 后台跑），只是入参来自命令行。
pub(crate) async fn execute_file(ctx: &ToolExecCtx, path: &str) -> Result<ToolResult, ToolError> {
    execute(json!({ "scriptPath": path }), ctx.clone()).await
}

/// 执行：登记后台运行并**立刻**返回 task id。
///
/// 脚本解析与 `meta` 校验在登记**之前**：写错的脚本是工具错误，不会留下一个永远跑不起来、只能等通知的任务。
pub(crate) async fn execute(args: Value, ctx: ToolExecCtx) -> Result<ToolResult, ToolError> {
    let (resolved, resume_from) = resolve_source(&args, &ctx)?;
    let extraction = meta::extract_meta(&resolved.script).map_err(|e| ToolError(e.0))?;
    let workflow_args = args.get("args").filter(|v| !v.is_null()).cloned();

    // 登记 run：`abort` 既是脚本的中断信号，也是派生给子代理的取消信号。
    let run_id = task::new_id();

    // 每次调用都把脚本落到任务目录（会话同级），于是迭代方式是"改文件 + 用 scriptPath 重跑"，
    // 而不是把整份源码再发一遍。落盘失败只是少了一个便利，不影响运行。
    let script_path = persist_script(&ctx.cwd, &run_id, &resolved.script)
        .map(|p| p.display().to_string())
        .or_else(|| resolved.path.as_ref().map(|p| p.display().to_string()));

    let journal = open_journal(&ctx.cwd, &run_id, resume_from.as_ref());
    let run_abort = task::register(
        &run_id,
        &extraction.meta.name,
        &extraction.meta.description,
        extraction.meta.phases.clone(),
        script_path.clone(),
        journal.resumed_from.clone(),
    );
    let progress = task::find(&run_id).expect("just-registered run").progress;

    // 运行中的 run 会出现在 dock 的一段里 → 自动打开停靠面板（对齐 plan-mode/goal）。
    // 放在 `execute`（而非 `task::register`）里：注册表保持"只登记不碰 UI"，
    // 测试直接调 `register` 也不会往共享 UI 队列里塞请求。
    request_show_dock();

    let inputs = build_inputs(
        &ctx,
        &run_id,
        extraction.body,
        workflow_args,
        run_abort,
        progress,
        journal,
    );
    spawn_run(run_id.clone(), inputs);

    Ok(started_result(&extraction.meta, &run_id, script_path))
}

/// 解析入参里的脚本来源与（可选的）续跑目标。
///
/// `resumeFromRunId` 可能在本次调用没给来源时补上"上一次跑的那份脚本"。
fn resolve_source(
    args: &Value,
    ctx: &ToolExecCtx,
) -> Result<(Resolved, Option<ResumeTarget>), ToolError> {
    let resume_from = match args
        .get("resumeFromRunId")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => Some(resolve_resume_target(id, &ctx.cwd)?),
        None => None,
    };

    let given_source = args.get("script").is_some_and(|v| !v.is_null())
        || args.get("scriptPath").is_some_and(|v| !v.is_null())
        || args.get("name").is_some_and(|v| !v.is_null());

    let fallback_path = if given_source {
        None
    } else {
        resume_from
            .as_ref()
            .and_then(|r| r.script_path.as_deref())
            .map(str::to_string)
    };

    // 三者合流：`scriptPath` > `script` > `name`
    let resolved = saved::resolve(
        args.get("script").and_then(|v| v.as_str()),
        args.get("scriptPath")
            .and_then(|v| v.as_str())
            .or(fallback_path.as_deref()),
        args.get("name").and_then(|v| v.as_str()),
        &ctx.cwd,
    )
    .map_err(ToolError)?;

    Ok((resolved, resume_from))
}

/// 本次运行的 journal：新文件（`<runId>.workflow.jsonl`，与脚本同目录）+ 续跑读来的旧记录。
fn open_journal(cwd: &str, run_id: &str, resume_from: Option<&ResumeTarget>) -> Journal {
    Journal {
        entries: resume_from
            .map(|r| journal::read(&r.journal_path))
            .unwrap_or_default(),
        path: Some(journal_path(cwd, run_id)),
        resumed_from: resume_from.map(|r| r.run_id.clone()),
    }
}

/// 组装 bridge 的运行输入：派生 ctx、runner、schema 闸门、控制面与 `workflow()` 解析器。
fn build_inputs(
    ctx: &ToolExecCtx,
    run_id: &str,
    body: String,
    workflow_args: Option<Value>,
    run_abort: Arc<AtomicBool>,
    progress: ProgressLog,
    journal: Journal,
) -> WorkflowInputs {
    let spent = Arc::new(AtomicU64::new(0));
    let child_ctx = ToolExecCtx {
        execute_tool: ctx.execute_tool.clone(),
        parent_tool_call_id: ctx.parent_tool_call_id.clone(),
        nested_calls: ctx.nested_calls.clone(),
        session_branch_entries: Default::default(),
        script_tools: Default::default(),
        cwd: ctx.cwd.clone(),
        make_sub_agent: ctx.make_sub_agent.clone(),
        parent_abort: run_abort.clone(),
        agent_id: None,
        depth: 0,
        parent_model: None,
        script_call: false,
    };
    let runner = build_runner(child_ctx, Arc::new(Mutex::new(HashMap::new())));

    // schema 闸门：在 spawn 之前判 schema 能不能用（不能就整次运行致命），
    // 并在重放时再校验一遍交回的文本。kernel 因此不需要认识 JSON Schema。
    let schema = Some(SchemaHooks {
        compile: Arc::new(|schema: &Value| structured_output::compile(schema).map(|_| ())),
        check_text: Arc::new(|schema: &Value, text: &str| {
            structured_output::check_text(schema, text)
        }),
    });

    // 控制面交回：挂在运行记录上，供检查器（`p`/`s`/`r`/`x`）驱动
    let run_id = run_id.to_string();
    let control_sink = move |ctl: bridge::WorkflowControl| {
        task::set_control(&run_id, ctl);
    };

    // `workflow()`：具名/路径脚本的解析在扩展层
    let cwd = ctx.cwd.clone();
    let resolve_workflow = move |payload: &Value| {
        let name = payload.get("name").and_then(|v| v.as_str());
        let script_path = payload.get("scriptPath").and_then(|v| v.as_str());
        let resolved = saved::resolve(None, script_path, name, &cwd)?;
        let extraction = meta::extract_meta(&resolved.script).map_err(|e| e.0)?;

        Ok(bridge::ResolvedWorkflow {
            name: extraction.meta.name,
            body: extraction.body,
        })
    };

    WorkflowInputs {
        body,
        args: workflow_args,
        limits: WorkflowLimits::default(),
        runner,
        cancel: run_abort,
        spent,
        progress,
        schema,
        journal: Some(journal),
        control_sink: Some(Arc::new(control_sink)),
        resolve_workflow: Some(Arc::new(resolve_workflow)),
    }
}

/// 把运行体丢进后台任务；结束走 [`settle_run`]。
fn spawn_run(run_id: String, inputs: WorkflowInputs) {
    tokio::spawn(async move {
        let outcome = bridge::run_script(inputs).await;
        settle_run(&run_id, outcome);
    });
}

/// 立刻返回给模型的工具结果（含 task id 与落盘脚本路径）。
fn started_result(meta: &WorkflowMeta, run_id: &str, script_path: Option<String>) -> ToolResult {
    let mut result = ToolResult::text(format!(
        "Workflow \"{}\" started in the background.\nTask ID: {run_id}\n{}\nYou will be notified when it finishes — do NOT poll or sleep waiting for it.\nTo iterate, edit the script file and call SubagentWorkflow again with scriptPath.",
        meta.name,
        match script_path.as_deref() {
            Some(p) => format!("Script: {p}\n"),
            None => String::new(),
        }
    ));
    result.details = Some(json!({
        "taskId": run_id,
        "workflow": meta.name,
        "status": "running",
    }));
    result
}

/// 一次 `resumeFromRunId` 的目标。
struct ResumeTarget {
    /// 被续跑的运行 id。
    run_id: String,
    /// 它的 journal 文件路径（读旧记录用）。
    journal_path: PathBuf,
    /// 上一次运行用的脚本文件（本次没给来源时用它）
    script_path: Option<String>,
}

/// 解析 `resumeFromRunId`
fn resolve_resume_target(id: &str, cwd: &str) -> Result<ResumeTarget, ToolError> {
    let Some(prior) = task::find(id) else {
        let known: Vec<String> = task::list().iter().map(|r| r.id.clone()).collect();
        return Err(ToolError(format!(
            "SubagentWorkflow: no workflow run \"{id}\" in this session.{}",
            if known.is_empty() {
                " Nothing has run yet — call this without `resumeFromRunId`.".to_string()
            } else {
                format!(" Runs this session: {}.", known.join(", "))
            }
        )));
    };

    if !prior.status.is_terminal() {
        return Err(ToolError(format!(
            "SubagentWorkflow: workflow \"{id}\" is still running; stop it before resuming it."
        )));
    }

    Ok(ResumeTarget {
        run_id: prior.id.clone(),
        journal_path: journal_path(cwd, &prior.id),
        script_path: prior.script_path.clone(),
    })
}

/// 某次运行的 journal 文件路径（与脚本同目录）。
fn journal_path(cwd: &str, run_id: &str) -> PathBuf {
    let session = session::session_key();
    output_file::artifact_path(cwd, session.as_deref(), &format!("{run_id}.workflow.jsonl"))
}

/// 把脚本写进本次会话的任务目录（`<runId>.workflow.js`）；失败返回 None。
fn persist_script(cwd: &str, run_id: &str, script: &str) -> Option<PathBuf> {
    let session = session::session_key();
    let path =
        output_file::artifact_path(cwd, session.as_deref(), &format!("{run_id}.workflow.js"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }

    std::fs::write(&path, script).ok()?;
    Some(path)
}

/// 运行结束：写状态 → 投递通知（模型）+ 卡片（人）。
fn settle_run(id: &str, outcome: Result<WorkflowOutcome, String>) {
    let stopped = task::find(id).is_some_and(|r| r.stop_requested);
    let (status, value, error) = match outcome {
        Ok(out) => {
            task::set_agent_count(id, out.agent_count);
            (RunStatus::Completed, Some(out.value), None)
        }
        Err(e) if stopped => (RunStatus::Killed, None, Some(e)),
        Err(e) => (RunStatus::Failed, None, Some(e)),
    };

    task::finish(id, status, value, error);

    // 运行收尾：如果连同子代理在内已经没有活儿了，已完成的记录从 dock 隐去
    // （工作流段本就只列运行中的 run，这里管的是与代理共享的那段面板）。
    manager::dismiss_finished();

    let Some(run) = task::find(id) else {
        return; // 会话已切换（`reset_all` 清空）：不再通知
    };

    // 通知模型：与后台代理同一个 join 批次
    let peers_live = manager::background_live() > 0 || task::running_count() > 0;
    let (_tool_calls, tokens) = notify::totals(&run);

    manager::batch_push(
        manager::BatchItem::Rendered {
            envelope: super::notify::envelope(&run),
            card: Some((WORKFLOW_CARD_TYPE.to_string(), card::card_payload(&run))),
            tokens,
            duration_ms: run.elapsed_ms(),
        },
        peers_live,
    );
}

/// 构造 AgentRunner：把每个 `AgentRequest` 交给 `run_one_agent` 执行，并共享 label→记录 id 索引
/// 以支持 `resume`；返回的 runner 被 workflow 运行时反复调用。
fn build_runner(ctx: ToolExecCtx, labels: Arc<Mutex<HashMap<String, String>>>) -> AgentRunner {
    Arc::new(move |req: AgentRequest| {
        let ctx = ctx.clone();
        let labels = labels.clone();
        Box::pin(async move { run_one_agent(&ctx, req, &labels).await })
    })
}

/// 用**本次尝试**的取消标志派生执行上下文：子代理的 `parent_abort` 因此是它自己那一个
/// （跳过/重跑/掐 run 都能真的停掉它，而不只是不再启动新的）。
fn attempt_ctx(base: &ToolExecCtx, req: &AgentRequest) -> ToolExecCtx {
    ToolExecCtx {
        execute_tool: base.execute_tool.clone(),
        parent_tool_call_id: base.parent_tool_call_id.clone(),
        nested_calls: base.nested_calls.clone(),
        session_branch_entries: Default::default(),
        script_tools: Default::default(),
        cwd: base.cwd.clone(),
        make_sub_agent: base.make_sub_agent.clone(),
        parent_abort: req.abort.clone(),
        agent_id: None,
        depth: 0,
        parent_model: None,
        script_call: false,
    }
}

/// 一次 `agent()` 的进度标签（与桥的进度条目同一个口径：显式 label 优先，否则取 prompt 首行）。
fn agent_label(req: &AgentRequest) -> String {
    req.label
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| bridge::derived_label(&req.prompt))
}

/// 跑一次 `agent()`：解析类型 → `manager::dispatch`（相对本次运行是前台）→ 取结果与元数据。
async fn run_one_agent(
    base_ctx: &ToolExecCtx,
    req: AgentRequest,
    labels: &LabelIndex,
) -> AgentReport {
    let ctx = &attempt_ctx(base_ctx, &req);
    let ty = match resolve_request_type(ctx, &req) {
        Ok(ty) => ty,
        Err(e) => return AgentReport::failed(e),
    };
    let (spawn, capture, compiled) = match build_child_request(&req, &ty, labels) {
        Ok(built) => built,
        Err(e) => return AgentReport::failed(e),
    };

    // 拿记录 id 就立刻回传（桥把它写进进度条目，运行中的 agent 也能被检查器 `c` 打开）。
    let on_id = req.on_spawn.callback();
    let d = match manager::dispatch_with_id(ctx, &ty, spawn, on_id).await {
        Ok(d) => d,
        // 请求本身不合法（未知 resume 目标 / 模型解析失败等）：没有记录可读，元数据空着。
        Err(e) => return AgentReport::failed(e),
    };

    let Some(rec) = manager::record(&d.id) else {
        return AgentReport::failed("sub-agent disappeared");
    };
    let meta = meta_from_record(&rec);

    // 记下这次跑的是哪个 label，供以后 `agent({ resume: '<label>' })` 找回它
    labels
        .lock()
        .unwrap()
        .insert(agent_label(&req), rec.id.clone());

    // 失败/被停的子代理 → 脚本侧得 `null`。`manager::dispatch` 对前台失败也返回 `Ok`（失败写进了记录），
    // 所以这里必须看状态，不能直接交回结果文本——否则一个跑挂的 agent 会被当成“返回了空字符串”。
    if !agent_succeeded(&rec) {
        let reason = rec
            .result
            .clone()
            .unwrap_or_else(|| format!("sub-agent ended with status {}", rec.status.as_str()));
        return AgentReport::failed(reason).with_meta(meta);
    }

    // `opts.gate`：子代理**成功**之后跑一条命令，非零退出即该 agent 失败，命令输出就是错误。
    // 失败/被停的子代理不跑 gate（它的失败已经说明问题，跑一遍纯属浪费）。
    // gate 在本 runner 里跑，而 runner 是在并发的 permit 持有期间被调的——
    // 也就是说 gate 占着它那个槽位，所以超时上限必须有。
    if let Some(command) = req.gate.as_deref()
        && let Err(output) = run_gate(command, &ctx.cwd).await
    {
        return AgentReport::failed(output).with_meta(meta);
    }

    if req.schema.is_none() {
        return AgentReport::text(rec.result.unwrap_or_default()).with_meta(meta);
    }

    structured_report(&capture, compiled.as_deref()).with_meta(meta)
}

/// 解析 `agent()` 请求要用的类型；未知/禁用即该次调用失败。
fn resolve_request_type(ctx: &ToolExecCtx, req: &AgentRequest) -> Result<AgentType, String> {
    let cfg = manager::config();
    let roster = agent_types::discover_with(&ctx.cwd, discovery_opts(&cfg));
    let type_name = req
        .agent_type
        .clone()
        .unwrap_or_else(|| "general-purpose".to_string());

    agent_types::resolve(&roster.types, &type_name)
        .cloned()
        .map_err(|_| format!("agent type {type_name:?} is not available"))
}

/// 构造工作流 child 的 spawn 请求：注入 `StructuredOutput`、把 label 解析成 resume 目标。
///
/// 返回 `(请求, schema 捕获槽, 编译好的校验器)`；捕获槽即使没有 schema 也要带出——run 结束后统一读它判定结果。
fn build_child_request(
    req: &AgentRequest,
    ty: &AgentType,
    labels: &LabelIndex,
) -> Result<(SpawnRequest, CaptureSlot, Option<Arc<CompiledSchema>>), String> {
    let description = req
        .label
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| mention::describe_mention(&req.prompt));

    // `agent({schema})`：把 `StructuredOutput` 工具**注入**给这个 child（不是给你提示词要求 JSON）。
    // 捕获槽由本次调用持有，run 结束后读它；schema 已在 bridge 岗位验过，
    // 这里重建只是为了拿到编译好的校验器（同一个 `compile`，结果必然一致）。
    let capture = structured_output::capture_slot();
    let mut injected_tools = Vec::new();
    let mut compiled: Option<Arc<CompiledSchema>> = None;

    if let Some(schema) = req.schema.as_ref() {
        let c = Arc::new(structured_output::compile(schema)?);
        injected_tools.push(structured_output::build_tool(c.clone(), capture.clone()));
        compiled = Some(c);
    }

    // `opts.resume`：把 label 换成管理器的记录 id，交给 `dispatch` 的 resume 路径
    // （它会打开既有子会话、以其上下文继续，并复用同一条记录）。
    let resume_target = match req.resume.as_deref() {
        Some(label) => match labels.lock().unwrap().get(label).cloned() {
            Some(id) => Some(id),
            None => return Err(format!("Cannot resume \"{label}\" — it never started.")),
        },
        None => None,
    };

    let cfg = manager::config();
    let mut spawn = SpawnRequest::from_type(ty, &cfg, req.prompt.clone(), description);
    spawn.model_source = if req.model.is_some() {
        ModelSource::Caller
    } else {
        types::model_source_from_type(ty)
    };

    // `agent({ isolation })`：frontmatter 钉死的优先于脚本传的（与 Agent 工具同一条权威性规则）
    spawn.isolation = ty.isolation.clone().or_else(|| req.isolation.clone());
    spawn.model = req.model.clone().or_else(|| ty.model.clone());
    spawn.thinking = req.effort.clone().or_else(|| ty.thinking.clone());
    spawn.max_turns = ty.max_turns;
    spawn.run_in_background = false;
    spawn.inherit_context = false;
    spawn.persist_session = ty.persist_session.unwrap_or(true);
    spawn.resume = resume_target;
    spawn.injected_tools = injected_tools;
    spawn.workflow_owned = true;

    Ok((spawn, capture, compiled))
}

/// 子代理是否成功交付（`Completed` 或 `Steered`）。
fn agent_succeeded(rec: &AgentRecord) -> bool {
    matches!(rec.status, AgentStatus::Completed | AgentStatus::Steered)
}

/// 有 schema 时只认工具交回的载荷；散文里的 JSON **不算**，捕获不到就是这个 agent 失败——脚本侧看到 `null`。
fn structured_report(capture: &CaptureSlot, compiled: Option<&CompiledSchema>) -> AgentReport {
    let captured = capture.lock().unwrap();
    match captured.json.clone() {
        Some(value) => {
            // 定稿再校验一次：工具侧已经校验过，但“脚本要了一个形状”这件事只应该由一个地方承诺。
            // 用同一个编译好的校验器再走一遍（若将来有别的途径把载荷递进来，这里就是唯一的闸门）。
            match compiled.map(|c| c.check(&value)) {
                Some(Err(verdict)) => AgentReport::failed(format!(
                    "The agent's answer did not match the requested schema: {verdict}"
                )),
                _ => AgentReport::structured(value),
            }
        }
        None => AgentReport::failed(structured_output::failure_message(&captured)),
    }
}

/// 在 `cwd` 跑一条 gate 命令（`sh -c` / `cmd /c`）。`Ok(())` = 通过。
///
/// 超时按失败算，且**明确**把超时说清楚：坑是"超时被杀但退出码是 0"，只看退出码会把被杀的 gate 读成通过。
async fn run_gate(command: &str, cwd: &str) -> Result<(), String> {
    let (exe, arg) = if cfg!(windows) {
        ("cmd", "/c")
    } else {
        ("sh", "-c")
    };

    let mut child = tokio::process::Command::new(exe);
    child.arg(arg).arg(command);
    child.current_dir(cwd);
    child.kill_on_drop(true);

    let out = match tokio::time::timeout(GATE_TIMEOUT, child.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("gate command could not run: {e}")),
        Err(_) => {
            return Err(format!(
                "gate command timed out after {}s: {command}",
                GATE_TIMEOUT.as_secs()
            ));
        }
    };

    if out.status.success() {
        return Ok(());
    }

    let mut text: String = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }

        text.push_str(&stderr);
    }

    if text.is_empty() {
        text = format!("gate command exited with {}", out.status);
    }

    Err(text)
}

/// 从记录里取进度条目要的元数据。
///
/// 只在结算时读一次：运行期间那行只有 recordId（派发时回传）/标签/阶段/状态/已用时，token、tool 次数、耗时这些要结算了才准。
fn meta_from_record(rec: &AgentRecord) -> AgentMeta {
    AgentMeta {
        record_id: Some(rec.id.clone()),
        agent_type: Some(rec.agent_type.clone()),
        tokens: Some(rec.usage.display_total()),
        output_tokens: Some(rec.usage.output),
        tool_calls: Some(rec.tool_uses as u64),
        duration_ms: Some(rec.duration_ms()),
        skipped: rec.status == AgentStatus::Stopped, // 被用户停掉的子代理要画成 skipped 而不是 failed
        ..Default::default()
    }
}

/// 工具描述正文（`agent()` / `phase()` 用法与使用时机）。
const TOOL_DESCRIPTION: &str = r#"Execute a workflow script that orchestrates multiple subagents deterministically. Workflows run in the background — this tool returns immediately with a task ID, and you are notified when the workflow completes. Use `/agents workflows` to watch live progress.

ONLY call this tool when the user has explicitly opted into multi-agent orchestration ("use a workflow", "fan out agents", "orchestrate this with subagents", or a skill/command that says to). Otherwise use the Agent tool for individual subagents.

Every invocation persists its script under the session's task directory and returns the path in the tool result. To iterate on a workflow, edit that file and re-invoke SubagentWorkflow with `scriptPath: "<path>"` instead of resending the full source. A script you will run more than once belongs in `.prux/workflows/<name>.js` (or `<agent_dir>/extensions/workflows/<name>.js`); call it with `name: "<name>"` instead of re-sending the source.

Pass the script inline via `script`. Every script must begin with `export const meta = {...}`:
  export const meta = {
    name: 'find-flaky-tests',
    description: 'Find flaky tests and propose fixes',
    phases: [{ title: 'Scan', detail: 'grep test logs' }, { title: 'Fix' }],
  }
  phase('Scan')
  const flaky = await agent('grep CI logs for retry markers', {schema: FLAKY_SCHEMA})
  return { flaky }

The `meta` object must be a PURE LITERAL (no variables, calls, spreads, or template interpolation). Required: `name`, `description`. Optional: `whenToUse`, `phases`.

Script globals:
- agent(prompt: string, opts?: {label?, phase?, schema?, model?, effort?, agentType?, gate?, resume?, isolation?}): Promise<any> — spawn a subagent. Without schema, returns its final text as a string. With schema (a JSON Schema), the subagent is given a StructuredOutput tool built from it and returns the validated object — no parsing needed. A payload that does not match is rejected back to the child, which corrects it; a child that never answers through the tool fails, so the call returns null — filter after every schema stage. opts.gate: '<command>' runs a shell command after the agent finishes — a non-zero exit (or a timeout) marks the agent failed and the command's output becomes the error; prefer gate: 'npm test' over asking another agent whether the code looks right. opts.resume: '<label>' continues the child that ran under that label instead of starting fresh, so an iterative loop keeps its context — it cannot be combined with agentType, model, effort, gate, isolation or schema. opts.isolation: 'worktree' runs the child in a git worktree copy that is committed to a branch when it finishes (requires a git repo with at least one commit). Returns null if the subagent fails (filter with .filter(Boolean)).
- parallel(thunks: Array<() => Promise<any>>): Promise<any[]> — BARRIER; a thunk that throws resolves to null (only a fatal run error propagates).
- pipeline(items, stage1, stage2, ...): Promise<any[]> — run each item through all stages independently, NO barrier between stages. Each stage gets (prevResult, originalItem, index). A throwing stage drops that item to null.
- workflow(nameOrRef: string | {name?, scriptPath?}, args?): Promise<any> — run another workflow inline, one level deep. Use it to compose reusable stages you already saved; the child's agents count against the same agent cap, and its `return` value comes back here.
- phase(title): void — start a phase; subsequent agent() calls group under it.
- log(message): void — emit a narrator line.
- args: any — the value passed as `args` verbatim.
- budget: {total: null, spent(): number, remaining(): number}.

DEFAULT TO pipeline(); use parallel only when stage N genuinely needs ALL of stage N-1's results together.

Scripts are plain JavaScript, NOT TypeScript. The body runs in an async context — use await directly, and top-level `return` to produce the result. `Date.now()` / `Math.random()` / argless `new Date()` throw (they would break resume); pass timestamps via `args`.

Concurrent agent() calls are capped at min(16, available CPUs - 2) per workflow; total agents per run are capped at 1000; a single parallel()/pipeline() accepts at most 4096 items."#;

/// 测试专用入口：暴露 journal 文件路径计算，便于断言断点续跑用的日志落点。
#[cfg(test)]
pub(crate) fn journal_path_for_test(cwd: &str, run_id: &str) -> std::path::PathBuf {
    journal_path(cwd, run_id)
}
