//! subagent 的可调设置：缺省值、校验、面板档位映射与 JSON 落盘。
//!
//! 持久化在 `agent_dir()/extensions/subagent.json`（首次加载自动创建并回填缺省键，
//! 沿用 loop-detect 的既有约定）。**键名一律 camelCase**，与面板键同名，便于对照。
//!
//! 读取路径：`manager` 在进程内缓存一份配置（首次访问时 `load_config()`），
//! 面板 `apply_setting` 写完盘后经 `manager::reload_config()` 刷新缓存，避免 `dock_lines()` 每帧读盘。

use super::discovery_cwd;
use crate::{
    PROJECT_SCOPE_NAME,
    core::{
        config_cache::ConfigCache, extensions::ExtensionSetting, project_trust::is_project_trusted,
        settings_manager::agent_dir,
    },
    extensions::util::cycle_setting,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
    time::SystemTime,
};
use strum_macros::{EnumString, IntoStaticStr};

/// 配置版本戳
const CONFIG_VERSION: i64 = 1;

/// 配置文件名（位于 `agent_dir()/extensions/` 下）
pub(super) const CONFIG_FILE: &str = "subagent.json";

/// subagent.json 读写锁（面板写盘与各行读配置可能并发）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// 全局层配置缓存（读/写都只在 [`CONFIG_LOCK`] 作用域内进行，
/// 锁序恒为「RwLock → 缓存 Mutex」）。
static GLOBAL_CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 项目层配置缓存。与全局层分开：两层路径不同，共用一个缓存会互相顶掉导致每次重读。
static PROJECT_CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 后台完成通知的分组策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(super) enum JoinMode {
    /// 同轮多个后台代理合并成一条通知（默认；30s 窗口内未齐也发）
    Smart,
    /// 每个代理完成即单独通知
    Async,
    /// 只按显式分组通知（本实现等同 `smart` 但窗口更短）
    Group,
}

impl JoinMode {
    /// 该策略在配置里的字符串（小写名）。
    fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    fn parse(raw: &str) -> Option<Self> {
        raw.trim().parse().ok()
    }

    /// 批处理窗口（毫秒）。
    pub(super) fn window_ms(self) -> u64 {
        match self {
            JoinMode::Smart => 30_000,
            JoinMode::Group => 15_000,
            JoinMode::Async => 0,
        }
    }
}

/// 未知/禁用类型的回退策略（对齐上游 `fallbackSubagent`）。
///
/// 变体名与字符串的互转由 `strum` 派生（小写、大小写不敏感）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(super) enum FallbackMode {
    /// 回退到 `general-purpose` 并在结果里说明（默认，对齐上游）
    #[strum(serialize = "general-purpose")]
    GeneralPurpose,
    /// 不回退：直接报错（严格模式）
    None,
}

impl FallbackMode {
    /// 该策略在配置里的字符串（`general-purpose` 或 `none`）。
    fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    fn parse(raw: &str) -> Option<Self> {
        raw.trim().parse().ok()
    }
}

/// `Agent` 工具描述模式（对齐上游 `toolDescriptionMode`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(super) enum ToolDescriptionMode {
    /// 完整说明（默认）
    Full,
    /// 精简说明（小模型友好；细节留在各参数 description 里）
    Compact,
    /// 从 `agent-tool-description.md` 读取自定义模板；文件缺失时回退 full 并告警
    Custom,
}

impl ToolDescriptionMode {
    /// 该模式在配置里的字符串（小写名）。
    fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    fn parse(raw: &str) -> Option<Self> {
        raw.trim().parse().ok()
    }
}

/// 常驻 widget 的可见范围
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(super) enum WidgetMode {
    /// 运行中 + 最近完成的全部代理
    All,
    /// 只显示后台代理（前台代理在这一回合内已有 inline 结果）
    Background,
    /// 不显示
    Off,
}

impl WidgetMode {
    /// 该模式在配置里的字符串（小写名）。
    fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    fn parse(raw: &str) -> Option<Self> {
        raw.trim().parse().ok()
    }
}

/// `@mention` 模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(super) enum AgentMentions {
    /// 不拦截 `@handle` 输入
    Off,
    /// 直接路由到子代理（steer / resume / spawn）
    Direct,
    /// 上游的 clone 模式：把当前对话克隆进一次性会话，屏幕外写 prompt 并后台 spawn
    Model,
}

impl AgentMentions {
    /// 该模式在配置里的字符串（小写名）。
    pub(super) fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    fn parse(raw: &str) -> Option<Self> {
        raw.trim().parse().ok()
    }

    /// 是否启用句柄拦截（`off` 之外都启用；`model` 降级为 direct）。
    pub(super) fn enabled(self) -> bool {
        !matches!(self, AgentMentions::Off)
    }
}

/// 工作流（`SubagentWorkflow`）的可用策略。
///
/// - `Auto`（缺省）：发现有**别的扩展**提供 `Workflow`/`workflow` 工具时**让位**；
/// - `On`：不让位（选择不让位），只在冲突时报告一次；
/// - `Off`：从不提供工作流（工具、子命令、运行注册表都不参与）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(super) enum WorkflowsMode {
    /// 缺省：发现别的扩展已提供工作流工具时让位
    Auto,
    /// 始终提供工作流，不让位（仅在冲突时报告一次）
    On,
    /// 从不提供工作流（工具、子命令与运行注册表都不参与）
    Off,
}

impl WorkflowsMode {
    /// 该模式在配置里的字符串（小写名）。
    pub(super) fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    fn parse(raw: &str) -> Option<Self> {
        raw.trim().parse().ok()
    }
}

/// 运行期配置（全部字段都有缺省，缺键/非法值回退缺省）。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct SubagentConfig {
    /// 常驻 widget 显示范围
    pub widget: WidgetMode,
    /// `Agent` 不写 `run_in_background` 时的默认值
    pub background_default: bool,
    /// 后台并发上限
    pub max_concurrent: usize,
    /// 前台并发上限（0 = 不限）
    pub foreground_max_concurrent: usize,
    /// 后台完成通知的分组策略
    pub join: JoinMode,
    /// `Agent` 工具描述模式
    pub tool_description: ToolDescriptionMode,
    /// 未知/禁用类型是否回退 `general-purpose`
    pub fallback_subagent: FallbackMode,
    /// agent 文件解析失败是否视为致命（false = 跳过 + 告警）
    pub strict_agent_files: bool,
    /// 是否不注册内嵌默认三类型
    pub disable_default_agents: bool,
    /// 是否为子代理写 `.output` 运行转写（JSONL）
    pub output_transcript: bool,
    /// widget / 结果行显示 token 用量
    pub show_tokens: bool,
    /// widget / 结果行显示估算成本
    pub show_cost: bool,
    /// widget / 结果行显示子代理实际生效的型号
    pub show_model: bool,
    /// 把子代理开销计入主会话（缺省关；对齐上游 `reportUsage`）
    pub report_usage: bool,
    /// `@handle` 输入拦截模式
    pub agent_mentions: AgentMentions,
    /// 工作流可用策略（见 [`WorkflowsMode`]）
    pub workflows: WorkflowsMode,
    /// 是否把子代理的型号限制在 `enabledModels` 之内（缺省关）
    pub scope_models: bool,
    /// 定时子代理总开关（缺省开）
    pub schedule: bool,
    /// 是否允许 `isolation: worktree` 复制仓库（缺省开；关掉即"任何路径都不建副本"）
    pub worktree_isolation: bool,
    /// 嵌套上限：主会话 0、它的子代理 1、孙代理 2（缺省 2；0/1 = 关掉嵌套）
    pub max_subagent_depth: u32,
    /// 是否在 `/agents` 菜单里提供 FleetView 入口（缺省开）
    pub fleet_view: bool,
    /// 子代理是否默认落盘会话（缺省开）。frontmatter `persist_session` 覆盖它。
    pub remember_agents: bool,
    /// 未显式给 `max_turns` 时的默认上限（0 = 不限）
    pub default_max_turns: u32,
    /// graceful max_turns 的收尾宽限轮数（缺省 5）
    pub grace_turns: u32,
}

impl Default for SubagentConfig {
    /// 全部字段的缺省值：无配置文件或某键非法时按此回退。
    fn default() -> Self {
        SubagentConfig {
            widget: WidgetMode::All,
            background_default: true,
            max_concurrent: 10,
            foreground_max_concurrent: 0,
            join: JoinMode::Smart,
            tool_description: ToolDescriptionMode::Full,
            fallback_subagent: FallbackMode::GeneralPurpose,
            strict_agent_files: false,
            disable_default_agents: false,
            output_transcript: false,
            show_tokens: true,
            show_cost: false,
            show_model: false,
            report_usage: false,
            agent_mentions: AgentMentions::Model,
            workflows: WorkflowsMode::Auto,
            scope_models: false,
            schedule: true,
            worktree_isolation: true,
            max_subagent_depth: 2,
            fleet_view: true,
            remember_agents: true,
            default_max_turns: 0,
            grace_turns: 5,
        }
    }
}

/// 面板项（顺序即面板显示顺序）。
///
/// 逐项用 [`cycle_setting`] 现构 [`ExtensionSetting`]：本扩展的面板键就是配置键，
/// 当前值由 [`panel_value`] 直接读出，故不再需要一份中间规格表。
pub(super) fn panel_settings(cfg: &SubagentConfig) -> Vec<ExtensionSetting> {
    vec![
        cycle_setting(
            "widget",
            "Dock widget",
            "Which sub-agents the dock widget lists: every agent, background-only, or none",
            panel_value(cfg, "widget"),
            &["all", "background", "off"],
        ),
        cycle_setting(
            "backgroundDefault",
            "Background by default",
            "Run Agent calls detached unless run_in_background: false is passed explicitly",
            panel_value(cfg, "backgroundDefault"),
            &["on", "off"],
        ),
        cycle_setting(
            "maxConcurrent",
            "Background limit",
            "How many background agents may run at once (further spawns queue FIFO)",
            panel_value(cfg, "maxConcurrent"),
            &["1", "2", "4", "6", "8", "10", "16", "32"],
        ),
        cycle_setting(
            "foregroundMaxConcurrent",
            "Foreground limit",
            "How many foreground agents may run at once (0 = unlimited)",
            panel_value(cfg, "foregroundMaxConcurrent"),
            &["0", "1", "2", "4", "8"],
        ),
        cycle_setting(
            "join",
            "Group notifications",
            "Batch background completions into one notification (smart: 30s window, group: 15s, async: one per agent)",
            panel_value(cfg, "join"),
            &["smart", "group", "async"],
        ),
        cycle_setting(
            "toolDescription",
            "Tool description",
            "Agent tool description: full, compact (small models), or custom (agent-tool-description.md)",
            panel_value(cfg, "toolDescription"),
            &["full", "compact", "custom"],
        ),
        cycle_setting(
            "fallbackSubagent",
            "Unknown type fallback",
            "What to do when subagent_type is unknown/disabled (none = fail instead of falling back)",
            panel_value(cfg, "fallbackSubagent"),
            &["general-purpose", "none"],
        ),
        cycle_setting(
            "strictAgentFiles",
            "Strict agent files",
            "Treat an unparseable agent .md as a fatal error instead of skipping it with a warning",
            panel_value(cfg, "strictAgentFiles"),
            &["off", "on"],
        ),
        cycle_setting(
            "disableDefaultAgents",
            "Disable defaults",
            "Do not register the built-in general-purpose / Explore / Plan types",
            panel_value(cfg, "disableDefaultAgents"),
            &["off", "on"],
        ),
        cycle_setting(
            "outputTranscript",
            "Output transcript",
            "Write a JSONL run transcript per sub-agent (path is reported in the result card)",
            panel_value(cfg, "outputTranscript"),
            &["off", "on"],
        ),
        cycle_setting(
            "showTokens",
            "Show tokens",
            "Show token usage in the dock widget and result lines",
            panel_value(cfg, "showTokens"),
            &["on", "off"],
        ),
        cycle_setting(
            "showCost",
            "Show cost",
            "Show the provider-reported cost estimate in the dock widget and result lines",
            panel_value(cfg, "showCost"),
            &["on", "off"],
        ),
        cycle_setting(
            "reportUsage",
            "Report usage",
            "Count sub-agent spend towards the parent session's /session totals (best-effort)",
            panel_value(cfg, "reportUsage"),
            &["off", "on"],
        ),
        cycle_setting(
            "showModel",
            "Show model",
            "Show the model each sub-agent actually ran on in the dock widget and result lines",
            panel_value(cfg, "showModel"),
            &["off", "on"],
        ),
        cycle_setting(
            "agentMentions",
            "Agent mentions",
            "Route @handle messages to a sub-agent (direct = send to it; model = clone this conversation and let the clone write the prompt off-screen)",
            panel_value(cfg, "agentMentions"),
            &["off", "direct", "model"],
        ),
        cycle_setting(
            "maxSubagentDepth",
            "Max subagent depth",
            "How deep nesting may go: main session = 0, its subagents = 1, their children = 2 (0 or 1 disables nesting)",
            panel_value(cfg, "maxSubagentDepth"),
            &["0", "1", "2", "3", "4"],
        ),
        cycle_setting(
            "worktreeIsolation",
            "Worktree isolation",
            "Allow isolation: worktree to copy the repo (off refuses worktrees everywhere and drops the `isolation` param)",
            panel_value(cfg, "worktreeIsolation"),
            &["on", "off"],
        ),
        cycle_setting(
            "schedule",
            "Scheduling",
            "Allow registering scheduled sub-agent jobs (Agent's `schedule` param + /agents → Scheduled jobs)",
            panel_value(cfg, "schedule"),
            &["on", "off"],
        ),
        cycle_setting(
            "scopeModels",
            "Scope models",
            "Restrict sub-agent models to enabledModels: a caller-supplied out-of-scope model is refused, an agent file's pinned one only warns",
            panel_value(cfg, "scopeModels"),
            &["off", "on"],
        ),
        cycle_setting(
            "workflows",
            "Workflows",
            "SubagentWorkflow availability (auto = stand down if another extension offers a workflow tool; on = keep both; off = never offer)",
            panel_value(cfg, "workflows"),
            &["auto", "on", "off"],
        ),
        cycle_setting(
            "fleetView",
            "Fleet view",
            "Offer the FleetView entry in the /agents menu (navigable main + subagent list)",
            panel_value(cfg, "fleetView"),
            &["on", "off"],
        ),
        cycle_setting(
            "rememberAgents",
            "Remember agents",
            "Persist subagent sessions by default so @handle can reopen them later (frontmatter persist_session overrides)",
            panel_value(cfg, "rememberAgents"),
            &["on", "off"],
        ),
        cycle_setting(
            "defaultMaxTurns",
            "Default max turns",
            "Turn cap for agents that do not set max_turns (0 = unlimited)",
            panel_value(cfg, "defaultMaxTurns"),
            &["0", "5", "10", "20", "50"],
        ),
        cycle_setting(
            "graceTurns",
            "Grace turns",
            "Extra wrap-up turns allowed after an agent hits its turn cap before it is stopped hard",
            panel_value(cfg, "graceTurns"),
            &["0", "1", "2", "3", "5", "10"],
        ),
    ]
}

/// 合法取值集合（配置键 → 允许的字符串值；未列出的键不做枚举校验）。
const ENUM_KEYS: &[(&str, &[&str])] = &[
    ("widget", &["all", "background", "off"]),
    ("backgroundDefault", &["on", "off"]),
    (
        "maxConcurrent",
        &["1", "2", "4", "6", "8", "10", "16", "32"],
    ),
    ("foregroundMaxConcurrent", &["0", "1", "2", "4", "8"]),
    ("join", &["smart", "group", "async"]),
    ("toolDescription", &["full", "compact", "custom"]),
    ("fallbackSubagent", &["general-purpose", "none"]),
    ("strictAgentFiles", &["off", "on"]),
    ("disableDefaultAgents", &["off", "on"]),
    ("outputTranscript", &["off", "on"]),
    ("showTokens", &["on", "off"]),
    ("showCost", &["on", "off"]),
    ("reportUsage", &["off", "on"]),
    ("showModel", &["off", "on"]),
    ("agentMentions", &["off", "direct", "model"]),
    ("workflows", &["auto", "on", "off"]),
    ("scopeModels", &["off", "on"]),
    ("schedule", &["on", "off"]),
    ("worktreeIsolation", &["on", "off"]),
    ("maxSubagentDepth", &["0", "1", "2", "3", "4"]),
    ("fleetView", &["on", "off"]),
    ("rememberAgents", &["on", "off"]),
    ("defaultMaxTurns", &["0", "5", "10", "20", "50"]),
    ("graceTurns", &["0", "1", "2", "3", "5", "10"]),
];

/// 当前配置 → 面板值（枚举项原样返回）。
pub(super) fn panel_value(cfg: &SubagentConfig, key: &str) -> String {
    match key {
        "widget" => cfg.widget.as_str().to_string(),
        "backgroundDefault" => on_off(cfg.background_default).to_string(),
        "maxConcurrent" => cfg.max_concurrent.to_string(),
        "foregroundMaxConcurrent" => cfg.foreground_max_concurrent.to_string(),
        "join" => cfg.join.as_str().to_string(),
        "toolDescription" => cfg.tool_description.as_str().to_string(),
        "fallbackSubagent" => cfg.fallback_subagent.as_str().to_string(),
        "strictAgentFiles" => on_off(cfg.strict_agent_files).to_string(),
        "disableDefaultAgents" => on_off(cfg.disable_default_agents).to_string(),
        "outputTranscript" => on_off(cfg.output_transcript).to_string(),
        "showTokens" => on_off(cfg.show_tokens).to_string(),
        "showCost" => on_off(cfg.show_cost).to_string(),
        "reportUsage" => on_off(cfg.report_usage).to_string(),
        "showModel" => on_off(cfg.show_model).to_string(),
        "agentMentions" => cfg.agent_mentions.as_str().to_string(),
        "workflows" => cfg.workflows.as_str().to_string(),
        "scopeModels" => on_off(cfg.scope_models).to_string(),
        "schedule" => on_off(cfg.schedule).to_string(),
        "worktreeIsolation" => on_off(cfg.worktree_isolation).to_string(),
        "maxSubagentDepth" => cfg.max_subagent_depth.to_string(),
        "fleetView" => on_off(cfg.fleet_view).to_string(),
        "rememberAgents" => on_off(cfg.remember_agents).to_string(),
        "defaultMaxTurns" => cfg.default_max_turns.to_string(),
        "graceTurns" => cfg.grace_turns.to_string(),
        _ => String::new(),
    }
}

/// 应用面板选择（写回配置）。
pub(super) fn apply_panel_choice(
    cfg: &mut SubagentConfig,
    key: &str,
    value: &str,
) -> Result<(), String> {
    if let Some((_, allowed)) = ENUM_KEYS.iter().find(|(k, _)| *k == key)
        && !allowed.contains(&value)
    {
        return Err(format!("invalid option for {key}: {value}"));
    }

    match key {
        "widget" => cfg.widget = WidgetMode::parse(value).ok_or_else(|| bad(key, value))?,
        "backgroundDefault" => {
            cfg.background_default = parse_on_off(value).ok_or_else(|| bad(key, value))?
        }
        "maxConcurrent" => {
            cfg.max_concurrent = parse_count(value).ok_or_else(|| bad(key, value))?
        }
        "foregroundMaxConcurrent" => {
            cfg.foreground_max_concurrent = parse_count(value).ok_or_else(|| bad(key, value))?
        }
        "join" => cfg.join = JoinMode::parse(value).ok_or_else(|| bad(key, value))?,
        "toolDescription" => {
            cfg.tool_description =
                ToolDescriptionMode::parse(value).ok_or_else(|| bad(key, value))?
        }
        "fallbackSubagent" => {
            cfg.fallback_subagent = FallbackMode::parse(value).ok_or_else(|| bad(key, value))?
        }
        "strictAgentFiles" => {
            cfg.strict_agent_files = parse_on_off(value).ok_or_else(|| bad(key, value))?
        }
        "disableDefaultAgents" => {
            cfg.disable_default_agents = parse_on_off(value).ok_or_else(|| bad(key, value))?
        }
        "outputTranscript" => {
            cfg.output_transcript = parse_on_off(value).ok_or_else(|| bad(key, value))?
        }
        "showTokens" => cfg.show_tokens = parse_on_off(value).ok_or_else(|| bad(key, value))?,
        "showCost" => cfg.show_cost = parse_on_off(value).ok_or_else(|| bad(key, value))?,
        "reportUsage" => cfg.report_usage = parse_on_off(value).ok_or_else(|| bad(key, value))?,
        "showModel" => cfg.show_model = parse_on_off(value).ok_or_else(|| bad(key, value))?,
        "agentMentions" => {
            cfg.agent_mentions = AgentMentions::parse(value).ok_or_else(|| bad(key, value))?
        }
        "workflows" => {
            cfg.workflows = WorkflowsMode::parse(value).ok_or_else(|| bad(key, value))?
        }
        "scopeModels" => cfg.scope_models = parse_on_off(value).ok_or_else(|| bad(key, value))?,
        "schedule" => cfg.schedule = parse_on_off(value).ok_or_else(|| bad(key, value))?,
        "worktreeIsolation" => {
            cfg.worktree_isolation = parse_on_off(value).ok_or_else(|| bad(key, value))?
        }
        "maxSubagentDepth" => {
            cfg.max_subagent_depth = parse_count(value).ok_or_else(|| bad(key, value))? as u32
        }
        "fleetView" => cfg.fleet_view = parse_on_off(value).ok_or_else(|| bad(key, value))?,
        "rememberAgents" => {
            cfg.remember_agents = parse_on_off(value).ok_or_else(|| bad(key, value))?
        }
        "defaultMaxTurns" => {
            cfg.default_max_turns = parse_count(value).ok_or_else(|| bad(key, value))? as u32
        }
        "graceTurns" => cfg.grace_turns = parse_count(value).ok_or_else(|| bad(key, value))? as u32,
        other => return Err(format!("unknown setting: {other}")),
    }
    Ok(())
}

/// 生成「非法配置值」错误文本（含键名与原始值），用于设置面板逐键报错。
fn bad(key: &str, value: &str) -> String {
    format!("invalid option for {key}: {value}")
}

/// 布尔值的配置字符串（`on` / `off`）。
fn on_off(v: bool) -> &'static str {
    if v { "on" } else { "off" }
}

/// 解析布尔配置值（`on`/`true`/`off`/`false`，大小写不敏感）；其它输入返回 `None`。
fn parse_on_off(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" => Some(true),
        "off" | "false" => Some(false),
        _ => None,
    }
}

/// 解析非负整数配置值；非数字返回 `None`。
fn parse_count(value: &str) -> Option<usize> {
    value.trim().parse::<usize>().ok()
}

impl SubagentConfig {
    /// 序列化为 JSON（键名小写；面板值是枚举标签而不是内部数值）。
    fn to_json(&self) -> Value {
        json!({
            "widget": self.widget.as_str(),
            "backgroundDefault": self.background_default,
            "maxConcurrent": self.max_concurrent,
            "foregroundMaxConcurrent": self.foreground_max_concurrent,
            "join": self.join.as_str(),
            "toolDescription": self.tool_description.as_str(),
            "fallbackSubagent": self.fallback_subagent.as_str(),
            "strictAgentFiles": self.strict_agent_files,
            "disableDefaultAgents": self.disable_default_agents,
            "outputTranscript": self.output_transcript,
            "showTokens": self.show_tokens,
            "showCost": self.show_cost,
            "reportUsage": self.report_usage,
            "showModel": self.show_model,
            "agentMentions": self.agent_mentions.as_str(),
            "workflows": self.workflows.as_str(),
            "scopeModels": self.scope_models,
            "schedule": self.schedule,
            "worktreeIsolation": self.worktree_isolation,
            "maxSubagentDepth": self.max_subagent_depth,
            "fleetView": self.fleet_view,
            "rememberAgents": self.remember_agents,
            "defaultMaxTurns": self.default_max_turns,
            "graceTurns": self.grace_turns,
            "configVersion": CONFIG_VERSION,
        })
    }
}

/// 缺省 + 文件覆盖合并（非法值逐键回退缺省）。
pub(super) fn merge_config(from_file: Option<&Value>) -> SubagentConfig {
    let mut cfg = SubagentConfig::default();
    if let Some(file) = from_file {
        apply_layer(&mut cfg, file);
    }
    cfg
}

/// 把一层配置盖到当前值上（**已设的键**优先于更下层的值；非法值跳过 = 保留下层）。
pub(super) fn apply_layer(cfg: &mut SubagentConfig, from_file: &Value) {
    let Some(file) = from_file.as_object() else {
        return;
    };

    if let Some(mode) = file
        .get("widget")
        .and_then(|v| v.as_str())
        .and_then(WidgetMode::parse)
    {
        cfg.widget = mode;
    }
    if let Some(v) = file.get("backgroundDefault").and_then(|v| v.as_bool()) {
        cfg.background_default = v;
    }
    if let Some(v) = file.get("maxConcurrent").and_then(count_from_json) {
        cfg.max_concurrent = v;
    }
    if let Some(v) = file
        .get("foregroundMaxConcurrent")
        .and_then(count_from_json)
    {
        cfg.foreground_max_concurrent = v;
    }
    if let Some(v) = file
        .get("join")
        .and_then(|v| v.as_str())
        .and_then(JoinMode::parse)
    {
        cfg.join = v;
    }
    if let Some(v) = file
        .get("toolDescription")
        .and_then(|v| v.as_str())
        .and_then(ToolDescriptionMode::parse)
    {
        cfg.tool_description = v;
    }
    if let Some(v) = file
        .get("fallbackSubagent")
        .and_then(|v| v.as_str())
        .and_then(FallbackMode::parse)
    {
        cfg.fallback_subagent = v;
    }
    if let Some(v) = file
        .get("workflows")
        .and_then(|v| v.as_str())
        .and_then(WorkflowsMode::parse)
    {
        cfg.workflows = v;
    }
    if let Some(v) = file.get("scopeModels").and_then(|v| v.as_bool()) {
        cfg.scope_models = v;
    }
    if let Some(v) = file.get("schedule").and_then(|v| v.as_bool()) {
        cfg.schedule = v;
    }
    if let Some(v) = file.get("worktreeIsolation").and_then(|v| v.as_bool()) {
        cfg.worktree_isolation = v;
    }
    if let Some(v) = file.get("maxSubagentDepth").and_then(count_from_json) {
        cfg.max_subagent_depth = v as u32;
    }
    if let Some(v) = file.get("fleetView").and_then(|v| v.as_bool()) {
        cfg.fleet_view = v;
    }
    if let Some(v) = file.get("rememberAgents").and_then(|v| v.as_bool()) {
        cfg.remember_agents = v;
    }
    if let Some(v) = file.get("defaultMaxTurns").and_then(count_from_json) {
        cfg.default_max_turns = v as u32;
    }
    if let Some(v) = file.get("graceTurns").and_then(count_from_json) {
        cfg.grace_turns = v as u32;
    }
    for (key, slot) in [
        ("strictAgentFiles", &mut cfg.strict_agent_files),
        ("disableDefaultAgents", &mut cfg.disable_default_agents),
        ("outputTranscript", &mut cfg.output_transcript),
    ] {
        if let Some(v) = file.get(key).and_then(|v| v.as_bool()) {
            *slot = v;
        }
    }
    if let Some(v) = file.get("showTokens").and_then(|v| v.as_bool()) {
        cfg.show_tokens = v;
    }
    if let Some(v) = file.get("showCost").and_then(|v| v.as_bool()) {
        cfg.show_cost = v;
    }
    if let Some(v) = file.get("reportUsage").and_then(|v| v.as_bool()) {
        cfg.report_usage = v;
    }
    if let Some(v) = file.get("showModel").and_then(|v| v.as_bool()) {
        cfg.show_model = v;
    }
    if let Some(v) = file
        .get("agentMentions")
        .and_then(|v| v.as_str())
        .and_then(AgentMentions::parse)
    {
        cfg.agent_mentions = v;
    }
}

/// 非负整数（非数/负数/浮点一律视为非法）。
fn count_from_json(v: &Value) -> Option<usize> {
    let n = v.as_f64()?;
    if n < 0.0 || n.fract() != 0.0 || n > u32::MAX as f64 {
        return None;
    }
    Some(n as usize)
}

/// 配置目录：`agent_dir()/extensions/`
pub(super) fn config_path() -> PathBuf {
    agent_dir().join("extensions").join(CONFIG_FILE)
}

/// 项目级配置路径（`<cwd>/.prux/extensions/subagent.json`）。
///
/// **受项目信任门控**：项目里的文件不该在未受信任的仓库里悄悄改掉子代理的行为
/// （模型、并发、是否自动后台都从这里来）。未受信任时为 `None`（只有全局层生效）。
pub(super) fn project_config_path() -> Option<PathBuf> {
    let cwd = discovery_cwd();
    let cwd_path = Path::new(&cwd);
    let agent_dir = agent_dir();

    if !is_project_trusted(cwd_path, &agent_dir) {
        return None;
    }

    let path = cwd_path
        .join(PROJECT_SCOPE_NAME)
        .join("extensions")
        .join(CONFIG_FILE);

    path.is_file().then_some(path)
}

/// 配置缓存键：两层的（路径, mtime）——任一层变化都要重读。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ConfigKey {
    /// 全局配置文件路径 + mtime。
    pub global: (PathBuf, Option<SystemTime>),
    /// 项目级配置（未受信/不存在为 None）。
    pub project: Option<(PathBuf, Option<SystemTime>)>,
}

/// 当前两层的缓存键。
pub(super) fn config_key() -> ConfigKey {
    let project = project_config_path();
    ConfigKey {
        global: (config_path(), mtime_of(&config_path())),
        project: project.map(|p| {
            let m = mtime_of(&p);
            (p, m)
        }),
    }
}

/// 文件的修改时间，用于 [`ConfigCache`] 判定是否需要重读；元数据或 mtime 不可用时返回 `None`。
fn mtime_of(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// 首次启用且无配置文件时落盘默认值（已存在则绝不覆盖）：方便用户对照增删。
pub(super) fn ensure_config_file() {
    let path = config_path();
    if path.exists() {
        return;
    }
    write_config(&path, &SubagentConfig::default());
}

/// 从磁盘读取配置（不写盘；缺键/非法值逐键回退**下一层**）。
///
/// 两层：全局 → 项目（受信任门控后）。**逐键合并**，
/// 项目里写坏的某一项回退到全局那一项（而不是回退缺省）——上层文件只覆盖它真写了的键。
/// 各层文件读盘走各自的 [`ConfigCache`]，命中免读盘/解析。
pub(super) fn load_config() -> SubagentConfig {
    let mut cfg = merge_config(read_json(&config_path(), &GLOBAL_CONFIG_CACHE).as_ref());
    if let Some(path) = project_config_path()
        && let Some(project) = read_json(&path, &PROJECT_CONFIG_CACHE)
    {
        apply_layer(&mut cfg, &project);
    }
    cfg
}

/// 读一层的 JSON 对象（不存在/非法/非对象 → `None`）。走 `cache`（两层各一个，互不顶掉）。
fn read_json(path: &Path, cache: &ConfigCache) -> Option<Value> {
    let _guard = CONFIG_LOCK.read().unwrap_or_else(|e| e.into_inner());
    let value = cache.read(path, || {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(|v| v.is_object())
            .unwrap_or(Value::Null)
    });
    (!value.is_null()).then_some(value)
}

/// 写盘（键名一律 camelCase）。落盘成功后回填对应层的缓存（取写盘后的元数据）。
pub(super) fn write_config(path: &Path, cfg: &SubagentConfig) {
    let _guard = CONFIG_LOCK.write().unwrap_or_else(|e| e.into_inner());

    if let Some(parent) = path.parent() {
        _ = fs::create_dir_all(parent);
    }

    if let Ok(text) = serde_json::to_string_pretty(&cfg.to_json())
        && fs::write(path, format!("{text}\n")).is_ok()
    {
        let cache = if path == config_path() {
            &GLOBAL_CONFIG_CACHE
        } else {
            &PROJECT_CONFIG_CACHE
        };
        cache.store(path, cfg.to_json());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    /// strum 派生的双向转换：`as_str` 是规范名、`parse` 去空白且大小写不敏感。
    #[test]
    fn strum_enum_conversions() {
        assert_eq!(JoinMode::Smart.as_str(), "smart");
        assert_eq!(JoinMode::parse(" Group "), Some(JoinMode::Group));
        assert_eq!(
            WidgetMode::parse("BACKGROUND"),
            Some(WidgetMode::Background)
        );
        assert_eq!(
            ToolDescriptionMode::parse("compact"),
            Some(ToolDescriptionMode::Compact)
        );
        assert_eq!(FallbackMode::GeneralPurpose.as_str(), "general-purpose");
        assert_eq!(
            FallbackMode::parse("General-Purpose"),
            Some(FallbackMode::GeneralPurpose)
        );
        assert_eq!(AgentMentions::parse("direct"), Some(AgentMentions::Direct));
        assert_eq!(WorkflowsMode::parse("auto"), Some(WorkflowsMode::Auto));
        assert_eq!(JoinMode::parse("nope"), None);
    }

    #[test]
    fn defaults_match_documented_values() {
        let cfg = SubagentConfig::default();
        assert_eq!(cfg.widget, WidgetMode::All);
        assert!(cfg.background_default, "Tier2 起默认后台");
        assert_eq!(cfg.max_concurrent, 10);
        assert_eq!(cfg.foreground_max_concurrent, 0);
        assert_eq!(cfg.join, JoinMode::Smart);
        assert!(cfg.show_tokens);
        assert!(!cfg.show_cost);
        assert!(
            !cfg.report_usage,
            "report_usage 默认关（对齐上游 reportUsage=false）"
        );
        assert!(
            !cfg.show_model,
            "show_model 默认关（对齐上游 showModel=false）"
        );
    }

    #[test]
    fn merge_ignores_invalid_values_per_key() {
        let cfg = merge_config(Some(&json!({
            "widget": "nope",
            "backgroundDefault": "yes",
            "maxConcurrent": -3,
            "foregroundMaxConcurrent": 2.5,
            "showTokens": false,
            "showCost": true,
        })));
        assert_eq!(cfg.widget, WidgetMode::All, "非法枚举回退缺省");
        assert!(cfg.background_default, "非布尔回退缺省");
        assert_eq!(cfg.max_concurrent, 10, "负数回退缺省");
        assert_eq!(cfg.foreground_max_concurrent, 0, "浮点回退缺省");
        assert!(!cfg.show_tokens);
        assert!(cfg.show_cost);
    }

    #[test]
    fn panel_choices_round_trip_through_json() {
        let mut cfg = SubagentConfig::default();
        for (key, _) in ENUM_KEYS {
            let value = panel_value(&cfg, key);
            assert!(!value.is_empty(), "{key} 应有面板值");
            apply_panel_choice(&mut cfg, key, &value).expect("面板值应可回写");
        }
        // 面板值写盘后再读回，语义不变
        let round = merge_config(Some(&cfg.to_json()));
        assert_eq!(round, cfg);

        assert!(apply_panel_choice(&mut cfg, "widget", "sometimes").is_err());
        assert!(apply_panel_choice(&mut cfg, "nope", "all").is_err());
        // 面板只接受声明的档位；未知键必须报错
        assert!(apply_panel_choice(&mut cfg, "maxConcurrent", "3").is_err());
        assert!(apply_panel_choice(&mut cfg, "maxConcurrent", "4").is_ok());
    }

    #[test]
    fn panel_values_cover_every_declared_setting() {
        let cfg = SubagentConfig::default();
        let settings = panel_settings(&cfg);
        for s in &settings {
            assert!(ENUM_KEYS.iter().any(|(k, _)| *k == s.key), "{}", s.key);
            assert!(!s.value.is_empty(), "{} 应有面板值", s.key);
            assert!(
                s.choices.contains(&s.value),
                "{} 的当前值 {} 应在档位表里",
                s.key,
                s.value
            );
        }
        assert_eq!(settings.len(), ENUM_KEYS.len());
    }

    /// 缓存以 `(path, mtime, len)` 为有效键：外部（非 `write_config`）改写/删除后，
    /// 下一次 `load_config` 必须看到新值。
    #[test]
    fn config_cache_reflects_external_edits() {
        let _ad = AgentDirGuard::temp();
        let path = config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        fs::write(&path, r#"{"widget":"off"}"#).unwrap();
        assert_eq!(load_config().widget, WidgetMode::Off);

        // 外部改写（长度也不同）→ 缓存失效，读到新值
        fs::write(&path, r#"{"widget":"background","maxConcurrent":4}"#).unwrap();
        let cfg = load_config();
        assert_eq!(cfg.widget, WidgetMode::Background);
        assert_eq!(cfg.max_concurrent, 4);

        // 删除后回落缺省
        fs::remove_file(&path).unwrap();
        assert_eq!(load_config(), SubagentConfig::default());
    }

    #[test]
    fn load_merges_file_and_ensure_writes_defaults_once() {
        let _ad = AgentDirGuard::temp();
        // 无文件时读到的就是缺省（不产生副作用）
        let path = config_path();
        assert_eq!(load_config(), SubagentConfig::default());
        assert!(!path.exists(), "load_config 不写盘");

        ensure_config_file();
        assert!(path.exists(), "首次启用应落盘默认值: {}", path.display());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"widget\": \"all\""), "{text}");
        assert!(
            text.contains("\"configVersion\""),
            "键名必须 camelCase: {text}"
        );

        // 已存在时不覆盖用户改动
        let mut changed = SubagentConfig {
            widget: WidgetMode::Off,
            ..Default::default()
        };
        write_config(&path, &changed);
        ensure_config_file();
        assert_eq!(load_config(), changed);

        // 面板变更 → 写盘 → 重新加载保持一致
        apply_panel_choice(&mut changed, "maxConcurrent", "4").unwrap();
        write_config(&path, &changed);
        assert_eq!(load_config(), changed);
        assert!(mtime_of(&config_path()).is_some(), "写入后应有 mtime");
    }

    /// 项目层（`.prux/extensions/subagent.json`）**逐键**覆盖全局层：
    /// 只盖它写了的键；写坏的键回退到全局那一项（不是回退缺省）；未受信任时整层不生效。
    #[test]
    fn project_layer_overrides_global_key_by_key() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let project_dir = cwd.join(crate::PROJECT_SCOPE_NAME).join("extensions");
        fs::create_dir_all(&project_dir).unwrap();

        // 全局：widget=off、show_cost=true
        let global = SubagentConfig {
            widget: WidgetMode::Off,
            show_cost: true,
            ..Default::default()
        };
        write_config(&config_path(), &global);

        // 项目：改了 widget 与 max_concurrent，另外塞了一个非法值
        fs::write(
            project_dir.join(CONFIG_FILE),
            r#"{"widget":"background","maxConcurrent":3,"showTokens":"nonsense"}"#,
        )
        .unwrap();

        // 会话 cwd 指向项目（`project_config_path` 用它）
        crate::core::extensions::Extension::on_session_start(
            &crate::extensions::subagent::Subagent,
            cwd.to_str().unwrap(),
            &[],
            None,
        );

        // 未受信任：项目层不参与
        assert!(project_config_path().is_none(), "未受信任时不该读项目层");
        let cfg = load_config();
        assert_eq!(cfg.widget, WidgetMode::Off, "只剩全局层");
        assert_eq!(cfg.max_concurrent, 10);

        // 信任后：逐键合并
        crate::core::project_trust::set_project_trust(&cwd, &agent_dir(), true);
        assert!(project_config_path().is_some());
        let cfg = load_config();
        assert_eq!(cfg.widget, WidgetMode::Background, "项目覆盖全局");
        assert_eq!(cfg.max_concurrent, 3, "项目改了并发");
        assert!(cfg.show_cost, "项目没写的键保留全局值（不是回退缺省）");
        assert!(
            cfg.show_tokens,
            "项目里写坏的值回退全局（这里全局=缺省 true）"
        );

        // 缓存键跟着两层走
        let key = config_key();
        assert!(key.project.is_some());
        fs::remove_dir_all(&cwd).unwrap();
        assert_ne!(config_key(), key, "项目层消失要能失效缓存");
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
