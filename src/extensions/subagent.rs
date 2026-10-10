//! 子代理扩展（移植 `@tintinweb/pi-subagents` v0.19.0）。
//!
//! 定位：子代理是**扩展**、非内置。启用后 `Agent` / `get_subagent_result` /
//! `steer_subagent` 三个工具进入模型可见工具集，`/agents` 提供只读查询。
//!
//! 迁移策略与偏离清单见仓库根目录 `migration-subagents.md`；Tier1 范围：
//! 两层 agent 类型发现 + 默认三类型 + 前/后台执行 + 并发池与排队 + graceful max_turns +
//! resume + `inherit_context`。**默认前台**。
//!
//! 模块划分：`types` 数据类型 / `agent_types` 发现与解析 / `prompt` 提示词与工具描述 /
//! `manager` 注册表与并发 / `notify` 输出整形。

mod agent_files;
mod agent_types;
mod config;
mod events;
mod fleet;
mod manager;
mod memory;
mod mention;
mod model;
mod nested;
mod notify;
mod output_file;
mod prompt;
mod schedule;
mod session;
mod types;
mod widget;
mod wizard;
mod worktree;

pub(crate) mod autoresearch;
pub(crate) mod workflow;

use super::util::{choice_key, notify, notify_text, opt_str, req_str, type_arg_label};
use crate::{
    core::{
        extensions::{
            self, AfterToolCallOutcome, CliFlagDef, Extension, ExtensionCommand, ExtensionHook,
            ExtensionMode, ExtensionSetting, ExtensionSuggestion, ExtensionSuggestions,
            ExtensionTool, ExtensionUiRequest, ForkProjectInfo, OverlayEvent, OverlayView,
            RichSpan, SelectOption, ToolAnnotations, ToolExecCtx, ToolExposure, UiNotifyLevel,
            UserPromptAction,
        },
        provider::AgentMessage,
        session_manager::Session,
        tools::{ToolError, ToolExecutionMode, ToolResult},
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_SUBAGENT, command_arg},
    modes::interactive::{app::App, handlers::register_slash_command},
    utils::time::rel_time,
};
use agent_types::{DiscoveryOptions, DiscoveryWarning, ResolveError, Roster};
use config::SubagentConfig;
use futures_util::{
    future::{BoxFuture, Either, select},
    pin_mut,
};
use manager::{Caller, Dispatched};
use model::ModelSource;
use prompt::AgentToolDescription;
use serde_json::{Value, json};
use session::Verdict;
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use types::{AgentRecord, AgentType, SpawnRequest};

/// 扩展名（settings 的 `enabledExtensions` 键、诊断与 UI 展示）。
pub const EXT: &str = "subagent";

/// 后台完成通知的稳定前缀（Tier2 自定义渲染依赖它）。
pub const NOTIFICATION_HEAD: &str = "<subagent_result";

/// mention clone：屏幕外一次性回合带的身份前缀（clone 发起 spawn 时当顶层）。
const CLONE_MARKER_PREFIX: &str = "__mention_clone__";

/// 自声明工厂：linkme 分布式切片，priority 值大者先注册（见 [`crate::extensions`]）。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static SUBAGENT_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_SUBAGENT,
    make: || Arc::new(Subagent),
};

/// 子代理扩展。
pub struct Subagent;

/// `/agents` 面板的状态：请求 id + 面板层级。
///
/// 一级菜单（[`SelectKind::Menu`]）的选项文本以稳定 token 开头（`list` / `types` / `settings`），
/// 二级列表（[`SelectKind::Agents`]）的选项以 agent id 开头，
/// 因此回传时**无需**保存"选项列表"，只需知道当前是哪一层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectKind {
    /// `/agents` 一级菜单：list / types / settings 等入口
    Menu,
    /// 代理列表层：选项以 agent id 开头，选中查看该代理记录卡片
    Agents,
    /// 类型清单：选中一个进二级操作菜单
    Types,
    /// 某个类型的操作菜单（eject/enable/disable/delete/reset；末项 `back` 回类型清单）
    TypeActions,
}

/// `/agents` 实参补全的上下文：决定 `@` 候选来源，并让候选独占该 `@`（不混文件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuggestionContext {
    /// `/agents stop|result `：选一个在飞/已完成的代理（候选 = 代理 id）
    AgentArg(AgentArgKind),
    /// `/agents eject|enable|disable|delete|reset|edit `：选一个类型（候选 = 类型）
    TypeArg(TypeArgKind),
}

/// `/agents stop|result @` 的实参上下文：只列真正可选的代理，并把 id 一并展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentArgKind {
    /// 只能停非终态代理
    Stop,
    /// 任意记录都能看结果
    Result,
}

/// `/agents eject|enable|disable|delete|reset|edit @` 的实参上下文。
///
/// 决定哪些类型**真正可操作**：内嵌默认三型没有 `.md` 文件，`enable|disable|delete|edit`
/// 对它们只会报错（`reset` 也需要先有 `eject` 出来的覆盖文件），因此这些类型不进候选；
/// 一旦某内置类型被 `eject` 出文件，它就和其他类型一样可以被操作、照常出现在候选里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeArgKind {
    /// 导出为 `.md`：只列**还没有** `.md` 的类型（已有文件的会拒绝覆盖）
    Eject,
    /// 启用：需要 `.md` 且当前被禁用（已启用的会报“无法重新启用”）
    Enable,
    /// 禁用：需要 `.md` 且当前启用
    Disable,
    /// 删除 `.md`：需要文件（内置覆盖文件与自定义类型都算）
    Delete,
    /// 恢复内嵌默认：需要内置默认且存在覆盖 `.md`
    Reset,
    /// 编辑 `.md`：需要文件
    Edit,
}

/// `/agents` 选择面板的请求 id 与层级（on_ui_choice 据此分发）。
fn select_id_slot() -> &'static Mutex<Option<(u64, SelectKind)>> {
    /// 待应答的 `/agents` 选择请求 (id, 层级)，供 on_ui_choice 分发。
    static S: OnceLock<Mutex<Option<(u64, SelectKind)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// 会话 cwd：`Extension::tools()` 没有入参，而项目级 `.prux/agents/` 必须按会话 cwd 发现，
/// 故在 `on_session_start` 记下当前会话 cwd 供工具描述使用。
///
/// 注意：它只影响**工具描述里的类型清单**（诊断面）。
/// spawn 时一律用 `ToolExecCtx.cwd` 重新发现，所以这里滞后不会导致派发到错误类型。
fn session_cwd_slot() -> &'static Mutex<Option<String>> {
    /// 缓存的会话 cwd，供无入参的 tools() 按项目发现 agents。
    static S: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// 发现用 cwd：会话 cwd 优先，回退进程 cwd。
fn discovery_cwd() -> String {
    session_cwd_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| ".".to_string())
}

/// 从光标前的整行前缀判断 `@` 是否落在某条 `/agents` 子命令的实参位置。
///
/// 只看前两个词（`agents` + 子命令），因此 `/agents stop @exp`、`/agents  enable  @e` 都算。
fn suggestion_context(line_prefix: &str) -> Option<SuggestionContext> {
    let rest = line_prefix.trim_start().strip_prefix('/')?;
    let mut words = rest.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case("agents") {
        return None;
    }
    match words.next()?.to_ascii_lowercase().as_str() {
        "stop" => Some(SuggestionContext::AgentArg(AgentArgKind::Stop)),
        "result" => Some(SuggestionContext::AgentArg(AgentArgKind::Result)),
        "eject" => Some(SuggestionContext::TypeArg(TypeArgKind::Eject)),
        "enable" => Some(SuggestionContext::TypeArg(TypeArgKind::Enable)),
        "disable" => Some(SuggestionContext::TypeArg(TypeArgKind::Disable)),
        "delete" => Some(SuggestionContext::TypeArg(TypeArgKind::Delete)),
        "reset" => Some(SuggestionContext::TypeArg(TypeArgKind::Reset)),
        "edit" => Some(SuggestionContext::TypeArg(TypeArgKind::Edit)),
        _ => None,
    }
}

/// `/agents stop|result` 的 `@` 候选：只列候选的 **agent id**（不显示类型/文件名）。
///
/// `stop` 只列非终态（已结束的不能再停）；`result` 全列（终态也有结果可看）。
/// 展示名就是 id，说明行用 spawn 简述 + 状态，便于区分（不含 agent type）。
fn agent_arg_suggestions(query: &str, kind: AgentArgKind) -> Vec<ExtensionSuggestion> {
    let q = query.to_lowercase();
    let matches = |s: &str| q.is_empty() || s.to_lowercase().contains(&q);

    let mut out = Vec::new();
    for rec in manager::list() {
        if kind == AgentArgKind::Stop && rec.status.is_terminal() {
            continue;
        }
        // 匹配面宽一点（id / 简述 / name / alias / handle），但展示与插入的都是 id
        let hit = matches(&rec.id)
            || matches(&rec.description)
            || rec.name.as_deref().is_some_and(&matches)
            || rec.alias.as_deref().is_some_and(&matches)
            || rec.handle.as_deref().is_some_and(&matches);
        if !hit {
            continue;
        }
        let status = rec.status.as_str();
        let description = if rec.description.is_empty() {
            status.to_string()
        } else {
            format!("{} · {status}", rec.description)
        };
        out.push(ExtensionSuggestion {
            name: format!("@{}", rec.id),
            description,
            insert: rec.id.clone(),
        });
    }
    out
}

/// `/agents eject|enable|disable|delete|reset|edit` 的 `@` 候选：类型名。
///
/// 只列该子命令**真能处理**的类型（见 [`TypeArgKind`]）：已有 `.md` 的不再出现在 `eject`
/// 候选里（会拒绝覆盖），内嵌默认三型没有 `.md` 时也不能 enable/disable/delete/edit；
/// 被 `eject` 出文件后即可操作，候选随之出现（禁用中的标注出来）。
/// 与 `@handle` 提及无关，不受 `agent_mentions` 开关影响；插入句柄（[`find_type_arg`] 会映射回类型）。
fn type_arg_suggestions(query: &str, kind: TypeArgKind) -> Vec<ExtensionSuggestion> {
    let cwd = discovery_cwd();
    let q = query.to_lowercase();
    let mut out = Vec::new();

    for t in agent_types::discover(&cwd).types {
        if !type_arg_supported(&t, kind, &cwd) {
            continue;
        }
        let handle = mention::handle_base(&t.name);
        if !(q.is_empty() || handle.contains(&q) || t.name.to_lowercase().contains(&q)) {
            continue;
        }
        let mark = if t.enabled { "" } else { " · disabled" };
        out.push(ExtensionSuggestion {
            name: format!("@{handle}"),
            description: format!("{}{mark}", t.label()),
            insert: handle,
        });
    }
    out
}

/// 该类型是否值得出现在某个子命令的 `@` 候选里（操作要么能生效、要么不会只报错）。
///
/// 判定对齐各动作的实际前置条件（见 [`toggle_type`] / [`eject_type`] / [`delete_type`] / [`reset_type`] / [`edit_type`]）：
/// 文件是否存在用 [`agent_files::find_agent_file`]，与动作自身用的是同一个探测。
fn type_arg_supported(ty: &AgentType, kind: TypeArgKind, cwd: &str) -> bool {
    let has_file = agent_files::find_agent_file(ty, cwd).is_some();
    match kind {
        TypeArgKind::Eject => !has_file,
        TypeArgKind::Enable => has_file && !ty.enabled,
        TypeArgKind::Disable => has_file && ty.enabled,
        TypeArgKind::Delete | TypeArgKind::Edit => has_file,
        TypeArgKind::Reset => has_file && is_builtin_default(&ty.name),
    }
}

impl Extension for Subagent {
    /// 返回扩展名常量 [`EXT`]（`subagent`），用于事件归属与 UI 标识。
    fn name(&self) -> &str {
        EXT
    }

    /// `/extension` 面板详情里展示的一句话描述。
    fn description(&self) -> &str {
        "Sub-agents with custom agent types (Agent / get_subagent_result / steer_subagent)."
    }

    /// 声明移植来源（pi-subagents 插件），供 /extension 详情展示与跳转。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "pi-subagents".to_string(),
            plugin_version: "0.19.0".to_string(),
            url: "https://github.com/tintinweb/pi-subagents".to_string(),
        })
    }

    /// 声明 `Dev` / `Creator` 两个并排模式：`Minimal` 下不可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 主动注入工具并改变默认 agentic 行为 → 按需开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 返回本扩展的工具：`Agent` / `get_subagent_result` / `steer_subagent` 及工作流工具；
    /// `Agent` 的描述里含类型清单，每次调用现扫（改 .md 后随下次 rebuild 生效）。
    fn tools(&self) -> Vec<ExtensionTool> {
        // 工具描述里的类型清单在每次 tools() 求值时现扫（改 .md 后随下次 rebuild 生效）
        let cwd = discovery_cwd();
        let cfg = manager::config();
        let roster = agent_types::discover_with(&cwd, discovery_opts(&cfg));

        let description =
            match prompt::agent_tool_description_for(&roster.types, cfg.tool_description, &cwd) {
                AgentToolDescription::Text(text) => text,
                AgentToolDescription::FallbackToFull { text, warning }
                | AgentToolDescription::TextWithWarning { text, warning } => {
                    // 与发现告警同一套去重：同一原因每会话只提示一次
                    if manager::note_warning(&format!("tool-description|{warning}")) {
                        util_notify(&warning, UiNotifyLevel::Warning);
                    }
                    text
                }
            };

        vec![
            ExtensionTool {
                exposure: ToolExposure::Direct,
                namespace: None,
                annotations: ToolAnnotations::default(),
                output_schema: None,
                name: "Agent".to_string(),
                description,
                label: Some("Sub-agent".to_string()),
                parameters: {
                let mut properties = json!({
                        "prompt": {
                            "type": "string",
                            "description": "The task for the agent to perform. Must be self-contained: the agent has not seen this conversation."
                        },
                        "description": {
                            "type": "string",
                            "description": "Short (3-5 word) summary of what the agent will do (shown in UI)."
                        },
                        "subagent_type": {
                            "type": "string",
                            "description": "Which agent type to use (see the available types in this tool's description)."
                        },
                        "name": {
                            "type": "string",
                            "description": "Optional memorable name for this agent, later usable with steer_subagent / get_subagent_result."
                        },
                        "model": {
                            "type": "string",
                            "description": "Exact \"provider/modelId\" to run this agent on. Defaults to the parent's model (or the agent type's pinned model)."
                        },
                        "thinking": {
                            "type": "string",
                            "description": "Thinking level: off, minimal, low, medium, high, xhigh, max. Clamped to what the model supports."
                        },
                        "max_turns": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Maximum agentic turns before graceful shutdown. 0 or omitted means unlimited."
                        },
                        "run_in_background": {
                            "type": "boolean",
                            "description": "Run detached and get notified on completion (this is the default; the subagent settings panel can flip it). Pass false to block and get the result inline."
                        },
                        "resume": {
                            "type": "string",
                            "description": "Agent id (or name) of a finished agent whose session should be continued instead of starting fresh."
                        },
                        "inherit_context": {
                            "type": "boolean",
                            "description": "Fork the parent conversation into this agent so it knows what has been discussed."
                        },
                        "isolated": {
                            "type": "boolean",
                            "description": "Run with built-in tools only: no extension-provided tools at all."
                        },
                });
                // `schedule` 只在启用时进 schema（关掉时零上下文成本）
                if let Some(param) = schedule_param() {
                    properties["schedule"] = param;
                }
                // `isolation` 同理（关掉时"无处可写"，而不是收下再悄悄降级）
                if let Some(param) = isolation_param() {
                    properties["isolation"] = param;
                }
                json!({
                    "type": "object",
                    "properties": properties,
                    "required": ["prompt", "description", "subagent_type"],
                    "additionalProperties": false
                })
                },
                snippet: prompt::AGENT_TOOL_SNIPPET.to_string(),
                prompt_guidelines: prompt::agent_tool_guidelines(),
                constrained_sampling: false,
                render_shell: None,
                execution_mode: Some(ToolExecutionMode::Parallel), // 多条 Agent 调用并发执行
                prepare_arguments: None,
                grammar_sampling: None,
            },
            ExtensionTool {
                exposure: ToolExposure::Direct,
                namespace: None,
                annotations: ToolAnnotations::default(),
                output_schema: None,
                name: "get_subagent_result".to_string(),
                description: "Check the status of a sub-agent and read its result. Use wait: true to block until it finishes (only when you genuinely need the result now), and verbose: true for the full conversation transcript.".to_string(),
                label: Some("Sub-agent result".to_string()),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": { "type": "string", "description": "Agent id (or the name it was spawned with)." },
                        "wait": { "type": "boolean", "description": "Block until the agent finishes. Defaults to false." },
                        "verbose": { "type": "boolean", "description": "Include the conversation transcript (truncated to the last 200 lines / 8 KB)." }
                    },
                    "required": ["agent_id"],
                    "additionalProperties": false
                }),
                snippet: "Check a sub-agent's status and read its result".to_string(),
                prompt_guidelines: Vec::new(),
                constrained_sampling: false,
                render_shell: None,
                execution_mode: None,
                prepare_arguments: None,
                grammar_sampling: None,
            },
            ExtensionTool {
                exposure: ToolExposure::Direct,
                namespace: None,
                annotations: ToolAnnotations::default(),
                output_schema: None,
                name: "steer_subagent".to_string(),
                description: "Send a steering message to a running sub-agent. The message is injected after its current tool call and redirects its work without restarting it.".to_string(),
                label: Some("Steer sub-agent".to_string()),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": { "type": "string", "description": "Agent id (or the name it was spawned with)." },
                        "message": { "type": "string", "description": "Message to inject into the agent's conversation." }
                    },
                    "required": ["agent_id", "message"],
                    "additionalProperties": false
                }),
                snippet: "Send a steering message to a running sub-agent".to_string(),
                prompt_guidelines: Vec::new(),
                constrained_sampling: false,
                render_shell: None,
                execution_mode: None,
                prepare_arguments: None,
                grammar_sampling: None,
            },
            workflow::tool::tool_def(),
        ]
    }

    /// 声明 `/agents` 斜杠命令及其子命令（types/result/stop/workflows/schedules/settings/eject/…）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![
            ExtensionCommand {
                name: "agents".to_string(),
                description: "List sub-agents of this session (types, running, recent results)"
                    .to_string(),
                busy_safe: true, // handler 只读扩展自身状态，不锁 agent
                subcommands: vec![
                    extensions::SubcommandDef {
                        name: "types",
                        description: "List available agent types (read-only)",
                    },
                    extensions::SubcommandDef {
                        name: "result",
                        description: "Show one agent's status and result: /agents result <id> (`@` picks the id)",
                    },
                    extensions::SubcommandDef {
                        name: "stop",
                        description: "Stop a running or queued agent, or a workflow run: /agents stop <id> (`@` picks the id)",
                    },
                    extensions::SubcommandDef {
                        name: "workflows",
                        description: "Open the workflow runs list (live progress)",
                    },
                    extensions::SubcommandDef {
                        name: "schedules",
                        description: "List scheduled sub-agent jobs (cancel with d)",
                    },
                    extensions::SubcommandDef {
                        name: "settings",
                        description: "Open the subagent settings panel",
                    },
                    extensions::SubcommandDef {
                        name: "eject",
                        description: "Write an agent type out as a .md file: /agents eject <type> [project] (`@` picks the type)",
                    },
                    extensions::SubcommandDef {
                        name: "enable",
                        description: "Re-enable a disabled agent .md: /agents enable <type> (`@` picks the type)",
                    },
                    extensions::SubcommandDef {
                        name: "disable",
                        description: "Disable an agent .md (line-wise edit): /agents disable <type> (`@` picks the type)",
                    },
                    extensions::SubcommandDef {
                        name: "delete",
                        description: "Delete a custom agent's .md file: /agents delete <type> (`@` picks the type)",
                    },
                    extensions::SubcommandDef {
                        name: "reset",
                        description: "Delete a built-in default's override .md, restoring the default: /agents reset <type> (`@` picks the type)",
                    },
                    extensions::SubcommandDef {
                        name: "edit",
                        description: "Edit an agent type's .md in a multi-line editor: /agents edit <type> (`@` picks the type)",
                    },
                ],
            },
            ExtensionCommand {
                name: "autoresearch".to_string(),
                description: "Iterate one sub-agent on a goal until it reports [goal-complete]: \
                              /autoresearch <goal> [--iterations N] starts a job, bare /autoresearch lists jobs"
                    .to_string(),
                busy_safe: true, // handler 只动本扩展的作业表与子代理注册表，不锁 agent
                subcommands: vec![extensions::SubcommandDef {
                    name: "stop",
                    description: "Stop a running job: /autoresearch stop <id> (`#3` / `autoresearch-3` also work)",
                }],
            },
        ]
    }

    /// 声明本扩展关心的钩子（Dock/Overlay/UserPrompt/AgentEvent/ExtensionEvent/AfterToolCall/Suggestions）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        // `UserPrompt` 常声明（`hooks()` 在流式事件里高频调用，不应在这里做 `manager::config()` 的 `stat`）；
        // `agent_mentions = off` 时由 `on_user_prompt` 立即返回 `None`，行为等价。
        vec![
            ExtensionHook::Dock,
            ExtensionHook::Overlay,
            ExtensionHook::UserPrompt,
            ExtensionHook::AgentEvent,
            ExtensionHook::ExtensionEvent,
            ExtensionHook::AfterToolCall,
            ExtensionHook::Suggestions,
        ]
    }

    /// agent 内核事件：硬中止（`agent_end(reason=aborted)`）时停掉顶层的子代理——
    /// 它们挂在本轮 run 上，主回合被丢弃后没人再收尾（dock 会一直显示 running）。
    fn on_agent_event(&self, event: &Value) {
        if event.get("type").and_then(Value::as_str) == Some("agent_end")
            && event.get("reason").and_then(Value::as_str) == Some("aborted")
        {
            manager::abort_top_level();
        }
    }

    /// 停靠面板内容：合并子代理注册表、工作流与 autoresearch 作业的行
    /// （三段各有 header，互不影响）。
    fn dock_lines(&self) -> Vec<extensions::DockLine> {
        // 三个注册表在此组合（各有一段 header、互不影响）。
        let mut lines = manager::widget_lines();
        lines.extend(workflow::task::dock_lines());
        lines.extend(autoresearch::dock_lines());
        lines
    }

    /// 有后台活儿在跑时请求持续重绘：dock 里的 spinner 与计数才会动
    /// （覆盖层打开时 TUI 本来就每帧重绘，不依赖这里）。
    fn wants_redraw(&self) -> bool {
        manager::background_live() > 0 || workflow::task::has_live() || autoresearch::has_live()
    }

    /// 会话执行上下文：核心在会话建立 / turn 开始 / 会话切换后交给扩展，
    /// 使输入钩子（`@mention`）在工具执行之外也能 spawn/resume。
    fn on_exec_ctx(&self, ctx: &ToolExecCtx) {
        fleet::remember_ctx(ctx);
        launch_pending_workflow_file(ctx);
    }

    /// 输入拦截（上下文增强）：`model` 模式且空闲时走 clone，否则走 direct。
    ///
    /// - `@main` → `Rewrite`（剥前缀落回主模型）；
    /// - 存活运行/排队 → steer；已终结且有 session → 后台 resume；
    /// - 从未跑过的类型 → 后台 spawn；
    /// - 裸句柄 / 未知句柄 / 解析不到类型 → `None`（不吞输入，落回主模型）。
    fn on_user_prompt_with_context(
        &self,
        text: &str,
        messages: &[AgentMessage],
        busy: bool,
    ) -> Option<UserPromptAction> {
        let cfg = manager::config();
        if !cfg.agent_mentions.enabled() {
            return None;
        }

        // `model` 模式：空闲时用对话克隆在屏幕外写 prompt；clone 不可用则回落 direct。
        if cfg.agent_mentions == config::AgentMentions::Model
            && !busy
            && let Some(action) = mention_clone(text, messages)
        {
            return Some(action);
        }

        direct_mention(text, &cfg)
    }

    /// 用户开新回合：把上一轮已完成的子代理从 dock 隐去（面板不再白占位置）。
    /// 与 `on_user_prompt` 不同，这个通知不看 `agent_mentions` 配置、也不
    /// 受别人先认领输入影响（见 `dispatch_user_submit`）。
    fn on_user_submit(&self, _text: &str, _busy: bool) {
        manager::dismiss_finished();
    }

    /// 覆盖层内容（FleetView / 会话查看器）：每帧拉取，只读本扩展状态。
    fn overlay_view(&self, id: u64, cols: u16) -> Option<OverlayView> {
        fleet::overlay_view(id, cols)
    }

    /// 完成卡片渲染（实时投递与会话恢复重放共用同一个纯函数）。
    fn render_custom_message(&self, custom_type: &str, data: &Value) -> Option<Vec<RichSpan>> {
        if custom_type == notify::CARD_TYPE {
            return Some(notify::render_card(data));
        }
        if custom_type == workflow::card::WORKFLOW_CARD_TYPE {
            return Some(workflow::card::render_card(data));
        }
        None
    }

    /// 覆盖层按键/输入/关闭事件。
    fn on_overlay_event(&self, id: u64, ev: &OverlayEvent) -> bool {
        fleet::on_overlay_event(id, ev)
    }

    /// 面板项：widget 显示范围 / 默认前后台 / 并发额度 / 用量展示。
    fn settings(&self) -> Vec<ExtensionSetting> {
        config::panel_settings(&manager::config())
    }

    /// 面板选择立即落盘（`agent_dir()/extensions/subagent.json`）并刷新进程内缓存。
    /// `--subagents-workflow-file <path>`：记下待跑的文件，等拿到能物化子代理的上下文再跑。
    fn cli_flags(&self) -> Vec<CliFlagDef> {
        vec![CliFlagDef {
            name: "subagents-workflow-file",
            description: "Run a workflow script from a file at session start",
            takes_value: true,
        }]
    }

    /// 处理 `--subagents-workflow-file <path>`：把待跑的工作流文件记入进程内槽位，
    /// 等拿到可物化子代理的执行上下文后再实际跑。
    fn apply_cli_flag_value(&self, name: &str, value: &str) {
        if name == "subagents-workflow-file" && !value.trim().is_empty() {
            *pending_workflow_file()
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(value.trim().to_string());
        }
    }

    /// 应用设置面板的一项变更：改写配置并落盘、刷新进程内缓存；
    /// `workflows` 变更时额外失效让位判定。返回 `Err` 表示值非法（面板保持原值）。
    fn apply_setting(&self, key: &str, value: &str) -> Result<(), String> {
        let mut cfg = manager::config();
        config::apply_panel_choice(&mut cfg, key, value)?;
        config::write_config(&config::config_path(), &cfg);
        manager::reload_config();

        // `workflows` 可能刚变过（auto/on/off）→ 让位判定重算
        if key == "workflows" {
            session::invalidate();
        }

        Ok(())
    }

    /// 记录会话 cwd 供工具描述里的类型清单使用。
    fn on_session_start(&self, cwd: &str, _messages: &[AgentMessage], session: Option<&Session>) {
        *session_cwd_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(cwd.to_string());

        // 任务目录键（脚本落盘/journal 用）：会话文件的 stem；无落盘文件时为 None（退化）
        session::set_session_key(
            session
                .and_then(|s| s.get_session_file())
                .and_then(|p| p.file_stem())
                .and_then(|s| s.to_str())
                .map(str::to_string),
        );
        session::invalidate();
        session::announce_if_needed();
        schedule::sync_session(); // 定时任务：绑定本会话的 store（`/new` 空、`/resume` 读回）

        // 上次进程崩了可能留下 worktree 注册项：清理掉（不 await，失败无所谓）
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let cwd = cwd.to_string();
            handle.spawn(async move { worktree::prune(&cwd).await });
        }

        events::ready(); // 所有扩展已注册完毕：宣告可用
    }

    /// 会话切换（/new /resume /import /fork）：旧会话的子代理一并中止并清空。
    fn on_session_switched(&self, session_path: Option<&str>, _messages: &[AgentMessage]) {
        autoresearch::cancel_all(); // 作业挂在旧会话的上下文上：先停，再清注册表
        manager::reset_all();
        workflow::task::reset_all();
        fleet::reset();

        session::set_session_key(session_path.and_then(session::key_from_path));
        session::invalidate();
        session::announce_if_needed();
        schedule::reset(); // 定时任务按会话分：先清掉旧的，再绑定新会话的 store
        schedule::sync_session();

        // 重放本会话里已落盘的完成卡片（`/resume` 后仍能看到历史代理结果）
        if let Some(path) = session_path {
            for (custom_type, payload) in notify::cards_from_session(path) {
                extensions::request_ui(ExtensionUiRequest::CustomMessage {
                    label: EXT.to_string(),
                    custom_type,
                    data: payload,
                });
            }
        }
    }

    /// 扩展启用/禁用：启用时落盘默认配置模板（对照用），禁用时中止全部子代理。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            config::ensure_config_file();
            manager::reload_config();
        } else {
            autoresearch::cancel_all();
            manager::reset_all();
            workflow::task::reset_all();
            fleet::reset();
            session::reset();
            schedule::reset();
        }

        session::invalidate();

        if enabled {
            session::announce_if_needed();
            events::ready(); // 重新可用：向订阅者宣告（先于任何 spawn）
        }
    }

    /// 注册时接线 `/agents` 命令 handler，失效让位缓存，并在扩展此刻生效时播报启动提示。
    fn on_registered(&self) {
        register_slash_command(EXT, "agents", command_agents);
        register_slash_command(EXT, "autoresearch", autoresearch::command);
        session::invalidate(); // 注册顺序在首个回合之前定下来，正是判让位的时候

        // 启动提示只在扩展此刻生效（启用且当前模式可用）时发：Minimal 模式下
        // 本扩展不可用，此时播报“workflows off / 另一个扩展接管”属于噪声。
        if extensions::is_extension_active(EXT) {
            session::announce_if_needed();
        }
    }

    /// 跨扩展总线事件流入：处理 `subagents:rpc:*` 四个方法。
    fn on_extension_event(&self, name: &str, payload: &Value) {
        events::on_event(name, payload);
    }

    /// `@` 候选：默认是存活代理的 `@handle` + 可用类型的 `@type`（只在 mentions 启用时，不独占）；
    /// 但 `/agents` 子命令实参位置改为独占候选——`stop|result` 列代理 id，类型类子命令列类型名，
    /// 都不再混入文件候选。每个键入都会调：只读 manager 列表 + 小目录扫描，不锁 agent。
    fn suggestions(&self, query: &str, line_prefix: &str) -> ExtensionSuggestions {
        match suggestion_context(line_prefix) {
            Some(SuggestionContext::AgentArg(kind)) => {
                return ExtensionSuggestions {
                    items: agent_arg_suggestions(query, kind),
                    exclusive: true,
                };
            }
            Some(SuggestionContext::TypeArg(kind)) => {
                return ExtensionSuggestions {
                    items: type_arg_suggestions(query, kind),
                    exclusive: true,
                };
            }
            None => {}
        }

        if !manager::config().agent_mentions.enabled() {
            return ExtensionSuggestions::default();
        }
        let q = query.to_lowercase();
        let matches = |handle: &str, type_name: &str| {
            q.is_empty()
                || handle.to_lowercase().contains(&q)
                || type_name.to_lowercase().contains(&q)
        };

        let mut seen = HashSet::new();
        let mut out: Vec<ExtensionSuggestion> = Vec::new();
        for rec in manager::list() {
            let Some(h) = rec.handle.as_deref() else {
                continue;
            };

            if matches(h, &rec.agent_type) && seen.insert(h.to_string()) {
                out.push(ExtensionSuggestion {
                    name: format!("@{h}"),
                    description: format!("{} · {}", rec.display_name, rec.status.as_str()),
                    insert: h.to_string(),
                });
            }
        }

        for t in agent_types::discover(&discovery_cwd()).enabled() {
            if out.len() >= 20 {
                break;
            }
            let h = mention::handle_base(&t.name);
            if seen.contains(&h) {
                continue;
            }

            if matches(&h, &t.name) && seen.insert(h.clone()) {
                out.push(extensions::ExtensionSuggestion {
                    name: format!("@{h}"),
                    description: t.label().to_string(),
                    insert: h,
                });
            }
        }
        ExtensionSuggestions {
            items: out,
            exclusive: false,
        }
    }

    /// `reportUsage`：把池里的子代理开销挂到**下一个**真实工具结果上。
    /// 池只在配置开启时累积（sink 侧门控），所以关掉时这里拿到 None。
    fn after_tool_call_ext(
        &self,
        _name: &str,
        _args: &Value,
        result_text: &str,
        _is_error: bool,
    ) -> Result<AfterToolCallOutcome, ToolError> {
        Ok(AfterToolCallOutcome {
            text: result_text.to_string(),
            content: None,
            details: None,
            is_error: None,
            usage: manager::drain_pooled_usage(),
            terminate: None,
        })
    }

    /// 收起本扩展自己的工具：工作流让位（`auto` 遇冲突）或关闭（`off`）时不再提供 `SubagentWorkflow`。
    /// `compose_tools` 与 `get_all_tools` 都走这个钩子，所以模型看不到它、孙代理也继承不到。
    fn filter_extension_tools(&self, tools: Vec<ExtensionTool>) -> Vec<ExtensionTool> {
        if session::verdict().withdrawn() {
            return tools
                .into_iter()
                .filter(|t| t.name != workflow::tool::WORKFLOW_TOOL_NAME)
                .collect();
        }
        tools
    }

    /// `/agents` 选择面板的结果：一/二级菜单分发，只读展示、不触发 prompt。
    fn on_ui_choice(&self, id: u64, choice: Option<String>) -> Option<String> {
        let kind = {
            let mut slot = select_id_slot().lock().unwrap_or_else(|e| e.into_inner());
            match *slot {
                Some((pending, kind)) if pending == id => {
                    *slot = None;
                    kind
                }
                _ => return None,
            }
        };
        let choice = choice?;
        let key = choice_key(&choice);

        match kind {
            SelectKind::Menu => menu_choice(key),
            SelectKind::Agents => {
                if let Some(rec) = manager::record(key) {
                    util_notify_spans(notify::record_card(&rec, false), UiNotifyLevel::Info);
                }
            }
            SelectKind::Types => open_type_actions(key), // 类型清单：`choice` 以类型名开头，进操作菜单
            SelectKind::TypeActions => type_actions_choice(&choice),
        }
        None
    }

    /// 异步执行本扩展工具：先记住物化上下文（供 TUI 侧发起 spawn/resume），
    /// 再按工具名分发到 [`dispatch_tool`]。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        ctx: ToolExecCtx,
    ) -> BoxFuture<'static, Result<ToolResult, ToolError>> {
        // 记下物化上下文（含运行时句柄）：FleetView/查看器的 `r` 续跑要在 TUI 线程发起 spawn
        fleet::remember_ctx(&ctx);
        Box::pin(async move { dispatch_tool(&name, args, ctx).await })
    }
}

/// 工具分发。
async fn dispatch_tool(name: &str, args: Value, ctx: ToolExecCtx) -> Result<ToolResult, ToolError> {
    match name {
        "Agent" => agent_tool(args, ctx).await,
        "SubagentWorkflow" => workflow::tool::execute(args, ctx).await,
        "get_subagent_result" => result_tool(args, &ctx).await,
        "steer_subagent" => steer_tool(args, &ctx),
        other => Err(ToolError(format!("extension {EXT}: unknown tool: {other}"))),
    }
}

/// `schedule` 参数的 schema 片段：**没启用就完全不出现**。
fn schedule_param() -> Option<Value> {
    if !manager::config().schedule {
        return None;
    }
    Some(json!({
        "type": "string",
        "description": "Register this run as a scheduled job instead of starting it now. Use 6-field cron (\"0 0 9 * * 1\"), an interval (\"5m\"/\"1h\"), or a one-shot (\"+10m\"/ISO). Only when the user explicitly asked for scheduled/recurring/delayed execution."
    }))
}

/// `isolation` 参数的 schema 片段：`worktree_isolation` 关掉时**不出现**。
///
/// 两个值都写出来（`off` 在前、说明里点明它是默认）：只给一个 `"worktree"` 会让
/// "每个可选参数都填点什么"的模型反复建副本——`off` 是那个无害的填法。
fn isolation_param() -> Option<Value> {
    if !manager::config().worktree_isolation {
        return None;
    }
    Some(json!({
        "type": "string",
        "enum": ["off", "worktree"],
        "description": "Isolation mode. Default \"off\". \"off\" runs the agent in the current checkout, the same as omitting the field. \"worktree\" creates a temporary git worktree so the agent works on an isolated copy of the repo (a copy cannot see uncommitted or staged changes in the main checkout)."
    }))
}

/// `Agent` 工具的参数（schema → 强类型视图）。
///
/// 策略值（`run_in_background` / `inherit_context` / `isolated`）在这里原样带出，
/// 待类型解析完成后再由 [`build_spawn_request`] 按 frontmatter 权威性合并。
struct AgentArgs {
    /// 必填：交给子代理的任务（必须自包含——它看不到本会话）。
    prompt: String,
    /// 必填：3~5 词简述（UI 展示；定时任务也拿它当任务名）。
    description: String,
    /// 必填：类型名（大小写不敏感；未知/禁用按 `fallbackSubagent` 回退或报错）。
    type_name: String,
    /// 可选：便于日后 `steer_subagent` / `get_subagent_result` 定位的名字。
    name: Option<String>,
    /// 可选：`provider/modelId`；缺省用父级型号（或类型钉死的型号）。
    model: Option<String>,
    /// 可选：思考档位（off/minimal/low/medium/high/xhigh/max），按模型能力夹取。
    thinking: Option<String>,
    /// 可选：续跑某个**已终结**代理的会话（id 或 name），而不是新起一个。
    resume: Option<String>,
    /// 可选：cron / 间隔 / 一次性表达式；给了就注册定时任务而非现在跑。
    schedule: Option<String>,
    /// 可选：`off`（当前 checkout）或 `worktree`（临时副本）。
    isolation: Option<String>,
    /// 可选：最大 agentic 轮数；`0`/缺省 = 不限，负数在解析时被拒。
    max_turns: Option<u32>,
    /// 可选：`run_in_background` 原值；与 frontmatter、全局默认按权威性合并。
    background: Option<bool>,
    /// 可选：`inherit_context` 原值；fork 父会话给子代理。
    inherit_context: Option<bool>,
    /// 可选：`isolated` 原值；只用内置工具、不带任何扩展工具。
    isolated: Option<bool>,
}

impl AgentArgs {
    /// 把工具参数 JSON 解析为强类型视图：必填项缺失按 prompt → description → subagent_type 顺序报错，
    /// `max_turns` 为负报错、`0` 视作不限（`None`）。
    fn parse(args: &Value) -> Result<Self, ToolError> {
        // 必填项先读：错误信息按 prompt → description → subagent_type 的顺序暴露。
        let prompt = req_str(args, "prompt")?;
        let description = req_str(args, "description")?;
        let type_name = req_str(args, "subagent_type")?;
        let max_turns = match args.get("max_turns").and_then(|v| v.as_i64()) {
            None => None,
            Some(v) if v < 0 => {
                return Err(ToolError("max_turns must be >= 0".to_string()));
            }
            Some(0) => None,
            Some(v) => Some(v as u32),
        };
        Ok(Self {
            prompt,
            description,
            type_name,
            name: opt_str(args, "name"),
            model: opt_str(args, "model"),
            thinking: opt_str(args, "thinking"),
            resume: opt_str(args, "resume"),
            schedule: opt_str(args, "schedule"),
            isolation: opt_str(args, "isolation"),
            max_turns,
            background: args.get("run_in_background").and_then(|v| v.as_bool()),
            inherit_context: args.get("inherit_context").and_then(|v| v.as_bool()),
            isolated: args.get("isolated").and_then(|v| v.as_bool()),
        })
    }
}

/// 解析要派的类型；未知/禁用时按 `fallbackSubagent` 决定回退还是报错。
///
/// 返回解析后的类型 + 可选的“用了回退”提示（由调用方拼进结果正文）。
fn resolve_agent_type(
    roster: &Roster,
    type_name: &str,
    cfg: &SubagentConfig,
) -> Result<(AgentType, Option<String>), ToolError> {
    match agent_types::resolve(&roster.types, type_name) {
        Ok(ty) => Ok((ty.clone(), None)),
        // 歧义是配置问题，不该由模型消化 → 明确失败
        Err(ResolveError::Ambiguous(names)) => Err(ToolError(format!(
            "agent type {type_name:?} is ambiguous: {} differ only by case; rename one of them",
            names.join(", ")
        ))),
        Err(reason) => {
            let why = match reason {
                ResolveError::Unknown => "does not exist",
                _ => "is disabled",
            };

            // fallbackSubagent = none：严格模式，未知/禁用类型直接失败
            if cfg.fallback_subagent == config::FallbackMode::None {
                return Err(ToolError(format!(
                    "agent type {type_name:?} {why} (fallbackSubagent = none)"
                )));
            }

            let fallback = agent_types::resolve(&roster.types, "general-purpose").map_err(|_| {
                ToolError(format!(
                    "agent type {type_name:?} {why} and the fallback type general-purpose is unavailable"
                ))
            })?;
            Ok((
                fallback.clone(),
                Some(format!(
                    "agent type {type_name:?} {why}; using general-purpose instead"
                )),
            ))
        }
    }
}

/// `allowed_subagents`：父 agent 限定嵌套 Agent 可派的类型（主会话不受限）。
///
/// 比对的是**解析后的 canonical 名**，allowlist 里写别名也能对上。
/// 空列表 = `all`/`*`（开启嵌套但不限型），不拦。返回 `Some(拒绝文本)` 表示不允许。
fn check_allowed_subagents(
    roster: &Roster,
    ctx: &ToolExecCtx,
    ty: &AgentType,
) -> Option<ToolResult> {
    let parent_id = manager::caller_of(ctx).agent_id?;
    let rec = manager::record(&parent_id)?;
    let owner = agent_types::find_any(&roster.types, &rec.agent_type)?;
    let list = owner.allowed_subagents.clone()?;

    if list.is_empty() {
        return None;
    }

    let allowed: Vec<String> = list
        .iter()
        .map(|a| {
            agent_types::resolve(&roster.types, a)
                .map(|t| t.name.clone())
                .unwrap_or_else(|_| a.clone())
        })
        .collect();

    if allowed.iter().any(|a| a.eq_ignore_ascii_case(&ty.name)) {
        return None;
    }

    Some(ToolResult::text(format!(
        "agent type {:?} is not allowed for {} — allowed_subagents: {}",
        ty.name,
        rec.agent_type,
        allowed.join(", ")
    )))
}

/// 定时分支：注册任务而**不**现在跑。返回 `None` 表示本次调用没要求定时。
fn try_schedule(a: &AgentArgs, cfg: &SubagentConfig) -> Option<ToolResult> {
    let spec = a.schedule.as_deref()?;
    if !cfg.schedule {
        return Some(ToolResult::text(
            "Scheduling is disabled in the subagent settings. Enable it via /agents → Settings → Schedule.".to_string(),
        ));
    }
    if a.resume.is_some() {
        return Some(ToolResult::text(
            "Cannot combine `schedule` with `resume` — schedules create fresh agents.".to_string(),
        ));
    }
    if a.inherit_context == Some(true) {
        return Some(ToolResult::text(
            "Cannot combine `schedule` with `inherit_context` — there is no parent conversation at fire time.".to_string(),
        ));
    }
    if a.background == Some(false) {
        return Some(ToolResult::text(
            "Cannot combine `schedule` with `run_in_background: false` — scheduled jobs always run in background.".to_string(),
        ));
    }

    // 类型在**解析之后**才走到这里：未知/禁用类型在创建时就报错（而不是到点才发现）
    Some(ToolResult::text(
        match schedule::add(schedule::NewJob {
            name: a.description.clone(),
            description: a.description.clone(),
            schedule: spec.to_string(),
            subagent_type: a.type_name.clone(),
            prompt: a.prompt.clone(),
            model: a.model.clone(),
            thinking: a.thinking.clone(),
            max_turns: a.max_turns,
        }) {
            Ok(job) => format!(
                "Scheduled {:?} (id: {}, type: {}). Next run: {}. Manage via /agents → Scheduled jobs.",
                job.name,
                job.id,
                job.schedule_type,
                rel_time(schedule::next_run(&job.id))
            ),
            Err(e) => e,
        },
    ))
}

/// 应用 frontmatter 权威性：类型定义里写了的字段不允许被调用参数覆盖，再构造 spawn 请求。
fn build_spawn_request(
    a: &AgentArgs,
    ty: &AgentType,
    cfg: &SubagentConfig,
    caller: &Caller,
    clone_marker: Option<&str>,
) -> SpawnRequest {
    // 基线 = frontmatter 全量透传；下面只盖"调用参数"那一层。
    let mut req = SpawnRequest::from_type(ty, cfg, a.prompt.clone(), a.description.clone());
    req.name = a.name.clone();
    req.model_source = if ty.model.is_some() {
        ModelSource::Frontmatter
    } else if a.model.is_some() {
        ModelSource::Caller
    } else {
        ModelSource::Inherited
    };

    // frontmatter 钉死的 isolation 优先于调用参数（与其它策略字段同权威性规则）
    req.isolation = ty.isolation.clone().or_else(|| a.isolation.clone());
    req.model = ty.model.clone().or_else(|| a.model.clone());
    req.thinking = ty.thinking.clone().or_else(|| a.thinking.clone());
    req.max_turns = ty.max_turns.or(a.max_turns);
    req.inherit_context = ty.inherit_context.or(a.inherit_context).unwrap_or(false);
    // isolated：frontmatter 优先；工具参数次之；`extensions: false` 与 isolated 等价
    req.isolated = ty.isolated.or(a.isolated).unwrap_or(false) || ty.extensions_none;
    req.resume = a.resume.clone();

    // 嵌套归属：谁派的、自己第几层；clone 发起的算顶层（None/0）
    req.parent_agent_id = if clone_marker.is_some() {
        None
    } else {
        caller.agent_id.clone()
    };
    req.depth = if clone_marker.is_some() {
        0
    } else {
        caller.depth + 1
    };

    // 主会话用全局默认，嵌套默认前台。
    // 被派发的子代理会随父代理结束一起被掐掉，后台跑等于把它的活儿丢掉
    let default_background = if caller.agent_id.is_some() {
        false
    } else {
        cfg.background_default
    };

    req.run_in_background = if clone_marker.is_some() {
        true // clone 强制后台：前台结果会随副本被丢弃而丢失（上游同此）
    } else {
        ty.run_in_background
            .or(a.background)
            .unwrap_or(default_background)
    };

    // `tools:` 里的 `ext:<扩展>` 选择器在此展开（拼错的选择器原样保留 + 一次性告警）
    req.tools = match ty.tools.as_ref() {
        None => None,
        Some(list) => {
            let (expanded, unknown) = agent_types::expand_ext_selectors(list);
            for sel in unknown {
                let key = format!("ext-selector|{sel}");
                if manager::note_warning(&key) {
                    util_notify(
                        &format!(
                            "agent type {:?}: unknown tool selector {sel:?} was ignored",
                            ty.name
                        ),
                        UiNotifyLevel::Warning,
                    );
                }
            }
            Some(expanded)
        }
    };

    req
}

/// 派发并渲染工具结果（体量小，从编排里单独拎出来保持 [`agent_tool`] 线性可读）。
async fn dispatch_and_render(
    ctx: &ToolExecCtx,
    ty: &AgentType,
    req: SpawnRequest,
    note: Option<String>,
) -> Result<ToolResult, ToolError> {
    let dispatched = manager::dispatch(ctx, ty, req).await.map_err(ToolError)?;
    let Some(rec) = manager::record(&dispatched.id) else {
        return Err(ToolError(format!(
            "sub-agent {} disappeared before it could be reported",
            dispatched.id
        )));
    };

    let mut body = String::new();
    if let Some(note) = note {
        body.push_str(&format!("note: {note}\n"));
    }
    body.push_str(&render_dispatch(&dispatched, &rec));

    let mut result = ToolResult::text(body);
    result.details = Some(record_details(&rec));
    Ok(result)
}

/// `Agent`：解析类型 → 应用 frontmatter 权威性 → 分发。
async fn agent_tool(args: Value, ctx: ToolExecCtx) -> Result<ToolResult, ToolError> {
    let a = AgentArgs::parse(&args)?;

    let cfg = manager::config();
    let roster = agent_types::discover_with(&ctx.cwd, discovery_opts(&cfg));
    emit_warnings(&roster.warnings);

    // strict_agent_files：坏文件是致命配置错误，直接拒绝派发（而不是静默少一个类型）
    if !roster.errors.is_empty() {
        return Err(ToolError(format!(
            "strict agent files: {} unparseable agent definition(s): {}",
            roster.errors.len(),
            roster.errors.join("; ")
        )));
    }

    let (ty, note) = resolve_agent_type(&roster, &a.type_name, &cfg)?;

    if let Some(denied) = check_allowed_subagents(&roster, &ctx, &ty) {
        return Ok(denied);
    }

    // ---- 定时：注册任务，**不**现在跑 ----
    if let Some(scheduled) = try_schedule(&a, &cfg) {
        return Ok(scheduled);
    }

    // 谁在调用：主会话，还是某个子代理（嵌套委派）？
    let caller = manager::caller_of(&ctx);
    // mention clone：屏幕外的一次性回合，它发起的 spawn 要当**顶层**（不是嵌套）。
    let clone_marker = caller
        .agent_id
        .clone()
        .filter(|id| id.starts_with(CLONE_MARKER_PREFIX));

    // 深度上限：到顶就拒绝。正常路径下到顶的子代理**根本拿不到**这三个工具
    // （`build_spec` 不把名字写进它的白名单），但用户可以在自己的 agent 文件里写
    // `tools: Agent`，那句话不该绕过上限。clone 是顶层，不受嵌套限深约束。
    if clone_marker.is_none()
        && caller.agent_id.is_some()
        && !nested::nesting_allowed(caller.depth, cfg.max_subagent_depth)
    {
        return Err(ToolError(nested::depth_exceeded(caller.depth)));
    }

    let req = build_spawn_request(&a, &ty, &cfg, &caller, clone_marker.as_deref());

    // 一个 mention 只允许发起一次 spawn：clone 只有一个工具、用完就该停，
    // 但"顺手"再派一个的模型会在没人看得见的地方做事。
    if let Some(marker) = &clone_marker
        && !clone_mark_spawned(marker)
    {
        return Ok(ToolResult::text(
            "Already started an agent for this mention. Stop here.",
        ));
    }

    dispatch_and_render(&ctx, &ty, req, note).await
}

/// `get_subagent_result`：查询（可等待）并读取结果。
async fn result_tool(args: Value, ctx: &ToolExecCtx) -> Result<ToolResult, ToolError> {
    let key = req_str(&args, "agent_id")?;
    let wait = args.get("wait").and_then(|v| v.as_bool()).unwrap_or(false);
    let verbose = args
        .get("verbose")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // 子代理只能看自己派发的那些（嵌套的属主作用域）
    let caller = manager::caller_of(ctx);
    if caller.agent_id.is_some()
        && let Err(e) = manager::owned_record(&caller, &key)
    {
        return Ok(ToolResult::text(e));
    }

    let rec = if wait {
        // Esc（父级回合 abort）只取消**这次等待**，不取消 agent：与取消信号 select，
        // 中止先到就返回当前快照（子代理继续跑，完成通知照常）。
        let abort = ctx.parent_abort.clone();
        let wait_fut = manager::wait(&key);
        let watch = async move {
            while !abort.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };

        pin_mut!(wait_fut, watch);

        match select(wait_fut, watch).await {
            Either::Left((rec, _)) => rec,
            Either::Right((_, _)) => manager::record(&key),
        }
    } else {
        manager::record(&key)
    };

    let Some(rec) = rec else {
        // 查不到用普通文本回复（让模型自救），不算工具错误
        let known: Vec<String> = manager::list().iter().map(|r| r.id.clone()).collect();
        let known = if known.is_empty() {
            "(no agents in this session)".to_string()
        } else {
            known.join(", ")
        };
        return Ok(ToolResult::text(format!(
            "unknown agent id: {key}\nknown agent ids: {known}"
        )));
    };

    let mut result = ToolResult::text(render_record(&rec, verbose));
    result.details = Some(record_details(&rec));

    // 结果已读：抑制尚未投递的完成通知
    if rec.status.is_terminal() {
        manager::consume_result(&rec.id);
    }
    Ok(result)
}

/// `steer_subagent`：注入消息（失败用普通文本回复）。
fn steer_tool(args: Value, ctx: &ToolExecCtx) -> Result<ToolResult, ToolError> {
    let key = req_str(&args, "agent_id")?;
    let message = req_str(&args, "message")?;

    let caller = manager::caller_of(ctx);
    if caller.agent_id.is_some()
        && let Err(e) = manager::owned_record(&caller, &key)
    {
        return Ok(ToolResult::text(e));
    }

    match manager::steer(&key, &message) {
        Ok(()) => Ok(ToolResult::text(format!(
            "steering message delivered to sub-agent {key}"
        ))),
        Err(e) => Ok(ToolResult::text(e)),
    }
}

/// 前台/后台的工具结果文本。
fn render_dispatch(dispatched: &Dispatched, rec: &AgentRecord) -> String {
    if dispatched.background {
        if dispatched.queued {
            return format!(
                "subagent {} queued ({}). The background pool is full; it starts automatically. \
                 You will be notified when it finishes — do not poll.",
                rec.id, rec.display_name
            );
        }
        return format!(
            "subagent {} launched ({}). You will be notified when it finishes — do not sleep or \
             poll for it. Use get_subagent_result(agent_id=\"{}\", wait=true) only if you must block on it now.",
            rec.id, rec.display_name, rec.id
        );
    }
    render_record(rec, false)
}

/// 状态头 + 结果正文（`/agents` 与工具结果共用）。
fn render_record(rec: &AgentRecord, verbose: bool) -> String {
    let mut out = rec.status_header();
    if let Some(result) = rec
        .result
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push('\n');
        out.push_str(result);
    } else if !rec.status.is_terminal() {
        out.push('\n');
        out.push_str(&format!("(still {})", rec.status.as_str()));
    }
    if verbose {
        out.push_str("\n\n--- transcript ---\n");
        out.push_str(&notify::verbose_transcript(rec));
    }
    out
}

/// 结构化元数据（TUI/其它消费者可读）。
fn record_details(rec: &AgentRecord) -> Value {
    json!({
        "id": rec.id,
        "name": rec.name,
        "type": rec.agent_type,
        "displayName": rec.display_name,
        "status": rec.status.as_str(),
        "background": rec.background,
        "turns": rec.turns,
        "toolUses": rec.tool_uses,
        "tokens": {
            "input": rec.usage.input,
            "output": rec.usage.output,
            "cacheRead": rec.usage.cache_read,
            "cacheWrite": rec.usage.cache_write,
            "total": rec.usage.display_total(),
        },
        "durationMs": rec.duration_ms(),
        "sessionPath": rec.session_path,
    })
}

/// 发现告警：按 (来源, 原因) 每会话只提示一次，避免每轮 spawn 刷屏。
fn emit_warnings(warnings: &[DiscoveryWarning]) {
    for w in warnings {
        let key = format!(
            "{}|{}",
            w.source
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            w.message
        );
        if manager::note_warning(&key) {
            util_notify(&w.message, UiNotifyLevel::Warning);
        }
    }
}

/// 发送纯文本用户通知（转发到 [`super::util::notify_text`]）。
fn util_notify(text: &str, level: UiNotifyLevel) {
    notify_text(text, level);
}

/// 富文本通知（着色卡片）。
fn util_notify_spans(spans: Vec<RichSpan>, level: UiNotifyLevel) {
    notify(spans, level);
}

/// `--subagents-workflow-file` 记下的待跑文件（每进程最多跑一次）。
fn pending_workflow_file() -> &'static Mutex<Option<String>> {
    /// 待执行的 `--subagents-workflow-file` 路径，每进程最多跑一次。
    static SLOT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// 拿到能物化子代理的上下文后，把 `--subagents-workflow-file` 指的那个工作流跑起来。
///
/// 为什么拖到这里：跑工作流必须有 `make_sub_agent`，而它只随 [`ToolExecCtx`] 来
/// （`on_exec_ctx` 在会话建立与每回合开始都会给一次）。
/// 拿不到运行时句柄时**不取走**待跑文件——留给下一次给上下文的机会。
fn launch_pending_workflow_file(ctx: &ToolExecCtx) {
    // 工作流被关掉/让位了：清掉待跑项，别在以后莫名其妙跑起来
    if session::verdict().withdrawn() {
        pending_workflow_file().lock().unwrap().take();
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let pending = pending_workflow_file().lock().unwrap().as_ref().cloned();
    let Some(path) = pending else {
        return;
    };

    let ctx = ctx.clone();
    handle.spawn(async move {
        match workflow::tool::execute_file(&ctx, &path).await {
            Ok(_) => {
                pending_workflow_file().lock().unwrap().take(); // 取走待跑项（只有在真启动之后）
                util_notify(
                    &format!("workflow file started in the background ({})", path),
                    UiNotifyLevel::Info,
                );
            }
            Err(e) => {
                pending_workflow_file().lock().unwrap().take();
                util_notify(
                    &format!("--subagents-workflow-file: {}", e.0),
                    UiNotifyLevel::Warning,
                );
            }
        }
    });
}

/// `/agents [types|result <id>|stop <id>|settings]`：无参打开菜单。
fn command_agents(_app: &mut App, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim()),
        None => (arg, ""),
    };

    match sub {
        "types" => show_types(),
        "result" => {
            if rest.is_empty() {
                util_notify("usage: /agents result <agent-id>", UiNotifyLevel::Warning);
            } else {
                // 允许 `@handle` / `@id`（与输入框 `@` 候选一致）
                let key = rest.strip_prefix('@').unwrap_or(rest);
                match manager::record(key) {
                    Some(rec) => {
                        util_notify_spans(notify::record_card(&rec, false), UiNotifyLevel::Info)
                    }
                    None => {
                        util_notify(&format!("unknown agent id: {key}"), UiNotifyLevel::Warning)
                    }
                }
            }
        }
        "stop" => {
            if rest.is_empty() {
                util_notify(
                    "usage: /agents stop <agent-id | workflow-run-id>",
                    UiNotifyLevel::Warning,
                );
            } else {
                // 允许 `@handle` / `@id`（与输入框 `@` 候选一致）
                let key = rest.strip_prefix('@').unwrap_or(rest);
                if key.starts_with("wf_") {
                    match workflow::task::stop(key) {
                        Ok(()) => {
                            util_notify(&format!("stopping workflow {key}"), UiNotifyLevel::Info)
                        }
                        Err(e) => util_notify(&e, UiNotifyLevel::Warning),
                    }
                } else {
                    match manager::stop(key) {
                        Ok(()) => {
                            util_notify(&format!("stopping sub-agent {key}"), UiNotifyLevel::Info)
                        }
                        Err(e) => util_notify(&e, UiNotifyLevel::Warning),
                    }
                }
            }
        }
        "schedules" | "schedule" => open_schedules_or_warn(),
        "workflows" => open_workflows_or_warn(),
        "settings" => open_settings(),
        "eject" => {
            let (name, rest) = match rest.split_once(char::is_whitespace) {
                Some((a, b)) => (a, b.trim()),
                None => (rest, ""),
            };
            if name.is_empty() {
                util_notify(
                    "usage: /agents eject <type> [project]",
                    UiNotifyLevel::Warning,
                );
            } else {
                match eject_type(name, rest == "project") {
                    Ok(msg) => util_notify(&msg, UiNotifyLevel::Info),
                    Err(e) => util_notify(&e, UiNotifyLevel::Warning),
                }
            }
        }
        "enable" | "disable" => {
            if rest.is_empty() {
                util_notify(
                    &format!("usage: /agents {sub} <type>"),
                    UiNotifyLevel::Warning,
                );
            } else {
                match toggle_type(rest, sub == "enable") {
                    Ok(msg) => util_notify(&msg, UiNotifyLevel::Info),
                    Err(e) => util_notify(&e, UiNotifyLevel::Warning),
                }
            }
        }
        "delete" => {
            if rest.is_empty() {
                util_notify("usage: /agents delete <type>", UiNotifyLevel::Warning);
            } else {
                match delete_type(rest) {
                    Ok(msg) => util_notify(&msg, UiNotifyLevel::Info),
                    Err(e) => util_notify(&e, UiNotifyLevel::Warning),
                }
            }
        }
        "reset" => {
            if rest.is_empty() {
                util_notify("usage: /agents reset <type>", UiNotifyLevel::Warning);
            } else {
                match reset_type(rest) {
                    Ok(msg) => util_notify(&msg, UiNotifyLevel::Info),
                    Err(e) => util_notify(&e, UiNotifyLevel::Warning),
                }
            }
        }
        "edit" => {
            if rest.is_empty() {
                util_notify("usage: /agents edit <type>", UiNotifyLevel::Warning);
            } else if let Err(e) = edit_type(rest) {
                util_notify(&e, UiNotifyLevel::Warning);
            }
        }
        "" => open_menu(),
        other => util_notify(
            &format!("unknown subcommand: {other} (try /agents types)"),
            UiNotifyLevel::Warning,
        ),
    }
    false
}

/// 打开二级选择面板（`SelectKind` + 请求 id 的通用入口）。
///
/// 选项都是回传串（`<action> <name>` / agent id），不需要逐项配色 → 全默认配色。
fn open_select(kind: SelectKind, title: &str, options: Vec<String>) {
    let id = extensions::next_ui_id();
    *select_id_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some((id, kind));
    extensions::request_ui(ExtensionUiRequest::Select {
        id,
        title: title.to_string(),
        options: options.into_iter().map(SelectOption::new).collect(),
    });
}

/// clone 的一个 spawn 守卫（标记 → 已发起）。
fn clone_registry() -> &'static Mutex<HashSet<String>> {
    /// 已发起 spawn 的 clone 标记集合，防止同一 clone 重复派发。
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 生成进程内唯一的 clone 标记（固定前缀 + 自增序号）。
fn next_clone_marker() -> String {
    /// clone 标记的自增序号，保证同进程内生成的标记唯一。
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{CLONE_MARKER_PREFIX}{}", N.fetch_add(1, Ordering::Relaxed))
}

/// 记下这个 clone 已发起一次 spawn；返回 false = 已发起过。
fn clone_mark_spawned(marker: &str) -> bool {
    clone_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(marker.to_string())
}

/// 喂给 clone，让它用 `Agent` 工具写 prompt。
fn agent_mention_reminder(type_name: &str) -> String {
    format!(
        "<system-reminder>\nThe user has expressed a desire to invoke the agent \"{type_name}\". Please invoke the agent appropriately, passing in the required context to it. \n</system-reminder>"
    )
}

/// `agent_mentions = model`：把当前对话克隆进一次性会话，
/// 让副本在**屏幕外**跑一个只有 `Agent` 工具的回合（副本自己写 prompt 并后台 spawn）。
///
/// 无法解析类型 / 无执行上下文时返回 `None`，调用方回落 `direct`。
fn mention_clone(text: &str, messages: &[AgentMessage]) -> Option<UserPromptAction> {
    let mention = mention::parse_mention(text)?;
    if mention::is_reserved_handle(&mention.handle) {
        return None; // `@main` 由 direct 处理
    }

    let cfg = manager::config();
    let roster = agent_types::discover_with(&discovery_cwd(), discovery_opts(&cfg));
    let names: Vec<String> = roster.types.iter().map(|t| t.name.clone()).collect();
    let alias = mention::strip_agent_prefix(&mention.handle);
    let type_name = mention::resolve_handle_to_type(&mention.handle, &names).or_else(|| {
        alias
            .as_deref()
            .and_then(|a| mention::resolve_handle_to_type(a, &names))
    })?;
    let ty = agent_types::resolve(&roster.types, &type_name)
        .ok()?
        .clone();
    let Some((ctx, rt)) = fleet::cached_ctx() else {
        util_notify(
            &format!("Could not start @{type_name}: no execution context yet"),
            UiNotifyLevel::Warning,
        );
        return None;
    };

    // "Prompting"：还没起，等副本写完 prompt；direct 才是 "Started"
    let label = format!("@{}", mention::handle_base(&type_name));
    util_notify(&format!("Prompting {label}…"), UiNotifyLevel::Info);

    let message = mention.message.clone();
    let messages = messages.to_vec();
    rt.spawn(async move {
        let spawned = run_mention_clone(&ctx, &ty, message.clone(), messages).await;
        if !spawned {
            // 副本没发起 spawn（或跑不起来）：不吞 mention，走 direct
            let describe = mention::describe_mention(&message);
            if fleet::spawn_background_agent(ty.clone(), message, describe, None) {
                util_notify(
                    &format!("Started @{} directly", mention::handle_base(&ty.name)),
                    UiNotifyLevel::Warning,
                );
            }
        }
    });
    Some(UserPromptAction::Handled)
}

/// 跑一次 clone：物化只带 `Agent` 的隐藏 child，把 `message + reminder` 交给它。
/// 返回副本是否发起过 spawn（handler 侧标记）。
async fn run_mention_clone(
    ctx: &ToolExecCtx,
    ty: &AgentType,
    message: String,
    messages: Vec<AgentMessage>,
) -> bool {
    let marker = next_clone_marker();
    let mut spec = extensions::SubAgentSpec::new(format!("mention-clone-{}", ty.name));
    spec.tools = Some(vec!["Agent".to_string()]);
    spec.context_messages = Some(messages);
    spec.persist_session = false;
    spec.agent_id = Some(marker.clone());
    spec.depth = 0;

    let mut runner = match (ctx.make_sub_agent)(spec) {
        Ok(r) => r,
        Err(e) => {
            util_notify(
                &format!("Could not prompt {}: {e}", ty.name),
                UiNotifyLevel::Warning,
            );
            return false;
        }
    };

    let prompt = format!("{}\n\n{}", message, agent_mention_reminder(&ty.name));
    _ = runner.run(prompt).await;

    // clone 跑完：看 handler 有没有替它发起 spawn（并清掉标记）
    clone_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&marker)
}

/// `@handle` 直接路由（`direct` 模式、或 clone 不可用 / 忙碌时的回落）。
fn direct_mention(text: &str, cfg: &SubagentConfig) -> Option<UserPromptAction> {
    let mention = mention::parse_mention(text)?;

    // `@main` 地址主对话：剥前缀 + trim 后落回主模型
    if mention::is_reserved_handle(&mention.handle) {
        return Some(UserPromptAction::Rewrite(mention.message));
    }
    let alias = mention::strip_agent_prefix(&mention.handle);

    // 命中存活记录（handle 或 alias，大小写不敏感）
    let resolved = manager::resolve_mention(&mention.handle)
        .or_else(|| alias.as_deref().and_then(manager::resolve_mention));
    if let Some(rec) = resolved {
        let target = format!(
            "@{}",
            rec.alias
                .as_deref()
                .or(rec.handle.as_deref())
                .unwrap_or(&mention.handle)
        );

        if !rec.status.is_terminal() {
            if let Err(e) = manager::steer(&rec.id, &mention.message) {
                util_notify(
                    &format!("Could not message {target}: {e}"),
                    UiNotifyLevel::Warning,
                );
            } else {
                util_notify(&format!("Sent to {target}"), UiNotifyLevel::Info);
            }
            return Some(UserPromptAction::Handled);
        }

        if rec.session_path.is_some() {
            let roster = agent_types::discover_with(&discovery_cwd(), discovery_opts(cfg));
            match agent_types::resolve(&roster.types, &rec.agent_type) {
                Ok(ty) => {
                    if fleet::spawn_background_agent(
                        ty.clone(),
                        mention.message.clone(),
                        format!("resume {}", rec.id),
                        Some(rec.id.clone()),
                    ) {
                        util_notify(&format!("Resuming {target}"), UiNotifyLevel::Info);
                    } else {
                        util_notify(
                            &format!("Could not resume {target}: no execution context yet"),
                            UiNotifyLevel::Warning,
                        );
                    }
                }
                Err(_) => util_notify(
                    &format!(
                        "Could not resume {target}: the {} agent is no longer available",
                        rec.agent_type
                    ),
                    UiNotifyLevel::Warning,
                ),
            }
            return Some(UserPromptAction::Handled);
        }
        // 存活但无 session：落到下面的类型派发
    }

    // 句柄 → 类型（从未跑过的类型也能 `@`）
    let roster = agent_types::discover_with(&discovery_cwd(), discovery_opts(cfg));
    let names: Vec<String> = roster.types.iter().map(|t| t.name.clone()).collect();
    let type_name = mention::resolve_handle_to_type(&mention.handle, &names).or_else(|| {
        alias
            .as_deref()
            .and_then(|a| mention::resolve_handle_to_type(a, &names))
    });

    let Some(type_name) = type_name else {
        return None; // 未知句柄：不吞输入
    };

    let Ok(ty) = agent_types::resolve(&roster.types, &type_name) else {
        return None;
    };

    let handle = mention::handle_base(&type_name);
    if fleet::spawn_background_agent(
        ty.clone(),
        mention.message.clone(),
        mention::describe_mention(&mention.message),
        None,
    ) {
        util_notify(&format!("Started @{handle}"), UiNotifyLevel::Info);
    } else {
        util_notify(
            &format!("Could not start @{handle}: no execution context yet"),
            UiNotifyLevel::Warning,
        );
    }
    Some(UserPromptAction::Handled)
}

/// `/agents`：一级菜单（子代理列表 / 类型 / 设置）。
fn open_menu() {
    let recs = manager::list();
    let active = recs.iter().filter(|r| !r.status.is_terminal()).count();
    let recent = recs.len().saturating_sub(active);
    let types = agent_types::discover(&discovery_cwd()).enabled().count();

    let cfg = manager::config();
    let mut options = Vec::new();

    // Fleet view 入口受 `fleet_view` 开关控制（关掉时菜单不广告它）。
    if cfg.fleet_view {
        options.push(format!(
            "view  Fleet view ({active} active, {recent} recent)"
        ));
    }

    options.push(format!(
        "list  Sub-agents ({active} active, {recent} recent)"
    ));
    options.push(format!("types  Agent types ({types} available)"));

    // 调度/工作流段只在功能开启时出现（菜单不广告已关掉的东西）
    if cfg.schedule {
        options.push(format!(
            "schedules  Scheduled jobs ({} total)",
            schedule::list().len()
        ));
    }

    if !session::verdict().withdrawn() {
        options.push(format!(
            "workflows  Workflows ({} runs)",
            workflow::task::list().len()
        ));
    }
    options.push("create  Create new agent".to_string());
    options.push("settings  Settings".to_string());

    open_select(SelectKind::Menu, "Sub-agents", options);
}

/// 一级菜单选择（token 由 [`open_menu`] 定义）。
fn menu_choice(token: &str) {
    match token {
        "view" => fleet::open_fleet(),
        "list" => show_agents(),
        "types" => show_types(),
        "schedules" => open_schedules_or_warn(),
        "workflows" => open_workflows_or_warn(),
        "create" => fleet::open_wizard(),
        "settings" => open_settings(),
        _ => {}
    }
}

/// `/agents schedules` 与一级菜单共用：未开启/未激活时给出可读原因。
fn open_schedules_or_warn() {
    if !manager::config().schedule {
        util_notify(
            "scheduling is off (`schedule: off` in the subagent extension config)",
            UiNotifyLevel::Warning,
        );
    } else if !schedule::is_active() {
        util_notify(
            "the scheduler is not active in this session yet (send a message first)",
            UiNotifyLevel::Warning,
        );
    } else {
        fleet::open_schedules();
    }
}

/// `/agents workflows` 与一级菜单共用：让位/关闭时给出可读原因。
fn open_workflows_or_warn() {
    if session::verdict().withdrawn() {
        let msg = match session::verdict() {
            Verdict::Disabled => {
                "workflows are off (`workflows: off` in the subagent extension config)"
            }
            _ => {
                "another extension provides a workflow tool, so this extension's workflows are stood down (`workflows: auto`; set `on` to keep both)"
            }
        };
        util_notify(msg, UiNotifyLevel::Warning);
    } else {
        fleet::open_workflows();
    }
}

/// 打开本扩展的设置面板（TUI 占据输入区，与 `/settings` 同形）。
fn open_settings() {
    extensions::request_ui(ExtensionUiRequest::ShowSettings {
        ext: EXT.to_string(),
    });
}

/// `/agents` → 子代理列表：运行中 + 本会话最近完成的二级选择面板。
fn show_agents() {
    let recs = manager::list();
    if recs.is_empty() {
        util_notify("no sub-agents in this session yet", UiNotifyLevel::Info);
        return;
    }

    // 首列是 agent id：on_ui_choice 靠它回查记录（无状态映射）
    let options: Vec<String> = recs
        .iter()
        .map(|r| format!("{} {}", r.id, r.list_line()))
        .collect();
    open_select(SelectKind::Agents, "Sub-agents", options);
}

/// `/agents types`：可用类型只读展示。
fn show_types() {
    let cfg = manager::config();
    let roster = agent_types::discover_with(&discovery_cwd(), discovery_opts(&cfg));

    emit_warnings(&roster.warnings);

    for err in &roster.errors {
        util_notify(
            &format!("strict agent files: {err}"),
            UiNotifyLevel::Warning,
        );
    }

    if roster.types.is_empty() {
        util_notify("(no agent types available)", UiNotifyLevel::Info);
        return;
    }

    // 交互列表：每行以类型名开头（on_ui_choice 靠首个空白 token 取回名字），
    // 选中后进二级操作菜单（eject/enable/disable/delete/reset）。
    let options: Vec<String> = roster
        .types
        .iter()
        .map(|ty| {
            let mark = if ty.enabled { "" } else { " (disabled)" };
            format!(
                "{} — {} [{}] · {}{}",
                ty.name,
                ty.description,
                ty.source.as_str(),
                ty.tools_label(),
                mark
            )
        })
        .collect();

    open_select(SelectKind::Types, "Agent types", options);
}

/// 发现期配置（来自 `subagent.json`）。
fn discovery_opts(cfg: &SubagentConfig) -> DiscoveryOptions {
    DiscoveryOptions {
        strict: cfg.strict_agent_files,
        disable_defaults: cfg.disable_default_agents,
    }
}

/// 解析以类型名为参数的子命令实参（`eject` / `enable` / `disable` / `delete` / `reset` / `edit`）。
///
/// 先按原样匹配（去空白、大小写不敏感）；匹配不到且带 `@` 前缀时，
/// 再把句柄按提及规则映射回类型（含 `@agent-<type>` 别名），
/// 于是 `@explore`、`@general-purpose`、`@code-review`（类型 `Code Review!`）都能直接拿来用。
fn find_type_arg<'a>(types: &'a [AgentType], raw: &str) -> Option<&'a AgentType> {
    if let Some(ty) = agent_types::find_any(types, raw) {
        return Some(ty);
    }

    let handle = raw.trim().strip_prefix('@')?;
    let names: Vec<String> = types.iter().map(|t| t.name.clone()).collect();
    let alias = mention::strip_agent_prefix(handle);
    let name = mention::resolve_handle_to_type(handle, &names).or_else(|| {
        alias
            .as_deref()
            .and_then(|a| mention::resolve_handle_to_type(a, &names))
    })?;
    types.iter().find(|t| t.name == name)
}

/// `/agents eject <type> [project]`：把类型写成 `.md`（默认全局目录）。
fn eject_type(name: &str, project: bool) -> Result<String, String> {
    let cfg = manager::config();
    let cwd = discovery_cwd();
    let roster = agent_types::discover_with(&cwd, discovery_opts(&cfg));

    // `@handle` 先归一成类型名，再走 `resolve`（保留 disabled / ambiguous 的既有诊断）
    let lookup = find_type_arg(&roster.types, name)
        .map(|t| t.name.as_str())
        .unwrap_or_else(|| type_arg_label(name));

    let ty = agent_types::resolve(&roster.types, lookup).map_err(|e| match e {
        ResolveError::Unknown => format!("unknown agent type: {lookup}"),
        ResolveError::Disabled => format!("agent type {lookup:?} is disabled"),
        ResolveError::Ambiguous(_) => {
            format!("agent type {lookup:?} is ambiguous (differs only by case)")
        }
    })?;

    let path = agent_files::eject_path(&ty.name, &cwd, project);
    if path.exists() {
        return Err(format!(
            "refusing to overwrite {}: delete it first or edit it directly",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&path, agent_files::serialize_agent_file(ty))
        .map_err(|e| format!("{}: {e}", path.display()))?;

    Ok(format!(
        "ejected agent type {:?} to {} ({})",
        ty.name,
        path.display(),
        if project { "project" } else { "global" }
    ))
}

/// `/agents enable|disable <type>`：逐行改 frontmatter 的 `enabled:`。
fn toggle_type(name: &str, enable: bool) -> Result<String, String> {
    let cfg = manager::config();
    let cwd = discovery_cwd();
    let roster = agent_types::discover_with(&cwd, discovery_opts(&cfg));

    // 这里必须能操作**已禁用**的类型（否则 disable 过的文件永远 enable 不回来）；
    // `@handle` 也接受（find_type_arg 先按名、再按句柄解析）。
    let ty = find_type_arg(&roster.types, name)
        .ok_or_else(|| format!("unknown agent type: {}", type_arg_label(name)))?;
    let path = agent_files::find_agent_file(ty, &cwd).ok_or_else(|| {
        format!(
            "agent type {:?} has no .md file yet; eject it first with /agents eject {}",
            ty.name, ty.name
        )
    })?;
    let content = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;

    if enable {
        let (updated, changed) = agent_files::enable_in_content(&content);
        if !changed {
            // 无法逐行改写的写法（如 `enabled: False`）：如实拒绝，不谎报
            return Err(format!(
                "could not re-enable {:?} in {}: remove the enabled: line by hand",
                ty.name,
                path.display()
            ));
        }

        std::fs::write(&path, updated).map_err(|e| format!("{}: {e}", path.display()))?;

        return Ok(format!(
            "enabled agent type {:?} ({})",
            ty.name,
            path.display()
        ));
    }

    let (updated, outcome) = agent_files::disable_in_content(&content);
    match outcome {
        agent_files::DisableOutcome::Disabled => {
            std::fs::write(&path, updated).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(format!(
                "disabled agent type {:?} ({})",
                ty.name,
                path.display()
            ))
        }
        agent_files::DisableOutcome::AlreadyDisabled => {
            Ok(format!("agent type {:?} is already disabled", ty.name))
        }
        agent_files::DisableOutcome::NoFrontmatter => Err(format!(
            "{} has no YAML frontmatter block; add `enabled: false` by hand",
            path.display()
        )),
    }
}

/// 解析一个类型（**忽略 enabled**，供 delete/reset 操作已禁用项）。
fn resolve_type_any(name: &str) -> Result<AgentType, String> {
    let cfg = manager::config();
    let cwd = discovery_cwd();
    let roster = agent_types::discover_with(&cwd, discovery_opts(&cfg));
    find_type_arg(&roster.types, name)
        .cloned()
        .ok_or_else(|| format!("unknown agent type: {}", type_arg_label(name)))
}

/// 类型是不是内置默认三型之一（`reset` 只对它有意义）。
fn is_builtin_default(name: &str) -> bool {
    agent_types::default_types()
        .iter()
        .any(|t| t.name.eq_ignore_ascii_case(name))
}

/// `/agents delete <type>`：删掉它的 `.md`（内置默认本无文件 → 报错说明）。
fn delete_type(name: &str) -> Result<String, String> {
    let ty = resolve_type_any(name)?;
    let cwd = discovery_cwd();
    let path = agent_files::find_agent_file(&ty, &cwd).ok_or_else(|| {
        format!(
            "agent type {:?} has no .md file to delete (built-in defaults live in the binary)",
            ty.name
        )
    })?;
    std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("Deleted {}", path.display()))
}

/// `/agents reset <type>`：删掉内置默认的覆盖 `.md`，恢复内嵌定义。
fn reset_type(name: &str) -> Result<String, String> {
    // 先解析出类型（顺带处理 `@handle`），再按规范化后的名字判是否内置默认
    let ty = resolve_type_any(name)?;
    if !is_builtin_default(&ty.name) {
        return Err(format!(
            "{:?} is not a built-in default — nothing to reset (use /agents delete to remove it)",
            ty.name
        ));
    }

    let cwd = discovery_cwd();
    let path = agent_files::find_agent_file(&ty, &cwd).ok_or_else(|| {
        format!(
            "{:?} has no override file; the built-in default is already in effect",
            ty.name
        )
    })?;
    std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!(
        "Restored built-in default {:?} (deleted {})",
        ty.name,
        path.display()
    ))
}

/// `/agents edit <type>`：在核心多行编辑器里打开该类型的 `.md`。
fn edit_type(name: &str) -> Result<(), String> {
    let ty = resolve_type_any(name)?;
    let cwd = discovery_cwd();
    let path = agent_files::find_agent_file(&ty, &cwd).ok_or_else(|| {
        format!(
            "agent type {:?} has no .md file to edit; eject it first with /agents eject {}",
            ty.name, ty.name
        )
    })?;
    let content = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    fleet::open_type_editor(&ty.name, path, content);
    Ok(())
}

/// 一个类型的二级操作菜单（按当前状态只列出这时真能用的动作）。
fn open_type_actions(name: &str) {
    let ty = match resolve_type_any(name) {
        Ok(t) => t,
        Err(e) => {
            util_notify(&e, UiNotifyLevel::Warning);
            return;
        }
    };
    let cwd = discovery_cwd();
    let file = agent_files::find_agent_file(&ty, &cwd);
    let is_default = is_builtin_default(&ty.name);

    let mut options: Vec<String> = Vec::new();
    if !ty.enabled && file.is_some() {
        options.push(format!("enable {}", ty.name));
    }
    if ty.enabled && file.is_some() {
        options.push(format!("disable {}", ty.name));
    }
    // 内置默认且无覆盖文件 → 只能 eject（导出为可编辑的 .md）
    if file.is_none() && is_default {
        options.push(format!("eject {}", ty.name));
    }
    if file.is_some() {
        options.push(format!("edit {}", ty.name));
    }
    // 内置默认的覆盖文件用 `reset` 恢复内嵌默认；
    // 对内置默认再列一个 `delete` 只是同义（两者都删同一个覆盖文件），
    // 所以只留 `reset`。自定义类型没有内嵌定义可恢复，才用 `delete`。
    if file.is_some() && !is_default {
        options.push(format!("delete {}", ty.name));
    }
    if file.is_some() && is_default {
        options.push(format!("reset {}", ty.name));
    }

    // 末项固定为 `back`（不带类型名）：选中后回到上一级类型清单
    options.push("back".to_string());

    open_select(SelectKind::TypeActions, &format!("Agent: {name}"), options);
}

/// 二级操作菜单的选择结果：`<action> <name>`（`back` 不带类型名，返回上一级类型清单）。
///
/// 通知在这里就直接发出：动作成败都只影响 UI，没有返回值要交给调用方。
fn type_actions_choice(choice: &str) {
    let mut parts = choice.splitn(2, ' ');
    let action = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("").trim();

    if action == "back" {
        show_types();
        return;
    }

    match type_action(action, name) {
        Ok(Some(msg)) => util_notify(&msg, UiNotifyLevel::Info),
        Ok(None) => {}
        Err(e) => util_notify(&e, UiNotifyLevel::Warning),
    }
}

/// 执行二级菜单动作；`Ok(None)` = 无动作（未知 token/空）。
///
/// `back` 不在这里处理：它由 [`type_actions_choice`] 拦截并重新打开类型清单。
fn type_action(action: &str, name: &str) -> Result<Option<String>, String> {
    if name.is_empty() {
        return Ok(None);
    }

    match action {
        "enable" => toggle_type(name, true).map(Some),
        "disable" => toggle_type(name, false).map(Some),
        "eject" => eject_type(name, false).map(Some),
        "edit" => edit_type(name).map(|_| None),
        "delete" => delete_type(name).map(Some),
        "reset" => reset_type(name).map(Some),
        _ => Ok(None),
    }
}

#[cfg(test)]
pub(crate) use manager::TestLock;

/// 测试互斥：本扩展的全局态（manager 注册表 + UI 队列）跨模块共享，
/// 同 crate 内**所有**触碰它的测试都必须取它（含 `core/agent_session.rs` 的端到端用例：
/// 那里的 register/unregister 与 `reset_all` 会互相清空对方的在飞记录）。
#[cfg(test)]
pub(crate) fn test_lock() -> manager::TestLock {
    manager::test_lock()
}

/// 测试用：当前的记录树（id, 派发者, 深度）。
///
/// 核心的端到端用例（`core/agent_session.rs`）要断言嵌套关系，而 `manager` 对它是私有模块；
/// 这个只读快照把"谁派了谁"暴露到刚好够断言的程度。
#[cfg(test)]
pub(crate) fn record_tree_for_test() -> Vec<(String, Option<String>, u32)> {
    manager::list()
        .iter()
        .map(|r| (r.id.clone(), r.parent_agent_id.clone(), r.depth))
        .collect()
}

/// 测试用：某个记录的 agent 类型与结果（同上，供核心用例断言）。
#[cfg(test)]
pub(crate) fn record_for_test(id: &str) -> Option<(String, Option<String>)> {
    manager::record(id).map(|r| (r.agent_type.clone(), r.result.clone()))
}

/// 测试接缝：agent 记录的子会话路径。
///
/// `agent_session.rs` 的 autoresearch 端到端用例用它断言"resume 续的是同一个文件"
/// （子会话落在哪个目录取决于子任务所在线程的 agent_dir，测试线程看不见它）。
#[cfg(test)]
pub(crate) fn agent_session_path(id: &str) -> Option<String> {
    manager::record(id).and_then(|rec| rec.session_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_args_are_enforced() {
        assert!(req_str(&json!({}), "prompt").is_err());
        assert!(req_str(&json!({"prompt": "  "}), "prompt").is_err());
        assert_eq!(req_str(&json!({"prompt": " x "}), "prompt").unwrap(), "x");
        assert_eq!(opt_str(&json!({"model": ""}), "model"), None);
    }

    #[test]
    fn unknown_tool_is_rejected() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: ".".to_string(),
            make_sub_agent: Arc::new(|_| Err("unused".to_string())),
            parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        };
        let err = rt
            .block_on(dispatch_tool("nope", json!({}), ctx))
            .unwrap_err();
        assert!(err.0.contains("unknown tool"));
    }

    #[test]
    fn missing_prompt_and_type_are_tool_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let make = || ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: ".".to_string(),
            make_sub_agent: Arc::new(|_| Err("unused".to_string())),
            parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        };
        let err = rt
            .block_on(dispatch_tool("Agent", json!({"description": "d"}), make()))
            .unwrap_err();
        assert!(err.0.contains("prompt"), "{}", err.0);
        let err = rt
            .block_on(dispatch_tool(
                "Agent",
                json!({"prompt": "p", "description": "d"}),
                make(),
            ))
            .unwrap_err();
        assert!(err.0.contains("subagent_type"), "{}", err.0);
    }

    #[test]
    fn negative_max_turns_is_rejected() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: ".".to_string(),
            make_sub_agent: Arc::new(|_| Err("unused".to_string())),
            parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        };
        let err = rt
            .block_on(dispatch_tool(
                "Agent",
                json!({"prompt": "p", "description": "d", "subagent_type": "Explore", "max_turns": -1}),
                ctx,
            ))
            .unwrap_err();
        assert!(err.0.contains("max_turns"), "{}", err.0);
    }

    #[test]
    fn unknown_result_id_returns_plain_text_not_error() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt
            .block_on(dispatch_tool(
                "get_subagent_result",
                json!({"agent_id": "deadbeef"}),
                ToolExecCtx {
                    execute_tool: crate::core::extensions::unavailable_tool_exec(),
                    parent_tool_call_id: None,
                    nested_calls: Default::default(),
                    session_branch_entries: Default::default(),
                    script_tools: Default::default(),
                    cwd: ".".to_string(),
                    make_sub_agent: Arc::new(|_| Err("unused".to_string())),
                    parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    agent_id: None,
                    depth: 0,
                    parent_model: None,
                    script_call: false,
                },
            ))
            .unwrap();
        assert!(out.text.contains("unknown agent id"), "{}", out.text);
    }

    #[test]
    fn steer_unknown_target_returns_plain_text() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();
        let out = rt().block_on(async {
            dispatch_tool(
                "steer_subagent",
                json!({"agent_id": "deadbeef", "message": "hi"}),
                ToolExecCtx {
                    execute_tool: crate::core::extensions::unavailable_tool_exec(),
                    parent_tool_call_id: None,
                    nested_calls: Default::default(),
                    session_branch_entries: Default::default(),
                    script_tools: Default::default(),
                    cwd: ".".to_string(),
                    make_sub_agent: Arc::new(|_| Err("unused".to_string())),
                    parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    agent_id: None,
                    depth: 0,
                    parent_model: None,
                    script_call: false,
                },
            )
            .await
            .unwrap()
        });
        assert!(out.text.contains("unknown agent id"), "{}", out.text);
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    /// `@mention` 路由测试用的驻留 runner：steer 收集到共享 inbox，run 挂起直到被 abort。
    struct HoldRunner {
        inbox: Arc<Mutex<Vec<String>>>,
        abort: Arc<std::sync::atomic::AtomicBool>,
    }
    impl crate::core::extensions::SubAgentRunner for HoldRunner {
        fn controls(&self) -> crate::core::extensions::SubAgentControls {
            let inbox = self.inbox.clone();
            crate::core::extensions::SubAgentControls {
                steer: Arc::new(move |t: String| inbox.lock().unwrap().push(t)),
                abort: self.abort.clone(),
                session_path: None,
            }
        }
        fn run(&mut self, _prompt: String) -> BoxFuture<'_, std::result::Result<String, String>> {
            let abort = self.abort.clone();
            Box::pin(async move {
                while !abort.load(std::sync::atomic::Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err("Operation aborted".to_string())
            })
        }
    }

    /// 回显 runner：`run` 立即返回 `R:<prompt>`（工作流工具端到端测试用）。
    struct EchoRunner;
    impl crate::core::extensions::SubAgentRunner for EchoRunner {
        fn controls(&self) -> crate::core::extensions::SubAgentControls {
            crate::core::extensions::SubAgentControls {
                steer: Arc::new(|_t: String| {}),
                abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                session_path: None,
            }
        }
        fn run(&mut self, prompt: String) -> BoxFuture<'_, std::result::Result<String, String>> {
            Box::pin(async move { Ok(format!("R:{prompt}")) })
        }
    }

    fn echo_ctx(cwd: &str) -> ToolExecCtx {
        ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: cwd.to_string(),
            make_sub_agent: Arc::new(|_spec: crate::core::extensions::SubAgentSpec| {
                Ok(Box::new(EchoRunner) as Box<dyn crate::core::extensions::SubAgentRunner>)
            }),
            parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        }
    }

    /// 总是失败的 runner：验证“跑挂的子代理 → 脚本拿到 null”。
    struct FailRunner;
    impl crate::core::extensions::SubAgentRunner for FailRunner {
        fn controls(&self) -> crate::core::extensions::SubAgentControls {
            crate::core::extensions::SubAgentControls {
                steer: Arc::new(|_t: String| {}),
                abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                session_path: None,
            }
        }
        fn run(&mut self, _prompt: String) -> BoxFuture<'_, std::result::Result<String, String>> {
            Box::pin(async move { Err("provider exploded".to_string()) })
        }
    }

    fn fail_ctx(cwd: &str) -> ToolExecCtx {
        ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: cwd.to_string(),
            make_sub_agent: Arc::new(|_spec: crate::core::extensions::SubAgentSpec| {
                Ok(Box::new(FailRunner) as Box<dyn crate::core::extensions::SubAgentRunner>)
            }),
            parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        }
    }

    /// 等一个 run 到终态（短超时；这些用例里的 runner 都是同步的）。
    fn await_run(run_id: &str) -> workflow::task::WorkflowRun {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let run = workflow::task::find(run_id).expect("run registered");
            if run.status.is_terminal() {
                return run;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "run 未在预期时间内结束"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// 工具 + `name`：脚本从磁盘读，落盘路径回给模型（"改它再跑"的回路）。
    #[test]
    fn workflow_tool_runs_a_saved_workflow_and_reports_its_path() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        workflow::task::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}

        // 全局根里放一个具名工作流
        let dir = crate::core::settings_manager::agent_dir()
            .join("extensions")
            .join("workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("greet.js"),
            "export const meta = { name: 'greet', description: 'd' }\nconst a = await agent('hi')\nreturn { a }\n",
        )
        .unwrap();

        // 运行体是 tokio::spawn 出去的，必须让 runtime 活到它跑完（别用 `rt()` 的临时值）
        let rt = rt();
        let out = rt
            .block_on(dispatch_tool(
                "SubagentWorkflow",
                json!({ "name": "greet" }),
                echo_ctx("."),
            ))
            .unwrap();
        let run_id = out.details.as_ref().unwrap()["taskId"]
            .as_str()
            .unwrap()
            .to_string();
        // 正文带上落盘路径（现在是会话任务目录里的 <runId>.workflow.js）
        assert!(out.text.contains("Script: "), "{}", out.text);
        assert!(out.text.contains(".workflow.js"), "{}", out.text);
        assert!(out.text.contains("To iterate"), "{}", out.text);

        let run = await_run(&run_id);
        assert_eq!(run.status, workflow::task::RunStatus::Completed);
        assert_eq!(
            run.value,
            Some(json!({ "a": "R:hi" })),
            "具名脚本被读到并跑通"
        );
        // 落盘的脚本内容 = 具名文件内容
        let saved = run.script_path.expect("script_path");
        let text = std::fs::read_to_string(&saved).unwrap();
        assert!(text.contains("name: 'greet'"), "{text}");
        let _ = std::fs::remove_file(&saved);
        while crate::core::extensions::take_pending_ui().is_some() {}
        config::write_config(&config::config_path(), &config::SubagentConfig::default());
        manager::reload_config();
        workflow::task::reset_all();
        manager::reset_all();
    }

    /// 真失败的子代理 → 脚本侧得 `null`（不是空字符串）。
    #[test]
    fn workflow_agent_failure_returns_null() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        workflow::task::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}

        let rt = rt();
        let out = rt
            .block_on(dispatch_tool(
                "SubagentWorkflow",
                json!({
                    "script": "export const meta = { name: 'f', description: 'd' }\nconst r = await agent('boom')\nreturn { r }"
                }),
                fail_ctx("."),
            ))
            .unwrap();
        let run_id = out.details.as_ref().unwrap()["taskId"]
            .as_str()
            .unwrap()
            .to_string();
        let run = await_run(&run_id);
        assert_eq!(
            run.status,
            workflow::task::RunStatus::Completed,
            "脚本本身跑完"
        );
        assert_eq!(
            run.value,
            Some(json!({ "r": null })),
            "失败的 agent 应是 null 而不是空串"
        );
        while crate::core::extensions::take_pending_ui().is_some() {}
        config::write_config(&config::config_path(), &config::SubagentConfig::default());
        manager::reload_config();
        workflow::task::reset_all();
        manager::reset_all();
    }

    /// 嵌套 `workflow()` 端到端：外层脚本内联调用另一个具名工作流，拿到子脚本 `return` 值。
    #[test]
    fn workflow_tool_runs_a_nested_named_workflow() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        workflow::task::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}

        let dir = tempfile::tempdir().unwrap();
        let child_path = dir.path().join("child.js");
        std::fs::write(
            &child_path,
            "export const meta = { name: 'child', description: 'd' }\nconst a = await agent('hi')\nreturn { a }\n",
        )
        .unwrap();
        let parent = format!(
            "export const meta = {{ name: 'parent', description: 'd' }}\nconst got = await workflow({{ scriptPath: '{}' }})\nreturn {{ nested: got }}\n",
            child_path.display()
        );

        let rt = rt();
        let out = rt
            .block_on(dispatch_tool(
                "SubagentWorkflow",
                json!({ "script": parent }),
                echo_ctx("."),
            ))
            .unwrap();
        let run_id = out.details.as_ref().unwrap()["taskId"]
            .as_str()
            .unwrap()
            .to_string();
        let run = await_run(&run_id);
        assert_eq!(
            run.status,
            workflow::task::RunStatus::Completed,
            "err: {:?}",
            run.error
        );
        assert_eq!(
            run.value,
            Some(json!({ "nested": { "a": "R:hi" } })),
            "子脚本的 return 值应原样交回外层"
        );
        while crate::core::extensions::take_pending_ui().is_some() {}
        config::write_config(&config::config_path(), &config::SubagentConfig::default());
        manager::reload_config();
        workflow::task::reset_all();
        manager::reset_all();
    }

    /// `opts.gate`：子代理跑完后跑命令；非零退出即该 agent 失败（输出即错误）→ 脚本拿到 null。
    #[test]
    fn agent_gate_failure_fails_the_agent_and_returns_null() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        workflow::task::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}

        let script = "export const meta = { name: 'g', description: 'd' }\n\
                      const ok = await agent('pass', { gate: 'true' })\n\
                      const bad = await agent('fail', { gate: 'echo gate-broke >&2; exit 3' })\n\
                      return { ok, bad }";
        let rt = rt();
        let out = rt
            .block_on(dispatch_tool(
                "SubagentWorkflow",
                json!({ "script": script }),
                echo_ctx("."),
            ))
            .unwrap();
        let run_id = out.details.as_ref().unwrap()["taskId"]
            .as_str()
            .unwrap()
            .to_string();
        let run = await_run(&run_id);
        assert_eq!(
            run.status,
            workflow::task::RunStatus::Completed,
            "{:?}",
            run.error
        );
        assert_eq!(
            run.value,
            Some(json!({ "ok": "R:pass", "bad": null })),
            "gate 通过的不受影响；gate 失败的那个变成 null"
        );
        // 失败原因就是 gate 的输出，且落在进度条目的 error 上
        let log = run.log();
        let failed = log
            .iter()
            .find(|e| e["type"] == "workflow_agent" && e["state"] == "error")
            .expect("应有失败条目");
        assert!(
            failed["error"]
                .as_str()
                .unwrap_or_default()
                .contains("gate-broke"),
            "{failed}"
        );
        while crate::core::extensions::take_pending_ui().is_some() {}
        workflow::task::reset_all();
        manager::reset_all();
    }

    /// 让位：别的扩展提供了 `Workflow`/`workflow` 工具时，`auto`（缺省）收起本扩展的工具；
    /// `on` 保留；`off` 一律不提供。
    #[test]
    fn workflow_tool_stands_down_when_another_extension_provides_one() {
        struct ForeignWorkflow;
        impl Extension for ForeignWorkflow {
            fn name(&self) -> &str {
                "foreign-workflow-test"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![ExtensionTool::simple(
                    "Workflow",
                    "another orchestrator",
                    json!({ "type": "object" }),
                    "run a workflow",
                )]
            }
        }

        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        session::reset();

        let visible = |ext: &Subagent| {
            ext.filter_extension_tools(ext.tools())
                .iter()
                .any(|t| t.name == workflow::tool::WORKFLOW_TOOL_NAME)
        };

        // 无冲突：提供
        assert!(visible(&Subagent));
        // 有别的编排扩展：auto 让位
        crate::core::extensions::register_extension(ForeignWorkflow);
        session::invalidate();
        assert!(!visible(&Subagent), "auto 遇冲突必须收起工作流工具");
        assert_eq!(session::verdict(), session::Verdict::StandDown);

        // on：保留（用户显式选择不让位）
        let mut cfg = manager::config();
        cfg.workflows = config::WorkflowsMode::On;
        config::write_config(&config::config_path(), &cfg);
        manager::reload_config();
        session::invalidate();
        assert!(visible(&Subagent), "workflows: on 时不让位");
        assert_eq!(session::verdict(), session::Verdict::Available);

        // off：不提供
        cfg.workflows = config::WorkflowsMode::Off;
        config::write_config(&config::config_path(), &cfg);
        manager::reload_config();
        session::invalidate();
        assert!(!visible(&Subagent), "workflows: off 时必须收起");
        assert_eq!(session::verdict(), session::Verdict::Disabled);

        _ = crate::core::extensions::unregister_extension("foreign-workflow-test");
        session::reset();
    }

    /// `resumeFromRunId`：第一次跑付全价，第二次跑**只有改动的那个 agent 真跑**，
    /// 前缀从 journal 回来（进度条目带 `cached`），摘要说明重放了多少。
    #[test]
    fn resume_from_run_id_replays_the_unchanged_prefix() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        workflow::task::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}
        // 会话键：journal 与脚本都落在会话任务目录里
        session::reset();
        session::set_session_key(Some("test-session".to_string()));
        let cwd = std::env::temp_dir().to_string_lossy().to_string();

        let script = |last: &str| {
            format!(
                "export const meta = {{ name: 'r', description: 'd' }}\n\
                 const a = await agent('one')\n\
                 const b = await agent('two')\n\
                 const c = await agent('{last}')\n\
                 return {{ a, b, c }}"
            )
        };

        // 第一遍（3 个 agent 全真跑）
        let rt = rt();
        let first = rt
            .block_on(dispatch_tool(
                "SubagentWorkflow",
                json!({ "script": script("three") }),
                echo_ctx(&cwd),
            ))
            .unwrap();
        let first_id = first.details.as_ref().unwrap()["taskId"]
            .as_str()
            .unwrap()
            .to_string();
        let run = await_run(&first_id);
        assert_eq!(
            run.value,
            Some(json!({ "a": "R:one", "b": "R:two", "c": "R:three" }))
        );
        let journal_path = workflow::tool::journal_path_for_test(&cwd, &first_id);
        assert!(
            journal_path.exists(),
            "第一遍应留下 journal: {}",
            journal_path.display()
        );

        // 第二遍：同一个前缀，只有第三个 agent 变了 → 前两个重放
        let second = rt
            .block_on(dispatch_tool(
                "SubagentWorkflow",
                json!({ "script": script("three-changed"), "resumeFromRunId": first_id }),
                echo_ctx(&cwd),
            ))
            .unwrap();
        let second_id = second.details.as_ref().unwrap()["taskId"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(second_id, first_id, "恢复产生新的 run id");
        let run = await_run(&second_id);
        assert_eq!(
            run.value,
            Some(json!({ "a": "R:one", "b": "R:two", "c": "R:three-changed" })),
            "前缀来自 journal，改动那个真跑"
        );
        let log = run.log();
        let cached: Vec<&serde_json::Value> = log
            .iter()
            .filter(|e| e["type"] == "workflow_agent" && e["cached"] == true)
            .collect();
        assert_eq!(cached.len(), 2, "前两个 agent 应标 cached: {log:?}");
        assert!(cached.iter().all(|e| e["durationMs"] == 0));
        let live: Vec<&serde_json::Value> = log
            .iter()
            .filter(|e| {
                e["type"] == "workflow_agent" && e["state"] == "done" && e["cached"] != true
            })
            .collect();
        assert_eq!(live.len(), 1, "只有改动的那个真跑");
        let env = workflow::notify::envelope(&run);
        assert!(env.contains("2 of 3 agents replayed from"), "{env}");

        // 未知 run id / 仍在运行的 run：明确报错
        let err = rt
            .block_on(dispatch_tool(
                "SubagentWorkflow",
                json!({ "script": script("x"), "resumeFromRunId": "wf_nope" }),
                echo_ctx(&cwd),
            ))
            .unwrap_err();
        assert!(err.0.contains("no workflow run"), "{}", err.0);

        let _ = std::fs::remove_file(&journal_path);
        while crate::core::extensions::take_pending_ui().is_some() {}
        session::reset();
        workflow::task::reset_all();
        manager::reset_all();
    }

    /// `agent({ resume })`：label 不存在时该 agent 失败（脚本得 null），错误可读。
    ///
    /// 真正"续跑同一个 child"的链路（要打开既有子会话）在 `core/agent_session.rs` 的
    /// 端到端用例里——那边有真 `make_sub_agent` 与假 provider；本文件的假 runner
    /// 报不出会话路径，`manager` 的 resume 会（正确地）拒绝。
    #[test]
    fn agent_resume_unknown_label_fails_that_agent_only() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        workflow::task::reset_all();
        session::reset();
        session::set_session_key(Some("test-session".to_string()));
        while crate::core::extensions::take_pending_ui().is_some() {}
        let script = "export const meta = { name: 'res', description: 'd' }\n\
                      const a = await agent('start')\n\
                      const b = await agent('more', { resume: 'never-ran' })\n\
                      return { a, b }";
        let rt = rt();
        let out = rt
            .block_on(dispatch_tool(
                "SubagentWorkflow",
                json!({ "script": script }),
                echo_ctx("."),
            ))
            .unwrap();
        let run_id = out.details.as_ref().unwrap()["taskId"]
            .as_str()
            .unwrap()
            .to_string();
        let run = await_run(&run_id);
        let value = run.value.clone().unwrap();
        assert_eq!(value["a"], "R:start");
        assert_eq!(value["b"], serde_json::Value::Null);
        assert!(
            run.log().iter().any(|e| e["error"]
                .as_str()
                .is_some_and(|m| m.contains("Cannot resume \"never-ran\""))),
            "错误要说清是哪个 label: {:?}",
            run.log()
        );
        while crate::core::extensions::take_pending_ui().is_some() {}
        session::reset();
        workflow::task::reset_all();
        manager::reset_all();
    }

    /// `--subagents-workflow-file <path>`：flag 记下文件，拿到可物化的上下文后自动跑一次；
    /// 工作流被关闭/让位时不做。
    #[test]
    fn cli_flag_workflow_file_launches_a_run() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        workflow::task::reset_all();
        session::reset();
        session::set_session_key(Some("cli-session".to_string()));
        while crate::core::extensions::take_pending_ui().is_some() {}
        // 清掉可能残留的待跑项
        pending_workflow_file().lock().unwrap().take();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cli-workflow.js");
        std::fs::write(
            &path,
            "export const meta = { name: 'cli', description: 'd' }\nreturn { ok: await agent('hi') }\n",
        )
        .unwrap();

        Subagent.apply_cli_flag_value("subagents-workflow-file", path.to_str().unwrap());
        assert_eq!(
            pending_workflow_file().lock().unwrap().as_deref(),
            Some(path.to_str().unwrap())
        );

        let rt = rt();
        rt.block_on(async {
            // 第一次给上下文 → 启动
            Subagent.on_exec_ctx(&echo_ctx("."));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(run) = workflow::task::list().into_iter().next() {
                    assert_eq!(run.name, "cli");
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "flag 指定的工作流没启动"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            // 待跑项已取走（不会重复启动）
            assert!(pending_workflow_file().lock().unwrap().is_none());
        });

        // 工作流被关掉时不启动
        workflow::task::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}
        let mut cfg = manager::config();
        cfg.workflows = config::WorkflowsMode::Off;
        config::write_config(&config::config_path(), &cfg);
        manager::reload_config();
        session::invalidate();
        Subagent.apply_cli_flag_value("subagents-workflow-file", path.to_str().unwrap());
        rt.block_on(async { Subagent.on_exec_ctx(&echo_ctx(".")) });
        assert!(workflow::task::list().is_empty(), "off 时不该启动");
        assert!(
            pending_workflow_file().lock().unwrap().is_none(),
            "待跑项应被清掉"
        );

        // 复位配置
        cfg.workflows = config::WorkflowsMode::Auto;
        config::write_config(&config::config_path(), &cfg);
        manager::reload_config();
        session::reset();
        workflow::task::reset_all();
        manager::reset_all();
    }

    /// 嵌套限深：到顶的子代理即使手里有 `Agent` 工具（用户 agent 文件里写了 `tools: Agent`）
    /// 也会被 handler 拒掉。
    #[test]
    fn nested_calls_past_the_depth_limit_are_refused() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        let max = config::SubagentConfig::default().max_subagent_depth;
        let mut ctx = echo_ctx(".");
        ctx.agent_id = Some("deep".to_string());
        ctx.depth = max;
        let rt = rt();
        let err = rt
            .block_on(dispatch_tool(
                "Agent",
                json!({"prompt": "x", "description": "d", "subagent_type": "general-purpose"}),
                ctx,
            ))
            .unwrap_err();
        assert!(err.0.contains("nesting limit reached"), "{}", err.0);
        assert!(err.0.contains("maxSubagentDepth"), "{}", err.0);
        // 主会话不受影响（深度 0）
        let out = rt
            .block_on(dispatch_tool(
                "Agent",
                json!({"prompt": "x", "description": "d", "subagent_type": "general-purpose", "run_in_background": false}),
                echo_ctx("."),
            ))
            .unwrap();
        assert!(
            out.text.contains("Agent ID") || out.text.contains("subagent"),
            "{}",
            out.text
        );
        while crate::core::extensions::take_pending_ui().is_some() {}
        manager::reset_all();
    }

    /// 后台契约：工具**立刻**返回 task id，运行在没有它的地方继续；完成后注册表里有
    /// 终态、进度日志里有 per-agent 条目、并且投出了给模型的通知。
    #[test]
    fn workflow_tool_returns_task_id_and_runs_in_the_background() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        workflow::task::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}
        // 通知立刻投递（`async` 不攒批）：本用例验的是"完成后有通知"，
        // 而"要不要攒批"取决于进程内还有没有别的后台活儿——那不该影响这条断言。
        // 写**配置文件**而不是内存：`config()` 可能因 mtime 变化重载，内存补丁会被清掉。
        let cfg = config::SubagentConfig {
            join: config::JoinMode::Async,
            ..Default::default()
        };
        config::write_config(&config::config_path(), &cfg);
        manager::reload_config();
        let rt = rt();
        let out = rt
            .block_on(async {
                dispatch_tool(
                    "SubagentWorkflow",
                    json!({
                        "script": "export const meta = { name: 't', description: 'd' }\nconst a = await agent('one')\nconst b = await parallel([() => agent('two'), () => agent('three')])\nreturn { a, b }",
                        "args": {}
                    }),
                    echo_ctx("."),
                )
                .await
                .unwrap()
            });
        // 立刻返回：正文是 task id，结果是"还在跑"
        assert!(
            out.text.contains("started in the background"),
            "{}",
            out.text
        );
        let details = out.details.clone().unwrap();
        let run_id = details["taskId"]
            .as_str()
            .expect("details.taskId")
            .to_string();
        assert!(run_id.starts_with("wf_"), "{run_id}");
        assert_eq!(details["status"], "running");
        assert!(out.text.contains(&run_id), "{}", out.text);

        // 等它跑完（假 runner 是同步的；只等状态，不轮询 sleep 语义）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let run = workflow::task::find(&run_id).expect("run 仍在注册表");
            if run.status.is_terminal() {
                // 结果就是脚本 return 的值；进度日志里有三个 agent 的条目
                assert_eq!(run.status, workflow::task::RunStatus::Completed);
                let value = run.value.clone().unwrap();
                assert_eq!(value["a"], "R:one");
                assert_eq!(value["b"][0], "R:two");
                assert_eq!(value["b"][1], "R:three");
                let log = run.log();
                let agents: Vec<&serde_json::Value> = log
                    .iter()
                    .filter(|e| e["type"] == "workflow_agent" && e["state"] == "done")
                    .collect();
                assert_eq!(agents.len(), 3, "{log:?}");
                assert!(agents.iter().all(|a| a["recordId"].is_string()));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "运行没有在预期时间内结束"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // 通知投给模型（Continuation），卡片投给人（CustomMessage）。
        // `settle_run` 是"先 finish 再 push"，所以状态先可见、通知随后入队——
        // 这里必须轮询等待，一次 drain 会与那个先后顺序赛跑。
        let mut saw_continuation = false;
        let mut saw_card = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !(saw_continuation && saw_card) && std::time::Instant::now() < deadline {
            while let Some(req) = crate::core::extensions::take_pending_ui() {
                match req {
                    extensions::ExtensionUiRequest::Continuation { message } => {
                        let text = serde_json::to_string(&message).unwrap_or_default();
                        assert!(text.contains(&run_id), "{text}");
                        assert!(text.contains("workflow_result"), "{text}");
                        saw_continuation = true;
                    }
                    extensions::ExtensionUiRequest::CustomMessage { custom_type, .. }
                        if custom_type == workflow::card::WORKFLOW_CARD_TYPE =>
                    {
                        saw_card = true;
                    }
                    _ => {}
                }
            }
            if !(saw_continuation && saw_card) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        assert!(saw_continuation, "完成后必须通知模型");
        assert!(saw_card, "完成后必须投递卡片");
        workflow::task::reset_all();
        manager::reset_all();
    }

    /// 工作流一开始跑（尚未派任何 agent）就会出现在 dock 的一段里 →
    /// 工具调用必须请求打开停靠面板（对齐 plan-mode/goal 的自动展示）。
    #[test]
    fn workflow_tool_auto_opens_the_dock() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        workflow::task::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}

        // 无 agent 的脚本：队列里唯一可能出现的 UI 请求就是"打开 dock"本身。
        rt().block_on(async {
            dispatch_tool(
                "SubagentWorkflow",
                json!({
                    "script": "export const meta = { name: 't', description: 'd' }\nreturn 1"
                }),
                echo_ctx("."),
            )
            .await
            .unwrap()
        });

        let mut saw_dock = false;
        while let Some(req) = crate::core::extensions::take_pending_ui() {
            if matches!(req, extensions::ExtensionUiRequest::ShowDock) {
                saw_dock = true;
            }
        }
        assert!(saw_dock, "工作流开跑应请求打开停靠面板");

        workflow::task::reset_all();
        manager::reset_all();
    }

    /// 取走队列里所有通知的纯文本（`NotifyRich` 也拼回字符串）。
    fn notice_texts() -> Vec<String> {
        std::iter::from_fn(crate::core::extensions::take_pending_ui)
            .filter_map(|r| match r {
                extensions::ExtensionUiRequest::Notify { text, .. } => Some(text),
                extensions::ExtensionUiRequest::NotifyRich { spans, .. } => {
                    Some(spans.iter().map(|s| s.text.as_str()).collect())
                }
                _ => None,
            })
            .collect()
    }

    /// dock 与重绘请求：运行中的 run 出现在 dock 的一段里并请求持续重绘
    /// （后台代理/工作流的 spinner 以前在 idle 时是冻的）。
    #[test]
    fn dock_shows_workflow_runs_and_requests_redraw() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        workflow::task::reset_all();
        let ext = Subagent;
        assert!(!ext.wants_redraw());

        let id = workflow::task::new_id();
        let _abort = workflow::task::register(&id, "demo-run", "d", None, None, None);
        assert!(ext.wants_redraw(), "有运行中的 run 必须请求重绘");
        let dock = ext.dock_lines();
        let text: Vec<String> = dock
            .iter()
            .map(|l| l.iter().map(|sp| sp.text.as_str()).collect())
            .collect();
        assert!(text.iter().any(|l| l.contains("Workflows")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("demo-run")), "{text:?}");
        assert!(text.iter().any(|l| l.contains(&id)), "{text:?}");

        // 终态后 dock 段消失、也不再请求重绘（记录仍留在覆盖层里）
        workflow::task::finish(&id, workflow::task::RunStatus::Completed, None, None);
        assert!(!ext.wants_redraw());
        let text: Vec<String> = ext
            .dock_lines()
            .iter()
            .map(|l| l.iter().map(|sp| sp.text.as_str()).collect())
            .collect();
        assert!(!text.iter().any(|l| l.contains("demo-run")), "{text:?}");
        workflow::task::reset_all();
    }

    /// 运行列表覆盖层：只读（不消费按键），列出 run 与它的 agent 行。
    #[test]
    fn workflows_overlay_is_read_only_and_lists_runs() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        workflow::task::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}

        let id = workflow::task::new_id();
        let _abort = workflow::task::register(&id, "demo-run", "why", None, None, None);
        workflow::task::set_agent_count(&id, 1);
        workflow::task::find(&id)
            .unwrap()
            .progress
            .lock()
            .unwrap()
            .push(json!({
                "type": "workflow_agent", "index": 0, "label": "scan a", "state": "done",
                "tokens": 10
            }));

        fleet::open_workflows();
        let Some(extensions::ExtensionUiRequest::ShowOverlay { id: overlay_id }) =
            crate::core::extensions::take_pending_ui()
        else {
            panic!("expected a ShowOverlay request");
        };
        let view = Subagent
            .overlay_view(overlay_id, 100)
            .expect("workflows view");
        assert!(view.title.contains("Workflows"), "{}", view.title);
        assert!(view.title.contains("1 running"), "{}", view.title);
        let text: Vec<String> = view
            .lines
            .iter()
            .map(|l| l.iter().map(|sp| sp.text.as_str()).collect())
            .collect();
        assert!(text.iter().any(|l| l.contains("demo-run")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("scan a")), "{text:?}");
        assert!(
            text.iter().any(|l| l.contains("▶")),
            "选中行有标记: {text:?}"
        );

        // 列表本身不消费 `s`/`p`（那些键属于检查器）；↑↓ 与 Enter 归它
        let key = |k: extensions::OverlayKey| extensions::OverlayEvent::Key {
            key: k,
            input: None,
        };
        assert!(!Subagent.on_overlay_event(overlay_id, &key(extensions::OverlayKey::Char('s'))));
        assert!(Subagent.on_overlay_event(overlay_id, &key(extensions::OverlayKey::Down)));
        assert!(Subagent.on_overlay_event(overlay_id, &key(extensions::OverlayKey::Enter)));

        // Enter 打开检查器（两栏、全屏），标题是那次运行
        let Some(extensions::ExtensionUiRequest::ShowOverlay { id: ins_id }) =
            crate::core::extensions::take_pending_ui()
        else {
            panic!("expected the inspector overlay request");
        };
        let ins = Subagent.overlay_view(ins_id, 100).expect("inspector view");
        assert!(ins.title.contains("demo-run"), "{}", ins.title);
        assert!(matches!(ins.size, extensions::OverlaySize::Fullscreen));
        assert!(
            ins.lines
                .iter()
                .any(|l| l.iter().any(|sp| sp.text.contains('│'))),
            "两栏"
        );

        // `f` 切扁平/阶段；`s`/`p`/`r`/`x` 在没有控制面时也不 panic
        assert!(Subagent.on_overlay_event(ins_id, &key(extensions::OverlayKey::Char('f'))));
        for c in ['s', 'p', 'r', 'x'] {
            assert!(Subagent.on_overlay_event(ins_id, &key(extensions::OverlayKey::Char(c))));
        }
        while crate::core::extensions::take_pending_ui().is_some() {}
        workflow::task::reset_all();
    }

    /// `/agents stop wf_…` 走运行注册表；未知 id 报可读错误（不静默）。
    #[test]
    fn agents_stop_routes_workflow_run_ids() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        workflow::task::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}

        let id = workflow::task::new_id();
        let abort = workflow::task::register(&id, "demo-run", "d", None, None, None);
        let mut app = App::new();
        command_agents(&mut app, &format!("/agents stop {id}"));
        assert!(abort.load(std::sync::atomic::Ordering::Relaxed));
        let notices = notice_texts();
        assert!(
            notices.iter().any(|t| t.contains("stopping workflow")),
            "{notices:?}"
        );

        command_agents(&mut app, "/agents stop wf_nope");
        let notices = notice_texts();
        assert!(
            notices.iter().any(|t| t.contains("unknown workflow run")),
            "{notices:?}"
        );
        workflow::task::reset_all();
    }

    #[test]
    fn workflow_tool_rejects_unimplemented_params_and_bad_meta() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();
        let rt = rt();
        // `name` 现在是实现好的：未知名字报"找不到 + 列出已存在的"（可自纠）
        let err = rt
            .block_on(dispatch_tool(
                "SubagentWorkflow",
                json!({ "name": "no-such-workflow" }),
                echo_ctx("."),
            ))
            .unwrap_err();
        assert!(err.0.contains("no saved workflow named"), "{}", err.0);
        // 非法名字先被挡下（模型给的可能带路径）
        let err = rt
            .block_on(dispatch_tool(
                "SubagentWorkflow",
                json!({ "name": "../../etc/passwd" }),
                echo_ctx("."),
            ))
            .unwrap_err();
        assert!(err.0.contains("not a usable workflow name"), "{}", err.0);
        // `resumeFromRunId` 现在也实现好了：未知 id 报"本会话没有这次运行"
        let err = rt
            .block_on(dispatch_tool(
                "SubagentWorkflow",
                json!({ "resumeFromRunId": "wf_x" }),
                echo_ctx("."),
            ))
            .unwrap_err();
        assert!(err.0.contains("no workflow run"), "{}", err.0);
        let err = rt
            .block_on(dispatch_tool(
                "SubagentWorkflow",
                json!({ "script": "const x = 1" }),
                echo_ctx("."),
            ))
            .unwrap_err();
        assert!(err.0.contains("meta"), "{}", err.0);
    }

    fn hold_ctx(inbox: Arc<Mutex<Vec<String>>>) -> ToolExecCtx {
        let make = Arc::new(move |_spec: crate::core::extensions::SubAgentSpec| {
            Ok(Box::new(HoldRunner {
                inbox: inbox.clone(),
                abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })
                as Box<dyn crate::core::extensions::SubAgentRunner>)
        }) as Arc<crate::core::extensions::MakeSubAgentFn>;
        ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: ".".to_string(),
            make_sub_agent: make,
            parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        }
    }

    #[test]
    fn mention_main_rewrites_and_unknown_continues() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        // `@main` 剥前缀 + trim，落回主模型（direct 模式，避开 clone）
        configure_mentions(config::AgentMentions::Direct);
        assert_eq!(
            Subagent.on_user_prompt_with_context("@main hello  there", &[], false),
            Some(UserPromptAction::Rewrite("hello  there".into()))
        );
        assert_eq!(
            Subagent.on_user_prompt_with_context("@MAIN hi", &[], false),
            Some(UserPromptAction::Rewrite("hi".into()))
        );
        // 裸句柄 / 未知句柄 / 普通散文 → 不认领
        assert_eq!(
            Subagent.on_user_prompt_with_context("@explore", &[], false),
            None
        );
        assert_eq!(
            Subagent.on_user_prompt_with_context("@zzz-no-such-type hi", &[], false),
            None
        );
        assert_eq!(
            Subagent.on_user_prompt_with_context("just prose", &[], false),
            None
        );
    }

    /// 写配置 + reload（`agent_mentions` 的测试夹具）。
    fn configure_mentions(mode: config::AgentMentions) {
        let cfg = config::SubagentConfig {
            agent_mentions: mode,
            ..Default::default()
        };
        config::write_config(&config::config_path(), &cfg);
        manager::reload_config();
    }

    #[test]
    fn mention_to_live_agent_steers() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        configure_mentions(config::AgentMentions::Direct);
        let inbox = Arc::new(Mutex::new(Vec::new()));
        rt().block_on(async {
            dispatch_tool(
                "Agent",
                json!({
                    "prompt": "go",
                    "description": "d",
                    "subagent_type": "Explore",
                    "run_in_background": true
                }),
                hold_ctx(inbox.clone()),
            )
            .await
            .unwrap();
        });
        let recs = manager::list();
        assert_eq!(recs.len(), 1, "应有一个存活代理");
        assert_eq!(recs[0].handle.as_deref(), Some("explore"));

        // 大小写不敏感地命中存活句柄 → steer
        let action = Subagent.on_user_prompt_with_context("@Explore check again", &[], false);
        assert_eq!(action, Some(UserPromptAction::Handled));
        assert_eq!(
            inbox.lock().unwrap().as_slice(),
            &["check again".to_string()]
        );
        manager::reset_all();
    }

    #[test]
    fn mention_to_type_spawns_background() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        configure_mentions(config::AgentMentions::Direct);
        let inbox = Arc::new(Mutex::new(Vec::new()));
        rt().block_on(async {
            // 发布 ctx（模拟核心 `on_exec_ctx`）→ mention 能 spawn
            let ctx = hold_ctx(inbox.clone());
            Subagent.on_exec_ctx(&ctx);
            let action =
                Subagent.on_user_prompt_with_context("@explore audit the rpc path", &[], false);
            assert_eq!(action, Some(UserPromptAction::Handled));
            // 等后台 spawn 落地
            for _ in 0..40 {
                if !manager::list().is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let recs = manager::list();
            assert_eq!(recs.len(), 1, "mention 应后台起一个 Explore");
            assert_eq!(recs[0].agent_type, "Explore");
            assert_eq!(recs[0].handle.as_deref(), Some("explore"));
            assert_eq!(recs[0].description, "audit the rpc path");
        });
        manager::reset_all();
    }

    /// 模拟 clone 里的模型：收到提示后调一次 `Agent`（用副本的 marker 身份）。
    struct CloneRunner {
        marker: Option<String>,
    }
    impl crate::core::extensions::SubAgentRunner for CloneRunner {
        fn controls(&self) -> crate::core::extensions::SubAgentControls {
            crate::core::extensions::SubAgentControls {
                steer: Arc::new(|_t: String| {}),
                abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                session_path: None,
            }
        }
        fn run(&mut self, _prompt: String) -> BoxFuture<'_, std::result::Result<String, String>> {
            let marker = self.marker.clone();
            Box::pin(async move {
                let mut c = echo_ctx(".");
                c.agent_id = marker;
                c.depth = 0;
                dispatch_tool(
                    "Agent",
                    json!({
                        "prompt": "inner prompt",
                        "description": "inner",
                        "subagent_type": "Plan",
                        "run_in_background": false
                    }),
                    c,
                )
                .await
                .map(|r| r.text)
                .map_err(|e| e.0)
            })
        }
    }

    /// `agent_mentions = model`：隐藏回合（只带 `Agent`）拿实时对话，副本的 spawn 是顶层后台。
    #[test]
    fn mention_clone_prompts_offscreen_and_spawns_top_level() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        configure_mentions(config::AgentMentions::Model);

        let seen: Arc<Mutex<Vec<crate::core::extensions::SubAgentSpec>>> =
            Arc::new(Mutex::new(Vec::new()));
        let make = {
            let seen = seen.clone();
            Arc::new(move |spec: crate::core::extensions::SubAgentSpec| {
                seen.lock().unwrap().push(spec.clone());
                Ok(Box::new(CloneRunner {
                    marker: spec.agent_id.clone(),
                })
                    as Box<dyn crate::core::extensions::SubAgentRunner>)
            }) as Arc<crate::core::extensions::MakeSubAgentFn>
        };
        let ctx = ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: ".".to_string(),
            make_sub_agent: make,
            parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: Some("anthropic/parent".to_string()),
            script_call: false,
        };
        let prior = vec![crate::core::provider::AgentMessage::user_text("prior turn")];
        let rt = rt();
        rt.block_on(async {
            fleet::remember_ctx(&ctx);
            let action = Subagent.on_user_prompt_with_context("@explore inner", &prior, false);
            assert_eq!(action, Some(UserPromptAction::Handled));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(rec) = manager::list().into_iter().find(|r| r.agent_type == "Plan") {
                    assert!(rec.parent_agent_id.is_none(), "clone 的 spawn 应是顶层");
                    assert_eq!(rec.depth, 0);
                    assert!(rec.background, "clone 强制后台");
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "clone 未发起顶层 spawn"
                );
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });
        let specs = seen.lock().unwrap();
        assert_eq!(specs.len(), 1, "应物化一个隐藏副本");
        assert_eq!(specs[0].tools, Some(vec!["Agent".to_string()]));
        assert_eq!(
            specs[0].context_messages.as_ref().map(|m| m.len()),
            Some(1),
            "副本应拿到实时对话"
        );
        assert!(
            specs[0]
                .agent_id
                .as_deref()
                .unwrap()
                .starts_with("__mention_clone__")
        );
        assert!(!specs[0].persist_session);
        drop(specs);
        manager::reset_all();
    }

    #[test]
    fn extension_identity_and_tools() {
        let ext = Subagent;
        assert_eq!(ext.name(), "subagent");
        assert!(!ext.default_enabled());
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
        let agent = ext.tools().into_iter().find(|t| t.name == "Agent").unwrap();
        assert!(agent.description.contains("- general-purpose:"));
        assert!(agent.description.contains("- Explore:"));
        assert!(agent.description.contains(".prux/agents/"));
        let required = agent.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "subagent_type"));
        assert_eq!(agent.execution_mode, Some(ToolExecutionMode::Parallel));
    }

    #[test]
    fn commands_declare_subcommands() {
        let cmds = Subagent.commands();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "agents");
        assert!(cmds[0].busy_safe);
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

        // `/autoresearch`：候选表只声明 `stop`，无参是启动（参数是目标本身）
        assert_eq!(cmds[1].name, "autoresearch");
        assert!(cmds[1].busy_safe);
        let subs: Vec<&str> = cmds[1].subcommands.iter().map(|s| s.name).collect();
        assert_eq!(subs, vec!["stop"]);
    }

    /// 面板项与配置键一一对应，且值可往返。
    #[test]
    fn settings_panel_reflects_config_and_apply_persists() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}

        let items = Subagent.settings();
        assert_eq!(
            items.len(),
            config::panel_settings(&config::SubagentConfig::default()).len()
        );
        let widget = items.iter().find(|s| s.key == "widget").unwrap();
        assert_eq!(widget.value, "all");
        assert_eq!(widget.choices, vec!["all", "background", "off"]);

        Subagent.apply_setting("widget", "background").unwrap();
        assert_eq!(manager::config().widget, config::WidgetMode::Background);
        // 立即落盘：重新读盘可见
        assert_eq!(
            config::load_config().widget,
            config::WidgetMode::Background,
            "面板变更应写入 agent_dir()/extensions/subagent.json"
        );
        assert!(Subagent.apply_setting("nope", "all").is_err());
        assert!(Subagent.apply_setting("widget", "sometimes").is_err());
        manager::reset_all();
    }

    /// S2：strict / fallback=none / frontmatter 锁定（后台、inherit_context）。
    #[test]
    fn strict_and_fallback_and_frontmatter_locking() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_s = cwd.path().to_string_lossy().to_string();
        let global = prux_agent_dir_join(agent_types::GLOBAL_AGENT_DIR);
        std::fs::create_dir_all(&global).unwrap();

        // 1) 坏文件：默认跳过 + 告警；strict 下拒绝派发
        std::fs::write(global.join("broken.md"), "no frontmatter here").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (ctx, seen) = capturing_ctx(&cwd_s);
        let out = rt
            .block_on(Subagent.execute_tool_async(
                "Agent".to_string(),
                json!({"prompt": "p", "description": "d", "subagent_type": "Explore"}),
                ctx.clone(),
            ))
            .expect("默认：坏文件只是告警");
        assert_eq!(
            out.details.as_ref().unwrap()["status"],
            "error",
            "物化失败仍如实回报"
        );

        Subagent.apply_setting("strictAgentFiles", "on").unwrap();
        let err = rt
            .block_on(Subagent.execute_tool_async(
                "Agent".to_string(),
                json!({"prompt": "p", "description": "d", "subagent_type": "Explore"}),
                ctx.clone(),
            ))
            .unwrap_err();
        assert!(err.0.contains("strict agent files"), "{}", err.0);
        assert!(err.0.contains("broken.md"), "{}", err.0);
        Subagent.apply_setting("strictAgentFiles", "off").unwrap();

        // 2) fallbackSubagent = none：未知类型直接报错（不回退 general-purpose）
        Subagent.apply_setting("fallbackSubagent", "none").unwrap();
        let err = rt
            .block_on(Subagent.execute_tool_async(
                "Agent".to_string(),
                json!({"prompt": "p", "description": "d", "subagent_type": "nope"}),
                ctx.clone(),
            ))
            .unwrap_err();
        assert!(err.0.contains("fallbackSubagent = none"), "{}", err.0);
        Subagent
            .apply_setting("fallbackSubagent", "general-purpose")
            .unwrap();
        let out = rt
            .block_on(Subagent.execute_tool_async(
                "Agent".to_string(),
                json!({"prompt": "p", "description": "d", "subagent_type": "nope"}),
                ctx.clone(),
            ))
            .expect("默认回退");
        assert!(
            out.text.contains("using general-purpose instead"),
            "{}",
            out.text
        );

        // 3) frontmatter 锁定：类型文件写 run_in_background:false / inherit_context:true
        std::fs::remove_file(global.join("broken.md")).unwrap();
        std::fs::write(
            global.join("pinned.md"),
            "---\nname: pinned\ndescription: pinned agent\nrun_in_background: false\ninherit_context: true\n---\nbody",
        )
        .unwrap();
        let out = rt
            .block_on(Subagent.execute_tool_async(
                "Agent".to_string(),
                // 调用参数想覆盖，但 frontmatter 优先
                json!({"prompt": "p", "description": "d", "subagent_type": "pinned", "run_in_background": true, "inherit_context": false}),
                ctx.clone(),
            ))
            .expect("物化失败也如实回报");
        let d = out.details.as_ref().unwrap();
        assert_eq!(
            d["background"], false,
            "frontmatter 锁定的前台必须生效: {d}"
        );
        let specs = seen.lock().unwrap();
        let spec = specs.last().expect("应捕获一次物化请求");
        assert!(
            spec.inherit_context,
            "frontmatter 锁定的 inherit_context 必须生效"
        );
        assert!(!spec.clear_context_files || spec.system_prompt.is_some());

        // 4) disableDefaultAgents：默认三类型不再注册
        Subagent
            .apply_setting("disableDefaultAgents", "on")
            .unwrap();
        let err = rt
            .block_on(Subagent.execute_tool_async(
                "Agent".to_string(),
                json!({"prompt": "p", "description": "d", "subagent_type": "Explore"}),
                ctx,
            ))
            .unwrap_err();
        assert!(
            err.0.contains("general-purpose is unavailable"),
            "关掉默认类型后回退也无处可去: {}",
            err.0
        );
        Subagent
            .apply_setting("disableDefaultAgents", "off")
            .unwrap();
        manager::reset_all();
    }

    /// S2：`isolated` 工具参数、`extensions: false` 等价 isolated、技能预载进 child 提示。
    #[test]
    fn isolated_param_extension_scope_and_skill_preload() {
        // `ext:` 选择器要查注册表（展开成"该扩展的工具名"），因此需要注册本扩展
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::extensions::register_extension(Subagent);
        crate::core::extensions::set_extension_enabled(EXT, true);
        let cwd = tempfile::tempdir().unwrap();
        let cwd_s = cwd.path().to_string_lossy().to_string();
        Subagent.on_session_start(&cwd_s, &[], None);
        let (ctx, seen) = capturing_ctx(&cwd_s);
        let rt = tokio::runtime::Runtime::new().unwrap();

        // 1) isolated 工具参数 → spec.isolated
        rt.block_on(Subagent.execute_tool_async(
            "Agent".to_string(),
            json!({"prompt": "p", "description": "d", "subagent_type": "Explore", "isolated": true}),
            ctx.clone(),
        ))
        .unwrap();
        assert!(
            seen.lock().unwrap().last().unwrap().isolated,
            "isolated 参数应透传到 spec"
        );

        // 2) frontmatter `extensions: false` 等价 isolated
        let global = prux_agent_dir_join(agent_types::GLOBAL_AGENT_DIR);
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("no-ext.md"),
            "---\nname: no-ext\ndescription: d\nextensions: false\n---\nbody",
        )
        .unwrap();
        rt.block_on(Subagent.execute_tool_async(
            "Agent".to_string(),
            json!({"prompt": "p", "description": "d", "subagent_type": "no-ext"}),
            ctx.clone(),
        ))
        .unwrap();
        assert!(
            seen.lock().unwrap().last().unwrap().isolated,
            "extensions:false 应等价 isolated"
        );

        // 3) `extensions: [subagent]` 白名单透传
        std::fs::write(
            global.join("scoped.md"),
            "---\nname: scoped\ndescription: d\nextensions: subagent\nexclude_extensions: noisy\n---\nbody",
        )
        .unwrap();
        rt.block_on(Subagent.execute_tool_async(
            "Agent".to_string(),
            json!({"prompt": "p", "description": "d", "subagent_type": "scoped"}),
            ctx.clone(),
        ))
        .unwrap();
        {
            let specs = seen.lock().unwrap();
            let spec = specs.last().unwrap();
            assert_eq!(
                spec.extensions.as_ref().unwrap(),
                &vec!["subagent".to_string()]
            );
            assert_eq!(spec.exclude_extensions, vec!["noisy".to_string()]);
            assert!(!spec.isolated, "白名单不是 isolated");
        }

        // 4) `tools:` 里的 ext: 选择器展开（Agent 是 subagent 的工具之一）
        std::fs::write(
            global.join("selector.md"),
            "---\nname: selector\ndescription: d\ntools: read, ext:subagent/steer_subagent\n---\nbody",
        )
        .unwrap();
        rt.block_on(Subagent.execute_tool_async(
            "Agent".to_string(),
            json!({"prompt": "p", "description": "d", "subagent_type": "selector"}),
            ctx.clone(),
        ))
        .unwrap();
        {
            let specs = seen.lock().unwrap();
            let tools = specs.last().unwrap().tools.clone().unwrap();
            assert!(tools.contains(&"read".to_string()), "{tools:?}");
            assert!(tools.contains(&"steer_subagent".to_string()), "{tools:?}");
            assert!(!tools.iter().any(|t| t.starts_with("ext:")), "{tools:?}");
        }

        // 5) 技能预载：`skills: [demo]` 把正文注入 child 提示；`skills: false` 清空继承
        let skill_dir = prux_agent_dir_join("skills").join("demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: demo skill\n---\n\nPRELOADED BODY\n",
        )
        .unwrap();
        std::fs::write(
            global.join("with-skills.md"),
            "---\nname: with-skills\ndescription: d\nskills: demo\n---\nbody",
        )
        .unwrap();
        rt.block_on(Subagent.execute_tool_async(
            "Agent".to_string(),
            json!({"prompt": "p", "description": "d", "subagent_type": "with-skills"}),
            ctx.clone(),
        ))
        .unwrap();
        {
            let specs = seen.lock().unwrap();
            let spec = specs.last().unwrap();
            let prompt = spec
                .system_prompt
                .clone()
                .or_else(|| spec.append_system_prompt.clone())
                .unwrap();
            assert!(prompt.contains("<preloaded_skills>"), "{prompt}");
            assert!(prompt.contains("PRELOADED BODY"), "{prompt}");
            assert!(!spec.clear_skills);
        }

        std::fs::write(
            global.join("no-skills.md"),
            "---\nname: no-skills\ndescription: d\nskills: false\n---\nbody",
        )
        .unwrap();
        rt.block_on(Subagent.execute_tool_async(
            "Agent".to_string(),
            json!({"prompt": "p", "description": "d", "subagent_type": "no-skills"}),
            ctx.clone(),
        ))
        .unwrap();
        assert!(
            seen.lock().unwrap().last().unwrap().clear_skills,
            "skills:false 应清空继承的技能索引"
        );
        manager::reset_all();
        crate::core::extensions::set_extension_enabled(EXT, false);
        _ = crate::core::extensions::unregister_extension(EXT);
    }

    /// S2：eject → disable → enable 的文件往返（真实文件系统，agent_dir 已隔离）。
    #[test]
    fn eject_disable_enable_round_trip() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_s = cwd.path().to_string_lossy().to_string();
        Subagent.on_session_start(&cwd_s, &[], None);

        // eject 内置 Explore 到全局目录
        let msg = eject_type("Explore", false).expect("eject");
        assert!(msg.contains("ejected agent type \"Explore\""), "{msg}");
        let path = prux_agent_dir_join(agent_types::GLOBAL_AGENT_DIR).join("Explore.md");
        assert!(path.is_file(), "{}", path.display());
        // 已在 → 拒绝覆盖（不静默覆盖用户文件）
        let err = eject_type("Explore", false).unwrap_err();
        assert!(err.contains("refusing to overwrite"), "{err}");

        // 文件里应含 replace 提示与正文
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("prompt_mode: replace"), "{text}");

        // disable → 文件出现 enabled: false，且加载侧认为禁用
        let msg = toggle_type("Explore", false).expect("disable");
        assert!(msg.contains("disabled agent type"), "{msg}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\nenabled: false\n"), "{text}");
        assert!(agent_files::is_disabled_content(&text));
        assert!(matches!(
            agent_types::resolve(
                &agent_types::discover_with(&cwd_s, discovery_opts(&manager::config())).types,
                "Explore"
            ),
            Err(ResolveError::Disabled)
        ));

        // 再 disable → 幂等提示
        let msg = toggle_type("Explore", false).expect("disable twice");
        assert!(msg.contains("already disabled"), "{msg}");

        // enable → 还原为 ejected 原文
        let msg = toggle_type("Explore", true).expect("enable");
        assert!(msg.contains("enabled agent type"), "{msg}");
        let restored = std::fs::read_to_string(&path).unwrap();
        assert!(!restored.contains("enabled: false"), "{restored}");
        assert_eq!(restored, text.replacen("---\nenabled: false\n", "---\n", 1));

        // 没有 .md 的类型（内置 general-purpose 未被 eject）→ 明确拒绝
        let err = toggle_type("general-purpose", false).unwrap_err();
        assert!(err.contains("has no .md file"), "{err}");
        // 未知类型 → 明确报错
        assert!(
            eject_type("nope", false)
                .unwrap_err()
                .contains("unknown agent type")
        );
        manager::reset_all();
    }

    /// `delete` / `reset`：删自定义 `.md`、删内置默认的覆盖 `.md`（恢复内嵌定义）。
    #[test]
    fn delete_and_reset_manage_agent_files() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_s = cwd.path().to_string_lossy().to_string();
        Subagent.on_session_start(&cwd_s, &[], None);

        // 内置 Explore 先 eject，再 reset → 覆盖文件被删、内嵌默认回来了
        eject_type("Explore", false).expect("eject");
        let path = prux_agent_dir_join(agent_types::GLOBAL_AGENT_DIR).join("Explore.md");
        assert!(path.is_file(), "{}", path.display());
        let msg = reset_type("Explore").expect("reset");
        assert!(msg.contains("Restored built-in default"), "{msg}");
        assert!(!path.exists(), "reset 应删掉覆盖文件");
        // 再 reset → 没有覆盖文件可删
        let err = reset_type("Explore").unwrap_err();
        assert!(err.contains("already in effect"), "{err}");

        // 自定义类型：delete 直接移除文件
        let custom = prux_agent_dir_join(agent_types::GLOBAL_AGENT_DIR).join("mybot.md");
        std::fs::write(&custom, "---\nname: mybot\ndescription: d\n---\nbody").unwrap();
        let msg = delete_type("mybot").expect("delete");
        assert!(msg.contains("Deleted"), "{msg}");
        assert!(!custom.exists());

        // 内置默认且无覆盖文件 → delete 明确拒绝
        let err = delete_type("Plan").unwrap_err();
        assert!(err.contains("no .md file to delete"), "{err}");
        // 非默认类型 → reset 明确拒绝
        std::fs::write(&custom, "---\nname: mybot\ndescription: d\n---\nbody").unwrap();
        let err = reset_type("mybot").unwrap_err();
        assert!(err.contains("not a built-in default"), "{err}");
        // 未知 token → 无动作（`back` 由 on_ui_choice 拦截，不走这里）
        assert!(type_action("wat", "mybot").unwrap().is_none());
        // 未知类型
        assert!(
            delete_type("nope")
                .unwrap_err()
                .contains("unknown agent type")
        );
        manager::reset_all();
    }

    /// 二级操作菜单：内置默认有覆盖文件时只给 `reset`（`delete` 与它同义，都删同一个覆盖文件）；
    /// 自定义类型没有内嵌默认可恢复，只给 `delete`。
    #[test]
    fn type_actions_menu_hides_redundant_delete_for_builtin_defaults() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_s = cwd.path().to_string_lossy().to_string();
        Subagent.on_session_start(&cwd_s, &[], None);

        fn take_select() -> (u64, Vec<String>) {
            while let Some(req) = crate::core::extensions::take_pending_ui() {
                if let extensions::ExtensionUiRequest::Select { id, options, .. } = req {
                    return (id, options.into_iter().map(|o| o.text).collect());
                }
            }
            panic!("expected a Select panel");
        }

        // 内置 Explore：eject 后出现覆盖文件 → 菜单只给 reset，不给同义的 delete
        eject_type("Explore", false).expect("eject");
        while crate::core::extensions::take_pending_ui().is_some() {}
        open_type_actions("Explore");
        let (_, builtin_options) = take_select();
        assert!(
            builtin_options.contains(&"reset Explore".to_string()),
            "{builtin_options:?}"
        );
        assert!(
            !builtin_options.iter().any(|o| o.starts_with("delete ")),
            "内置默认不应再列 delete: {builtin_options:?}"
        );
        assert!(builtin_options.contains(&"edit Explore".to_string()));
        assert_eq!(builtin_options.last().map(String::as_str), Some("back"));

        // 自定义类型：只给 delete，不给 reset
        let custom = prux_agent_dir_join(agent_types::GLOBAL_AGENT_DIR).join("mybot.md");
        std::fs::write(&custom, "---\nname: mybot\ndescription: d\n---\nbody").unwrap();
        while crate::core::extensions::take_pending_ui().is_some() {}
        open_type_actions("mybot");
        let (_, custom_options) = take_select();
        assert!(
            custom_options.contains(&"delete mybot".to_string()),
            "{custom_options:?}"
        );
        assert!(
            !custom_options.iter().any(|o| o.starts_with("reset ")),
            "自定义类型不应列 reset: {custom_options:?}"
        );
        manager::reset_all();
    }

    /// `@handle` 前缀在所有以类型名为参数的子命令里都要被接受
    /// （eject / enable / disable / delete / reset / edit，对齐 `@handle message` 提及语法）。
    #[test]
    fn at_prefixed_type_args_are_accepted_by_type_commands() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_s = cwd.path().to_string_lossy().to_string();
        let mut app = App::new();
        Subagent.on_session_start(&cwd_s, &[], None);

        // eject：`@Explore` 同样能写出覆盖文件
        let msg = eject_type("@Explore", false).expect("eject @Explore");
        assert!(msg.contains("Explore"), "{msg}");
        let path = prux_agent_dir_join(agent_types::GLOBAL_AGENT_DIR).join("Explore.md");
        assert!(path.is_file(), "{}", path.display());

        // disable / enable：走命令行（原报错的路径 `/agents disable @foobar`）
        command_agents(&mut app, "/agents disable @Explore");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("enabled: false"), "{content}");
        command_agents(&mut app, "/agents enable @Explore");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("enabled: false"), "{content}");

        // reset：`@Explore` 删掉覆盖文件、恢复内嵌默认
        let msg = reset_type("@Explore").expect("reset @Explore");
        assert!(msg.contains("Restored built-in default"), "{msg}");
        assert!(!path.exists());

        // delete：自定义类型也能带 `@`
        let custom = prux_agent_dir_join(agent_types::GLOBAL_AGENT_DIR).join("mybot.md");
        std::fs::write(&custom, "---\nname: mybot\ndescription: d\n---\nbody").unwrap();
        let msg = delete_type("@mybot").expect("delete @mybot");
        assert!(msg.contains("Deleted"), "{msg}");
        assert!(!custom.exists());

        // 句柄映射：类型名 `Code Review!` 的句柄是 `code-review`，`@agent-<handle>` 别名也认
        let spaced = prux_agent_dir_join(agent_types::GLOBAL_AGENT_DIR).join("review.md");
        std::fs::write(
            &spaced,
            "---\nname: Code Review!\ndescription: d\n---\nbody",
        )
        .unwrap();
        assert_eq!(
            resolve_type_any("@code-review").unwrap().name,
            "Code Review!"
        );
        assert_eq!(
            resolve_type_any("@agent-code-review").unwrap().name,
            "Code Review!"
        );
        // 不带 `@` 的行为不变；未知名字报错时也不再带 `@`
        assert_eq!(
            resolve_type_any("Code Review!").unwrap().name,
            "Code Review!"
        );
        assert_eq!(
            resolve_type_any("@nope").unwrap_err(),
            "unknown agent type: nope"
        );
        manager::reset_all();
    }

    /// `edit`：读文件、开覆盖层编辑器、保存时整文件写回（含 frontmatter）。
    #[test]
    fn edit_type_opens_an_editor_and_saves_the_whole_file() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_s = cwd.path().to_string_lossy().to_string();
        Subagent.on_session_start(&cwd_s, &[], None);

        let path = prux_agent_dir_join(agent_types::GLOBAL_AGENT_DIR).join("mybot.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "---\nname: mybot\ndescription: d\n---\nold body").unwrap();

        // 内置默认且无文件 → edit 明确拒绝
        let err = edit_type("Plan").unwrap_err();
        assert!(err.contains("no .md file to edit"), "{err}");

        while crate::core::extensions::take_pending_ui().is_some() {}
        edit_type("mybot").expect("edit should open an overlay");
        let mut id = None;
        while let Some(r) = crate::core::extensions::take_pending_ui() {
            if let crate::core::extensions::ExtensionUiRequest::ShowOverlay { id: i } = r {
                id = Some(i);
            }
        }
        let id = id.expect("ShowOverlay request");
        let view = Subagent
            .overlay_view(id, 80)
            .expect("the editor overlay is claimed");
        let editor = view.editor.expect("view carries an editor");
        assert!(editor.value.contains("old body"), "{}", editor.value);
        assert!(editor.label.ends_with("mybot.md"), "{}", editor.label);

        // 保存：整文件（frontmatter + 正文）写回
        let updated = "---\nname: mybot\ndescription: changed\n---\nnew body\n";
        assert!(Subagent.on_overlay_event(
            id,
            &crate::core::extensions::OverlayEvent::EditorSubmit {
                text: updated.to_string()
            }
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
        manager::reset_all();
    }

    /// 测试辅助：`agent_dir()/extensions/agent` 路径。
    fn prux_agent_dir_join(sub: &str) -> std::path::PathBuf {
        crate::core::settings_manager::agent_dir().join(sub)
    }

    /// 测试辅助：物化请求捕获器（`make_sub_agent` 记录 spec 后返回 Err，
    /// 因此派发以 `status=error` 收尾，但 spec 内容可断言）。
    fn capturing_ctx(
        cwd: &str,
    ) -> (
        ToolExecCtx,
        Arc<Mutex<Vec<crate::core::extensions::SubAgentSpec>>>,
    ) {
        let seen: Arc<Mutex<Vec<crate::core::extensions::SubAgentSpec>>> =
            Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let make = Arc::new(move |spec: crate::core::extensions::SubAgentSpec| {
            sink.lock().unwrap().push(spec);
            Err("materialization is not expected here".to_string())
        }) as Arc<crate::core::extensions::MakeSubAgentFn>;
        (
            ToolExecCtx {
                execute_tool: crate::core::extensions::unavailable_tool_exec(),
                parent_tool_call_id: None,
                nested_calls: Default::default(),
                session_branch_entries: Default::default(),
                script_tools: Default::default(),
                cwd: cwd.to_string(),
                make_sub_agent: make,
                parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                agent_id: None,
                depth: 0,
                parent_model: None,
                script_call: false,
            },
            seen,
        )
    }

    /// 完成卡片：认领自己的 `custom_type`（渲染走纯函数），并在会话切换时重放落盘的卡片。
    #[test]
    fn custom_message_renders_and_replays_on_session_switch() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}

        // 渲染分派：只认领自己的类型
        let payload = json!({
            "id": "a1b2",
            "displayName": "Explore",
            "name": null,
            "type": "Explore",
            "status": "completed",
            "turns": 3,
            "toolUses": 1,
            "tokens": 1200,
            "cost": 0.0,
            "durationMs": 4200,
            "sessionPath": "/tmp/s.jsonl",
            "preview": "all good",
            "previewTruncated": false,
        });
        let spans = Subagent
            .render_custom_message(notify::CARD_TYPE, &payload)
            .expect("本扩展认领卡片类型");
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("Explore a1b2"), "{text}");
        assert!(text.contains("all good"), "{text}");
        assert!(text.contains("/tmp/s.jsonl"), "{text}");
        assert!(
            Subagent
                .render_custom_message("someone-elses-type", &payload)
                .is_none(),
            "不认领其它扩展的类型"
        );

        // 会话切换：把会话文件里已落盘的卡片重新投递（`/resume` 后仍能看到历史结果）
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let line = json!({
            "kind": "custom",
            "customType": notify::CARD_TYPE,
            "data": payload,
        });
        std::fs::write(&path, format!("{line}\n")).unwrap();
        Subagent.on_session_switched(Some(&path.to_string_lossy()), &[]);
        match crate::core::extensions::take_pending_ui() {
            Some(extensions::ExtensionUiRequest::CustomMessage {
                label,
                custom_type,
                data,
            }) => {
                assert_eq!(label, EXT);
                assert_eq!(custom_type, notify::CARD_TYPE);
                assert_eq!(data["id"], "a1b2");
            }
            other => panic!("expected a replayed CustomMessage, got {other:?}"),
        }
        assert!(
            crate::core::extensions::take_pending_ui().is_none(),
            "重放不应额外产生请求"
        );

        // 无会话路径（新会话）不重放
        Subagent.on_session_switched(None, &[]);
        assert!(crate::core::extensions::take_pending_ui().is_none());
        manager::reset_all();
    }

    /// `/agents` → 一级菜单 → 设置面板；二级列表按 id 回查记录。
    #[test]
    fn agents_menu_routes_to_submenus() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}

        let mut app = App::new();
        command_agents(&mut app, "/agents");
        let Some(extensions::ExtensionUiRequest::Select { id, options, .. }) =
            crate::core::extensions::take_pending_ui()
        else {
            panic!("expected a Select panel");
        };
        let options: Vec<String> = options.into_iter().map(|o| o.text).collect();
        assert_eq!(options.len(), 7);
        assert!(options[0].starts_with("view"), "{options:?}");
        assert!(options[1].starts_with("list"), "{options:?}");
        assert!(options[2].starts_with("types"), "{options:?}");
        assert!(options[3].starts_with("schedules"), "{options:?}");
        assert!(options[4].starts_with("workflows"), "{options:?}");
        assert!(options[5].starts_with("create"), "{options:?}");
        assert!(options[6].starts_with("settings"), "{options:?}");

        // 一级菜单 → 设置面板
        assert!(
            Subagent
                .on_ui_choice(id, Some(options[6].clone()))
                .is_none()
        );
        assert!(matches!(
            crate::core::extensions::take_pending_ui(),
            Some(extensions::ExtensionUiRequest::ShowSettings { ext }) if ext == "subagent"
        ));

        // 一级菜单 → 类型列表（交互 Select，选中后进操作菜单）
        command_agents(&mut app, "/agents");
        let Some(extensions::ExtensionUiRequest::Select { id, options, .. }) =
            crate::core::extensions::take_pending_ui()
        else {
            panic!("expected a Select panel");
        };
        let options: Vec<String> = options.into_iter().map(|o| o.text).collect();
        assert!(
            Subagent
                .on_ui_choice(id, Some(options[2].clone()))
                .is_none()
        );
        // 类型列表可能先附带若干告警通知；找出其中的 Select
        fn take_select() -> Option<(u64, Vec<String>)> {
            while let Some(req) = crate::core::extensions::take_pending_ui() {
                if let extensions::ExtensionUiRequest::Select { id, options, .. } = req {
                    return Some((id, options.into_iter().map(|o| o.text).collect()));
                }
            }
            None
        }
        let (list_id, list_options) = take_select().expect("类型列表应开一个 Select");
        assert!(
            list_options[0].starts_with("general-purpose "),
            "{list_options:?}"
        );
        // 选一个类型 → 二级操作菜单（动作项为 `<action> <name>`，末项固定是 `back`）
        assert!(
            Subagent
                .on_ui_choice(list_id, Some(list_options[0].clone()))
                .is_none()
        );
        let (type_id, type_options) = take_select().expect("操作菜单应开一个 Select");
        assert_eq!(
            type_options.last().map(String::as_str),
            Some("back"),
            "{type_options:?}"
        );
        // 选 `back` → 回到上一级：重开类型清单（选项与首次一致）
        assert!(
            Subagent
                .on_ui_choice(type_id, Some("back".to_string()))
                .is_none()
        );
        let (back_id, back_options) = take_select().expect("back 应重开类型清单");
        assert_eq!(back_options, list_options);
        // 旧的操作菜单 id 已失效（slot 已指向新的清单），且不产生 UI 请求
        assert!(
            Subagent
                .on_ui_choice(type_id, Some("eject general-purpose".to_string()))
                .is_none()
        );
        assert!(crate::core::extensions::take_pending_ui().is_none());
        // 取消重开的清单
        assert!(Subagent.on_ui_choice(back_id, None).is_none());

        // 过期 id 不再分发；取消（None）不产生任何 UI 请求
        assert!(
            Subagent
                .on_ui_choice(id, Some("list  x".to_string()))
                .is_none()
        );
        assert!(crate::core::extensions::take_pending_ui().is_none());

        // `/agents settings` 直达设置面板；无记录时列表项只发通知
        command_agents(&mut app, "/agents settings");
        assert!(matches!(
            crate::core::extensions::take_pending_ui(),
            Some(extensions::ExtensionUiRequest::ShowSettings { .. })
        ));
        command_agents(&mut app, "/agents");
        let Some(extensions::ExtensionUiRequest::Select { id, options, .. }) =
            crate::core::extensions::take_pending_ui()
        else {
            panic!("expected a Select panel");
        };
        let options: Vec<String> = options.into_iter().map(|o| o.text).collect();
        assert!(
            Subagent
                .on_ui_choice(id, Some(options[1].clone()))
                .is_none()
        );
        match crate::core::extensions::take_pending_ui() {
            Some(extensions::ExtensionUiRequest::NotifyRich { spans, .. }) => {
                assert!(spans.iter().any(|s| s.text.contains("no sub-agents")));
            }
            other => panic!("expected a not-rich notice, got {other:?}"),
        }
    }

    // ── S6c：跨扩展事件 + RPC ───────────────────────────────────────────

    /// 收集总线事件的测试扩展。
    #[derive(Clone)]
    struct CollectorExt {
        name: String,
        seen: Arc<Mutex<Vec<(String, Value)>>>,
    }
    impl Extension for CollectorExt {
        fn name(&self) -> &str {
            &self.name
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }
        fn hooks(&self) -> Vec<ExtensionHook> {
            vec![ExtensionHook::ExtensionEvent]
        }
        fn on_extension_event(&self, name: &str, payload: &Value) {
            self.seen
                .lock()
                .unwrap()
                .push((name.to_string(), payload.clone()));
        }
    }

    const COLLECTOR: &str = "s6c-collector";

    /// 注册 `Subagent`（启用）+ 一个事件收集器；返回收集器与注册名。
    fn with_bus(list: &Arc<Mutex<Vec<(String, Value)>>>) {
        crate::core::extensions::unregister_extension(EXT);
        crate::core::extensions::register_extension(Subagent);
        crate::core::extensions::set_extension_enabled(EXT, true);
        crate::core::extensions::unregister_extension(COLLECTOR);
        crate::core::extensions::register_extension(CollectorExt {
            name: COLLECTOR.to_string(),
            seen: list.clone(),
        });
    }

    fn teardown_bus() {
        crate::core::extensions::unregister_extension(COLLECTOR);
        crate::core::extensions::unregister_extension(EXT);
        manager::reset_all();
    }

    fn find_event(list: &Arc<Mutex<Vec<(String, Value)>>>, name: &str) -> Option<Value> {
        list.lock()
            .unwrap()
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, p)| p.clone())
    }

    fn wait_for_event(list: &Arc<Mutex<Vec<(String, Value)>>>, name: &str, secs: u64) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            if let Some(p) = find_event(list, name) {
                return p;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "event {name} not seen in time"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// 后台 spawn 一个 Explore，等它到终态，返回 id。
    fn spawn_and_settle_background(
        rt: &tokio::runtime::Runtime,
        _seen: &Arc<Mutex<Vec<(String, Value)>>>,
    ) -> String {
        let ctx = echo_ctx(".");
        rt.block_on(async {
            dispatch_tool(
                "Agent",
                json!({"prompt": "hi", "description": "t", "subagent_type": "Explore", "run_in_background": true}),
                ctx.clone(),
            )
            .await
            .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(rec) = manager::list().into_iter().find(|r| r.status.is_terminal()) {
                    return rec.id;
                }
                assert!(std::time::Instant::now() < deadline, "agent 未终结");
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
    }

    /// 顶层生命周期事件应到达订阅者（created → started → completed），且非订阅者不受影响。
    #[test]
    fn lifecycle_events_reach_subscribers() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        let seen = Arc::new(Mutex::new(Vec::new()));
        with_bus(&seen);

        let rt = rt();
        let id = spawn_and_settle_background(&rt, &seen);
        // `finish` 先置终态、后 emit，所以等事件而不是立即断言
        let _ = wait_for_event(&seen, "subagents:completed", 5);

        let names: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(names.iter().any(|n| n == "subagents:created"), "{names:?}");
        assert!(names.iter().any(|n| n == "subagents:started"), "{names:?}");
        assert!(
            names
                .iter()
                .any(|n| n == "subagents:completed" || n == "subagents:failed"),
            "{names:?}"
        );
        let created = find_event(&seen, "subagents:created").unwrap();
        assert_eq!(created["id"], id);
        assert_eq!(created["type"], "Explore");
        teardown_bus();
    }

    /// ping 回 `{version: 2}`，走 `"<channel>:reply:<id>"` 通道。
    #[test]
    fn rpc_ping_replies_with_protocol_version() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let seen = Arc::new(Mutex::new(Vec::new()));
        with_bus(&seen);

        events::on_event("subagents:rpc:ping", &json!({"requestId": "ping-1"}));
        let reply = find_event(&seen, "subagents:rpc:ping:reply:ping-1").expect("reply");
        assert_eq!(reply["success"], true);
        assert_eq!(reply["data"]["version"], events::PROTOCOL_VERSION);
        assert_eq!(events::PROTOCOL_VERSION, 2);
        teardown_bus();
    }

    /// 无活跃 ctx 时 spawn 回 `No active session`。
    #[test]
    fn rpc_spawn_without_session_errors() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();
        fleet::reset();
        fleet::clear_ctx_for_test();
        let seen = Arc::new(Mutex::new(Vec::new()));
        with_bus(&seen);

        events::on_event(
            "subagents:rpc:spawn",
            &json!({"requestId": "s0", "type": "Explore", "prompt": "go"}),
        );
        let reply = find_event(&seen, "subagents:rpc:spawn:reply:s0").expect("reply");
        assert_eq!(reply["success"], false);
        assert_eq!(reply["error"], "No active session");
        teardown_bus();
    }

    /// spawn 成功回 `{id}`，并真的起了一个顶层后台代理。
    #[test]
    fn rpc_spawn_returns_id_and_starts_agent() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        let seen = Arc::new(Mutex::new(Vec::new()));
        with_bus(&seen);

        let rt = rt();
        rt.block_on(async {
            let ctx = echo_ctx(".");
            fleet::remember_ctx(&ctx);
            events::on_event(
                "subagents:rpc:spawn",
                &json!({"requestId": "s1", "type": "Explore", "prompt": "go"}),
            );
            let reply = wait_for_event(&seen, "subagents:rpc:spawn:reply:s1", 5);
            assert_eq!(reply["success"], true, "{reply}");
            let id = reply["data"]["id"].as_str().expect("id");
            assert!(!id.is_empty());
            // 顶层后台：parent_agent_id 为空、background=true
            let rec = manager::record(id).expect("record");
            assert!(rec.parent_agent_id.is_none());
            assert!(rec.background);
            // 等它结束再收尾
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while manager::record(id).is_some_and(|r| !r.status.is_terminal()) {
                assert!(std::time::Instant::now() < deadline);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });
        teardown_bus();
    }

    /// `stop` 拒绝嵌套子代理（属主是另一个 agent）。
    #[test]
    fn rpc_stop_refuses_nested_agents() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        let seen = Arc::new(Mutex::new(Vec::new()));
        with_bus(&seen);

        let rt = rt();
        let id = rt.block_on(async {
            let mut ctx = echo_ctx(".");
            ctx.agent_id = Some("parent-x".to_string());
            ctx.depth = 1;
            let out = dispatch_tool(
                "Agent",
                json!({"prompt": "hi", "description": "t", "subagent_type": "Explore", "run_in_background": true}),
                ctx.clone(),
            )
            .await
            .unwrap();
            let _ = out;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(rec) = manager::list().into_iter().find(|r| r.parent_agent_id.is_some()) {
                    return rec.id;
                }
                assert!(std::time::Instant::now() < deadline, "嵌套记录未创建");
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });
        events::on_event(
            "subagents:rpc:stop",
            &json!({"requestId": "x1", "agentId": id}),
        );
        let reply = find_event(&seen, "subagents:rpc:stop:reply:x1").expect("reply");
        assert_eq!(reply["success"], false);
        assert!(
            reply["error"]
                .as_str()
                .unwrap()
                .contains("owned by another"),
            "{reply}"
        );
        // 收尾：别把挂起的嵌套 child 留给别的用例
        let _ = manager::stop(&id);
        teardown_bus();
    }

    /// `consume` 把已终结结果标为已读并抑制后续完成通知。
    #[test]
    fn rpc_consume_marks_result_read() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        let seen = Arc::new(Mutex::new(Vec::new()));
        with_bus(&seen);

        let rt = rt();
        let id = spawn_and_settle_background(&rt, &seen);
        assert!(!manager::record(&id).unwrap().consumed);

        events::on_event(
            "subagents:rpc:consume",
            &json!({"requestId": "c1", "agentId": id}),
        );
        let reply = find_event(&seen, "subagents:rpc:consume:reply:c1").expect("reply");
        assert_eq!(reply["success"], true, "{reply}");
        assert!(manager::record(&id).unwrap().consumed);

        // 未知 id → 失败
        events::on_event(
            "subagents:rpc:consume",
            &json!({"requestId": "c2", "agentId": "nope"}),
        );
        let reply = find_event(&seen, "subagents:rpc:consume:reply:c2").expect("reply");
        assert_eq!(reply["success"], false);
        teardown_bus();
    }

    /// `allowed_subagents` 限定嵌套 Agent 可派的类型（handler 层白名单）。
    #[test]
    fn allowed_subagents_restricts_nested_dispatch() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        let global = crate::core::settings_manager::agent_dir().join(agent_types::GLOBAL_AGENT_DIR);
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("boss.md"),
            "---\nname: Boss\ndescription: b\nallowed_subagents: Explore\n---\nbody",
        )
        .unwrap();
        manager::reset_all();

        let rt = rt();
        rt.block_on(async {
            let ctx = echo_ctx(".");
            // 顶层派 Boss（真实类型 + allowed_subagents）
            dispatch_tool(
                "Agent",
                json!({"prompt": "x", "description": "d", "subagent_type": "Boss", "run_in_background": true}),
                ctx.clone(),
            )
            .await
            .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let boss_id = loop {
                if let Some(rec) = manager::list().into_iter().find(|r| r.agent_type == "Boss") {
                    break rec.id;
                }
                assert!(std::time::Instant::now() < deadline, "Boss 记录未创建");
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            };

            // 以 Boss 身份派 Plan：不在白名单 → 文本拒绝
            let mut as_boss = ctx.clone();
            as_boss.agent_id = Some(boss_id.clone());
            as_boss.depth = 1;
            let denied = dispatch_tool(
                "Agent",
                json!({"prompt": "y", "description": "d", "subagent_type": "Plan", "run_in_background": true}),
                as_boss.clone(),
            )
            .await
            .unwrap();
            assert!(denied.text.contains("not allowed for Boss"), "{}", denied.text);

            // Explore 在白名单 → 放行
            let ok = dispatch_tool(
                "Agent",
                json!({"prompt": "z", "description": "d", "subagent_type": "Explore", "run_in_background": true}),
                as_boss,
            )
            .await
            .unwrap();
            assert!(!ok.text.contains("not allowed"), "{}", ok.text);
        });
        manager::reset_all();
    }

    /// `reportUsage`：池里的开销被挂到下一个工具结果，且取一次即空。
    #[test]
    fn report_usage_hook_drains_pool_once() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();

        // 空池 → 不附加
        let out = Subagent
            .after_tool_call_ext("read", &json!({}), "x", false)
            .unwrap();
        assert!(out.usage.is_none());

        manager::pool_usage(crate::core::provider::Usage {
            input: 10,
            output: 5,
            total_tokens: 15,
            ..Default::default()
        });
        let out = Subagent
            .after_tool_call_ext("read", &json!({}), "x", false)
            .unwrap();
        let u = out.usage.expect("pooled usage 应被挂上");
        assert_eq!((u.input, u.output, u.total_tokens), (10, 5, 15));

        // 再取一次为空
        let out = Subagent
            .after_tool_call_ext("read", &json!({}), "x", false)
            .unwrap();
        assert!(out.usage.is_none());
    }

    /// `showModel`：继承父级型号时，记录里记的是父级的实际生效型号。
    #[test]
    fn record_model_falls_back_to_parent_model() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        let rt = rt();
        rt.block_on(async {
            let mut ctx = echo_ctx(".");
            ctx.parent_model = Some("anthropic/claude-test".to_string());
            dispatch_tool(
                "Agent",
                json!({"prompt": "hi", "description": "d", "subagent_type": "Explore", "run_in_background": true}),
                ctx,
            )
            .await
            .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(rec) = manager::list().into_iter().next() {
                    assert_eq!(rec.model.as_deref(), Some("anthropic/claude-test"));
                    return;
                }
                assert!(std::time::Instant::now() < deadline);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });
        manager::reset_all();
    }

    /// Esc（父级回合 abort）只取消 `wait:true` 的等待，子代理继续跑。
    #[test]
    fn wait_cancels_without_stopping_agent() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        let inbox = Arc::new(Mutex::new(Vec::new()));
        let rt = rt();
        rt.block_on(async {
            let ctx = hold_ctx(inbox.clone());
            dispatch_tool(
                "Agent",
                json!({"prompt": "x", "description": "d", "subagent_type": "Explore", "run_in_background": true}),
                ctx.clone(),
            )
            .await
            .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let id = loop {
                if let Some(rec) = manager::list().into_iter().find(|r| r.status == types::AgentStatus::Running) {
                    break rec.id;
                }
                assert!(std::time::Instant::now() < deadline, "agent 未进入 running");
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            };

            // 父级回合 abort 已置位：wait:true 应立即返回运行中快照、不杀 agent
            ctx.parent_abort.store(true, Ordering::Relaxed);
            let out = dispatch_tool(
                "get_subagent_result",
                json!({"agent_id": id, "wait": true}),
                ctx.clone(),
            )
            .await
            .unwrap();
            assert!(out.text.contains("still running"), "{}", out.text);
            assert!(
                !manager::record(&id).unwrap().status.is_terminal(),
                "取消等待不该取消 agent"
            );
            let _ = manager::stop(&id);
        });
        manager::reset_all();
    }

    /// `@` 候选：普通上下文包含可用类型的 handle（`@explore`），大小写不敏感，且不独占。
    #[test]
    fn mention_suggestions_list_types() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        let s = Subagent.suggestions("", "");
        assert!(!s.exclusive, "普通 `@` 不独占（文件候选仍会并入）");
        assert!(s.items.iter().any(|i| i.insert == "explore"), "{s:?}");
        let s = Subagent.suggestions("EXP", "");
        assert!(s.items.iter().any(|i| i.insert == "explore"), "{s:?}");
        let s = Subagent.suggestions("zzz-nope", "");
        assert!(!s.items.iter().any(|i| i.insert == "explore"), "{s:?}");
    }

    /// `/agents` 实参上下文的识别（只看前两个词，大小写不敏感）。
    #[test]
    fn suggestion_context_detects_agent_and_type_args() {
        assert_eq!(
            suggestion_context("/agents stop @"),
            Some(SuggestionContext::AgentArg(AgentArgKind::Stop))
        );
        assert_eq!(
            suggestion_context("/AGENTS  result  @exp"),
            Some(SuggestionContext::AgentArg(AgentArgKind::Result))
        );
        for (sub, kind) in [
            ("eject", TypeArgKind::Eject),
            ("enable", TypeArgKind::Enable),
            ("disable", TypeArgKind::Disable),
            ("delete", TypeArgKind::Delete),
            ("reset", TypeArgKind::Reset),
            ("edit", TypeArgKind::Edit),
        ] {
            assert_eq!(
                suggestion_context(&format!("/agents {sub} @co")),
                Some(SuggestionContext::TypeArg(kind)),
                "{sub}"
            );
        }
        assert_eq!(suggestion_context("/agents types @"), None);
        assert_eq!(suggestion_context("/agents"), None);
        assert_eq!(suggestion_context("@explore hi"), None);
        assert_eq!(suggestion_context("plain text"), None);
    }

    /// `/agents stop @` / `/agents result @` 列出存活代理，说明行带真正的 id，且独占（不混文件）。
    #[test]
    fn stop_and_result_suggestions_list_agent_ids() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();

        let inbox = Arc::new(Mutex::new(Vec::new()));
        let rt = rt();
        rt.block_on(async {
            let ctx = hold_ctx(inbox.clone());
            dispatch_tool(
                "Agent",
                json!({"prompt": "x", "description": "d", "subagent_type": "Explore", "run_in_background": true}),
                ctx.clone(),
            )
            .await
            .unwrap();

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let rec = loop {
                if let Some(rec) = manager::list().into_iter().next() {
                    break rec;
                }
                assert!(std::time::Instant::now() < deadline, "agent 未登记");
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            };
            let id = rec.id.clone();
            let handle = rec.handle.clone().unwrap_or_else(|| id.clone());

            // `@` 候选：独占，插入句柄，说明行里有 id
            let s = Subagent.suggestions("", "/agents stop @");
            assert!(s.exclusive, "代理 id 候选应独占（不混文件/类型）");
            // 展示与插入都是 id（不显示 agent type）
            assert!(
                s.items.iter().any(|i| i.insert == id && i.name == format!("@{id}")),
                "stop 应以 id 作为候选: {:?}",
                s.items
            );
            assert!(
                s.items.iter().all(|i| !i.description.contains(&rec.display_name)),
                "不应显示 agent type: {:?}",
                s.items
            );
            // 按 id / handle 都筛得到
            assert!(
                !Subagent.suggestions(&id, "/agents stop @").items.is_empty(),
                "按 id 应筛得到"
            );
            assert!(
                !Subagent.suggestions(&handle, "/agents result @").items.is_empty(),
                "按 handle 应筛得到"
            );
            // 不匹配的查询不返回
            assert!(
                Subagent
                    .suggestions("zzz-nope", "/agents stop @zzz-nope")
                    .items
                    .is_empty()
            );
            // 普通 `@` 上下文仍走类型清单，不受影响
            let s = Subagent.suggestions("", "");
            assert!(s.items.iter().any(|i| i.insert == "explore"), "{s:?}");

            // 带 `@` 的 id 能定位（/agents stop 分发会剥前缀）
            assert!(
                manager::stop(&format!("@{id}")).is_ok(),
                "stop 应接受 @id"
            );
        });
        manager::reset_all();
    }

    /// `/agents enable @` 等类型实参：只列该子命令**真能处理**的类型，独占（不混文件）。
    #[test]
    fn type_arg_suggestions_are_exclusive_and_type_only() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_s = cwd.path().to_string_lossy().to_string();
        manager::reset_all();
        Subagent.on_session_start(&cwd_s, &[], None);

        let path = prux_agent_dir_join(agent_types::GLOBAL_AGENT_DIR).join("disabledbot.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nname: disabledbot\ndescription: d\nenabled: false\n---\nbody",
        )
        .unwrap();

        let s = Subagent.suggestions("", "/agents enable @");
        assert!(s.exclusive, "类型实参候选应独占（不扫描/并入文件）");
        assert!(
            s.items.iter().any(|i| i.insert == "disabledbot"),
            "enable 应列出已禁用类型: {:?}",
            s.items
        );
        assert!(
            s.items.iter().any(|i| i.description.contains("disabled")),
            "已禁用类型应标注: {:?}",
            s.items
        );
        // 查询过滤：只留匹配项
        let s = Subagent.suggestions("disabled", "/agents enable @disabled");
        assert_eq!(s.items.len(), 1, "{:?}", s.items);
        assert_eq!(s.items[0].insert, "disabledbot");
        manager::reset_all();
    }

    /// 内嵌默认三型没有 `.md`：`enable|disable|delete|edit|reset` 的 `@` 候选不得列出它们
    /// （执行只会报错），但 `eject` 仍要列出（正是它拿到文件的途径）；
    /// `eject` 出文件后，这些类型就变得可操作、候选照常出现，而 `eject` 本身不再列它们（拒绝覆盖）。
    #[test]
    fn type_arg_suggestions_hide_builtins_without_files() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_s = cwd.path().to_string_lossy().to_string();
        manager::reset_all();
        Subagent.on_session_start(&cwd_s, &[], None);

        let inserts = |line: &str| -> Vec<String> {
            Subagent
                .suggestions("", line)
                .items
                .into_iter()
                .map(|i| i.insert)
                .collect()
        };

        // 未 eject：内置三型对文件类操作全部隐藏
        for line in [
            "/agents enable @",
            "/agents disable @",
            "/agents delete @",
            "/agents edit @",
            "/agents reset @",
        ] {
            let got = inserts(line);
            assert!(!got.iter().any(|i| i == "explore"), "{line}: {got:?}");
            assert!(!got.iter().any(|i| i == "plan"), "{line}: {got:?}");
            assert!(
                !got.iter().any(|i| i == "general-purpose"),
                "{line}: {got:?}"
            );
        }
        // eject 仍可选中内置类型
        let got = inserts("/agents eject @");
        assert!(got.iter().any(|i| i == "explore"), "{got:?}");

        // eject 出文件后：disable / reset 能处理它，候选要出现；enable 因已启用仍不列
        eject_type("Explore", false).expect("eject");
        assert!(
            !inserts("/agents eject @").iter().any(|i| i == "explore"),
            "已有 .md 的类型不应再列在 eject 候选里（会拒绝覆盖）"
        );
        assert!(
            inserts("/agents disable @").iter().any(|i| i == "explore"),
            "eject 后的内置类型应可被 disable"
        );
        assert!(
            inserts("/agents reset @").iter().any(|i| i == "explore"),
            "eject 后的内置类型应可被 reset"
        );
        assert!(
            !inserts("/agents enable @").iter().any(|i| i == "explore"),
            "已启用的类型不应列在 enable 候选里"
        );

        // 禁用后（写回 enabled: false）：enable 候选能重新看到它
        toggle_type("Explore", false).expect("disable");
        assert!(
            inserts("/agents enable @").iter().any(|i| i == "explore"),
            "已禁用的类型应列在 enable 候选里"
        );
        assert!(
            !inserts("/agents disable @").iter().any(|i| i == "explore"),
            "已禁用的类型不应再列在 disable 候选里"
        );
        manager::reset_all();
    }
}
