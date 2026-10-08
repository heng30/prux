//! 任务跟踪与协调扩展（移植自 `@tintinweb/pi-tasks` v0.9.0）。
//!
//! 提供 Claude Code 风格的结构化任务列表：7 个模型可见工具
//! （`TaskCreate` / `TaskList` / `TaskGet` / `TaskUpdate` / `TaskOutput` / `TaskStop` /
//! `TaskExecute`）、常驻任务 widget、`/tasks` 命令、系统提醒注入，以及与
//! [`super::subagent`] 扩展的 RPC 联动（`TaskExecute` 走 `subagents:rpc:spawn`）。
//!
//! **默认不启用**：主动注入工具并改变默认 agentic 行为，注册后为禁用态，
//! 需在 `/extension` 面板开启（写 settings.json 的 `enabledExtensions`）。
//!
//! 联动面：与 `subagent` 扩展同一作者（`@tintinweb`），经核心扩展事件总线做
//! scoped request/reply RPC。subagent 扩展已实现协议 v2 的全部端点。

mod auto_clear;
mod auto_hide;
mod cadence;
mod config;
mod menu;
mod paths;
mod sort;
mod store;
mod subagent;
mod tools;
mod types;
mod widget;

use crate::{
    core::{
        extensions::{
            self, BoundaryOutcome, DockLine, Extension, ExtensionCommand, ExtensionHook,
            ExtensionMode, ExtensionSetting, ExtensionTool, ExtensionUiRequest, ForkProjectInfo,
            SubcommandDef, ToolAnnotations, ToolExecCtx, ToolExposure, UiNotifyLevel,
        },
        provider::AgentMessage,
        session_manager::{Session, session_parent_id},
        settings_manager::agent_dir,
        tools::{ToolError, ToolExecutionMode, ToolResult},
    },
    error::Result,
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_TASKS, command_arg,
        util::{SPINNER_TICK_MS, notify_text, push_context, spinner_frame_index},
    },
    modes::interactive::{app::App, handlers::register_slash_command},
    utils::{glyphs::DEF_SPINNER, time::now_ms},
};
use auto_clear::AutoClearManager;
use auto_hide::AutoHideManager;
use cadence::CadenceState;
use config::{AutoHideDelay, TaskScope, TasksConfig};
use futures_util::future::BoxFuture;
use menu::MenuState;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};
use store::TaskStore;
use subagent::RpcState;
use types::{Task, TaskStatus};
use widget::WidgetState;

/// 扩展名（也是事件总线的发送方身份：必须是稳定的 `"tasks"`）。
pub(super) const EXT: &str = "tasks";
/// 命令名。
const CMD: &str = "tasks";
/// 环境变量：覆盖存储落点。
const ENV_TASKS: &str = "PRUX_TASKS";

/// 已完成任务逗留多少个回合再自动清理。
const AUTO_CLEAR_DELAY: u64 = 4;
/// 未使用任务工具的回合数达到多少后考虑注入提醒。
const REMINDER_INTERVAL: u64 = 4;
/// 有任务 in_progress 时用的更短间隔，尽快抓到滞留工作。
const ACTIVE_REMINDER_INTERVAL: u64 = 2;

/// 命令子命令候选（顺序 = 输入框候选顺序）。
///
/// `create` / `run` 是带参分支；无参（或其它输入）仍走交互菜单。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "create",
        description: "Create a task directly: /tasks create <subject> [:: <description>]",
    },
    SubcommandDef {
        name: "run",
        description: "Start an agent turn to finish tasks: /tasks run [<id>,<id>...]",
    },
];

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static TASKS_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_TASKS,
    make: || -> Arc<dyn Extension> { Arc::new(Tasks) },
};

/// 扩展内存态（模块单例）。
struct State {
    /// 当前生效的任务配置（全局 + 项目层合并结果）。
    config: TasksConfig,
    /// 任务存储；落点随作用域/会话变化时重建。
    store: TaskStore,
    /// 当前 store 的身份键；与解析结果不同即重建。
    store_key: String,
    /// 当前工作目录；项目/相对路径类存储目标解析所需。
    cwd: Option<String>,
    /// 当前会话 id（取自会话文件 stem）；按会话持久化时用于定位文件。
    session_id: Option<String>,
    /// `/tasks` 命令的交互状态机。
    menu: MenuState,
    /// 常驻 widget 运行态（活跃任务与指标）。
    widget: WidgetState,
    /// 已完成任务的回合制自动清理管理器。
    auto_clear: AutoClearManager,
    /// 整列表完成后的墙钟自动隐藏计时器。
    auto_hide: AutoHideManager,
    /// subagent 联动（RPC / 映射 / 级联配置）。
    rpc: RpcState,
    /// 提示注入 cadence（回合计数也在这里）。
    cadence: CadenceState,
    /// 本会话的持久化任务是否已展示过（只做一次）。
    persisted_shown: bool,
}

impl Default for State {
    /// 全部字段取默认值：内存 store、无 cwd/session、默认配置与各管理器初始态。
    fn default() -> Self {
        State {
            config: TasksConfig::default(),
            store: TaskStore::new(None),
            store_key: String::new(),
            cwd: None,
            session_id: None,
            menu: MenuState::default(),
            widget: WidgetState::default(),
            auto_clear: AutoClearManager::new(AUTO_CLEAR_DELAY),
            auto_hide: AutoHideManager::default(),
            rpc: RpcState::default(),
            cadence: CadenceState::default(),
            persisted_shown: false,
        }
    }
}

/// 任务扩展的全局状态入口；本模块一律经 [`lock_state`] 加锁后访问。
fn state() -> &'static Mutex<State> {
    /// 进程级任务扩展状态单例，首次访问时惰性初始化。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 取状态单例的锁；锁被毒化（持锁线程 panic）时取回内部数据继续使用。
fn lock_state() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// 任务工具名：用于检测「任务工具被使用」以抑制/重置提醒。
fn is_task_tool(name: &str) -> bool {
    matches!(
        name,
        "TaskCreate"
            | "TaskList"
            | "TaskGet"
            | "TaskUpdate"
            | "TaskOutput"
            | "TaskStop"
            | "TaskExecute"
    )
}

/// 扩展（无字段；全部状态在模块单例）。
pub struct Tasks;

/// 解析当前应使用的 store 目标：`(身份键, 落盘路径)`。
///
/// 项目与相对路径需要 `cwd`，而扩展工厂运行时拿不到——因此那些目标先待在内存里，
/// 等第一个带上下文的钩子到达再落定。`PRUX_TASKS` 的绝对路径与具名覆盖可立即打开。
fn resolve_store_target(
    cwd: Option<&str>,
    session_id: Option<&str>,
    scope: TaskScope,
) -> (String, Option<PathBuf>) {
    let env = std::env::var(ENV_TASKS)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(env) = env {
        if env == "off" {
            return ("memory:env".to_string(), None);
        }

        if env.starts_with('/') {
            let p = PathBuf::from(&env);
            return (format!("path:{}", p.display()), Some(p));
        }

        if env.starts_with('.') {
            return match cwd {
                Some(cwd) => {
                    let p = Path::new(cwd).join(&env);
                    (format!("path:{}", p.display()), Some(p))
                }
                None => ("pending:relative".to_string(), None),
            };
        }

        let p = agent_dir().join("tasks").join(format!("{env}.json"));
        return (format!("path:{}", p.display()), Some(p));
    }

    // ---- 没有设置环境变量 ---- //

    if scope == TaskScope::Memory {
        return ("memory:config".to_string(), None);
    }

    let Some(cwd) = cwd else {
        return ("pending:workspace".to_string(), None);
    };

    if scope.is_session() {
        return match session_id {
            Some(sid) => {
                let p = paths::session_task_file(cwd, sid, scope);
                (format!("path:{}", p.display()), Some(p))
            }
            None => ("pending:session".to_string(), None),
        };
    }

    let p = paths::project_task_file(cwd);
    (format!("path:{}", p.display()), Some(p))
}

/// 让 store 对齐当前 cwd / 会话 / 配置；目标变化时重建 store。
fn sync_store(st: &mut State, reload_config: bool) {
    if reload_config && let Some(cwd) = st.cwd.clone() {
        st.config = config::load_tasks_config(&cwd);
    }

    let scope = st.config.task_scope();
    let (key, path) = resolve_store_target(st.cwd.as_deref(), st.session_id.as_deref(), scope);
    if key != st.store_key {
        st.store = TaskStore::new(path);
        st.store_key = key; // 新 store 拥有不同的任务列表：会话态一并归零。
        reset_session_state(st);
    }
}

/// 清掉「属于上一个会话」的运行期/展示态。
///
/// 任务数据按作用域落盘（由 [`sync_store`] 决定去留），这里只清进程内的会话态：
/// 活跃任务指针、回合 cadence、自动清理倒计时、菜单与 subagent 映射。
/// 与 subagent 扩展在 `on_session_switched` 里 `reset_all()` 同一用意：
/// 切过会话后 dock 不再挂着上一个会话的残影。
fn reset_session_state(st: &mut State) {
    st.menu = MenuState::default();
    st.widget.reset();
    st.auto_clear.reset();
    st.auto_hide.reset();
    st.cadence = CadenceState::default();
    st.rpc.agent_task_map.clear();
    st.persisted_shown = false;
}

/// 从会话文件路径取一个稳定且唯一的会话键（文件 stem）。
fn session_key_from_path(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
}

/// 空的任务文件（仅 session 作用域）随列表清空而删除；
/// `session-global` 下还会回收已空的全局会话目录。
fn cleanup_session_file(store: &mut TaskStore, scope: TaskScope, cwd: Option<&str>) {
    if !scope.is_session() {
        return;
    }

    // 显式环境变量指向的文件不归会话管，不删。
    if std::env::var(ENV_TASKS)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return;
    }

    if store.delete_file_if_empty()
        && scope == TaskScope::SessionGlobal
        && let Some(cwd) = cwd
    {
        paths::reclaim_global_session_tasks_dir(cwd);
    }
}

/// fork/clone 继承：把父会话的任务列表种入新会话（仅当新会话 store 为空）。
///
/// [`Tasks::on_session_switched`] 不给父会话路径，但新会话文件头带 `parent_session_id`，
/// 据此定位父会话的任务文件。显式存储路径（`PRUX_TASKS`）与共享的 `project` 作用域
/// 不按会话分文件，无继承可言。
fn seed_forked_tasks(st: &mut State, parent_session_id: &str) {
    if std::env::var(ENV_TASKS)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return;
    }

    let scope = st.config.task_scope();
    if !scope.is_session() {
        return;
    }

    let Some(cwd) = st.cwd.clone() else {
        return;
    };

    let parent_path = paths::session_task_file(&cwd, parent_session_id, scope);
    if !parent_path.exists() {
        return;
    }

    let mut parent = TaskStore::new(Some(parent_path));
    let data = parent.snapshot();
    if data.tasks.is_empty() {
        return;
    }

    // seed 对非空 store 是 no-op：重复切换（reload/再次 fork）不会重复灌入。
    _ = st.store.seed(data);
}

/// 会话建立/恢复后展示持久化任务。
///
/// `is_resume` 为假（全新会话：`/new` 或首次启动）时，不带入上一个会话的完成列表：
/// 全完成的列表直接清掉；还剩未完成的工作（`project` 等共享作用域）则保留，
/// 但**不**主动弹面板——它属于那个作用域，不属于刚切过来的这个会话。
/// 只有真正恢复（`/resume`、`/fork`、`--continue`）才自动打开停靠面板；但恢复到一个
/// 「任务已全部完成」的会话时例外——完成项直接隐藏，不弹面板（那批任务是上次的残影）。
fn after_session_sync(st: &mut State, is_resume: bool) {
    if st.persisted_shown {
        return;
    }

    st.persisted_shown = true;
    let tasks = st.store.list(None);
    if tasks.is_empty() {
        return;
    }

    if is_resume {
        // 重新打开一个「任务已全部完成」的会话：那批完成项是上次留下的残影，
        // 直接按已隐藏处理，不再弹面板等墙钟倒计时；还有未完成的工作才把面板摆出来。
        if tasks.iter().all(|t| t.status == TaskStatus::Completed) {
            st.auto_hide
                .hide_now(&tasks, now_ms(), st.config.hide_completed_after_ms());
        } else {
            extensions::request_show_dock();
        }
        return;
    }

    if tasks.iter().all(|t| t.status == TaskStatus::Completed) {
        _ = st.store.clear_completed();
        let scope = st.config.task_scope();
        let cwd = st.cwd.clone();
        cleanup_session_file(&mut st.store, scope, cwd.as_deref());
    }
}

/// `TaskUpdate` 的附带效果：跟踪活跃任务、重置/推进自动清理。
fn apply_update_side_effects(st: &mut State, task_id: &str, status: &str) {
    match status {
        "in_progress" => {
            st.widget.set_active(task_id, true);
            st.auto_clear.reset_batch_countdown();
        }
        "pending" => {
            st.widget.set_active(task_id, false);
            st.auto_clear.reset_batch_countdown();
        }
        "completed" => {
            st.widget.set_active(task_id, false);
            let turn = st.cadence.current_turn;
            let mode = st.config.auto_clear_completed();
            st.auto_clear
                .track_completion(&mut st.store, task_id, turn, mode);
        }
        "deleted" => st.widget.set_active(task_id, false),
        _ => {}
    }
}

impl Extension for Tasks {
    /// 固定返回 `"tasks"`，该字符串同时作为事件总线的发送方身份。
    fn name(&self) -> &str {
        EXT
    }

    /// 供 /extension 面板详情展示的一句话说明（列出 7 个工具与 /tasks 命令）。
    fn description(&self) -> &str {
        "Claude Code-style task tracking: TaskCreate / TaskList / TaskGet / TaskUpdate / TaskOutput / TaskStop / TaskExecute, a task widget, and /tasks."
    }

    /// 声明移植自 `@tintinweb/pi-tasks` v0.9.0，面板中可跳转上游仓库。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "pi-tasks".to_string(),
            plugin_version: "0.9.0".to_string(),
            url: "https://github.com/tintinweb/pi-tasks".to_string(),
        })
    }

    /// 仅在 Dev / Creator 两个模式下可用（All 之外更低的级别）。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认禁用：注册后需在 /extension 面板手动开启，避免主动注入工具改变默认行为。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 提供 TaskCreate / TaskList / TaskGet / TaskUpdate / TaskOutput / TaskStop / TaskExecute 七个工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        tools_schema()
    }

    /// 提供 `/tasks` 命令（busy 安全：只改扩展自身状态、发 UI 请求，不锁 agent）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Manage tasks — view, create, run, clear completed, settings".to_string(),
            busy_safe: true, // handler 只读/改扩展自身状态、发 UI 请求，不锁 agent
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 订阅停靠面板、agent 事件、边界、扩展总线、工具后置与系统提示注入六类钩子。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![
            ExtensionHook::Dock,
            ExtensionHook::AgentEvent,
            ExtensionHook::Boundary,
            ExtensionHook::ExtensionEvent,
            ExtensionHook::AfterToolCall,
            ExtensionHook::ContextWithSystem,
        ]
    }

    /// 停靠面板内容：任务列表（每帧采集，用内存缓存，不读盘）。
    fn dock_lines(&self) -> Vec<DockLine> {
        let mut guard = lock_state();
        let State {
            store,
            config,
            widget,
            auto_hide,
            ..
        } = &mut *guard;

        widget.prune(store);

        let frame = spinner_frame_index(DEF_SPINNER.len(), SPINNER_TICK_MS);
        widget::build_lines(store, config, widget, auto_hide, frame)
    }

    /// 有任务在跑、或墙钟隐藏倒计时进行中时请求每帧重绘。
    fn wants_redraw(&self) -> bool {
        let mut guard = lock_state();
        if guard.widget.has_active() {
            return true;
        }

        // 关闭时零开销。开启后即便 dock 当前不可见也要推进倒计时，
        // 否则 all_completed_at 会悬在那儿、wants_redraw 永远为真。
        if guard.config.hide_completed_after() == AutoHideDelay::Never {
            return false;
        }

        let State {
            store,
            config,
            auto_hide,
            ..
        } = &mut *guard;

        let tasks = store.list_cached(config.sort_order());
        let just_hid = auto_hide.tick(&tasks, now_ms(), config.hide_completed_after_ms());

        // 到点这一帧也要为真，好把隐藏真正渲染出去。
        just_hid || auto_hide.is_pending()
    }

    /// agent 内核事件：`turn_start` 推进回合、跑自动清理、重读磁盘；
    /// 硬中止（`agent_end(reason=aborted)`）则把本轮的后台任务退回 pending。
    fn on_agent_event(&self, event: &Value) {
        let ty = event.get("type").and_then(Value::as_str);

        // 硬中止：run 的 future 被丢弃，本轮派发的后台任务不能悬在 in_progress
        // （否则消息区已是「Operation aborted」，dock 里的任务行还在计时、转圈）。
        if ty == Some("agent_end") && event.get("reason").and_then(Value::as_str) == Some("aborted")
        {
            let agents = subagent::abort_running();
            if !agents.is_empty() {
                extensions::request_show_dock();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        for agent_id in agents {
                            subagent::stop_agent(&agent_id).await;
                        }
                    });
                }
            }
            return;
        }

        if ty != Some("turn_start") {
            return;
        }

        let mut guard = lock_state();
        let st = &mut *guard;
        st.cadence.on_turn_start();

        let turn = st.cadence.current_turn;
        let mode = st.config.auto_clear_completed();
        let scope = st.config.task_scope();
        let State {
            store,
            widget,
            auto_clear,
            cwd,
            ..
        } = st;

        // 回合起点重读磁盘：共享列表能看到其它会话的写入。
        _ = store.list(None);
        widget.prune(store);

        if auto_clear.on_turn_start(store, turn, mode) {
            cleanup_session_file(store, scope, cwd.as_deref());
        }
    }

    /// 边界：`turn_end` 记 token 用量 + 滞留检测；`agent_before_settle` 标记 run 结束。
    fn on_boundary(&self, event: &Value) -> Option<BoundaryOutcome> {
        let main_scope = event.get("agentScope").and_then(Value::as_str) == Some("main");

        match event.get("type").and_then(Value::as_str) {
            Some("turn_end") => {
                if main_scope {
                    let input = event
                        .pointer("/message/usage/input")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let output = event
                        .pointer("/message/usage/output")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);

                    let mut guard = lock_state();
                    let State {
                        widget,
                        cadence,
                        store,
                        ..
                    } = &mut *guard;

                    if input > 0 || output > 0 {
                        widget.add_usage(input, output);
                    }

                    // 滞留检测：捕获「纯文本回合（无工具调用，tool_result 不触发）但留下
                    // in_progress 任务」的情况。先做便宜的检查，再读 store。
                    let gap = cadence
                        .current_turn
                        .saturating_sub(cadence.last_task_tool_use_turn);

                    if gap >= ACTIVE_REMINDER_INTERVAL {
                        let has_in_progress = store
                            .list_cached(None)
                            .iter()
                            .any(|t| t.status == TaskStatus::InProgress);

                        cadence.note_stale_if_active(has_in_progress, ACTIVE_REMINDER_INTERVAL);
                    }
                }
                None
            }
            Some("agent_before_settle") => {
                if main_scope {
                    lock_state().auto_clear.on_run_ended();
                }
                None
            }
            _ => None,
        }
    }

    /// 工具结果：**只**推进 cadence，绝不改任何工具输出。
    ///
    /// 把 `<system-reminder>` 追加到无关工具（read/bash/grep…）的结果里会破坏模型可见
    /// 的 transcript 语义、让工具输出调试变得痛苦；真正的注入在 `transform_context_with_system` 里。
    fn after_tool_call(
        &self,
        name: &str,
        _args: &Value,
        result: &str,
    ) -> std::result::Result<String, ToolError> {
        let mut guard = lock_state();
        let State { cadence, store, .. } = &mut *guard;

        if is_task_tool(name) {
            cadence.note_task_tool();
            return Ok(result.to_string());
        }

        if cadence.reminder_injected_this_cycle {
            return Ok(result.to_string());
        }

        // 便宜优先：回合间隙不够时不必读 store（ACTIVE 是最小的间隔）。
        if cadence
            .current_turn
            .saturating_sub(cadence.last_task_tool_use_turn)
            < ACTIVE_REMINDER_INTERVAL
        {
            return Ok(result.to_string());
        }

        let tasks = cadence::tasks_snapshot(store);
        let interval = cadence::interval_for(&tasks, REMINDER_INTERVAL, ACTIVE_REMINDER_INTERVAL);
        cadence.maybe_mark_due(!tasks.is_empty(), interval);

        Ok(result.to_string())
    }

    /// 把**瞬态** system-reminder 注入即将发出的 LLM 调用，绝不写进工具结果。
    ///
    /// 用 `ContextWithSystem`（而非 `TransformContext`）：前者结果原样发送、**不改写 `agent.messages`**。
    /// 提醒以 user 消息的形式 append，兼容不支持自定义消息类型的模型。
    fn transform_context_with_system(&self, messages: &mut Vec<AgentMessage>) -> Result<()> {
        let due = lock_state().cadence.drain_reminder_for_context();
        if !due {
            return Ok(());
        }

        let tasks = lock_state().store.list_cached(None);
        push_context(messages, &cadence::build_system_reminder(&tasks));
        Ok(())
    }

    /// 设置面板项由当前配置派生（作用域、排序、自动清理与自动隐藏等）。
    fn settings(&self) -> Vec<ExtensionSetting> {
        config::panel_settings(&lock_state().config)
    }

    /// 应用一项设置：非法值返回 `Err`（面板保持原值）；成功则落盘，
    /// 若作用域变化导致 store 身份键改变则重建 store 并重置菜单状态。
    fn apply_setting(&self, key: &str, value: &str) -> std::result::Result<(), String> {
        let mut st = lock_state();
        config::apply_setting(&mut st.config, key, value)?;

        if let Some(cwd) = st.cwd.clone() {
            config::save_tasks_config(&st.config, &cwd);
        }

        // 作用域可能刚变过 → 下一次 sync 重定 store。
        let scope = st.config.task_scope();
        let (k, p) = resolve_store_target(st.cwd.as_deref(), st.session_id.as_deref(), scope);

        if k != st.store_key {
            st.store = TaskStore::new(p);
            st.store_key = k;
            st.menu = MenuState::default();
        }
        Ok(())
    }

    /// 注册 `/tasks` 斜杠命令的 handler。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_tasks);
    }

    /// 会话开始：记录 cwd 与会话 id、按作用域重定 store，并按是否 resume 恢复展示态；
    /// 最后 reattach 与 subagent 的 RPC 联动。
    fn on_session_start(&self, cwd: &str, messages: &[AgentMessage], session: Option<&Session>) {
        let is_resume = !messages.is_empty();
        let session_id = session
            .and_then(|s| s.get_session_file())
            .and_then(session_key_from_path);

        {
            let mut st = lock_state();
            st.cwd = Some(cwd.to_string());
            st.session_id = session_id;
            sync_store(&mut st, true);
            after_session_sync(&mut st, is_resume);
        }

        subagent::reattach(); // subagent 比扩展实例长寿：重连后才能收到它们的终态事件。
    }

    /// 会话切换（/new /resume /import /fork /clone）：作废上个会话的展示态并重定 store；
    /// 从当前会话 fork 出的新会话继承父任务，全新会话在 memory 作用域下清空列表。
    fn on_session_switched(&self, session_path: Option<&str>, messages: &[AgentMessage]) {
        let session_id = session_path
            .filter(|p| !p.is_empty())
            .and_then(|p| session_key_from_path(Path::new(p)));

        // fork/clone：新会话文件头记录了父会话 id。仅当父会话正是我们刚离开的会话时才
        // 继承——`/fork` 与 `/clone` 都从当前会话复制；`/resume` 一个旧的 fork 不会误触发。
        let fork_parent = session_path
            .filter(|p| !p.is_empty())
            .and_then(|p| session_parent_id(Path::new(p)));

        {
            let mut st = lock_state();
            let inherits_from_current = fork_parent.is_some() && fork_parent == st.session_id;

            // 全新会话：没有历史消息、也不是从当前会话 fork 出来的。
            let is_new = messages.is_empty() && !inherits_from_current;

            st.session_id = session_id;

            // 旧会话的展示态一律作废：即使作用域共享（`project`/`memory`）store 没换，
            // 也不能让 dock 继续挂着上一个会话的任务行（对照 subagent 的 `reset_all`）。
            reset_session_state(&mut st);
            sync_store(&mut st, false);

            // `memory` 作用域没有会话文件可切，新会话里显式清空：否则旧任务会一直留在 dock 上。
            if is_new && st.config.task_scope() == TaskScope::Memory {
                _ = st.store.clear_all();
            }

            if inherits_from_current && let Some(parent) = fork_parent {
                seed_forked_tasks(&mut st, &parent);
            }

            // 只有恢复/ fork 才把持久化任务重新摆出来并弹面板；新会话保持干净。
            after_session_sync(&mut st, !is_new);
        }

        subagent::reattach();
    }

    /// 会话执行上下文更新：cwd 变化时重定 store，并向 subagent 联动探活（幂等）。
    fn on_exec_ctx(&self, ctx: &ToolExecCtx) {
        {
            let mut st = lock_state();
            if st.cwd.as_deref() != Some(ctx.cwd.as_str()) {
                st.cwd = Some(ctx.cwd.clone());
                sync_store(&mut st, true);
            }
        }

        subagent::ping(); // 探活（幂等）；不持锁发总线事件。
    }

    /// 跨扩展总线：处理 `subagents:*` 生命周期事件与 RPC 回执。
    fn on_extension_event(&self, name: &str, payload: &Value) {
        subagent::on_event(name, payload);
    }

    /// 把菜单面板的选中/取消交给菜单状态机；列表有变化时清理会话文件并请求展示停靠面板。
    /// 始终返回 `None`（不触发 follow-up 输入）。
    fn on_ui_choice(&self, id: u64, choice: Option<String>) -> Option<String> {
        let mut guard = lock_state();
        let st = &mut *guard;
        let scope = st.config.task_scope();
        let State {
            store, menu, cwd, ..
        } = st;

        let changed = menu::on_choice(store, menu, id, choice);
        if changed {
            cleanup_session_file(store, scope, cwd.as_deref());
            drop(guard);
            extensions::request_show_dock();
        }
        None
    }

    /// 按工具名分发：三个 RPC 工具直接走 subagent 异步通道（不持状态锁），
    /// 其余任务工具在锁内同步执行，成功时请求展示停靠面板。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        _ctx: ToolExecCtx,
    ) -> BoxFuture<'static, std::result::Result<ToolResult, ToolError>> {
        Box::pin(async move {
            // 联动工具是异步 RPC，不持状态锁。
            match name.as_str() {
                "TaskExecute" => return subagent::execute(&args).await,
                "TaskOutput" => return subagent::output(&args).await,
                "TaskStop" => return subagent::stop(&args).await,
                _ => {}
            }

            let mut show_dock = false;
            let result = {
                let mut guard = lock_state();
                let st = &mut *guard;

                match name.as_str() {
                    "TaskCreate" => {
                        // 已完成列表不得收下紧随其后的批次（它属于上一个 run）。
                        let mode = st.config.auto_clear_completed();
                        st.auto_clear.start_new_batch(&mut st.store, mode);
                        show_dock = true;
                        tools::task_create(&args, &mut st.store)
                    }
                    "TaskList" => tools::task_list(&args, &mut st.store),
                    "TaskGet" => tools::task_get(&args, &mut st.store),
                    "TaskUpdate" => {
                        let r = tools::task_update(&args, &mut st.store);
                        let task_id = args
                            .get("taskId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let status = args.get("status").and_then(Value::as_str).unwrap_or("");
                        apply_update_side_effects(st, &task_id, status);
                        show_dock = true;
                        r
                    }
                    other => Err(ToolError(format!("extension {EXT}: unknown tool: {other}"))),
                }
            };

            if show_dock && result.is_ok() {
                extensions::request_show_dock();
            }

            result
        })
    }
}

/// `/tasks` 命令入口。
///
/// 无参（或未识别的输入）打开事件驱动的交互菜单；`/tasks create ...` 直接建任务，
/// 不弹输入面板（prux 无 `ui.input` 单行输入原语，故改用带参子命令）；
/// `/tasks run [<id>...]` 发一条续跑消息，让主 agent 按任务列表把未竟工作做完。
fn command_tasks(st: &mut App, raw: &str) -> bool {
    let arg = command_arg(raw).trim().to_string();

    {
        let mut state = lock_state();
        if state.cwd.is_none()
            && let Ok(cwd) = std::env::current_dir()
        {
            state.cwd = Some(cwd.to_string_lossy().to_string());
        }

        sync_store(&mut state, false);
    }

    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim()),
        None => (arg.as_str(), ""),
    };

    match sub {
        "create" => create_task_from_command(rest),
        "run" => run_tasks_from_command(st, rest),
        _ => {
            let mut state = lock_state();
            let State { store, menu, .. } = &mut *state;
            menu::open_main(store, menu);
        }
    }

    false
}

/// 解析 `/tasks create` 的参数：`<subject> [:: <description>]`。无 subject 返回 `None`。
///
/// description 缺省时回退为 subject（上游要求非空 description）。
fn parse_create_args(rest: &str) -> Option<(String, String)> {
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }

    match rest.split_once("::") {
        Some((subject, description)) if !subject.trim().is_empty() => {
            Some((subject.trim().to_string(), description.trim().to_string()))
        }
        _ => Some((rest.to_string(), rest.to_string())),
    }
}

/// `/tasks create` 的直接创建路径。
fn create_task_from_command(rest: &str) {
    let Some((subject, description)) = parse_create_args(rest) else {
        notify_text(
            "usage: /tasks create <subject> [:: <description>]",
            UiNotifyLevel::Warning,
        );
        return;
    };

    let created = {
        let mut st = lock_state();
        let mode = st.config.auto_clear_completed();
        let State {
            store, auto_clear, ..
        } = &mut *st;
        auto_clear.start_new_batch(store, mode);
        store.create(subject, description, None, None)
    };

    match created {
        Ok(task) => {
            extensions::request_show_dock();
            notify_text(
                &format!("Created task #{}: {}", task.id, task.subject),
                UiNotifyLevel::Info,
            );
        }
        Err(e) => notify_text(&format!("Failed to create task: {e}"), UiNotifyLevel::Error),
    }
}

/// 解析 `/tasks run` 的参数：任务 id 列表（空白/逗号分隔，允许 `#` 前缀）。
///
/// 空输入表示「全部未完成任务」。保留首次出现顺序并去重，便于提示文本稳定。
fn parse_run_ids(rest: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for token in rest.split(|c: char| c.is_whitespace() || c == ',') {
        let id = token.trim().trim_start_matches('#');
        if id.is_empty() || ids.iter().any(|x| x == id) {
            continue;
        }
        ids.push(id.to_string());
    }
    ids
}

/// `/tasks run` 的续跑提示：让**主对话**接着把列出的任务做完。
///
/// 不复用 `TaskExecute`（那是给带 `agentType` 的任务起子代理）；这里只列出目标、
/// 讲清任务工具的用法，具体怎么做仍由主 agent 自己决定。
///
/// 提示必须把范围**严格限定在列出的任务**：`TaskList` 会同时显示其它未完成任务，
/// 若只笼统地说「按 ID 顺序做完」，agent 很容易顺手把没被指定的任务也一起做掉。
fn run_prompt(tasks: &[Task]) -> String {
    let listed = tasks
        .iter()
        .map(|t| format!("#{}, {}", t.id, t.subject))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Continue working until the following tasks are completed. Work on these tasks \
only:\n\
{listed}\n\
\n\
Use TaskGet for details on each target. Do not start, modify, or complete any task that is not \
in this list, even if TaskList shows other pending tasks. Mark each listed task in_progress \
with TaskUpdate before starting it, and completed as soon as it is fully done. Among the listed \
tasks, respect blockedBy dependencies (finish blockers first) and work in ID order. Do not \
stop until every task in this list is completed."
    )
}

/// 把续跑消息交给 UI。
///
/// 空闲时走 `Continuation`（触发下一轮 prompt）；忙碌时作为 steer 注入当前回合——
/// 忙碌态下 `Continuation` 会被 TUI 丢弃（见 TUI 分发），而 `/tasks` 命令声明了
/// `busy_safe`，忙碌时也会照常执行，故必须自带一条不丢消息的回退路径。
fn dispatch_run_prompt(app: &mut App, prompt: String) {
    if app.busy {
        app.runtime_steer_inbox
            .lock()
            .unwrap()
            .push_back(AgentMessage::user_text(&prompt));
    } else {
        extensions::request_ui(ExtensionUiRequest::Continuation {
            message: Box::new(AgentMessage::user_text(&prompt)),
        });
    }
}

/// `/tasks run` 的入口：筛出未完成任务并投一条续跑消息。
///
/// 无参=全部未完成任务；带 id=只跑这些任务（已完成的会被过滤掉，全无目标时只提示）。
fn run_tasks_from_command(app: &mut App, rest: &str) {
    let ids = parse_run_ids(rest);

    let targets: Vec<Task> = {
        let mut st = lock_state();
        st.store
            .list(None)
            .into_iter()
            .filter(|t| t.status != TaskStatus::Completed)
            .filter(|t| ids.is_empty() || ids.iter().any(|id| id == &t.id))
            .collect()
    };

    if targets.is_empty() {
        notify_text(
            if ids.is_empty() {
                "No pending tasks to run."
            } else {
                "No matching pending tasks to run."
            },
            UiNotifyLevel::Warning,
        );
        return;
    }

    let count = targets.len();
    dispatch_run_prompt(app, run_prompt(&targets));
    extensions::request_show_dock();
    notify_text(&format!("Running {count} task(s)..."), UiNotifyLevel::Info);
}

/// 工具 schema
fn tools_schema() -> Vec<ExtensionTool> {
    vec![
        ExtensionTool {
            exposure: ToolExposure::Direct,
            namespace: None,
            annotations: ToolAnnotations::default(),
            output_schema: None,
            name: "TaskCreate".to_string(),
            description: TASK_CREATE_DESC.to_string(),
            label: Some("Create task".to_string()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "subject": { "type": "string", "description": "A brief title for the task" },
                    "description": { "type": "string", "description": "A detailed description of what needs to be done" },
                    "activeForm": { "type": "string", "description": "Present continuous form shown in spinner when in_progress (e.g., 'Running tests')" },
                    "agentType": { "type": "string", "description": "Agent type for subagent execution (e.g., 'general-purpose', 'Explore'). Tasks with agentType can be started via TaskExecute." },
                    "metadata": { "type": "object", "description": "Arbitrary metadata to attach to the task" }
                },
                "required": ["subject", "description"],
                "additionalProperties": false
            }),
            snippet: "Create a structured task for the current session".to_string(),
            prompt_guidelines: vec![
                "When working on complex multi-step tasks, use TaskCreate to track progress and TaskUpdate to update status.".to_string(),
                "Mark tasks as in_progress before starting work and completed when done.".to_string(),
                "Use TaskList to check for available work after completing a task.".to_string(),
            ],
            constrained_sampling: false,
            render_shell: None,
            execution_mode: Some(ToolExecutionMode::Parallel),
            prepare_arguments: None,
            grammar_sampling: None,
        },
        ExtensionTool {
            exposure: ToolExposure::Direct,
            namespace: None,
            annotations: ToolAnnotations::default(),
            output_schema: None,
            name: "TaskList".to_string(),
            description: TASK_LIST_DESC.to_string(),
            label: Some("List tasks".to_string()),
            parameters: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            snippet: "List all tasks with status, owner, and blocked-by info".to_string(),
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
            name: "TaskGet".to_string(),
            description: TASK_GET_DESC.to_string(),
            label: Some("Get task".to_string()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "taskId": { "type": "string", "description": "The ID of the task to retrieve" }
                },
                "required": ["taskId"],
                "additionalProperties": false
            }),
            snippet: "Get full details for a specific task".to_string(),
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
            name: "TaskUpdate".to_string(),
            description: TASK_UPDATE_DESC.to_string(),
            label: Some("Update task".to_string()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "taskId": { "type": "string", "description": "The ID of the task to update" },
                    "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "deleted"], "description": "New status for the task" },
                    "subject": { "type": "string", "description": "New subject for the task" },
                    "description": { "type": "string", "description": "New description for the task" },
                    "activeForm": { "type": "string", "description": "Present continuous form shown in spinner when in_progress" },
                    "owner": { "type": "string", "description": "New owner for the task" },
                    "metadata": { "type": "object", "description": "Metadata keys to merge into the task. Set a key to null to delete it." },
                    "addBlocks": { "type": "array", "items": { "type": "string" }, "description": "Task IDs that this task blocks" },
                    "addBlockedBy": { "type": "array", "items": { "type": "string" }, "description": "Task IDs that block this task" }
                },
                "required": ["taskId"],
                "additionalProperties": false
            }),
            snippet: "Update a task's fields, status, metadata, and dependencies".to_string(),
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
            name: "TaskOutput".to_string(),
            description: TASK_OUTPUT_DESC.to_string(),
            label: Some("Task output".to_string()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "The task ID to get output from" },
                    "block": { "type": "boolean", "description": "Whether to wait for completion", "default": true },
                    "timeout": { "type": "number", "description": "Max wait time in ms", "default": 30000, "minimum": 0, "maximum": 600000 }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
            snippet: "Retrieve output from a running or completed task".to_string(),
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
            name: "TaskStop".to_string(),
            description: TASK_STOP_DESC.to_string(),
            label: Some("Stop task".to_string()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "The ID of the background task to stop" },
                    "shell_id": { "type": "string", "description": "Deprecated: use task_id instead" }
                },
                "additionalProperties": false
            }),
            snippet: "Stop a running background task by its ID".to_string(),
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
            name: "TaskExecute".to_string(),
            description: TASK_EXECUTE_DESC.to_string(),
            label: Some("Execute tasks".to_string()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_ids": { "type": "array", "items": { "type": "string" }, "description": "Task IDs to execute as subagents" },
                    "additional_context": { "type": "string", "description": "Extra context for agent prompts" },
                    "model": { "type": "string", "description": "Model override for agents" },
                    "max_turns": { "type": "number", "description": "Max turns per agent", "minimum": 1 }
                },
                "required": ["task_ids"],
                "additionalProperties": false
            }),
            snippet: "Execute one or more tasks as background subagents".to_string(),
            prompt_guidelines: vec![
                "Never use the Agent tool for tasks launched via TaskExecute — agents are already running.".to_string(),
            ],
            constrained_sampling: false,
            render_shell: None,
            execution_mode: None,
            prepare_arguments: None,
            grammar_sampling: None,
        },
    ]
}

// 工具描述
/// TaskCreate 工具的完整提示词，说明创建任务的时机、字段与注意事项。
const TASK_CREATE_DESC: &str = r#"Use this tool to create a structured task list for your current coding session. This helps you track progress, organize complex tasks, and demonstrate thoroughness to the user.
It also helps the user understand the progress of the task and overall progress of their requests.

## When to Use This Tool

Use this tool proactively in these scenarios:

- Complex multi-step tasks - When a task requires 3 or more distinct steps or actions
- Non-trivial and complex tasks - Tasks that require careful planning or multiple operations
- Plan mode - When using plan mode, create a task list to track the work
- User explicitly requests todo list - When the user directly asks you to use the todo list
- User provides multiple tasks - When users provide a list of things to be done (numbered or comma-separated). Create them all in one response with one TaskCreate call per task
- After receiving new instructions - Immediately capture user requirements as tasks
- When you start working on a task - Mark it as in_progress BEFORE beginning work
- After completing a task - Mark it as completed and add any new follow-up tasks discovered during implementation

## When NOT to Use This Tool

Skip using this tool when:
- There is only a single, straightforward task
- The task is trivial and tracking it provides no organizational benefit
- The task can be completed in less than 3 trivial steps
- The task is purely conversational or informational

NOTE that you should not use this tool if there is only one trivial task to do. In this case you are better off just doing the task directly.

## Task Fields

- **subject**: A brief, actionable title in imperative form (e.g., "Fix authentication bug in login flow")
- **description**: Detailed description of what needs to be done, including context and acceptance criteria
- **activeForm** (optional): Present continuous form shown in the spinner when the task is in_progress (e.g., "Fixing authentication bug"). If omitted, the spinner shows the subject instead.

All tasks are created with status `pending`.

## Tips

- Create tasks with clear, specific subjects that describe the outcome
- Include enough detail in the description for another agent to understand and complete the task
- After creating tasks, use TaskUpdate to set up dependencies (blocks/blockedBy) if needed
- Check TaskList first to avoid creating duplicate tasks
- Include `agentType` (e.g., "general-purpose", "Explore") to mark tasks for subagent execution via TaskExecute
- To create several tasks at once, call TaskCreate multiple times in a single response — independent tool calls run in parallel, so the whole batch is created in one turn (one task per call)."#;

/// TaskList 工具的描述，说明如何查看任务进度、阻塞情况并认领可用任务。
const TASK_LIST_DESC: &str = r#"Use this tool to list all tasks in the task list.

## When to Use This Tool

- To see what tasks are available to work on (status: 'pending', no owner, not blocked)
- To check overall progress on the project
- To find tasks that are blocked and need dependencies resolved
- After completing a task, to check for newly unblocked work or claim the next available task
- **Prefer working on tasks in ID order** (lowest ID first) when multiple tasks are available, as earlier tasks often set up context for later ones

## Output

Returns a summary of each task:
- **id**: Task identifier (use with TaskGet, TaskUpdate)
- **subject**: Brief description of the task
- **status**: 'pending', 'in_progress', or 'completed'
- **owner**: Agent ID if assigned, empty if available
- **blockedBy**: List of open task IDs that must be resolved first (tasks with blockedBy cannot be claimed until dependencies resolve)

Use TaskGet with a specific task ID to view full details including description and comments."#;

/// TaskGet 工具的描述，用于按 ID 获取任务完整详情与依赖关系。
const TASK_GET_DESC: &str = r#"Use this tool to retrieve a task by its ID from the task list.

## When to Use This Tool

- When you need the full description and context before starting work on a task
- To understand task dependencies (what it blocks, what blocks it)
- After being assigned a task, to get complete requirements

## Output

Returns full task details:
- **subject**: Task title
- **description**: Detailed requirements and context
- **status**: 'pending', 'in_progress', or 'completed'
- **blocks**: Tasks waiting on this one to complete
- **blockedBy**: Tasks that must complete before this one can start

## Tips

- After fetching a task, verify its blockedBy list is empty before beginning work.
- Use TaskList to see all tasks in summary form."#;

/// TaskOutput 工具的描述，用于取后台任务输出并支持阻塞或非阻塞等待。
const TASK_OUTPUT_DESC: &str = r#"- Retrieves output from a running or completed task (background shell, agent, or remote session)
- Takes a task_id parameter identifying the task
- Returns the task output along with status information
- Use block=true (default) to wait for task completion
- Use block=false for non-blocking check of current status
- Task IDs can be found using the /tasks command
- Works with all task types: background shells, async agents, and remote sessions"#;

/// TaskStop 工具的描述，用于按 ID 终止长时间运行的后台任务。
const TASK_STOP_DESC: &str = r#"
- Stops a running background task by its ID
- Takes a task_id parameter identifying the task to stop
- Returns a success or failure status
- Use this tool when you need to terminate a long-running task"#;

/// TaskExecute 工具的描述，说明把已就绪的待办任务作为后台子代理执行。
const TASK_EXECUTE_DESC: &str = r#"Execute one or more tasks as subagents.

## When to Use This Tool

- To start execution of tasks that have `agentType` set (created via TaskCreate with agentType parameter)
- Tasks must be `pending` with all blockedBy dependencies `completed`
- Each task runs as an independent background subagent

## Parameters

- **task_ids**: Array of task IDs to execute
- **additional_context**: Extra context appended to each agent's prompt
- **model**: Model override for agents (e.g., "sonnet", "haiku")
- **max_turns**: Maximum turns per agent"#;

/// TaskUpdate 工具的描述，指导更新任务状态、依赖、元数据或删除任务。
const TASK_UPDATE_DESC: &str = r#"Use this tool to update a task in the task list.

## When to Use This Tool

**Before starting work on a task:**
- Mark it in_progress BEFORE beginning — do not start work without updating status first
- After resolving, call TaskList to find your next task

**Mark tasks as resolved:**
- When you have completed the work described in a task
- When a task is no longer needed or has been superseded
- IMPORTANT: Always mark your assigned tasks as resolved when you finish them
- After resolving, call TaskList to find your next task

- ONLY mark a task as completed when you have FULLY accomplished it
- If you encounter errors, blockers, or cannot finish, keep the task as in_progress
- When blocked, create a new task describing what needs to be resolved
- Never mark a task as completed if:
  - Tests are failing
  - Implementation is partial
  - You encountered unresolved errors
  - You couldn't find necessary files or dependencies

**Delete tasks:**
- When a task is no longer relevant or was created in error
- Setting status to `deleted` permanently removes the task

**Update task details:**
- When requirements change or become clearer
- When establishing dependencies between tasks

## Fields You Can Update

- **status**: The task status (see Status Workflow below)
- **subject**: Change the task title (imperative form, e.g., "Run tests")
- **description**: Change the task description
- **activeForm**: Present continuous form shown in spinner when in_progress (e.g., "Running tests")
- **owner**: Change the task owner (agent name)
- **metadata**: Merge metadata keys into the task (set a key to null to delete it)
- **addBlocks**: Mark tasks that cannot start until this one completes
- **addBlockedBy**: Mark tasks that must complete before this one can start

## Status Workflow

Status progresses: `pending` → `in_progress` → `completed`

Use `deleted` to permanently remove a task.

## Staleness

Make sure to read a task's latest state using `TaskGet` before updating it.

## Examples

Mark task as in progress when starting work:
```json
{"taskId": "1", "status": "in_progress"}
```

Mark task as completed after finishing work:
```json
{"taskId": "1", "status": "completed"}
```

Delete a task:
```json
{"taskId": "1", "status": "deleted"}
```

Claim a task by setting owner:
```json
{"taskId": "1", "owner": "my-name"}
```

Set up task dependencies:
```json
{"taskId": "2", "addBlockedBy": ["1"]}
```"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    #[test]
    fn store_target_env_off_is_memory() {
        // 环境变量是进程级的；测试内联设置需与其它测试串行。改用 scope 断言避免触碰 env。
        let (key, path) =
            resolve_store_target(Some("/tmp/x"), Some("s1"), config::TaskScope::Memory);
        assert_eq!(key, "memory:config");
        assert!(path.is_none());
    }

    #[test]
    fn store_target_session_and_project() {
        let _g = AgentDirGuard::temp();
        let (key, path) =
            resolve_store_target(Some("/tmp/proj"), Some("s1"), config::TaskScope::Session);
        assert!(key.starts_with("path:"));
        assert!(path.unwrap().to_string_lossy().contains("tasks-s1.json"));

        let (key, _) =
            resolve_store_target(Some("/tmp/proj"), Some("s1"), config::TaskScope::Project);
        assert!(key.ends_with("tasks/tasks.json"), "{key}");
    }

    #[test]
    fn tools_schema_is_stable() {
        let names: Vec<String> = tools_schema().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            [
                "TaskCreate",
                "TaskList",
                "TaskGet",
                "TaskUpdate",
                "TaskOutput",
                "TaskStop",
                "TaskExecute"
            ]
        );
    }

    /// `/tasks create` 的参数解析：`::` 分割 subject/description；无 `::` 时描述回退为 subject。
    #[test]
    fn parse_create_args_forms() {
        assert_eq!(parse_create_args(""), None);
        assert_eq!(parse_create_args("   "), None);
        assert_eq!(
            parse_create_args("Fix login"),
            Some(("Fix login".into(), "Fix login".into()))
        );
        assert_eq!(
            parse_create_args("Fix login :: Handle OAuth redirect"),
            Some(("Fix login".into(), "Handle OAuth redirect".into()))
        );
        // 空 subject + `::` 视为没有分隔符：整串作为 subject
        assert_eq!(
            parse_create_args(":: only desc"),
            Some((":: only desc".into(), ":: only desc".into()))
        );
    }

    /// 声明的子命令必须与 `command_tasks` 的解析分支一致（避免补全出无效子命令）。
    #[test]
    fn subcommands_match_parser() {
        let ext = Tasks;
        let subs: Vec<&str> = ext.commands()[0]
            .subcommands
            .iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(subs, ["create", "run"]);
        // 声明的每个子命令都能被 handler 识别（`create` 直接建任务，`run` 解析出 id 列表）
        assert!(parse_create_args("x").is_some());
        assert_eq!(parse_run_ids("1"), ["1"]);
    }

    /// `/tasks run` 的 id 解析：空白/逗号分隔，`#` 前缀可省，去重保序。
    #[test]
    fn parse_run_ids_forms() {
        assert!(parse_run_ids("").is_empty());
        assert!(parse_run_ids("   ").is_empty());
        assert_eq!(parse_run_ids("1 2"), ["1", "2"]);
        assert_eq!(parse_run_ids("#1, #2 ,3"), ["1", "2", "3"]);
        assert_eq!(parse_run_ids("2 1 2"), ["2", "1"]);
    }

    /// 续跑提示列出目标任务 id/subject，并用祈使句交代任务工具用法。
    #[test]
    fn run_prompt_lists_targets_and_tool_guidance() {
        let tasks = vec![
            Task {
                id: "1".into(),
                subject: "Fix login".into(),
                description: "d".into(),
                status: TaskStatus::Pending,
                active_form: None,
                owner: None,
                metadata: Default::default(),
                blocks: Vec::new(),
                blocked_by: Vec::new(),
                created_at: 0,
                updated_at: 0,
            },
            Task {
                id: "3".into(),
                subject: "Write tests".into(),
                description: "d".into(),
                status: TaskStatus::InProgress,
                active_form: None,
                owner: None,
                metadata: Default::default(),
                blocks: Vec::new(),
                blocked_by: Vec::new(),
                created_at: 0,
                updated_at: 0,
            },
        ];

        let prompt = run_prompt(&tasks);
        assert!(prompt.contains("#1, Fix login"), "{prompt}");
        assert!(prompt.contains("#3, Write tests"), "{prompt}");
        assert!(prompt.contains("TaskUpdate"), "{prompt}");
        assert!(prompt.contains("blockedBy"), "{prompt}");
        // 必须把范围限定在列出的任务，避免 agent 顺手做掉 TaskList 里的其它任务。
        assert!(prompt.contains("Work on these tasks only"), "{prompt}");
        assert!(
            prompt.contains("Do not start, modify, or complete any task that is not in this list"),
            "{prompt}"
        );
    }

    /// `/tasks run`（空闲）只投一条 `Continuation`，不落任何 transcript/历史；
    /// 提示覆盖全部未完成任务（已完成的被过滤）。
    #[test]
    fn command_run_queues_continuation_for_pending_tasks() {
        let _g = AgentDirGuard::temp();
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while crate::core::extensions::take_pending_ui().is_some() {}

        {
            let mut st = lock_state();
            st.cwd = Some("/tmp/tasks-run-cmd".to_string());
            st.session_id = None;
            st.config = TasksConfig {
                task_scope: Some(TaskScope::Memory),
                ..Default::default()
            };
            st.store = TaskStore::new(None);
            st.store_key = "memory:config".to_string();
            st.store.create("A".into(), "d".into(), None, None).unwrap();
            st.store.create("B".into(), "d".into(), None, None).unwrap();
            st.store
                .update(
                    "2",
                    super::types::TaskUpdateFields {
                        status: Some(super::types::TaskStatusOrDeleted::Status(
                            TaskStatus::Completed,
                        )),
                        ..Default::default()
                    },
                )
                .unwrap();
        }

        let mut app = App::new();
        assert!(!command_tasks(&mut app, "tasks run"));

        let mut continuation = None;
        while let Some(req) = crate::core::extensions::take_pending_ui() {
            if let ExtensionUiRequest::Continuation { message } = req {
                continuation = Some(message.text());
            }
        }
        let text = continuation.expect("空闲态应投递一条 Continuation");
        assert!(text.contains("#1, A"), "{text}");
        assert!(text.contains("TaskUpdate"), "{text}");
        assert!(!text.contains("#2, B"), "已完成任务不该再列出: {text}");
    }

    /// `/tasks run <id>` 只挑指定任务；没有未完成目标时只提示，不投 `Continuation`。
    #[test]
    fn command_run_filters_by_id_and_noops_without_targets() {
        let _g = AgentDirGuard::temp();
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while crate::core::extensions::take_pending_ui().is_some() {}

        {
            let mut st = lock_state();
            st.cwd = Some("/tmp/tasks-run-ids".to_string());
            st.session_id = None;
            st.config = TasksConfig {
                task_scope: Some(TaskScope::Memory),
                ..Default::default()
            };
            st.store = TaskStore::new(None);
            st.store_key = "memory:config".to_string();
            st.store.create("A".into(), "d".into(), None, None).unwrap();
            st.store.create("B".into(), "d".into(), None, None).unwrap();
        }

        let mut app = App::new();
        assert!(!command_tasks(&mut app, "tasks run #2"));

        let mut text = None;
        while let Some(req) = crate::core::extensions::take_pending_ui() {
            if let ExtensionUiRequest::Continuation { message } = req {
                text = Some(message.text());
            }
        }
        let text = text.expect("指定 id 有未完成任务时应续跑");
        assert!(text.contains("#2, B"), "{text}");
        assert!(!text.contains("#1, A"), "未选中的任务不该出现: {text}");

        // 明确只跑一个不存在的 id：无目标 → 不投递
        assert!(!command_tasks(&mut app, "tasks run 999"));
        let mut saw_continuation = false;
        while let Some(req) = crate::core::extensions::take_pending_ui() {
            if matches!(req, ExtensionUiRequest::Continuation { .. }) {
                saw_continuation = true;
            }
        }
        assert!(!saw_continuation, "无目标时不得续跑");
    }

    /// 忙碌时 `/tasks run` 改走 steer 注入箱：`Continuation` 在忙碌态会被 TUI 丢弃。
    #[test]
    fn command_run_steers_when_busy() {
        let _g = AgentDirGuard::temp();
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while crate::core::extensions::take_pending_ui().is_some() {}

        {
            let mut st = lock_state();
            st.cwd = Some("/tmp/tasks-run-busy".to_string());
            st.session_id = None;
            st.config = TasksConfig {
                task_scope: Some(TaskScope::Memory),
                ..Default::default()
            };
            st.store = TaskStore::new(None);
            st.store_key = "memory:config".to_string();
            st.store.create("A".into(), "d".into(), None, None).unwrap();
        }

        let mut app = App::new();
        app.busy = true;
        app.runtime_steer_inbox.lock().unwrap().clear();
        assert!(!command_tasks(&mut app, "tasks run"));

        assert!(
            !matches!(
                crate::core::extensions::take_pending_ui(),
                Some(ExtensionUiRequest::Continuation { .. })
            ),
            "忙碌态不应投 Continuation"
        );
        let inbox = app.runtime_steer_inbox.lock().unwrap();
        assert_eq!(inbox.len(), 1, "忙碌态应把续跑消息作为 steer 注入");
        assert!(inbox[0].text().contains("#1, A"));
    }

    /// fork/clone 继承：父会话任务种入新会话的空 store；重复切换是 no-op。
    #[test]
    fn fork_seed_inherits_parent_tasks() {
        if std::env::var(ENV_TASKS)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            return; // 显式存储路径下不按会话分文件
        }

        let _g = AgentDirGuard::temp();
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();

        let parent_path = paths::session_task_file(&cwd, "parent", TaskScope::Session);
        let mut parent = TaskStore::new(Some(parent_path));
        parent.create("A".into(), "d".into(), None, None).unwrap();
        parent.create("B".into(), "d".into(), None, None).unwrap();

        let child_path = paths::session_task_file(&cwd, "child", TaskScope::Session);
        let mut st = State {
            cwd: Some(cwd.clone()),
            store: TaskStore::new(Some(child_path)),
            store_key: "test".into(),
            ..Default::default()
        };

        seed_forked_tasks(&mut st, "parent");
        let tasks = st.store.list(None);
        assert_eq!(tasks.len(), 2, "继承父会话任务");
        assert_eq!(tasks[0].subject, "A");

        // 重复 seed：非空 store 是 no-op
        seed_forked_tasks(&mut st, "parent");
        assert_eq!(st.store.list(None).len(), 2);
    }

    /// `project`/`memory` 作用域不按会话分文件，fork 不继承。
    #[test]
    fn fork_seed_skips_non_session_scopes() {
        if std::env::var(ENV_TASKS)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            return;
        }

        let _g = AgentDirGuard::temp();
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();

        // 父会话文件存在，但 scope=project 时应被忽略
        let parent_path = paths::session_task_file(&cwd, "parent", TaskScope::Session);
        let mut parent = TaskStore::new(Some(parent_path));
        parent.create("A".into(), "d".into(), None, None).unwrap();

        let mut st = State {
            cwd: Some(cwd.clone()),
            config: TasksConfig {
                task_scope: Some(TaskScope::Project),
                ..Default::default()
            },
            store: TaskStore::new(None),
            ..Default::default()
        };

        seed_forked_tasks(&mut st, "parent");
        assert!(st.store.list(None).is_empty(), "project 作用域不继承");
    }

    /// `/tasks create <subject> :: <description>` 经命令入口直接建任务（不弹输入面板）。
    #[test]
    fn command_create_builds_task_with_description() {
        let _g = AgentDirGuard::temp();
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();

        {
            let mut st = lock_state();
            st.cwd = Some(dir.path().to_string_lossy().to_string());
            st.session_id = None; // 无会话 id → 内存 store，不落盘
            st.store = TaskStore::new(None);
            st.store_key = "memory:test".into();
            st.config = TasksConfig::default();
        }

        let mut app = App::new();
        assert!(!command_tasks(
            &mut app,
            "tasks create Fix login :: Handle OAuth"
        ));

        let st = lock_state();
        let tasks = st.store.list_cached(None);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].subject, "Fix login");
        assert_eq!(tasks[0].description, "Handle OAuth");
    }

    /// `/new`（无消息、非 fork）：`memory` 作用域没有会话文件可切，
    /// 旧任务、活跃指针与 dock 内容必须一起清掉（对照 subagent 切会话的 `reset_all`）。
    #[test]
    fn new_session_clears_memory_scope_dock() {
        if std::env::var(ENV_TASKS)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            return;
        }

        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while crate::core::extensions::take_pending_ui().is_some() {}

        {
            let mut st = lock_state();
            st.cwd = Some("/tmp/tasks-new-session".to_string());
            st.session_id = Some("old".to_string());
            st.config = TasksConfig {
                task_scope: Some(TaskScope::Memory),
                ..Default::default()
            };
            st.store = TaskStore::new(None);
            st.store_key = "memory:config".to_string();
            st.store
                .create("stale".into(), "d".into(), None, None)
                .unwrap();
            st.widget.set_active("1", true);
        }

        Tasks.on_session_switched(None, &[]);

        {
            let st = lock_state();
            assert!(
                st.store.list_cached(None).is_empty(),
                "新会话不应带着上一个会话的任务"
            );
            assert!(!st.widget.has_active(), "切会话清掉活跃任务指针");
        }
        assert!(
            Tasks.dock_lines().is_empty(),
            "dock 内容清空（面板随之自动收起）"
        );
    }

    /// 切到新会话不主动弹面板；恢复会话才把持久化任务重新摆出来。
    /// `project` 列表跨会话共享：数据保留，但不属于刚切过来的这个会话。
    #[test]
    fn session_switch_only_reopens_the_panel_on_resume() {
        if std::env::var(ENV_TASKS)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            return;
        }

        let _g = AgentDirGuard::temp();
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while crate::core::extensions::take_pending_ui().is_some() {}

        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let path = paths::project_task_file(&cwd);

        {
            let mut st = lock_state();
            st.cwd = Some(cwd);
            st.session_id = Some("old".to_string());
            st.config = TasksConfig {
                task_scope: Some(TaskScope::Project),
                ..Default::default()
            };
            st.store = TaskStore::new(Some(path.clone()));
            st.store_key = format!("path:{}", path.display());
            st.store
                .create("shared".into(), "d".into(), None, None)
                .unwrap();
        }

        // 新会话：不弹面板
        Tasks.on_session_switched(None, &[]);
        assert!(
            crate::core::extensions::take_pending_ui().is_none(),
            "新会话不应主动打开停靠面板"
        );
        {
            let mut st = lock_state();
            assert_eq!(st.store.list(None).len(), 1, "project 列表跨会话保留");
        }

        // 恢复会话：持久化任务重新展示 → 请求打开面板
        Tasks.on_session_switched(None, &[AgentMessage::user_text("hi")]);
        let mut saw_dock = false;
        while let Some(req) = crate::core::extensions::take_pending_ui() {
            if matches!(req, crate::core::extensions::ExtensionUiRequest::ShowDock) {
                saw_dock = true;
            }
        }
        assert!(saw_dock, "恢复会话应请求打开停靠面板");
    }

    /// `-c` / `/resume` 重新打开一个「任务已全部完成」的会话：完成项是上次留下的残影，
    /// 不请求弹面板，dock 里也不再出现。
    #[test]
    fn resume_with_all_completed_keeps_dock_clean() {
        if std::env::var(ENV_TASKS)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            return;
        }

        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while crate::core::extensions::take_pending_ui().is_some() {}

        {
            let mut st = lock_state();
            st.cwd = Some("/tmp/tasks-resume-all-done".to_string());
            st.session_id = None;
            st.config = TasksConfig {
                task_scope: Some(TaskScope::Memory),
                ..Default::default()
            };
            st.store = TaskStore::new(None);
            st.store_key = "memory:config".to_string();
            st.persisted_shown = false;
            st.auto_hide.reset();
            st.store
                .create("done".into(), "d".into(), None, None)
                .unwrap();
            st.store
                .update(
                    "1",
                    super::types::TaskUpdateFields {
                        status: Some(super::types::TaskStatusOrDeleted::Status(
                            TaskStatus::Completed,
                        )),
                        ..Default::default()
                    },
                )
                .unwrap();

            after_session_sync(&mut st, true);
            let tasks = st.store.list(None);
            assert!(
                st.auto_hide.hides(&tasks[0]),
                "全完成列表恢复会话应直接隐藏"
            );
        }

        assert!(
            crate::core::extensions::take_pending_ui().is_none(),
            "全完成列表恢复会话不应请求打开停靠面板"
        );
        assert!(
            Tasks.dock_lines().is_empty(),
            "全完成列表恢复后 dock 不应出现任务行"
        );
    }

    /// §3.15：提醒经 `ContextWithSystem` 注入——只 append 一条 user 消息，
    /// `<system-reminder>` 标签原样保留，既有 transcript 一字不改。
    ///
    /// 这同时证明提醒不会进入 UI 渲染管线：`agent.messages` 不含它，dock/消息区
    /// 也不会把尖括号内容当作 HTML 或正文展示。
    #[test]
    fn reminder_injection_appends_without_mutating_transcript() {
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        {
            let mut st = lock_state();
            st.store = TaskStore::new(None);
            st.store
                .create("Report bug".into(), "d".into(), None, None)
                .unwrap();
            st.cadence = CadenceState::default();
            st.cadence.reminder_due = true;
        }

        let snapshot = |m: &AgentMessage| (m.role.clone(), m.text());

        let mut system = AgentMessage::user_text("# sys");
        system.role = "system".to_string();
        let user = AgentMessage::user_text("hello");
        let before = vec![snapshot(&system), snapshot(&user)];
        let mut full = vec![system, user];

        Tasks.transform_context_with_system(&mut full).unwrap();

        assert_eq!(full.len(), before.len() + 1, "只 append 一条提醒");
        let after: Vec<_> = full.iter().map(snapshot).collect();
        assert_eq!(
            &after[..before.len()],
            before.as_slice(),
            "既有消息一字不动"
        );

        let reminder = full.last().unwrap();
        assert_eq!(reminder.role, "user");
        let text = reminder.text();
        assert!(text.starts_with("<system-reminder>"), "{text}");
        assert!(text.trim_end().ends_with("</system-reminder>"), "{text}");
        assert!(text.contains("Report bug"), "含任务回显: {text}");

        // drain 已清：再次调用不再注入（不重复累积）
        Tasks.transform_context_with_system(&mut full).unwrap();
        assert_eq!(full.len(), before.len() + 1);
    }
}
