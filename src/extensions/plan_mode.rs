//! `plan-mode` 扩展（只读探索模式）。
//!
//! **默认不启用**：注册后为禁用态，工具过滤、`/plan` `/todos` 命令、`Alt+P`、`--plan`
//! flag 均不生效；需在 `/extension` 面板开启（写 settings.json 的 `enabledExtensions`）。
//!
//! 功能入口：
//! - CLI flag：`--plan`（[`Extension::cli_flags`]） + [`Extension::apply_cli_flag`]；
//! - 快捷键：`Alt+P`（[`Extension::keybindings`]） + [`register_extension_keybinding`]；
//! - 斜杠命令：`/plan [text]`、`/todos`（[`Extension::commands`]）；`/todos` 仅在存在
//!   todo 任务时出现（与 [`register_slash_command`] 的 `/plan` 入口一起注册/分发）；
//! - 工具/上下文：`filter_tools` / `before_tool_call` / `transform_context`；
//! - UI 交互：扩展发起 core UI 请求（[`request_ui`]），[`Extension::on_ui_choice`]
//!   接收面板选择、[`Extension::on_user_prompt`] 接收输入。
//!
//! 行为：
//! - 进入 plan mode 后 `edit`/`write` 从工具表移除（[`Extension::filter_tools`]），
//!   bash 调用按只读白名单拦截（[`Extension::before_tool_call`]），并向上下文注入提示
//!   （提示 LLM 输出 `Plan:` 编号步骤列表）。
//! - 执行阶段（选择 Execute 后）按 `[DONE:n]` 标记或工具调用自动推进步骤；
//!   `agent_end` 后请求 TUI 选择面板（执行计划/回复精炼/停留）进行处理；步骤标记回写
//!   停靠面板与 `/todos` 列表。
//! - 会话持久化：状态写入会话文件的自定义条目 `Entry::Custom`（customType=`plan-mode`），
//!   不经全局 `plan-mode.json`（多会话/多目录不再互相覆盖）。启动/恢复
//!   （[`Extension::on_session_start`]）与会话切换（[`Extension::on_session_switched`]）
//!   从会话自定义条目 + 消息历史重建 todo（含 `[DONE:n]` 完成态重扫，崩溃恢复）；
//!   resume（`-c` / `--session` / `--resume` / `--session-id` / `--fork`）时
//!   未完成 todo 自动显示停靠面板。
//! - 广播 `plan-mode:changed` 事件（rich footer 显示 📋 plan 状态）。

use crate::{
    core::{
        self,
        extensions::{
            CliFlagDef, DockLine, DockSpan, Extension, ExtensionCommand, ExtensionHook,
            ExtensionKeybinding, ExtensionMode, ExtensionTool, ExtensionUiRequest, RichSpan,
            SelectOption, UiNotifyLevel, UserPromptAction, request_show_dock, request_ui,
        },
        provider::AgentMessage,
        session_manager::Session,
        tools::ToolError,
    },
    error::Result,
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_PLAN_MODE, command_arg},
    modes::interactive::{
        agent_actor::AgentCommand,
        app::{App, MsgLevel, SysSpan},
        handlers::{register_extension_keybinding, register_slash_command},
    },
    utils::glyphs::{DEF_CHECKBOX_EMPTY, DEF_DONE},
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// 会话自定义条目类型标签
const CUSTOM_TYPE: &str = "plan-mode";

/// plan 模式提示的起始标记，用于识别上下文中已注入的只读模式提示。
const PLAN_MODE_MARKER: &str = "[PLAN MODE ACTIVE]";
/// 执行模式提示的起始标记，用于识别已注入的执行阶段提示。
const EXECUTING_MARKER: &str = "[EXECUTING PLAN";
/// 进入 plan 模式时注入的提示词：声明只读限制并要求输出编号计划。
const PLAN_MODE_PROMPT: &str = "[PLAN MODE ACTIVE]
You are in plan mode - a read-only exploration mode for safe code analysis.

Restrictions:
- Built-in edit and write tools are disabled
- Bash is restricted to an allowlist of read-only commands
- External tools (MCP, extensions, etc.) remain available

Create a detailed numbered plan under a \"Plan:\" header:

Plan:
1. First step description
2. Second step description
...

Do NOT attempt to make changes - just describe what you would do.";

/// 执行阶段提示的前半段：声明已放开全部工具并引出剩余步骤列表。
const EXEC_MODE_PROMPT_HEAD: &str = "[EXECUTING PLAN - Full tool access enabled]

Remaining steps:
";

/// 执行阶段提示的后半段：要求按序执行步骤并回报 `[DONE:n]` 标记。
const EXEC_MODE_PROMPT_TAIL: &str = "
Execute each step in order.
After completing a step, include a [DONE:n] tag in your response.
Steps are also auto-completed after each turn with tool calls.";

/// 用户再次催促执行时的提示前半段：重新引出剩余步骤列表。
const EXECUTE_FOLLOWUP_HEAD: &str = "Execute the plan.

Remaining steps:
";

/// 用户再次催促执行时的提示后半段：重申 `[DONE:n]` 回报要求。
const EXECUTE_FOLLOWUP_TAIL: &str = "
After completing a step, include a [DONE:n] tag in your response.
Steps are also auto-completed after each turn with tool calls.";

/// 破坏性命令模式：命中即拦截（不区分位置、大小写不敏感）
const DESTRUCTIVE_PATTERNS: &[&str] = &[
    r"\brm\b",
    r"\brmdir\b",
    r"\bmv\b",
    r"\bcp\b",
    r"\bmkdir\b",
    r"\btouch\b",
    r"\bchmod\b",
    r"\bchown\b",
    r"\bchgrp\b",
    r"\bln\b",
    r"\btee\b",
    r"\btruncate\b",
    r"\bdd\b",
    r"\bshred\b",
    r"(?:^|[^<])>([^>]|$)",
    r">>",
    r"\bnpm\s+(install|uninstall|update|ci|link|publish)",
    r"\byarn\s+(add|remove|install|publish)",
    r"\bpnpm\s+(add|remove|install|publish)",
    r"\bpip\s+(install|uninstall)",
    r"\bapt(-get)?\s+(install|remove|purge|update|upgrade)",
    r"\bbrew\s+(install|uninstall|upgrade)",
    r"\bgit\s+(add|commit|push|pull|merge|rebase|reset|checkout|branch\s+-[dD]|stash|cherry-pick|revert|tag|init|clone)",
    r"\bsudo\b",
    r"\bsu\b",
    r"\bkill\b",
    r"\bpkill\b",
    r"\bkillall\b",
    r"\breboot\b",
    r"\bshutdown\b",
    r"\bsystemctl\s+(start|stop|restart|enable|disable)",
    r"\bservice\s+\S+\s+(start|stop|restart)",
    r"\b(vim?|nano|emacs|code|subl)\b",
];

/// 安全只读命令模式：行首前缀匹配（大小写不敏感）
const SAFE_PATTERNS: &[&str] = &[
    r"^\s*cat\b",
    r"^\s*head\b",
    r"^\s*tail\b",
    r"^\s*less\b",
    r"^\s*more\b",
    r"^\s*grep\b",
    r"^\s*find\b",
    r"^\s*ls\b",
    r"^\s*pwd\b",
    r"^\s*echo\b",
    r"^\s*printf\b",
    r"^\s*wc\b",
    r"^\s*sort\b",
    r"^\s*uniq\b",
    r"^\s*diff\b",
    r"^\s*file\b",
    r"^\s*stat\b",
    r"^\s*du\b",
    r"^\s*df\b",
    r"^\s*tree\b",
    r"^\s*which\b",
    r"^\s*whereis\b",
    r"^\s*type\b",
    r"^\s*env\b",
    r"^\s*printenv\b",
    r"^\s*uname\b",
    r"^\s*whoami\b",
    r"^\s*id\b",
    r"^\s*date\b",
    r"^\s*cal\b",
    r"^\s*uptime\b",
    r"^\s*ps\b",
    r"^\s*top\b",
    r"^\s*htop\b",
    r"^\s*free\b",
    r"^\s*git\s+(status|log|diff|show|branch|remote|config\s+--get)",
    r"^\s*git\s+ls-",
    r"^\s*npm\s+(list|ls|view|info|search|outdated|audit)",
    r"^\s*yarn\s+(list|info|why|audit)",
    r"^\s*node\s+--version",
    r"^\s*python\s+--version",
    r"^\s*curl\s",
    r"^\s*wget\s+-O\s*-",
    r"^\s*jq\b",
    r"^\s*sed\s+-n",
    r"^\s*awk\b",
    r"^\s*rg\b",
    r"^\s*fd\b",
    r"^\s*bat\b",
    r"^\s*eza\b",
];

/// 自声明工厂：linkme 分布式切片，priority 值大者先注册（见 [`crate::extensions`]）。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static PLAN_MODE_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_PLAN_MODE,
    make: || Arc::new(PlanMode::new()),
};

/// 把命令模式串批量编译为大小写不敏感（`(?i)`）的正则；模式串均为内建常量，编译失败即 panic。
fn compile_patterns(patterns: &[&str]) -> Vec<Regex> {
    patterns
        .iter()
        .map(|p| Regex::new(&format!("(?i){p}")).expect("plan-mode pattern"))
        .collect()
}

/// 破坏性命令正则表（惰性编译一次并全局缓存，避免每次检查重复编译）。
fn destructive_patterns() -> &'static Vec<Regex> {
    /// 缓存编译后的破坏性命令正则，避免每次检查重复编译。
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| compile_patterns(DESTRUCTIVE_PATTERNS))
}

/// 只读命令白名单正则表（惰性编译一次并全局缓存，避免每次检查重复编译）。
fn safe_patterns() -> &'static Vec<Regex> {
    /// 缓存编译后的只读白名单正则，避免每次检查重复编译。
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| compile_patterns(SAFE_PATTERNS))
}

/// plan 模式下 bash 是否放行：未命中破坏性模式、且命中只读白名单。
pub fn is_safe_command(command: &str) -> bool {
    let is_destructive = destructive_patterns().iter().any(|p| p.is_match(command));
    let is_safe = safe_patterns().iter().any(|p| p.is_match(command));
    !is_destructive && is_safe
}

/// 计划列表项
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TodoItem {
    /// 步骤序号，从 1 开始且与列表顺序一致。
    pub step: usize,
    /// 清洗后的步骤文本（去格式、去动词前缀并截断 50 字符）。
    pub text: String,
    /// 是否已完成，由模型输出的 `[DONE:n]` 标记置位。
    pub completed: bool,
}

/// 步骤文本清洗：去粗体/行内代码、去动词前缀、压缩空白、首字母大写、截断 50 字符。
pub fn clean_step_text(text: &str) -> String {
    let bold = Regex::new(r"\*{1,2}([^*]+)\*{1,2}").expect("bold");
    let code = Regex::new(r"`([^`]+)`").expect("code");
    let verb = Regex::new(
        r"^(Use|Run|Execute|Create|Write|Read|Check|Verify|Update|Modify|Add|Remove|Delete|Install)\s+(the\s+)?",
    )
    .expect("verb");
    let ws = Regex::new(r"\s+").expect("ws");

    let mut cleaned = bold.replace_all(text, "$1").to_string();
    cleaned = code.replace_all(&cleaned, "$1").to_string();
    cleaned = verb.replace_all(&cleaned, "").to_string();
    cleaned = ws.replace_all(&cleaned, " ").to_string();
    let cleaned = cleaned.trim().to_string();

    let mut cleaned = if cleaned.is_empty() {
        cleaned
    } else {
        let mut chars = cleaned.chars();
        match chars.next() {
            Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
            None => cleaned,
        }
    };

    if cleaned.chars().count() > 50 {
        let truncated: String = cleaned.chars().take(47).collect();
        cleaned = format!("{truncated}...");
    }
    cleaned
}

/// 从消息的 `Plan:` 部分提取编号步骤。
pub fn extract_todo_items(message: &str) -> Vec<TodoItem> {
    let mut items = Vec::new();
    let header = Regex::new(r"(?i)\*{0,2}Plan:\*{0,2}\s*\n").expect("header");
    let Some(hit) = header.find(message) else {
        return items;
    };
    let plan_section = &message[hit.end()..];
    let numbered = Regex::new(r"(?m)^\s*(\d+)[.)]\s+([^\n]+)").expect("numbered"); // (?m) 使 ^ 匹配每行行首

    for cap in numbered.captures_iter(plan_section) {
        let text = cap
            .get(2)
            .map(|m| m.as_str().trim().trim_end_matches(['*']).trim().to_string())
            .unwrap_or_default();
        if text.len() > 5
            && !text.starts_with('`')
            && !text.starts_with('/')
            && !text.starts_with('-')
        {
            let cleaned = clean_step_text(&text);
            if cleaned.chars().count() > 3 {
                items.push(TodoItem {
                    step: items.len() + 1,
                    text: cleaned,
                    completed: false,
                });
            }
        }
    }
    items
}

/// 提取消息中的 `[DONE:n]` 标记。
pub fn extract_done_steps(message: &str) -> Vec<usize> {
    let done = Regex::new(r"(?i)\[DONE:(\d+)\]").expect("done");
    done.captures_iter(message)
        .filter_map(|c| c.get(1).and_then(|m| m.as_str().parse().ok()))
        .collect()
}

/// 按 `[DONE:n]` 标记完成对应步骤，返回标记数量。
pub fn mark_completed_steps(text: &str, items: &mut [TodoItem]) -> usize {
    let done_steps = extract_done_steps(text);
    for step in &done_steps {
        if let Some(item) = items.iter_mut().find(|t| t.step == *step) {
            item.completed = true;
        }
    }
    done_steps.len()
}

/// 把未完成的步骤拼成 `序号. 文本` 的多行文本（供执行模式提示注入）。
fn remaining_todo_lines(todos: &[TodoItem]) -> String {
    todos
        .iter()
        .filter(|t| !t.completed)
        .map(|t| format!("{}. {}", t.step, t.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 渲染全部步骤为带已完成/未完成标记的多行列表（供精炼提示展示完整计划）。
fn todo_list_display(todos: &[TodoItem]) -> String {
    todos
        .iter()
        .map(|t| {
            format!(
                "{}. {} {}",
                t.step,
                if t.completed {
                    DEF_DONE
                } else {
                    DEF_CHECKBOX_EMPTY
                },
                t.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 内存态（plan mode 开关 / 执行模式 / 步骤 / 精炼待定 / 面板请求 id）
#[derive(Debug, Default)]
struct State {
    /// plan mode 是否已开启（工具过滤与上下文注入据此生效）。
    enabled: bool,
    /// 是否处于计划执行阶段（开启后按标记逐条推进 todo）。
    executing: bool,
    /// 当前解析出的计划步骤列表。
    todos: Vec<TodoItem>,
    /// 用户已选 Refine、正等待输入精炼文本以改写下一步 prompt。
    refine_pending: bool,
    /// 当前 Select 面板请求 id（on_ui_choice 按 id 校验，防旧面板误触发）
    select_id: Option<u64>,
    /// 停靠段是否可见（/todos 或 ShowDock 控制；false 时 dock_lines 返回空）
    dock_shown: bool,
    /// 会话持久化（合并写入）：待写入的载荷
    dirty_payload: Option<Value>,
    /// 是否有在途写入（extension:entry_persisted 回执后清，期间新写入合并）
    write_in_flight: bool,
    /// 离开 plan/exec 模式后仍需再跑一次 transform_context 以清除旧注入（one-shot）。
    /// 框架按 hooks() 实时门控，故此标志为真时仍订阅 TransformContext；清理后清位即退订。
    cleanup_pending: bool,
}

/// 持久化形态（写入会话文件的自定义条目 `Entry::Custom`；
/// 不经全局文件，因此多个不同目录/会话的 todo 计划互不覆盖）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedState {
    /// 载荷版本（未来字段演进时用于迁移判断）
    #[serde(default = "persist_version")]
    version: u32,
    /// 持久化时的 plan mode 开关状态。
    enabled: bool,
    /// 持久化时的执行阶段状态。
    executing: bool,
    /// 持久化的计划步骤列表（含完成态）。
    todos: Vec<TodoItem>,
}

/// `PersistedState::version` 的 serde 默认值（旧条目缺该字段时按 1 处理）。
fn persist_version() -> u32 {
    1
}

/// state() 锁的毒化恢复包装：任何线程在持锁时 panic 都会毒化 std Mutex，
/// 直接 unwrap 会让后续全部调用/测试级联失败（表现为"锁死"）。
/// 恢复后继续（快速失败、集中恢复）。测试用独立临时 agent_dir（线程本地 override）
/// 处理一致；测试用例各自使用独立 tempdir，不共用文件，因此无需跨模块共享文件锁。
#[derive(Default)]
struct StateLock(Mutex<State>);

impl StateLock {
    /// 获取状态锁；遇毒化（持锁线程 panic）时取回内部值继续，避免后续调用级联失败。
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 进程级全局 plan mode 状态容器（首次访问时初始化）。
fn state() -> &'static StateLock {
    /// 进程级全局 plan mode 状态容器，首次访问时初始化。
    static S: OnceLock<StateLock> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// 单一"分发兴趣"谓词：框架在分发每个回调前按 [`Extension::hooks`] 门控，
/// 这里集中判定当前状态关心哪些回调；空闲时返回空，框架便**完全跳过**本扩展
/// （不再逐轮调用 transform_context、不再逐事件调用 on_agent_event、不再逐帧采集 dock）。
///
/// 采用**实时求值**（不缓存掩码）：`hooks()` 每次按当前状态计算，状态迁移无需额外
/// 刷新调用，杜绝"忘记刷新 → 静默失联"的一整类缺陷；代价是每次分发一次廉价状态读锁。
fn interest_for(st: &State) -> Vec<ExtensionHook> {
    let mut out = Vec::new();

    // 注入提示：plan/exec 进行中；或刚退出时的一次性清理回合（见 cleanup_pending）
    if st.enabled || st.executing || st.cleanup_pending {
        out.push(ExtensionHook::TransformContext);
    }

    // 只读拦截：仅 plan 模式开启时
    if st.enabled {
        out.push(ExtensionHook::BeforeToolCall);
    }

    // 停靠面板：有可见的 todo 段时才采集
    if st.dock_shown && !st.todos.is_empty() {
        out.push(ExtensionHook::Dock);
    }

    // 输入拦截：仅精炼待提交时需要
    if st.refine_pending {
        out.push(ExtensionHook::UserPrompt);
    }

    // agent 事件：任何进行中/待处理态，或**持久化在途**
    // （在途时必须继续收 entry_persisted 回执，否则 write_in_flight 永久卡死、后续写入被吞）
    if st.enabled
        || st.executing
        || !st.todos.is_empty()
        || st.refine_pending
        || st.select_id.is_some()
        || st.write_in_flight
        || st.dirty_payload.is_some()
    {
        out.push(ExtensionHook::AgentEvent);
    }

    out
}

/// 序列化当前内存态为持久化载荷（自锁取值，返回时锁已释放）
fn persist_value() -> Option<Value> {
    let st = state().lock();
    serde_json::to_value(PersistedState {
        version: persist_version(),
        enabled: st.enabled,
        executing: st.executing,
        todos: st.todos.clone(),
    })
    .ok()
}

/// 合并写入：只保留最新载荷；无在途写入时入队一次 PersistSessionEntry。
/// worker 空闲时追加到当前会话，经 `extension:entry_persisted` 回执继续后续写入。
/// 写入一经派发即取走脏载荷（dirty_payload 置空）；在途期间再调 persist() 会重新置脏，
/// 回执时若仍有脏载荷才继续下一次写入。
fn persist() {
    let Some(payload) = persist_value() else {
        return;
    };

    let mut st = state().lock();
    st.dirty_payload = Some(payload);

    if !st.write_in_flight {
        st.write_in_flight = true;
        // 取走载荷：派发即视为已消费；若不清空，回执钩子会误判"有新写入"而再次派发，
        // 形成 写入→回执→写入 死循环（会话文件被刷屏）
        let data = st.dirty_payload.take().unwrap_or(Value::Null);
        core::extensions::request_ui(ExtensionUiRequest::PersistSessionEntry {
            custom_type: CUSTOM_TYPE.to_string(),
            data,
        });
    }
}

/// `extension:entry_persisted` 回执：清在途标记；期间有新写入则继续下一次
/// （取走脏载荷：仅当 persist() 确实在在途期间再次置脏才继续写入，避免回声循环）。
fn on_persisted(custom_type: &str) {
    if custom_type != CUSTOM_TYPE {
        return;
    }

    let mut st = state().lock();
    st.write_in_flight = false;

    if let Some(data) = st.dirty_payload.take() {
        st.write_in_flight = true;
        core::extensions::request_ui(ExtensionUiRequest::PersistSessionEntry {
            custom_type: CUSTOM_TYPE.to_string(),
            data,
        });
    }
}

/// 从会话自定义条目中还原最近一条 plan-mode 状态。
fn latest_from_session(session: Option<&Session>) -> Option<PersistedState> {
    for v in session?.get_entries().iter().rev() {
        if v.get("customType").and_then(|c| c.as_str()) != Some(CUSTOM_TYPE) {
            continue;
        }

        if let Some(data) = v.get("data")
            && let Ok(p) = serde_json::from_value::<PersistedState>(data.clone())
        {
            return Some(p);
        }
    }
    None
}

/// 从会话历史重建 todo（无持久化条目时的兜底，兼容旧版本会话/丢失写入）：
/// 取最后一条含 `Plan:` 的助手消息提取步骤，再按全历史 `[DONE:n]` 标记完成态。
fn todos_from_history(messages: &[AgentMessage]) -> Vec<TodoItem> {
    let mut todos = Vec::new();
    for m in messages.iter().rev() {
        if m.role != "assistant" {
            continue;
        }
        let extracted = extract_todo_items(&m.text());
        if !extracted.is_empty() {
            todos = extracted;
            break;
        }
    }

    let all = messages
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n");

    mark_completed_steps(&all, &mut todos);
    todos
}

/// 会话恢复（启动 `on_session_start` 与会话切换 `on_session_switched` 共用）：
/// 1) 优先取会话内最新 `plan-mode` 自定义条目（结构化状态）；
/// 2) 无条目时从历史助手消息重建 todo（兜底）；执行态置为"进行中"以便继续推进；
///    无 todo 则保留内存现值（如 `--plan` CLI flag 已开启的会话）；
/// 3) 全历史 `[DONE:n]` 重扫修正完成态（崩溃恢复：最后一次持久化之后的完成）；
/// 4) 含未完成 todo → 自动显示停靠面板（resume 语义；恢复内容经面板展示，
///    不再另发消息区通知，避免重复）。
///
/// `request_rebuild` 为真时额外请求重建工具集：仅会话切换需要
/// （启动路径 main 在本钩子后统一 rebuild_tools，重复请求会在启动时
/// 弹出多余的 "extensions updated: tools rebuilt"）。
fn restore_from_session(
    session: Option<&Session>,
    messages: &[AgentMessage],
    request_rebuild: bool,
) {
    let persisted = latest_from_session(session);
    let mut restored = false;

    {
        let mut st = state().lock();
        match persisted {
            Some(p) => {
                st.enabled = p.enabled;
                st.executing = p.executing;
                st.todos = p.todos;
                restored = true;
            }
            None => {
                let todos = todos_from_history(messages);
                if !todos.is_empty() {
                    // 无条目但历史里有计划 → 视为执行中（便于恢复后自动展示与继续推进）
                    st.executing = true;
                    st.enabled = false;
                    st.todos = todos;
                    restored = true;
                }
                // todos 为空（全新会话/已完成会话）：enabled/executing 保留内存现值
            }
        }
        st.dock_shown = false;
        st.refine_pending = false;
        st.select_id = None;
    }

    // 全历史 [DONE:n] 重扫（条目或历史兜底都以消息为准修正完成态）；
    // 仅当重扫实际推进了完成态（崩溃恢复：最后一次持久化之后又完成了步骤）才写回会话，
    // 避免每次 session 启动/切换都向会话文件写一条相同内容。
    let rescan_changed = {
        let all = messages
            .iter()
            .map(|m| m.text())
            .collect::<Vec<_>>()
            .join("\n");
        let mut st = state().lock();
        let before: Vec<bool> = st.todos.iter().map(|t| t.completed).collect();
        mark_completed_steps(&all, &mut st.todos);
        before != st.todos.iter().map(|t| t.completed).collect::<Vec<_>>()
    };

    let pending = state().lock().todos.iter().filter(|t| !t.completed).count();

    if restored && rescan_changed {
        // 已恢复的状态且 [DONE:n] 修正了完成态：写回会话保证下次恢复一致；
        // 其余情况内容与上次落盘一致，不产生冗余写入
        persist();
    }

    if pending > 0 {
        state().lock().dock_shown = true;
        request_show_dock();
    }

    emit_plan_changed(is_enabled());

    if request_rebuild {
        request_ui(ExtensionUiRequest::RebuildTools);
    }
}

/// 广播 plan-mode:changed（rich footer 显示 📋 plan；值变化才发）
fn emit_plan_changed(enabled: bool) {
    /// 上次广播的开关值，用于去重、仅在状态变化时发事件。
    static LAST: OnceLock<Mutex<Option<bool>>> = OnceLock::new();
    let mut last = LAST.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if *last == Some(enabled) {
        return;
    }
    *last = Some(enabled);
    core::extensions::dispatch_agent_event(
        &serde_json::json!({ "type": "plan-mode:changed", "enabled": enabled }),
    );
}

/// 切换 plan mode，返回新状态（调用方负责 rebuild_tools 与提示；仅命令/快捷键触发）。
/// 扩展被禁用时不切换（filter_tools 不生效，避免假只读）。
pub fn toggle_enabled() -> bool {
    if !core::extensions::is_extension_enabled("plan-mode") {
        return is_enabled();
    }

    let mut st = state().lock();
    st.enabled = !st.enabled;
    if !st.enabled {
        // 关闭时清执行模式与步骤
        st.executing = false;
        st.todos.clear();
        st.refine_pending = false;
        st.select_id = None;
        // 再跑一次 transform_context 清除已注入的 plan 标记（one-shot）
        st.cleanup_pending = true;
    }
    let enabled = st.enabled;
    drop(st);

    emit_plan_changed(enabled);
    persist();
    enabled
}

/// 直接设置 plan mode 开关（--plan 经 apply_cli_flag 调用）。开启时若扩展被禁用则忽略
/// （未启用扩展的 flag 不注入，此守卫为防御性兜底）；关闭总是允许。
pub fn set_enabled(enabled: bool) {
    if enabled && !core::extensions::is_extension_enabled("plan-mode") {
        return;
    }

    let mut st = state().lock();
    if st.enabled == enabled {
        return;
    }

    st.enabled = enabled;
    if !enabled {
        st.executing = false;
        st.todos.clear();
        st.refine_pending = false;
        st.select_id = None;
        // 再跑一次 transform_context 清除已注入的 plan 标记（one-shot）
        st.cleanup_pending = true;
    }
    drop(st);

    emit_plan_changed(enabled);
    persist();
}

/// 当前 plan mode 是否开启
pub fn is_enabled() -> bool {
    state().lock().enabled
}

/// 切换 plan-mode 的 UI 入口（/plan 无参命令与 Alt+P 快捷键共用）：
/// rebuild 工具列表使过滤立即生效。扩展被禁用时不切换、不提示
/// （命令/快捷键分发已按 enabled 过滤；这里兜底）。
fn toggle_plan_ui(st: &mut App) {
    if !core::extensions::is_extension_enabled("plan-mode") {
        return;
    }

    let enabled = toggle_enabled();
    apply_plan_mode_ui(st, enabled);
}

/// 确保进入 plan mode（已开启则不动）。
/// 扩展被禁用时不动作（同 [`toggle_plan_ui`] 的兜底）。
fn enable_plan_ui(st: &mut App) {
    if !core::extensions::is_extension_enabled("plan-mode") || is_enabled() {
        return;
    }

    set_enabled(true);
    apply_plan_mode_ui(st, true);
}

/// 开关生效后的 TUI 侧同步：rebuild 工具表 + 提示（开/关共用）
fn apply_plan_mode_ui(st: &mut App, enabled: bool) {
    st.worker.send(AgentCommand::RebuildTools);
    st.push_msg(
        if enabled {
            "Plan mode enabled. Built-in write tools disabled.".to_string()
        } else {
            "Plan mode disabled. Full access restored.".to_string()
        },
        MsgLevel::Info,
    );
}

/// /todos 展示片段（富文本）：已完成步骤的 ✓ 用 success 色渲染、文本用 muted 色；
/// 未完成保持默认（info 级）色。标题只出现一次。
pub fn todo_list_spans() -> Option<Vec<SysSpan>> {
    let st = state().lock();
    if st.todos.is_empty() {
        return None;
    }

    let mut spans = vec![SysSpan::plain("Plan Progress:\n")];
    for item in &st.todos {
        // 与既有纯文本格式一致：`step. ✓ text`；已完成 ✓ 用 success 色、
        // 文本用 muted 色，未完成保持默认（info 级）色
        spans.push(SysSpan::plain(&format!("{}. ", item.step)));
        if item.completed {
            spans.push(SysSpan::fg("success", &format!("{DEF_DONE} ")));
            spans.push(SysSpan::fg("muted", &format!("{}\n", item.text)));
        } else {
            spans.push(SysSpan::plain(&format!("{DEF_CHECKBOX_EMPTY} ")));
            spans.push(SysSpan::plain(&format!("{}\n", item.text)));
        }
    }
    Some(spans)
}

/// 停靠段是否可见（/todos 或 ShowDock 置位；false 时 `dock_lines` 返回空）
pub fn is_dock_shown() -> bool {
    state().lock().dock_shown
}

/// 显示 plan-mode 的停靠段（/todos、ShowDock 发送方用）
pub fn show_dock_section() {
    state().lock().dock_shown = true;
}

/// 隐藏 plan-mode 的停靠段（会话切换等场景下 TUI 主动关闭；内容随之不提供）
pub fn hide_dock_section() {
    state().lock().dock_shown = false;
}

/// 当前 todo 条数（停靠面板 /todos 开关判断用）
fn todo_count() -> usize {
    state().lock().todos.len()
}

/// `plan-mode` 扩展入口（main.rs 注册）。
#[derive(Default)]
pub struct PlanMode;

impl PlanMode {
    /// 构造无状态扩展实例（状态集中在全局单例 `state()` 中）。
    pub fn new() -> Self {
        Self
    }
}

/// 幂等注册（测试/嵌入场景用）：重复调用只注册一次，避免全局注册表出现重复条目。
/// 命令/快捷键执行入口由 [`Extension::on_registered`] 自动接线。
pub fn ensure_registered() {
    /// 保证扩展只注册一次的进程级 Once 标记。
    static REG: std::sync::Once = std::sync::Once::new();
    REG.call_once(|| {
        core::extensions::register_extension(PlanMode::new());
    });
}

impl Extension for PlanMode {
    /// 返回扩展名 `plan-mode`（注册表键与诊断用）。
    fn name(&self) -> &str {
        "plan-mode"
    }

    /// 返回 /extension 面板展示的一句话说明：只读探索模式及其限制。
    fn description(&self) -> &str {
        "Read-only exploration mode: disables edit/write, restricts bash to read-only commands, tracks numbered Plan: steps with [DONE:n] progress."
    }

    /// 声明在 Dev 与 Creator 两种模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认关闭：plan mode 会改变工具集并注入提示，须由用户显式开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供扩展工具（本扩展只做工具过滤与上下文/事件钩子）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    // 与 filter_tools 同款「调用时按当前状态动态决定」：无 todo 任务时不暴露
    // `/todos` —— 命令候选（自动补全/菜单）、busy_safe 判定、分发
    // （command_provider）全部经 commands() 查询，空列表时 `/todos` 按未知
    // 命令处理（普通用户输入），避免无计划时误触发停靠面板切换。
    //
    // NOTE(残留轮询面)：`commands()` 与 `on_user_prompt` 仍是"扩展侧动态声明 +
    // 框架侧每次查询"的同源模式（registered_commands() 会对每个扩展调用
    // commands()）。它们不像 transform_context / on_agent_event / dock_lines 那样
    // 高频（仅命令候选/回车时），暂不纳入兴趣门控，留待后续专项用同一套
    // "兴趣/脏标记"思路收敛。
    /// 声明 `/plan`；仅当已存在 todo 时额外声明只读的 `/todos`（无计划时不暴露以免误触发）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        let mut commands = vec![ExtensionCommand {
            name: "plan".to_string(),
            description: "Toggle plan mode, or enter it with an initial prompt: /plan [text]"
                .to_string(),
            busy_safe: false, // toggle_plan_ui 会 agent.lock() 切换工具集，忙碌时不可执行
            subcommands: Vec::new(),
        }];

        if todo_count() > 0 {
            commands.push(ExtensionCommand {
                name: "todos".to_string(),
                description: "Show current plan todo list".to_string(),
                busy_safe: true, // 只读视图：handler 不锁 agent，忙碌时可立即查看
                subcommands: Vec::new(),
            });
        }
        commands
    }

    /// 声明 `--plan`：启动即进入 plan mode。
    fn cli_flags(&self) -> Vec<CliFlagDef> {
        vec![CliFlagDef {
            takes_value: false,
            name: "plan",
            description: "Start in plan mode (read-only exploration; edit/write disabled, bash restricted to an allowlist)",
        }]
    }

    /// `--plan` 为真时启用 plan mode，其它 flag 忽略。
    fn apply_cli_flag(&self, name: &str, value: bool) {
        if name == "plan" && value {
            set_enabled(true);
        }
    }

    /// 声明 Alt+P 切换 plan mode。
    fn keybindings(&self) -> Vec<ExtensionKeybinding> {
        vec![ExtensionKeybinding {
            id: "plan-mode.toggle".to_string(),
            keys: vec!["alt+p".to_string()],
            description: "Toggle plan mode (read-only exploration)".to_string(),
        }]
    }

    // 实时按状态计算兴趣（单一谓词见 interest_for）：空闲时为空，框架完全跳过本扩展。
    /// 按当前状态实时返回关心的钩子集合（空闲时为空，框架据此完全跳过本扩展）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        interest_for(&state().lock())
    }

    /// 注册钩子：接线 /plan /todos 命令执行入口 + Alt+P 快捷键执行入口
    /// （命令/快捷键的"存在性"由 commands()/keybindings() 声明动态决定，
    /// 此处只绑定"谁来执行"——执行需要 TUI 状态，不能下沉到 core。
    /// `/todos` 的执行入口始终绑定，但其"存在性"由 commands() 按当前 todos
    /// 状态动态决定：无 todo 任务时不出现在候选、也不可分发（避免误操作）。
    fn on_registered(&self) {
        register_slash_command("plan-mode", "plan", command_plan);
        register_slash_command("plan-mode", "todos", command_todos);
        register_extension_keybinding("plan-mode", "plan-mode.toggle", toggle_plan_ui);
    }

    // /extension 面板开关：禁用时关闭 plan mode 并清状态
    /// 扩展被禁用时立即关闭 plan/exec 并清空 todo，置 `cleanup_pending` 以便重新启用后补一次上下文清理。
    fn on_enabled_changed(&self, enabled: bool) {
        // /extension 面板开关：禁用时强制关停并清状态。
        // 直接清状态而非走 set_enabled（该入口校验扩展启用状态，禁用后
        // is_extension_enabled 已为 false，无法在此强制关闭）。
        if !enabled {
            let mut st = state().lock();
            st.enabled = false;
            st.executing = false;
            st.todos.clear();
            st.refine_pending = false;
            st.select_id = None;
            // 扩展被整体禁用（不参与分发）时无法立即清理，置位以便重新启用后补一次清理
            st.cleanup_pending = true;
            drop(st);
            emit_plan_changed(false);
            persist();
        }
    }

    /// plan mode 开启时从内置工具白名单中移除 `edit` / `write`，未开启时原样返回。
    fn filter_tools(&self, selected: Vec<String>) -> Vec<String> {
        if state().lock().enabled {
            selected
                .into_iter()
                .filter(|t| !matches!(t.as_str(), "edit" | "write"))
                .collect()
        } else {
            selected
        }
    }

    /// plan mode 开启时拦截写操作：`edit`/`write` 直接拒绝，`bash` 仅放行命中只读白名单
    /// 且不含破坏性模式的命令；未开启时一律放行（`Ok(None)`）。
    fn before_tool_call(
        &self,
        name: &str,
        args: &Value,
    ) -> std::result::Result<Option<Value>, ToolError> {
        if !state().lock().enabled {
            return Ok(None);
        }
        match name {
            "edit" | "write" => Err(ToolError(format!(
                "Plan mode: {name} is disabled (read-only exploration). Use /plan to disable plan mode first."
            ))),
            "bash" => {
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !is_safe_command(command) {
                    return Err(ToolError(format!(
                        "Plan mode: command blocked (not allowlisted). Use /plan to disable plan mode first.\nCommand: {command}"
                    )));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// 先清掉本扩展上一轮注入的上下文消息，再按状态注入 plan 提示或执行中的剩余步骤列表；
    /// 同时消费 one-shot 的 `cleanup_pending`（退出 plan/exec 后靠它再跑一次以清除残留注入）。
    fn transform_context(&self, messages: &mut Vec<AgentMessage>) -> Result<()> {
        // 快照状态并立即释放状态锁：本回调不再跨 retain/push 持锁，避免与分发循环里
        // 对 hooks() 的再次加锁纠缠（锁序固定为 registry → 扩展 state，禁止反向）。
        let (enabled, executing, todos) = {
            let mut st = state().lock();
            // 本次即"清理回合"：无论是否仍在 plan/exec，都已消费掉 one-shot 标志。
            st.cleanup_pending = false;
            (st.enabled, st.executing, st.todos.clone())
        };

        // 清除旧注入（plan/execution 标记消息），避免多轮累积；退出 plan 模式的残留也在此清除。
        // 只清理「本扩展注入的上下文消息」：user 角色 + 未落 session（entry_id 为空）+ 正文以注入标记开头。
        // 绝不能按“正文包含标记”删除：模型可能在 assistant 文本里回显标记，误删带 tool_calls 的
        // assistant 会让其 tool 结果变成孤儿，上游报 "Messages with role 'tool' must be a response
        // to a preceding message with 'tool_calls'"。
        messages.retain(|m| !is_injected_plan_context(m));

        if enabled && !executing {
            push_context(messages, PLAN_MODE_PROMPT);
        } else if executing && !todos.is_empty() {
            push_context(
                messages,
                &format!(
                    "{}{}{}",
                    EXEC_MODE_PROMPT_HEAD,
                    remaining_todo_lines(&todos),
                    EXEC_MODE_PROMPT_TAIL
                ),
            );
        }
        Ok(())
    }

    /// 会话加载/恢复：从会话内 `plan-mode` 自定义条目与消息历史重建状态
    /// （`--plan` 已由 apply_cli_flag 在内存态反映；会话条目/历史恢复优先于内存态）。
    /// resume（`-c` / `--session` / `--resume` / `--session-id` / `--fork`）时
    /// 未完成 todo 自动显示停靠面板。启动路径 main 在本钩子后统一 rebuild_tools，
    /// 此处不再请求（避免启动即弹 "extensions updated: tools rebuilt"）。
    fn on_session_start(&self, _cwd: &str, messages: &[AgentMessage], session: Option<&Session>) {
        restore_from_session(session, messages, false);
    }

    /// 会话切换：/new /resume /import /fork /clone 汇合时 TUI 经
    /// [`crate::core::extensions::dispatch_session_switched`] 回调。
    /// 目标会话含 plan-mode 状态 → 恢复（未完成 todo 自动显示停靠面板）；
    /// 全新会话（路径 None）或打开失败 → 清空内存态（跨会话互不串扰）。
    fn on_session_switched(&self, session_path: Option<&str>, messages: &[AgentMessage]) {
        let Some(path) = session_path.filter(|p| !p.is_empty()) else {
            *state().lock() = State::default();
            return;
        };

        match Session::open(path) {
            // 会话切换后无自动 rebuild，需主动请求重建工具集（过滤立即生效）
            Ok(sess) => restore_from_session(Some(&sess), messages, true),
            Err(_) => *state().lock() = State::default(),
        }
    }

    /// 面板选择结果回传（Select 面板 Enter/Esc 经 core 分发）：
    /// - Execute → 关闭 plan mode、进入执行模式，返回 follow-up（触发下一轮）；
    /// - Refine → 置精炼待定 + 请求输入提示，返回 None；
    /// - 其余（Stay / Esc 取消）→ 停留，返回 None。
    fn on_ui_choice(&self, id: u64, choice: Option<String>) -> Option<String> {
        let mut st = state().lock();
        if st.select_id != Some(id) {
            return None;
        }

        st.select_id = None;
        let Some(choice) = choice else {
            return None; // 取消（Esc）：停留 plan mode
        };

        if choice.starts_with("Execute") {
            // 执行计划：关闭 plan mode、恢复完整工具、进入执行模式并返回 follow-up
            st.enabled = false;
            st.executing = true;

            // 执行中自动显示停靠面板（免手动 /todos；无锚点，从顶部开始）
            let first = st
                .todos
                .iter()
                .find(|t| !t.completed)
                .map(|t| t.text.clone());
            let mut text = format!(
                "{}{}{}",
                EXECUTE_FOLLOWUP_HEAD,
                remaining_todo_lines(&st.todos),
                EXECUTE_FOLLOWUP_TAIL
            );

            if let Some(first) = first {
                text.push_str(&format!("\n\nStart with: {first}")); // 追加 Start with 首项
            }

            // 打开自己的停靠段（免手动 /todos，执行中自动可见）并请求显示面板
            st.dock_shown = true;
            drop(st);

            // 注意：必须 drop 后再请求（anchors 已自 st 取出；此锁不可重入）
            request_show_dock();
            emit_plan_changed(false);
            persist();
            Some(text)
        } else if choice.contains("Refine") {
            // 进入精炼：提示用户在输入框输入，下一次 on_user_prompt 合并计划列表
            st.refine_pending = true;
            drop(st);
            request_ui(core::extensions::ExtensionUiRequest::Notify {
                text: "Plan mode: type your refinement below and press Enter to send".to_string(),
                level: UiNotifyLevel::Info,
            });
            None
        } else {
            None // Stay：留在 plan mode
        }
    }

    /// 输入提交钩子：用户提交精炼文本时合并 `Plan Steps` 列表 + 改写指令
    /// 作为下一条 prompt。仅在前一次面板选择为 Refine 时生效。
    fn on_user_prompt(&self, text: &str) -> Option<UserPromptAction> {
        let mut st = state().lock();
        if !st.refine_pending {
            return None;
        }
        st.refine_pending = false;
        let list = todo_list_display(&st.todos);
        Some(UserPromptAction::Rewrite(format!(
            "**Plan Steps ({}):**\n\n{}\n\nRefine the plan:\n{}",
            st.todos.len(),
            list,
            text.trim()
        )))
    }

    /// 停靠面板内容：段可见且 non-todos 时返回标题 + 列表（否则返回空 → 不占位）。
    /// 标题携带进度计数 `Plan Progress (已完成/总数)`；条目沿用 /todos 配色语义。
    fn dock_lines(&self) -> Vec<DockLine> {
        let st = state().lock();
        if !st.dock_shown || st.todos.is_empty() {
            return Vec::new();
        }

        let done = st.todos.iter().filter(|t| t.completed).count();
        let mut out = vec![vec![DockSpan::plain(format!(
            "Plan Progress ({}/{})",
            done,
            st.todos.len()
        ))]];

        for item in &st.todos {
            let mut line = vec![DockSpan::plain(format!("{}. ", item.step))];
            if item.completed {
                line.push(DockSpan::new("success", "#00ff00", format!("{DEF_DONE} ")));
                line.push(DockSpan::new("muted", "#808080", item.text.clone()));
            } else {
                line.push(DockSpan::plain(format!("{DEF_CHECKBOX_EMPTY} ")));
                line.push(DockSpan::plain(item.text.clone()));
            }
            out.push(line);
        }
        out
    }

    /// 只分派三类事件：`turn_end` / `agent_end` 驱动步骤推进与计划提取，
    /// `extension:entry_persisted` 回执驱动落盘合并。
    fn on_agent_event(&self, event: &Value) {
        match event.get("type").and_then(|v| v.as_str()) {
            Some("turn_end") => self.on_turn_end(event),
            Some("agent_end") => self.on_agent_end(event),
            Some("extension:entry_persisted")
                if let Some(ct) = event.get("customType").and_then(|v| v.as_str()) =>
            {
                on_persisted(ct);
            }
            _ => {}
        }
    }
}

/// 是否为本扩展注入的上下文消息（plan/exec 标记消息）。
///
/// 仅匹配「user 角色 + 未落 session（`entry_id` 为空）+ 正文以注入标记开头」。
/// 注入消息由 [`push_context`] 构造，从未写入 session，因此 `entry_id` 恒为 `None`；
/// 而模型产出的 assistant / tool 文本即使回显了标记也不会被误删——否则会删掉带
/// `tool_calls` 的 assistant，留下孤儿 tool 结果触发上游 400。
fn is_injected_plan_context(m: &AgentMessage) -> bool {
    if m.role != "user" || m.entry_id.is_some() {
        return false;
    }
    let text = m.text();
    let text = text.trim_start();
    text.starts_with(PLAN_MODE_MARKER) || text.starts_with(EXECUTING_MARKER)
}

/// 注入一条用户态上下文消息（不落 session，仅本轮内存上下文）
fn push_context(messages: &mut Vec<AgentMessage>, text: &str) {
    messages.push(AgentMessage::user_text(text));
}

/// `/plan [text]`：无参数切换 plan mode；带参数时先确保进入 plan mode（只读工具集生效），
/// 再把参数作为输入发送（经 `status_start` 触发下一轮，与扩展 follow-up 同路径）。
fn command_plan(st: &mut App, cmd: &str) -> bool {
    let arg = command_arg(cmd);
    if arg.is_empty() {
        toggle_plan_ui(st);
        return false;
    }

    enable_plan_ui(st);

    let text = arg.to_string();
    st.messages.push(AgentMessage::user_text(&text));
    st.status_start = Some(text);
    false
}

/// `/todos`：切换 plan 停靠段的显隐并置脏；调用前提是已存在 todo（commands() 已按此门控）。
fn command_todos(st: &mut App, _: &str) -> bool {
    // 分发本身保证此时存在 todos（commands() 仅在 todo_count>0 时声明 /todos），
    // 故无需空列表分支；这里只负责 plan 停靠段的显隐切换。
    if !st.dock_visible {
        // 面板未显示：显示 plan 段并打开面板
        show_dock_section();
        st.dock_visible = true;
        st.dirty = true;
    } else if is_dock_shown() {
        // 面板显示中：隐藏 plan 段（若面板再无其他内容，下一帧渲染自动收起）
        hide_dock_section();
        st.dirty = true;
    } else {
        // 面板显示中：重新显示 plan 段
        show_dock_section();
        st.dirty = true;
    }
    false
}

impl PlanMode {
    /// turn_end：执行模式下按 [DONE:n] 标记 / 工具调用自动推进步骤
    fn on_turn_end(&self, event: &Value) {
        {
            let st = state().lock();
            if !st.executing || st.todos.is_empty() {
                return;
            }
        }

        let message: Option<AgentMessage> = event
            .get("message")
            .and_then(|m| serde_json::from_value(m.clone()).ok());
        let Some(message) = message else {
            return;
        };
        let text = message.text();

        let progressed = {
            let mut st = state().lock();
            let before: Vec<bool> = st.todos.iter().map(|t| t.completed).collect();
            let explicitly = mark_completed_steps(&text, &mut st.todos);

            // 无 [DONE:n] 但本回合有工具调用 → 自动把首个未完成步骤标记完成
            let had_tool_calls = event
                .get("toolResults")
                .and_then(|v| v.as_array())
                .is_some_and(|a| !a.is_empty());

            if explicitly == 0
                && had_tool_calls
                && let Some(next) = st.todos.iter_mut().find(|t| !t.completed)
            {
                next.completed = true;
            }

            // 完成态是否真的发生了变化（有步骤从未完成 → 完成）
            before != st.todos.iter().map(|t| t.completed).collect::<Vec<_>>()
        };

        // 仅在 todo 取得进展（步骤完成）时才落盘，避免每回合都向会话文件写入相同内容（plan-mode 条目刷屏的根因）
        if progressed {
            persist();
        }
    }

    /// agent_end：执行完成 → 通知并清状态；plan mode → 提取步骤并请求选择面板
    fn on_agent_end(&self, event: &Value) {
        {
            // 执行模式完成检查
            let mut st = state().lock();
            if st.executing && !st.todos.is_empty() && st.todos.iter().all(|t| t.completed) {
                // 完成总结：富文本 spans（success 标题 + 逐行 ✓/muted），
                // 与 dock 面板已完成条目同配色语义，避免纯文本里 `**`/`~~` 无法渲染
                let mut spans = vec![RichSpan::fg(
                    "success",
                    format!(
                        "Plan Complete! ({}/{}) {DEF_DONE}\n",
                        st.todos.len(),
                        st.todos.len()
                    ),
                )];

                for item in &st.todos {
                    spans.push(RichSpan::plain(format!("{}. ", item.step)));
                    spans.push(RichSpan::fg("success", format!("{DEF_DONE} ")));
                    spans.push(RichSpan::fg("muted", format!("{}\n", item.text)));
                }

                st.executing = false;
                st.todos.clear();
                st.select_id = None;
                // 执行结束也要清掉 exec 标记注入（one-shot）
                st.cleanup_pending = true;
                drop(st);

                persist();
                request_ui(ExtensionUiRequest::NotifyRich {
                    spans,
                    level: UiNotifyLevel::Success,
                });
                return;
            }

            if !st.enabled || st.executing {
                return;
            }
        }

        // plan mode：从最后一个助手消息提取步骤
        let extracted: Vec<TodoItem> = event
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|arr| {
                let mut last_assistant: Option<String> = None;
                for m in arr.iter().rev() {
                    if m.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                        continue;
                    }
                    if let Ok(msg) = serde_json::from_value::<AgentMessage>(m.clone()) {
                        last_assistant = Some(msg.text());
                        break;
                    }
                }

                last_assistant
                    .map(|t| extract_todo_items(&t))
                    .unwrap_or_default()
            })
            .unwrap_or_default();

        if extracted.is_empty() {
            return;
        }

        state().lock().todos = extracted;
        persist();

        let id = core::extensions::next_ui_id();
        state().lock().select_id = Some(id);

        request_ui(ExtensionUiRequest::Select {
            id,
            title: "Plan mode - what next?".to_string(),
            options: vec![
                SelectOption::new("Execute the plan (track progress)"),
                SelectOption::new("Stay in plan mode"),
                SelectOption::new("Refine the plan"),
            ],
        });
    }
}

/// 测试用：复位进程级内存中的 plan 状态（state 是模块私有单例，跨测试文件无法直接访问；
/// `set_enabled(false)` 有 guard（已 disabled 时直接 return），可能残留非空 todos）。
/// plan-mode 测试串行化锁：内存态 state() 是进程级单例，plan_mode.rs /
/// handlers.rs / commands.rs 的 plan-mode 相关测试并行会互相踩。
/// 仅用于测试；不涉及任何共享文件（各测试用独立 tempdir 生成自己的文件），
/// 锁内 panic 会毒化 → 统一用 `unwrap_or_else(|e| e.into_inner())` 恢复。
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 测试用：把进程级 plan 状态复位为默认值，隔离用例间的内存态污染。
#[cfg(test)]
pub(crate) fn reset_state_for_tests() {
    *state().lock() = State::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独立持有：AUTH_TEST_LOCK（进程级全局态：pending UI 队列与
    /// 经全局注册表分发的扩展状态）+ TEST_LOCK（内存态 state() 单例串行化，
    /// 毒化自动恢复）与独立 tempdir（不共用任何本地文件，如 auth.json /
    /// plan-mode.json——无需跨模块共享文件锁，各测试自己生成文件）。
    struct TestEnv {
        _auth_lock: std::sync::MutexGuard<'static, ()>,
        _lock: std::sync::MutexGuard<'static, ()>,
        _ad: crate::test_support::AgentDirGuard,
        dir: tempfile::TempDir,
    }

    /// 清空并计数队列中**本扩展**的落盘请求。
    ///
    /// 全局 UI 队列是所有扩展共用的，并行跑的其它扩展测试（如 subagent）会入队自己的
    /// 请求——不做 `custom_type` 过滤，就会被别人的条目把计数抬起来，
    /// 使「该落盘 / 不该落盘」的断言随机失败（与 plan-mode 自身行为无关）。
    ///
    /// 不在此处补回执：调用方用显式 `on_persisted(CUSTOM_TYPE)` 模拟 worker 回执，
    /// 补两次会提前清掉在途标记（契约见 `drain_ui_requests` 注释）。
    fn take_own_persists() -> usize {
        let mut n = 0;
        while let Some(r) = crate::core::extensions::take_pending_ui() {
            if let core::extensions::ExtensionUiRequest::PersistSessionEntry { custom_type, .. } = r
                && custom_type == CUSTOM_TYPE
            {
                n += 1;
            }
        }
        n
    }

    fn drain_ui_requests() {
        // 丢弃队列项时必须补一次"回执"：`PersistSessionEntry` 一经入队就置了 `write_in_flight`，
        // 直接丢弃会让后续 persist() 被永久吞掉。
        while let Some(r) = crate::core::extensions::take_pending_ui() {
            if matches!(r, ExtensionUiRequest::PersistSessionEntry { .. }) {
                on_persisted(CUSTOM_TYPE);
            }
        }
    }

    /// 测试注册：plan-mode 是声明 `default_enabled() == false` 的内置扩展
    /// （默认关闭），测试直接注册并显式启用（幂等）。真实入口由 settings.json
    /// 的 `enabledExtensions` / /extension 面板控制。
    fn ensure_registered() {
        super::ensure_registered();
        crate::core::extensions::set_extension_enabled("plan-mode", true);
    }

    fn setup() -> TestEnv {
        // 锁序必须与 handlers.rs / handlers::commands 的 plan-mode 测试一致：
        // AUTH_TEST_LOCK → TEST_LOCK（反向持锁会 ABBA 死锁）。
        //
        // 为何需要 AUTH_TEST_LOCK：plan 状态是进程级单例，而 pending UI 队列与
        // 「经全局注册表分发」的钩子（on_session_switched / agent 事件）会被其它
        // 并行测试触达——不串行时，本测试刚恢复的状态/排入的 ShowDock 会被
        // 别的测试 take 走或清空（断言随机失败的 use-after-take）。
        let _auth_lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 全局扩展状态用 TEST_LOCK 串行；agent 目录用线程本地 guard 每测试独立
        // PRUX_AGENT_DIR。扩展注册启用态来自 settings.json 的
        // disabledExtensions / enabledExtensions（plan-mode 默认关闭），
        // 不隔离会读到真实 ~/.prux/settings.json（其中 plan-mode 可能被
        // /extension 面板禁用），导致 set_enabled/apply_cli_flag 测试随机失败。
        // 每测试独立临时 agent_dir（线程本地 override），不再劫持进程 env/共享目录
        let _ad = crate::test_support::AgentDirGuard::temp();
        // 每个测试独立 tempdir：测试自己生成/读写自己的文件，互不干扰
        let dir = tempfile::tempdir().expect("tempdir");
        // 全局注册表里 plan-mode 的启用态是进程级、跨测试残留的；而其它并行测试
        // （app/render 等未持 AUTH_TEST_LOCK 的同步测试）会把 agent 事件与会话切换
        // 经全局注册表分发进来，把本测试刚恢复的状态清掉。默认关闭扩展，让分发跳过
        // 本扩展；需要全局启用的测试自己调 `ensure_registered()` 显式开启。
        // （须在复位状态之前做：on_enabled_changed(false) 会置 cleanup_pending。）
        crate::core::extensions::set_extension_enabled("plan-mode", false);
        // 复位进程级内存态（避免测试间/外部污染；不切 cwd 保持进程 cwd 稳定）
        *state().lock() = State::default();
        drain_ui_requests();
        TestEnv {
            _auth_lock,
            _lock,
            _ad,
            dir,
        }
    }

    #[test]
    fn safe_command_allows_readonly() {
        let _g = setup();
        for cmd in [
            "ls -la",
            "grep -r foo src/",
            "cat Cargo.toml",
            "git status",
            "git log --oneline",
            "rg pattern .",
            "echo hello",
        ] {
            assert!(is_safe_command(cmd), "should allow: {cmd}");
        }
    }

    #[test]
    fn safe_command_blocks_destructive() {
        let _g = setup();
        for cmd in [
            "rm -rf target",
            "mv a b",
            "cp a b",
            "touch x",
            "mkdir x",
            "git commit -m x",
            "git push",
            "npm install",
            "pip install x",
            "sudo ls",
            "ls && rm x",
            "echo hi > file",
            "echo hi >> file",
            "vim Cargo.toml",
            "tee out.txt",
        ] {
            assert!(!is_safe_command(cmd), "should block: {cmd}");
        }
    }

    #[test]
    fn todos_spans_no_duplicate_title_and_success_color() {
        // 回归：/todos 标题不得重复；已完成步骤 ✓ 用 success 色，未完成保持默认
        let _g = setup();
        {
            let mut st = state().lock();
            st.todos = vec![
                TodoItem {
                    step: 1,
                    text: "Done step".to_string(),
                    completed: true,
                },
                TodoItem {
                    step: 2,
                    text: "Pending step".to_string(),
                    completed: false,
                },
            ];
        }

        let spans = todo_list_spans().expect("有 todos 应返回 spans");
        let text = crate::modes::interactive::app::sys_spans_text(&spans);
        assert_eq!(
            text.matches("Plan Progress:").count(),
            1,
            "标题不得重复: {text:?}"
        );
        assert_eq!(
            text, "Plan Progress:\n1. ✓ Done step\n2. ☐ Pending step\n",
            "got: {text:?}"
        );

        // 已完成 ✓ 片段使用 success 前景色，未完成无 fg 覆盖（用 info 默认色）
        let check = spans
            .iter()
            .find(|s| s.text == "✓ ")
            .expect("应有已完成标记");
        assert_eq!(check.fg.as_deref(), Some("success"));
        let pending = spans
            .iter()
            .find(|s| s.text == "☐ ")
            .expect("应有未完成标记");
        assert_eq!(pending.fg, None, "未完成项不应覆盖颜色");
    }

    #[test]
    fn extract_todos_from_plan_message() {
        let _g = setup();
        let msg = "Let me analyze this.\n\nPlan:\n1. **Read** the main file\n2. Check the `config`\n3. rm nothing (keep, not a verb prefix)\n4. Verify behavior\n\nThen some footer.";
        let items = extract_todo_items(msg);
        assert_eq!(items.len(), 4, "got: {items:?}");
        assert_eq!(items[0].text, "Main file");
        assert_eq!(items[0].step, 1);
        assert!(!items[0].completed);
        // 非编号列表 / 太短 / 代码起始被跳过
        let msg2 = "Plan:\n- bullet\n\nDone.";
        assert!(extract_todo_items(msg2).is_empty());
    }

    #[test]
    fn done_markers_complete_steps() {
        let _g = setup();
        let mut items = vec![
            TodoItem {
                step: 1,
                text: "A".to_string(),
                completed: false,
            },
            TodoItem {
                step: 2,
                text: "B".to_string(),
                completed: false,
            },
            TodoItem {
                step: 3,
                text: "C".to_string(),
                completed: false,
            },
        ];
        assert_eq!(
            mark_completed_steps("did [DONE:1] and [DONE:3]", &mut items),
            2
        );
        assert!(items[0].completed);
        assert!(!items[1].completed);
        assert!(items[2].completed);
        // 大小写不敏感
        let mut items2 = vec![TodoItem {
            step: 1,
            text: "A".to_string(),
            completed: false,
        }];
        assert_eq!(mark_completed_steps("[done:1]", &mut items2), 1);
        assert!(items2[0].completed);
    }

    #[test]
    fn filter_tools_removes_writes_in_plan_mode() {
        let _g = setup();
        let base = vec![
            "read".to_string(),
            "bash".to_string(),
            "edit".to_string(),
            "write".to_string(),
            "grep".to_string(),
        ];
        let ext = PlanMode::new();
        assert_eq!(ext.filter_tools(base.clone()), base);
        ensure_registered();
        set_enabled(true);
        let filtered = ext.filter_tools(base.clone());
        assert!(!filtered.contains(&"edit".to_string()));
        assert!(!filtered.contains(&"write".to_string()));
        assert!(filtered.contains(&"read".to_string()));
        set_enabled(false);
        assert_eq!(ext.filter_tools(base.clone()), base);
    }

    #[test]
    fn before_tool_call_intercepts_bash_and_writes() {
        let _g = setup();
        let ext = PlanMode::new();
        ensure_registered();
        set_enabled(true);
        let write = ext.before_tool_call("write", &serde_json::json!({"path":"x","content":"y"}));
        assert!(write.is_err(), "write tool blocked in plan mode");
        let bad_bash = ext.before_tool_call("bash", &serde_json::json!({"command":"rm x"}));
        assert!(bad_bash.is_err(), "destructive bash blocked");
        let ok_bash = ext.before_tool_call("bash", &serde_json::json!({"command":"ls -la"}));
        assert!(ok_bash.unwrap().is_none(), "read-only bash allowed");
        let read = ext.before_tool_call("read", &serde_json::json!({"path":"x"}));
        assert!(read.unwrap().is_none(), "other tools allowed");
        set_enabled(false);
        let write2 = ext.before_tool_call("write", &serde_json::json!({"path":"x","content":"y"}));
        assert!(
            write2.unwrap().is_none(),
            "disabled plan mode: write allowed"
        );
    }

    #[test]
    fn transform_context_injects_and_cleans() {
        let _g = setup();
        let ext = PlanMode::new();
        ensure_registered();
        let mut msgs = vec![AgentMessage::user_text("hello")];
        set_enabled(true);
        ext.transform_context(&mut msgs).unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[1].text().contains(PLAN_MODE_MARKER));

        // 再次 transform（模拟下一轮）：不累积
        ext.transform_context(&mut msgs).unwrap();
        assert_eq!(msgs.len(), 2, "old injection replaced, not duplicated");

        // 执行模式注入 remaining steps
        {
            let mut st = state().lock();
            st.executing = true;
            st.todos = vec![TodoItem {
                step: 1,
                text: "Do A".to_string(),
                completed: false,
            }];
        }
        ext.transform_context(&mut msgs).unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[1].text().contains(EXECUTING_MARKER));
        assert!(msgs[1].text().contains("1. Do A"));

        // 关闭 plan mode 后清除注入
        set_enabled(false);
        let mut st = state().lock();
        st.executing = false;
        st.todos.clear();
        drop(st);
        ext.transform_context(&mut msgs).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(!msgs[0].text().contains(PLAN_MODE_MARKER));
    }

    #[test]
    fn transform_context_keeps_assistant_that_echoes_marker() {
        // 回归：模型在 assistant 文本里回显了注入标记（如 "[EXECUTING PLAN"），
        // transform_context 不得因此删除这条带 tool_calls 的 assistant——
        // 否则其 tool 结果成为孤儿，上游报 "Messages with role 'tool' must be a
        // response to a preceding message with 'tool_calls'"。
        let _g = setup();
        let ext = PlanMode::new();
        ensure_registered();
        set_enabled(false);
        {
            let mut st = state().lock();
            st.executing = false;
            st.todos.clear();
        }

        let mut assistant = AgentMessage::user_text("");
        assistant.role = "assistant".into();
        assistant.entry_id = Some("entry-1".into());
        assistant.content = vec![
            crate::core::provider::ContentBlock::Text {
                text: format!("continuing {EXECUTING_MARKER} now"),
                text_signature: None,
            },
            crate::core::provider::ContentBlock::ToolCall {
                id: "call_1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({}),
                thought_signature: None,
                namespace: None,
            },
        ];
        let mut msgs = vec![assistant];

        ext.transform_context(&mut msgs).unwrap();
        assert_eq!(msgs.len(), 1, "带 tool_calls 的 assistant 不得被删");
        assert_eq!(msgs[0].role, "assistant");

        // 同一标记但 role=user 且未落 session（entry_id 为空）= 真正的注入消息，仍被清理。
        let injected = AgentMessage::user_text(&format!("{EXECUTING_MARKER} - Full tool access"));
        msgs.push(injected);
        ext.transform_context(&mut msgs).unwrap();
        assert_eq!(msgs.len(), 1, "注入消息应被清理");
    }

    #[test]
    fn turn_end_marks_done_and_auto_advances() {
        let _g = setup();
        let ext = PlanMode::new();
        {
            let mut st = state().lock();
            st.enabled = false;
            st.executing = true;
            st.todos = vec![
                TodoItem {
                    step: 1,
                    text: "A".to_string(),
                    completed: false,
                },
                TodoItem {
                    step: 2,
                    text: "B".to_string(),
                    completed: false,
                },
            ];
        }
        // 显式 [DONE:1]
        ext.on_agent_event(&serde_json::json!({
            "type": "turn_end",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "done [DONE:1]" }] },
            "toolResults": []
        }));
        let todos = state().lock().todos.clone();
        assert!(todos[0].completed);
        assert!(!todos[1].completed);

        // 无 [DONE:n] 但有工具调用 → 自动完成首个未完成
        ext.on_agent_event(&serde_json::json!({
            "type": "turn_end",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "next" }] },
            "toolResults": [{ "x": 1 }]
        }));
        let todos = state().lock().todos.clone();
        assert!(todos[1].completed, "auto-advanced");
    }

    #[test]
    fn agent_end_requests_select_then_execute() {
        let _g = setup();
        let ext = PlanMode::new();
        ensure_registered();
        set_enabled(true);
        // 一次计划回复
        ext.on_agent_event(&serde_json::json!({
            "type": "agent_end",
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
                { "role": "assistant", "content": [{ "type": "text", "text": "Plan:\n1. Inspect code\n2. Verify result" }] }
            ]
        }));
        // agent_end 提取步骤时 persist() 也会入队 PersistSessionEntry（测试无 worker
        // 回执，在途写入不完成）→ 扫描出需要的 Select 请求
        let (id, title, options) = loop {
            match crate::core::extensions::take_pending_ui() {
                Some(core::extensions::ExtensionUiRequest::Select { id, title, options }) => {
                    break (
                        id,
                        title,
                        options.into_iter().map(|o| o.text).collect::<Vec<_>>(),
                    );
                }
                Some(_) => continue,
                None => panic!("expected select request"),
            }
        };
        assert_eq!(title, "Plan mode - what next?");
        assert_eq!(options.len(), 3);
        {
            let st = state().lock();
            assert_eq!(st.todos.len(), 2);
        }
        // 选择 Execute → follow-up（触发下一轮）
        let follow = ext.on_ui_choice(id, Some(options[0].clone()));
        let text = follow.expect("execute follow-up");
        assert!(text.starts_with("Execute the plan."));
        assert!(text.contains("1. Inspect code"));
        assert!(text.contains("Start with: Inspect code"));
        assert!(!is_enabled(), "execute closes plan mode");
        assert!(state().lock().executing);
        // 执行模式自动请求显示停靠面板（免手动 /todos；ShowDock 在 persist 之前入队）。
        // 扫描队列而不是取队首：全局队列是所有扩展共用的，并行测试会插入自己的请求。
        let mut show_dock = false;
        while let Some(req) = crate::core::extensions::take_pending_ui() {
            if matches!(req, core::extensions::ExtensionUiRequest::ShowDock) {
                show_dock = true;
            }
        }
        assert!(show_dock, "execute 应请求显示停靠面板");
        // 旧 id 再回传不生效（已消费）
        assert!(ext.on_ui_choice(id, Some("x".to_string())).is_none());
    }

    #[test]
    fn stay_then_retoggle_agent_end_still_requests_select() {
        // 回归：选择 Stay 停留 plan mode → /plan 退出 → /plan 进入后，
        // 下一次 agent_end 仍应重新弹出选择面板（内部状态不得残留拦截）。
        let _g = setup();
        let ext = PlanMode::new();
        ensure_registered();
        set_enabled(true);
        let plan_event = serde_json::json!({
            "type": "agent_end",
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
                { "role": "assistant", "content": [{ "type": "text", "text": "Plan:\n1. Inspect code\n2. Verify result" }] }
            ]
        });

        // 第一次计划 → 弹 Select（跳过 persist 入队条目）
        ext.on_agent_event(&plan_event);
        let id1 = loop {
            match crate::core::extensions::take_pending_ui() {
                Some(core::extensions::ExtensionUiRequest::Select { id, .. }) => break id,
                Some(_) => continue,
                None => panic!("expected first select"),
            }
        };
        // 选择 Stay：停留 plan mode
        assert!(
            ext.on_ui_choice(id1, Some("Stay in plan mode".to_string()))
                .is_none(),
            "Stay 不应触发 follow-up"
        );
        assert!(is_enabled());

        // /plan 退出 → /plan 进入
        set_enabled(false);
        assert!(!is_enabled());
        set_enabled(true);
        assert!(is_enabled());

        // 第二次计划 → 必须重新弹 Select（跳过并行测试可能残留的 PersistSessionEntry 等）
        ext.on_agent_event(&plan_event);
        let (id2, options2) = loop {
            match crate::core::extensions::take_pending_ui() {
                Some(core::extensions::ExtensionUiRequest::Select { id, options, .. }) => {
                    break (id, options.into_iter().map(|o| o.text).collect::<Vec<_>>());
                }
                Some(_) => continue,
                None => panic!("expected second select after re-enter"),
            }
        };
        assert_eq!(options2.len(), 3);
        assert_ne!(id2, id1, "新请求应使用新 id");
        while crate::core::extensions::take_pending_ui().is_some() {}
    }

    #[test]
    fn agent_end_notify_on_completion() {
        let _g = setup();
        let ext = PlanMode::new();
        {
            let mut st = state().lock();
            st.enabled = false;
            st.executing = true;
            st.todos = vec![TodoItem {
                step: 1,
                text: "A".to_string(),
                completed: true,
            }];
        }
        ext.on_agent_event(&serde_json::json!({ "type": "agent_end", "messages": [] }));
        // 完成路径 persist() 先入队 PersistSessionEntry → 扫描 NotifyRich
        let spans = loop {
            match crate::core::extensions::take_pending_ui() {
                Some(core::extensions::ExtensionUiRequest::NotifyRich { spans, .. }) => {
                    break spans;
                }
                Some(_) => continue,
                None => panic!("expected completion notify"),
            }
        };
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("Plan Complete!"));
        assert!(text.contains("1. "));
        assert!(text.contains("A"));
        assert!(!text.contains("**"), "不得含字面粗体标记: {text:?}");
        assert!(!text.contains("~~"), "不得含字面删除线标记: {text:?}");
        // 标题与条目 ✓ 用 success 前景，条目文本用 muted
        assert_eq!(spans[0].fg.as_deref(), Some("success"));
        assert_eq!(spans[2].fg.as_deref(), Some("success"));
        assert_eq!(spans[3].fg.as_deref(), Some("muted"));
        {
            let st = state().lock();
            assert!(!st.executing);
            assert!(st.todos.is_empty());
        }
    }

    #[test]
    fn refine_flow_requests_input_then_merges() {
        let _g = setup();
        let ext = PlanMode::new();
        // 执行中请求 Select → 选 Refine → 输入钩子合并计划列表
        ensure_registered();
        set_enabled(true);
        // 输入钩子：非精炼待定时原样（不拦截普通输入）
        assert!(
            ext.on_user_prompt("plain text").is_none(),
            "非精炼待定不认领"
        );
        ext.on_agent_event(&serde_json::json!({
            "type": "agent_end",
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
                { "role": "assistant", "content": [{ "type": "text", "text": "Plan:\n1. Inspect code" }] }
            ]
        }));
        let select_id = loop {
            match crate::core::extensions::take_pending_ui() {
                Some(core::extensions::ExtensionUiRequest::Select { id, options, .. }) => {
                    let refine = options
                        .iter()
                        .map(|o| o.text.clone())
                        .find(|o| o.contains("Refine"))
                        .unwrap();
                    // 选择 Refine：无 follow-up（停留等待输入）
                    assert!(ext.on_ui_choice(id, Some(refine)).is_none());
                    break id;
                }
                Some(_) => continue,
                None => panic!("expected select request"),
            }
        };
        // 未知 id 不生效（旧面板/他扩展请求不会误触发）
        assert!(
            ext.on_ui_choice(select_id.wrapping_add(1000), Some("Execute".to_string()))
                .is_none()
        );
        // Refine 选择后发出输入提示（跳过可能残留的 persist 条目）
        let notify_text = loop {
            match crate::core::extensions::take_pending_ui() {
                Some(core::extensions::ExtensionUiRequest::Notify { text, .. }) => break text,
                Some(_) => continue,
                None => panic!("expected refine hint notify"),
            }
        };
        assert!(notify_text.contains("refinement"));
        // 输入钩子：Refine 待定后合并
        let merged = ext.on_user_prompt("make it faster").expect("refine merge");
        let core::extensions::UserPromptAction::Rewrite(merged) = merged else {
            panic!("refine 应返回 Rewrite");
        };
        assert!(merged.contains("**Plan Steps (1):**"));
        assert!(merged.contains("Refine the plan:\nmake it faster"));
        // 消费后不再认领
        assert!(ext.on_user_prompt("again").is_none());
    }

    #[test]
    fn disable_via_extension_panel_no_deadlock() {
        // 回归：plan mode 启用时，/extension 面板禁用 plan-mode 不得死锁。
        // set_extension_enabled 曾持 registry 锁调用 on_enabled_changed，其回调
        // （广播 plan-mode:changed → dispatch_agent_event → registered()）重入
        // registry 锁导致同线程自锁卡死整个 TUI。
        let _g = setup();
        ensure_registered();
        set_enabled(true);
        assert!(is_enabled());

        assert!(
            crate::core::extensions::set_extension_enabled("plan-mode", false),
            "面板禁用应返回成功"
        );
        assert!(!is_enabled(), "禁用扩展应强制关闭 plan mode");

        // 扩展禁用后 /plan 不应能重新开启（filter_tools 不生效，避免假只读）
        assert!(!toggle_enabled(), "扩展禁用时 toggle 不应开启");
        assert!(!is_enabled());

        // 重新启用后恢复可用
        assert!(crate::core::extensions::set_extension_enabled(
            "plan-mode",
            true
        ));
        assert!(toggle_enabled(), "重新启用后 toggle 生效");
        set_enabled(false);
    }

    #[test]
    fn agent_tools_filtered_when_plan_enabled() {
        // 真实 --plan 时序：先 apply_cli_flag(true) 再 Agent::new（main.rs 顺序），
        // compose_tools 经 filter_tools 移除 edit/write；关闭后 rebuild 恢复。
        let _g = setup();
        ensure_registered();
        let ext = PlanMode::new();
        ext.apply_cli_flag("plan", true);
        assert!(is_enabled(), "apply_cli_flag 开启 plan mode");
        let entry = crate::core::model_resolver::find_model("deepseek", "deepseek-flash").unwrap();
        let a = crate::core::agent_session::Agent::new(
            entry,
            "/tmp".to_string(),
            crate::core::agent_session::ToolSelection::tools(vec![
                "read".to_string(),
                "bash".to_string(),
                "edit".to_string(),
                "write".to_string(),
                "grep".to_string(),
                "find".to_string(),
                "ls".to_string(),
            ]),
            Vec::new(),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            false,
            Vec::new(),
        )
        .unwrap();
        assert!(!a.tools.iter().any(|(n, _, _)| n == "edit"));
        assert!(!a.tools.iter().any(|(n, _, _)| n == "write"));
        assert!(a.tools.iter().any(|(n, _, _)| n == "read"));

        // 关闭 plan mode 后 rebuild：工具恢复完整
        set_enabled(false);
        let mut a = a;
        a.rebuild_tools();
        assert!(a.tools.iter().any(|(n, _, _)| n == "edit"));
        assert!(a.tools.iter().any(|(n, _, _)| n == "write"));
    }

    #[test]
    fn turn_end_persists_only_on_progress() {
        // 落盘时机：只有在 todo 完成态推进（[DONE:n] 或工具调用自动完成）时才写会话，
        // 不得在每回合结束后冗余写入（plan-mode 条目刷屏的根因）。
        let _g = setup();
        ensure_registered();
        // ensure_registered 可能是本进程内首次注册：register_extension →
        // on_enabled_changed(false) 会 persist() 一次默认状态。是否是"首次"取决于
        // 其它测试的执行顺序（跨测试顺序依赖），故此处显式清空队列，使断言只反映
        // 本测试自己排队的内容。
        drain_ui_requests();
        let ext = PlanMode::new();
        {
            let mut st = state().lock();
            st.executing = true;
            st.todos = vec![
                TodoItem {
                    step: 1,
                    text: "A".to_string(),
                    completed: false,
                },
                TodoItem {
                    step: 2,
                    text: "B".to_string(),
                    completed: false,
                },
            ];
        }

        // 无进度回合（无 [DONE:n]、无工具调用）→ 不得落盘
        ext.on_agent_event(&serde_json::json!({
            "type": "turn_end",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "looking..." }] },
            "toolResults": []
        }));
        assert_eq!(take_own_persists(), 0, "无进度回合不应落盘");

        // [DONE:1] → 推进 → 落盘
        ext.on_agent_event(&serde_json::json!({
            "type": "turn_end",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "[DONE:1] done" }] },
            "toolResults": []
        }));
        assert_eq!(take_own_persists(), 1, "步骤完成应落盘");
        on_persisted(CUSTOM_TYPE); // 模拟 worker 回执，清在途标记

        // 工具调用 → 自动推进 → 落盘
        ext.on_agent_event(&serde_json::json!({
            "type": "turn_end",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "next" }] },
            "toolResults": [{ "x": 1 }]
        }));
        assert_eq!(take_own_persists(), 1, "工具调用自动完成应落盘");
        on_persisted(CUSTOM_TYPE); // 模拟 worker 回执，清在途标记

        // 重复标记已完成步骤 → 完成态未变 → 不得再落盘
        ext.on_agent_event(&serde_json::json!({
            "type": "turn_end",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "[DONE:1] again" }] },
            "toolResults": []
        }));
        assert_eq!(take_own_persists(), 0, "重复标记已完成步骤不应落盘");
    }

    #[test]
    fn persist_ack_does_not_echo_loop() {
        // 回归：写入回执（extension:entry_persisted）后不得再次入队写入。
        // 旧实现 dirty_payload 派发后不清空，回执钩子误判"有新写入"再次派发，
        // 形成 写入→回执→写入 死循环（会话文件被刷屏）。
        let _g = setup();
        ensure_registered();
        {
            let mut st = state().lock();
            st.enabled = true;
            st.executing = true;
            st.todos = vec![TodoItem {
                step: 1,
                text: "X".to_string(),
                completed: false,
            }];
        }
        persist();
        assert_eq!(take_own_persists(), 1, "首次 persist 应入队一次写入");
        on_persisted(CUSTOM_TYPE);
        assert_eq!(take_own_persists(), 0, "回执后不应再排队写入（回声循环）");
        assert!(!state().lock().write_in_flight);
    }

    /// 构造一个带 pi-plan 条目（未完成 enabled+executing）的会话，落在测试私有 tempdir 下
    fn session_with_plan_entry(dir: &std::path::Path) -> Session {
        let sess_dir = dir.join("sessions-pm");
        let _ = std::fs::create_dir_all(&sess_dir);
        let mut s = Session::create("/tmp", Some(sess_dir.clone()), true).unwrap();
        s.append_custom_entry(
            CUSTOM_TYPE,
            Some(serde_json::json!({
                "version": 1,
                "enabled": true,
                "executing": true,
                "todos": [
                    { "step": 1, "text": "Inspect", "completed": false },
                    { "step": 2, "text": "Verify", "completed": false }
                ]
            })),
        );
        s
    }

    #[test]
    fn session_persist_restore_roundtrip() {
        // 会话内 pi-plan 自定义条目 → on_session_start 恢复 + 历史 [DONE:n] 重扫
        let env = setup();
        let mut s = session_with_plan_entry(env.dir.path());
        // 追加消息模拟"最后一次持久化之后又完成了一步"（崩溃恢复：条目陈旧）
        s.append_message(&AgentMessage::user_text("did [DONE:1]"));
        let msgs = s.build_context_messages();
        let ext = PlanMode::new();
        ext.on_session_start("/tmp", &msgs, Some(&s));
        assert!(is_enabled(), "restored from session entry");
        {
            let st = state().lock();
            assert!(st.executing);
            assert_eq!(st.todos.len(), 2);
            assert!(st.todos[0].completed, "历史 [DONE:1] 重扫修正");
            assert!(!st.todos[1].completed, "未完成步骤保留");
            assert!(st.dock_shown, "resume 自动显示停靠面板");
        }
        // 挂起的 UI：ShowDock（自动展示；恢复提示不打入消息区，由面板承载，避免重复）
        assert!(
            {
                let mut dock = false;
                while let Some(req) = crate::core::extensions::take_pending_ui() {
                    if matches!(req, core::extensions::ExtensionUiRequest::ShowDock) {
                        dock = true;
                    }
                }
                dock
            },
            "应有 ShowDock 请求"
        );
        *state().lock() = State::default();
    }

    #[test]
    fn session_restore_falls_back_to_history_plan() {
        // 无 pi-plan 条目（旧版本会话/写入丢失）：从历史最后一条 Plan: 重建 + [DONE:n]
        let env = setup();
        let sess_dir = env.dir.path().join("sessions-pm");
        let _ = std::fs::create_dir_all(&sess_dir);
        let s = Session::create("/tmp", Some(sess_dir.clone()), true).unwrap();
        let msgs = vec![
            AgentMessage::user_text("analyze"),
            serde_json::from_value::<AgentMessage>(serde_json::json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": "Plan:\n1. Inspect code\n2. Verify result" }]
            }))
            .unwrap(),
            AgentMessage::user_text("[DONE:1]"),
        ];
        let ext = PlanMode::new();
        ext.on_session_start("/tmp", &msgs, Some(&s));
        {
            let st = state().lock();
            assert!(st.executing, "历史兜底视为执行中");
            assert!(!st.enabled);
            assert_eq!(st.todos.len(), 2);
            assert!(st.todos[0].completed, "[DONE:1] 重扫");
            assert!(!st.todos[1].completed);
            assert!(st.dock_shown, "未完成 todo 自动显示");
        }
        while crate::core::extensions::take_pending_ui().is_some() {}
        *state().lock() = State::default();
    }

    #[test]
    fn fresh_session_keeps_cli_plan_flag() {
        // 全新会话（无条目无历史）：on_session_start 不得清掉 --plan CLI flag
        let env = setup();
        let sess_dir = env.dir.path().join("sessions-pm");
        let _ = std::fs::create_dir_all(&sess_dir);
        let s = Session::create("/tmp", Some(sess_dir.clone()), true).unwrap();
        ensure_registered();
        set_enabled(true); // 模拟 --plan flag（apply_cli_flag 先于 on_session_start）
        let ext = PlanMode::new();
        ext.on_session_start("/tmp", &[], Some(&s));
        assert!(is_enabled(), "--plan 开启的会话不得被恢复逻辑关闭");
        set_enabled(false);
        ext.on_session_start("/tmp", &[], Some(&s));
        assert!(!is_enabled(), "普通全新会话保持关闭");
        while crate::core::extensions::take_pending_ui().is_some() {}
        *state().lock() = State::default();
    }

    #[test]
    fn session_switch_restores_or_clears() {
        let env = setup();
        let ext = PlanMode::new();
        // /new（路径 None）：清空内存态
        set_enabled(true);
        {
            let mut st = state().lock();
            st.todos = vec![TodoItem {
                step: 1,
                text: "X".to_string(),
                completed: false,
            }];
        }
        ext.on_session_switched(None, &[]);
        assert!(!is_enabled());
        assert!(state().lock().todos.is_empty());
        assert!(!is_dock_shown());

        // resume 到有 pi-plan 条目的会话：恢复并自动显示
        let s = session_with_plan_entry(env.dir.path());
        let file = s.get_session_file().unwrap().to_string_lossy().to_string();
        let msgs = s.build_context_messages();
        ext.on_session_switched(Some(&file), &msgs);
        assert!(is_enabled());
        assert_eq!(state().lock().todos.len(), 2);
        assert!(is_dock_shown());
        while crate::core::extensions::take_pending_ui().is_some() {}
        *state().lock() = State::default();
    }

    #[test]
    fn cli_flag_and_keybinding_declared() {
        let _g = setup();
        ensure_registered();
        let ext = PlanMode::new();
        let flags = ext.cli_flags();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].name, "plan");
        assert!(!flags[0].description.is_empty());
        // 未启用扩展时 apply 被 set_enabled 守卫拒绝（flag 不注入，防御兜底）
        assert!(crate::core::extensions::set_extension_enabled(
            "plan-mode",
            false
        ));
        ext.apply_cli_flag("plan", true);
        assert!(!is_enabled(), "扩展禁用时 apply_cli_flag 不应开启");
        crate::core::extensions::set_extension_enabled("plan-mode", true);
        ext.apply_cli_flag("plan", true);
        assert!(is_enabled());
        set_enabled(false);

        let kbs = ext.keybindings();
        assert_eq!(kbs.len(), 1);
        assert_eq!(kbs[0].id, "plan-mode.toggle");
        assert!(kbs[0].keys.contains(&"alt+p".to_string()));
    }

    #[test]
    fn interest_predicate_tracks_state() {
        // 兴趣谓词决定框架是否调用本扩展：空闲必须为空（否则等于每轮/每帧空转）。
        let _g = setup();
        assert!(
            PlanMode::new().hooks().is_empty(),
            "空闲状态不应订阅任何回调：{:?}",
            PlanMode::new().hooks()
        );

        // plan 模式：注入提示 + 只读拦截 + agent 事件；停靠段未显示 → 无 Dock
        {
            let mut st = state().lock();
            st.enabled = true;
        }
        let hooks = PlanMode::new().hooks();
        assert!(hooks.contains(&ExtensionHook::TransformContext));
        assert!(hooks.contains(&ExtensionHook::BeforeToolCall));
        assert!(hooks.contains(&ExtensionHook::AgentEvent));
        assert!(
            !hooks.contains(&ExtensionHook::Dock),
            "未显示停靠段不应订阅 Dock"
        );

        // 执行模式：仍注入 + agent 事件 + 可见停靠段，但不再拦截工具
        {
            let mut st = state().lock();
            st.enabled = false;
            st.executing = true;
            st.todos = vec![TodoItem {
                step: 1,
                text: "A".to_string(),
                completed: false,
            }];
            st.dock_shown = true;
        }
        let hooks = PlanMode::new().hooks();
        assert!(hooks.contains(&ExtensionHook::TransformContext));
        assert!(
            !hooks.contains(&ExtensionHook::BeforeToolCall),
            "执行模式恢复完整工具"
        );
        assert!(hooks.contains(&ExtensionHook::AgentEvent));
        assert!(hooks.contains(&ExtensionHook::Dock));
    }

    #[test]
    fn interest_cleanup_is_one_shot_after_disable() {
        // 关闭 plan 模式不能立即退订 TransformContext：需再跑一次以清除旧注入，
        // 清理后自动退订（one-shot）。否则残留的 plan 标记会永久留在上下文里。
        let _g = setup();
        let ext = PlanMode::new();
        ensure_registered();
        set_enabled(true);
        assert!(ext.hooks().contains(&ExtensionHook::TransformContext));

        set_enabled(false);
        assert!(
            state().lock().cleanup_pending,
            "关闭 plan 模式应置一次性清理标志"
        );
        assert!(
            ext.hooks().contains(&ExtensionHook::TransformContext),
            "清理回合仍需订阅 TransformContext"
        );

        let mut msgs = vec![AgentMessage::user_text("hello")];
        ext.transform_context(&mut msgs).unwrap();
        assert!(!state().lock().cleanup_pending, "清理后应清位");
        assert!(
            !ext.hooks().contains(&ExtensionHook::TransformContext),
            "清理回合结束后应退订 TransformContext：{:?}",
            ext.hooks()
        );

        // set_enabled(true)/(false) 各触发一次 persist()：合并写入在途时必须继续
        // 保留 AgentEvent 以收回执（见步骤 6）；回执可能触发下一次合并写入，需排空。
        assert!(state().lock().write_in_flight, "关闭触发落盘 → 在途");
        assert!(ext.hooks().contains(&ExtensionHook::AgentEvent));
        while state().lock().write_in_flight {
            on_persisted(CUSTOM_TYPE);
        }
        assert!(
            ext.hooks().is_empty(),
            "回执排空后彻底空闲：{:?}",
            ext.hooks()
        );
    }

    #[test]
    fn interest_keeps_agent_event_while_persist_in_flight() {
        // 持久化在途时必须继续订阅 AgentEvent，否则收不到 extension:entry_persisted
        // 回执 → write_in_flight 永久卡死、后续写入被吞。
        let _g = setup();
        persist();
        assert!(state().lock().write_in_flight);
        assert!(
            PlanMode::new().hooks().contains(&ExtensionHook::AgentEvent),
            "在途持久化必须订阅 AgentEvent"
        );

        on_persisted(CUSTOM_TYPE);
        assert!(!state().lock().write_in_flight);
        assert!(
            PlanMode::new().hooks().is_empty(),
            "回执后无其它状态应回到空闲：{:?}",
            PlanMode::new().hooks()
        );
    }

    #[test]
    fn interest_dock_requires_todos() {
        let _g = setup();
        {
            let mut st = state().lock();
            st.dock_shown = true; // 显示但无 todo → 不应订阅 Dock
        }
        assert!(!PlanMode::new().hooks().contains(&ExtensionHook::Dock));
        {
            let mut st = state().lock();
            st.todos = vec![TodoItem {
                step: 1,
                text: "A".to_string(),
                completed: false,
            }];
        }
        assert!(PlanMode::new().hooks().contains(&ExtensionHook::Dock));
    }
}
