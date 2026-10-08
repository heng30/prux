//! 任务配置
//!
//! 两层 JSON：`<agent_dir>/extensions/tasks.json` 提供全局默认，`<cwd>/.prux/tasks.json`
//! 提供项目覆盖，逐键合并。设置面板只写**项目覆盖**，且只写与全局不同的项。
//!
//! 配置是**数据，不是代码**：刻意没有可执行配置文件，因为 `.prux/` 位于克隆下来的仓库里。
//! 自定义排序用 JSON 排序 spec 表达（见 `sort.rs`）。
//!
//! 容错优先：手写文件里出现无法识别的值时，按字段回退默认，绝不让整个配置解析失败。
//!
//! 两层文件各自走一个 [`ConfigCache`]（`(path, mtime, len)` 命中免读盘/解析），读/写都在
//! [`CONFIG_LOCK`] 作用域内进行，锁序恒为「RwLock → 缓存 Mutex」。

use crate::{
    PROJECT_SCOPE_NAME,
    core::{config_cache::ConfigCache, extensions::ExtensionSetting, settings_manager::agent_dir},
    extensions::{tasks::sort::BUILT_IN_SORT_ORDERS, util::cycle_setting},
};
use serde_json::{Map, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};
use strum_macros::{EnumString, IntoStaticStr};

/// widget 中任务行的默认可见上限（未显式配置时生效）。
pub const DEFAULT_MAX_VISIBLE: u32 = 10;
/// 设置面板里 taskScope 的候选作用域。
const SCOPE_CHOICES: [&str; 4] = ["memory", "session", "session-global", "project"];
/// 设置面板里 autoClearCompleted 的候选清理模式。
const AUTO_CLEAR_CHOICES: [&str; 3] = ["never", "on_list_complete", "on_task_complete"];
/// 设置面板里 maxVisible 的候选上限值。
const MAX_VISIBLE_CHOICES: [&str; 7] = ["5", "10", "15", "20", "30", "50", "100"];
/// 设置面板里 hideCompletedAfter 的候选等待时长。
const AUTO_HIDE_CHOICES: [&str; 5] = ["never", "30s", "1m", "3m", "5m"];
/// 默认隐藏等待时长。
pub const DEFAULT_AUTO_HIDE: AutoHideDelay = AutoHideDelay::Min1;

/// 任务配置的读写锁（面板保存与读配置可能并发）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// 全局层缓存（`<agent_dir>/extensions/tasks.json`）。读/写都只在 [`CONFIG_LOCK`] 作用域内进行，
/// 锁序恒为「RwLock → 缓存 Mutex」。
static GLOBAL_CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 项目层缓存（`<cwd>/.prux/tasks.json`）。与全局层分开：两层路径不同，共用一个缓存会互相顶掉导致每次重读。
static PROJECT_CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 任务存储作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum TaskScope {
    /// 只存内存，会话结束即丢（CI/自动化）。
    Memory,
    /// 每会话一个文件，落在工作区（默认）。
    Session,
    /// 与 `session` 同为每会话一文件，但放在 agent 目录下，保持仓库干净。
    #[strum(serialize = "session-global")]
    SessionGlobal,
    /// 项目内所有会话共享一个文件。
    Project,
}

impl TaskScope {
    /// 该作用域在配置文件里的字符串（`strum` 派生的 snake_case 名）。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    pub fn parse(s: &str) -> Option<Self> {
        s.trim().parse().ok()
    }

    /// 是否按会话持久化（两种 session 作用域）。
    pub fn is_session(self) -> bool {
        matches!(self, TaskScope::Session | TaskScope::SessionGlobal)
    }
}

/// 已完成任务的自动清理模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum AutoClearMode {
    /// 从不自动清理，只能手动清。
    Never,
    /// 整个列表全部完成时开始倒计时并整批清理。
    OnListComplete,
    /// 每个任务各自完成后倒计时逐个清理。
    OnTaskComplete,
}

impl AutoClearMode {
    /// 该模式在配置文件里的字符串（`strum` 派生的 snake_case 名）。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    pub fn parse(s: &str) -> Option<Self> {
        s.trim().parse().ok()
    }
}

/// 列表溢出时从哪端折叠。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum HiddenAt {
    /// 隐藏列表开头的任务。
    Top,
    /// 隐藏列表末尾的任务。
    Bottom,
}

impl HiddenAt {
    /// 该端点在配置文件里的字符串（小写 `top` / `bottom`）。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    pub fn parse(s: &str) -> Option<Self> {
        s.trim().parse().ok()
    }
}

/// 整列表全部完成后，隔多久把这一段已完成任务从 dock 隐藏（墙钟，不删数据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum AutoHideDelay {
    /// 从不按时间隐藏（仍受回合制自动清理影响）。
    Never,
    /// 30 秒。
    #[strum(serialize = "30s")]
    Sec30,
    /// 1 分钟。
    #[strum(serialize = "1m")]
    Min1,
    /// 3 分钟。
    #[strum(serialize = "3m")]
    Min3,
    /// 5 分钟。
    #[strum(serialize = "5m")]
    Min5,
}

impl AutoHideDelay {
    /// 该时长在配置文件里的字符串（`never` / `30s` / `1m` / `3m` / `5m`）。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    pub fn parse(s: &str) -> Option<Self> {
        s.trim().parse().ok()
    }

    /// 等待时长（毫秒）；`never` 返回 `None`。
    pub fn delay_ms(self) -> Option<u64> {
        match self {
            AutoHideDelay::Never => None,
            AutoHideDelay::Sec30 => Some(30_000),
            AutoHideDelay::Min1 => Some(60_000),
            AutoHideDelay::Min3 => Some(180_000),
            AutoHideDelay::Min5 => Some(300_000),
        }
    }
}

/// 有效配置：每个字段都是「是否显式设置」。取默认值走访问器。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TasksConfig {
    /// 任务存储作用域。
    pub task_scope: Option<TaskScope>,
    /// 依赖完成后是否自动级联启动待执行的 agent 任务。
    pub auto_cascade: Option<bool>,
    /// 已完成任务的自动清理模式。
    pub auto_clear_completed: Option<AutoClearMode>,
    /// 是否把已完成行折叠成单行 `N completed`。
    pub collapse_completed: Option<bool>,
    /// 是否无视可见上限、展示全部任务。
    pub show_all: Option<bool>,
    /// widget 中最多展示多少行任务。
    pub max_visible: Option<u32>,
    /// 原始值（预设名或排序 spec）；解析与回退在 `sort.rs`。
    pub sort_order: Option<Value>,
    /// 列表溢出时从哪端折叠。
    pub hidden_at: Option<HiddenAt>,
    /// 整列表完成后隔多久从 dock 隐藏（墙钟）。
    pub hide_completed_after: Option<AutoHideDelay>,
}

/// 从 JSON 对象容错地读取一个配置层。
fn parse_layer(v: Value) -> TasksConfig {
    let Value::Object(obj) = v else {
        return TasksConfig::default();
    };

    let bool_of = |key: &str| obj.get(key).and_then(Value::as_bool);
    let str_of = |key: &str| obj.get(key).and_then(Value::as_str);

    let max_visible = obj
        .get("maxVisible")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .map(|n| n.min(u32::MAX as u64) as u32);

    TasksConfig {
        task_scope: str_of("taskScope").and_then(TaskScope::parse),
        auto_cascade: bool_of("autoCascade"),
        auto_clear_completed: str_of("autoClearCompleted").and_then(AutoClearMode::parse),
        collapse_completed: bool_of("collapseCompleted"),
        show_all: bool_of("showAll"),
        max_visible,
        sort_order: obj.get("sortOrder").cloned(),
        hidden_at: str_of("hiddenAt").and_then(HiddenAt::parse),
        hide_completed_after: str_of("hideCompletedAfter").and_then(AutoHideDelay::parse),
    }
}

/// 读取并解析一个配置层文件：先查 [`ConfigCache`]（命中则免读盘），读到的 JSON 交给
/// [`parse_layer`] 容错解析。文件缺失或格式非法时返回全默认配置，不报错。
fn read_layer(path: &Path, cache: &ConfigCache) -> TasksConfig {
    // 读锁只护住「缓存命中/读盘回填」；解析在锁内做，代价低且避免重复。
    let _guard = CONFIG_LOCK.read().unwrap_or_else(|e| e.into_inner());
    let value = cache.read(path, || {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null)
    });
    parse_layer(value)
}

/// 全局默认层：`<agent_dir>/extensions/tasks.json`。
pub fn load_global_tasks_config() -> TasksConfig {
    read_layer(&global_config_path(), &GLOBAL_CONFIG_CACHE)
}

/// 项目覆盖层：`<cwd>/.prux/tasks.json`。
pub fn project_config_path(cwd: &str) -> PathBuf {
    Path::new(cwd).join(PROJECT_SCOPE_NAME).join("tasks.json")
}

/// 全局默认层路径。
pub fn global_config_path() -> PathBuf {
    agent_dir().join("extensions").join("tasks.json")
}

/// 合并两层配置：项目逐键覆盖全局。
pub fn load_tasks_config(cwd: &str) -> TasksConfig {
    merge(
        load_global_tasks_config(),
        read_layer(&project_config_path(cwd), &PROJECT_CONFIG_CACHE),
    )
}

/// 逐键合并。
pub fn merge(mut global: TasksConfig, project: TasksConfig) -> TasksConfig {
    if project.task_scope.is_some() {
        global.task_scope = project.task_scope;
    }
    if project.auto_cascade.is_some() {
        global.auto_cascade = project.auto_cascade;
    }
    if project.auto_clear_completed.is_some() {
        global.auto_clear_completed = project.auto_clear_completed;
    }
    if project.collapse_completed.is_some() {
        global.collapse_completed = project.collapse_completed;
    }
    if project.show_all.is_some() {
        global.show_all = project.show_all;
    }
    if project.max_visible.is_some() {
        global.max_visible = project.max_visible;
    }
    if project.sort_order.is_some() {
        global.sort_order = project.sort_order;
    }
    if project.hidden_at.is_some() {
        global.hidden_at = project.hidden_at;
    }
    if project.hide_completed_after.is_some() {
        global.hide_completed_after = project.hide_completed_after;
    }

    global
}

/// 差异写入辅助：值不同且有值时写入覆盖项。
fn set_override(
    overrides: &mut Map<String, Value>,
    key: &str,
    value: Option<Value>,
    differs: bool,
) {
    if differs && let Some(v) = value {
        overrides.insert(key.to_string(), v);
    }
}

/// 只写与全局不同的项。
pub fn save_tasks_config(config: &TasksConfig, cwd: &str) {
    let global = load_global_tasks_config();
    let mut overrides: Map<String, Value> = Map::new();

    set_override(
        &mut overrides,
        "taskScope",
        config
            .task_scope
            .map(|s| Value::String(s.as_str().to_string())),
        config.task_scope != global.task_scope,
    );
    set_override(
        &mut overrides,
        "autoCascade",
        config.auto_cascade.map(Value::Bool),
        config.auto_cascade != global.auto_cascade,
    );
    set_override(
        &mut overrides,
        "autoClearCompleted",
        config
            .auto_clear_completed
            .map(|m| Value::String(m.as_str().to_string())),
        config.auto_clear_completed != global.auto_clear_completed,
    );
    set_override(
        &mut overrides,
        "collapseCompleted",
        config.collapse_completed.map(Value::Bool),
        config.collapse_completed != global.collapse_completed,
    );
    set_override(
        &mut overrides,
        "showAll",
        config.show_all.map(Value::Bool),
        config.show_all != global.show_all,
    );
    set_override(
        &mut overrides,
        "maxVisible",
        config.max_visible.map(Value::from),
        config.max_visible != global.max_visible,
    );
    set_override(
        &mut overrides,
        "sortOrder",
        config.sort_order.clone(),
        config.sort_order != global.sort_order,
    );
    set_override(
        &mut overrides,
        "hiddenAt",
        config
            .hidden_at
            .map(|h| Value::String(h.as_str().to_string())),
        config.hidden_at != global.hidden_at,
    );
    set_override(
        &mut overrides,
        "hideCompletedAfter",
        config
            .hide_completed_after
            .map(|d| Value::String(d.as_str().to_string())),
        config.hide_completed_after != global.hide_completed_after,
    );

    let path = project_config_path(cwd);
    let value = Value::Object(overrides);

    // 全局层已在上面（未持写锁时）读过；此处只护住落盘与缓存回填。
    let _guard = CONFIG_LOCK.write().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        _ = fs::create_dir_all(parent);
    }

    if let Ok(text) = serde_json::to_string_pretty(&value)
        && fs::write(&path, text).is_ok()
    {
        // 落盘成功后回填缓存（取写盘后的元数据），下次读取直接命中。
        PROJECT_CONFIG_CACHE.store(&path, value);
    }
}

impl TasksConfig {
    /// 任务存储作用域；未显式设置时为 `session`。
    pub fn task_scope(&self) -> TaskScope {
        self.task_scope.unwrap_or(TaskScope::Session)
    }

    /// 依赖完成后是否自动级联启动；未设置时关闭。
    pub fn auto_cascade(&self) -> bool {
        self.auto_cascade.unwrap_or(false)
    }

    /// 已完成任务的清理模式；未设置时为「整个列表完成时整批清理」。
    pub fn auto_clear_completed(&self) -> AutoClearMode {
        self.auto_clear_completed
            .unwrap_or(AutoClearMode::OnListComplete)
    }

    /// 是否把已完成行折叠成单行；未设置时不折叠。
    pub fn collapse_completed(&self) -> bool {
        self.collapse_completed.unwrap_or(false)
    }

    /// 是否无视可见上限展示全部任务；未设置时为否。
    pub fn show_all(&self) -> bool {
        self.show_all.unwrap_or(false)
    }

    /// widget 中任务行可见上限；未设置时为 [`DEFAULT_MAX_VISIBLE`]。
    pub fn max_visible(&self) -> u32 {
        self.max_visible.unwrap_or(DEFAULT_MAX_VISIBLE)
    }

    /// 列表溢出时从哪端折叠；未设置时为末尾。
    pub fn hidden_at(&self) -> HiddenAt {
        self.hidden_at.unwrap_or(HiddenAt::Bottom)
    }

    /// 整列表完成后隔多久隐藏；未设置时为 [`DEFAULT_AUTO_HIDE`]。
    pub fn hide_completed_after(&self) -> AutoHideDelay {
        self.hide_completed_after.unwrap_or(DEFAULT_AUTO_HIDE)
    }

    /// 隐藏等待时长（毫秒）；关闭时返回 `None`。
    pub fn hide_completed_after_ms(&self) -> Option<u64> {
        self.hide_completed_after().delay_ms()
    }

    /// 原始排序配置（预设名字符串或自定义 spec）；未设置时返回 `None`，由 `sort.rs` 决定回退。
    pub fn sort_order(&self) -> Option<&Value> {
        self.sort_order.as_ref()
    }
}

/// 面板项（`/tasks` → Settings 的 9 项可循环设置）。
///
/// 自定义排序 spec 是复合/自由文本，无法用离散循环表达，因此只读展示。
pub fn panel_settings(cfg: &TasksConfig) -> Vec<ExtensionSetting> {
    // 排序 spec 无法通过循环菜单重建，因此只读展示；选预设是丢弃它的唯一有意途径。
    let custom_sort = matches!(cfg.sort_order, Some(Value::Array(_)));
    let sort_value = if custom_sort {
        "custom".to_string()
    } else {
        cfg.sort_order
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or("id")
            .to_string()
    };

    vec![
        cycle_setting(
            "taskScope",
            "Task storage",
            "memory: in memory only. session: per-session file in the workspace (.prux/tasks/tasks-<sessionId>.json). session-global: same but under the agent directory. project: shared across sessions. Takes effect on next session start.",
            cfg.task_scope().as_str().to_string(),
            &SCOPE_CHOICES,
        ),
        cycle_setting(
            "autoCascade",
            "Auto-cascade agent tasks",
            "When ON: pending agent tasks start automatically once their dependencies complete. When OFF: use TaskExecute to launch them manually.",
            if cfg.auto_cascade() { "on" } else { "off" }.to_string(),
            &["on", "off"],
        ),
        cycle_setting(
            "collapseCompleted",
            "Collapse completed tasks",
            "When ON, completed tasks are replaced by a single 'N completed' line and the visible limit applies only to the tasks left.",
            if cfg.collapse_completed() {
                "on"
            } else {
                "off"
            }
            .to_string(),
            &["on", "off"],
        ),
        cycle_setting(
            "showAll",
            "Show all tasks in widget",
            "When ON, every listed task is shown regardless of the visible limit.",
            if cfg.show_all() { "on" } else { "off" }.to_string(),
            &["on", "off"],
        ),
        cycle_setting(
            "maxVisible",
            "Max visible tasks in widget",
            "Only applies when 'Show all tasks' is OFF. Caps how many task lines the widget shows.",
            cfg.max_visible().to_string(),
            &MAX_VISIBLE_CHOICES,
        ),
        cycle_setting(
            "sortOrder",
            "Widget sort order",
            "'active' groups by in-progress -> pending -> completed; 'status' is the reverse. 'id' sorts by creation order. A custom sort spec in tasks.json shows as 'custom' and can only be changed there.",
            sort_value,
            &BUILT_IN_SORT_ORDERS,
        ),
        cycle_setting(
            "hiddenAt",
            "Hidden tasks position",
            "'bottom' hides tasks from the end of the list. 'top' hides tasks from the start.",
            cfg.hidden_at().as_str().to_string(),
            &["bottom", "top"],
        ),
        cycle_setting(
            "autoClearCompleted",
            "Auto-clear completed tasks",
            "never: completed tasks stay until cleared manually. on_list_complete: cleared after all tasks are done. on_task_complete: each task cleared after it completes.",
            cfg.auto_clear_completed().as_str().to_string(),
            &AUTO_CLEAR_CHOICES,
        ),
        cycle_setting(
            "hideCompletedAfter",
            "Auto-hide completed list after",
            "Only applies once every task in the list is completed, and hides the whole task section from the dock after this wall-clock delay. It reappears as soon as any task is unfinished again or a new task is added. Tasks are never deleted; /tasks still lists them.",
            cfg.hide_completed_after().as_str().to_string(),
            &AUTO_HIDE_CHOICES,
        ),
    ]
}

/// 应用一项面板设置。`Err` 用于拒绝无法识别的键/值（面板保持原值并提示）。
pub fn apply_setting(cfg: &mut TasksConfig, key: &str, value: &str) -> Result<(), String> {
    match key {
        "taskScope" => {
            cfg.task_scope =
                Some(TaskScope::parse(value).ok_or_else(|| format!("bad taskScope: {value}"))?);
        }
        "autoCascade" => cfg.auto_cascade = Some(value == "on"),
        "collapseCompleted" => cfg.collapse_completed = Some(value == "on"),
        "showAll" => cfg.show_all = Some(value == "on"),
        "maxVisible" => {
            let n: u32 = value
                .parse()
                .map_err(|_| format!("bad maxVisible: {value}"))?;
            cfg.max_visible = Some(n);
        }
        "sortOrder" => {
            // "custom" 只读，循环不会产生它；拒绝以防万一。
            if value == "custom" {
                return Err("custom sort order can only be edited in tasks.json".to_string());
            }
            cfg.sort_order = Some(Value::String(value.to_string()));
        }
        "hiddenAt" => {
            cfg.hidden_at =
                Some(HiddenAt::parse(value).ok_or_else(|| format!("bad hiddenAt: {value}"))?);
        }
        "autoClearCompleted" => {
            cfg.auto_clear_completed = Some(
                AutoClearMode::parse(value)
                    .ok_or_else(|| format!("bad autoClearCompleted: {value}"))?,
            );
        }
        "hideCompletedAfter" => {
            cfg.hide_completed_after = Some(
                AutoHideDelay::parse(value)
                    .ok_or_else(|| format!("bad hideCompletedAfter: {value}"))?,
            );
        }
        other => return Err(format!("unknown setting: {other}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;
    use serde_json::json;

    #[test]
    fn layer_parsing_is_tolerant() {
        let c = parse_layer(json!({
            "taskScope": "bogus",
            "autoCascade": "yes",
            "maxVisible": 0,
            "sortOrder": "active",
            "hiddenAt": "top",
        }));
        assert_eq!(c.task_scope, None, "未知作用域忽略");
        assert_eq!(c.auto_cascade, None, "非 bool 忽略");
        assert_eq!(c.max_visible, None, "0 视为未设置");
        assert_eq!(c.hidden_at, Some(HiddenAt::Top));
        assert_eq!(c.sort_order, Some(json!("active")));
    }

    #[test]
    fn strum_enum_conversions() {
        // as_str 是规范名，parse 大小写不敏感且容忍首尾空白（strum 派生）。
        assert_eq!(TaskScope::SessionGlobal.as_str(), "session-global");
        assert_eq!(
            TaskScope::parse("session-global"),
            Some(TaskScope::SessionGlobal)
        );
        assert_eq!(TaskScope::parse(" Session "), Some(TaskScope::Session));
        assert_eq!(TaskScope::parse("bogus"), None);

        assert_eq!(AutoClearMode::OnListComplete.as_str(), "on_list_complete");
        assert_eq!(
            AutoClearMode::parse("on_task_complete"),
            Some(AutoClearMode::OnTaskComplete)
        );
        assert_eq!(AutoClearMode::parse("never"), Some(AutoClearMode::Never));
        assert_eq!(AutoClearMode::parse("sometimes"), None);

        assert_eq!(HiddenAt::Bottom.as_str(), "bottom");
        assert_eq!(HiddenAt::parse("TOP"), Some(HiddenAt::Top));
        assert_eq!(HiddenAt::parse("left"), None);

        assert_eq!(AutoHideDelay::Never.as_str(), "never");
        assert_eq!(AutoHideDelay::Sec30.as_str(), "30s");
        assert_eq!(AutoHideDelay::Min1.as_str(), "1m");
        assert_eq!(AutoHideDelay::parse("30S"), Some(AutoHideDelay::Sec30));
        assert_eq!(AutoHideDelay::parse("3m"), Some(AutoHideDelay::Min3));
        assert_eq!(AutoHideDelay::parse("2m"), None);
        assert_eq!(AutoHideDelay::Min5.delay_ms(), Some(300_000));
        assert_eq!(AutoHideDelay::Never.delay_ms(), None);
    }

    #[test]
    fn auto_hide_defaults_to_one_minute() {
        assert_eq!(
            TasksConfig::default().hide_completed_after(),
            AutoHideDelay::Min1
        );
        assert_eq!(
            TasksConfig::default().hide_completed_after_ms(),
            Some(60_000)
        );
        assert_eq!(
            parse_layer(json!({ "hideCompletedAfter": "bogus" })).hide_completed_after,
            None,
            "未知值回退默认"
        );
    }

    #[test]
    fn merge_prefers_project() {
        let global = parse_layer(json!({
            "autoCascade": false,
            "taskScope": "session",
        }));
        let project = parse_layer(json!({
            "autoCascade": true,
        }));
        let merged = merge(global, project);
        assert!(merged.auto_cascade());
        assert_eq!(merged.task_scope(), TaskScope::Session, "未覆盖的键保留");
    }

    #[test]
    fn save_writes_only_overrides() {
        let _g = AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd = cwd.path().to_string_lossy().to_string();

        // 全局层：autoCascade=true（写成全局文件）
        let global_path = global_config_path();
        fs::create_dir_all(global_path.parent().unwrap()).unwrap();
        fs::write(&global_path, r#"{"autoCascade": true}"#).unwrap();

        let mut cfg = load_tasks_config(&cwd);
        assert!(cfg.auto_cascade());
        // 改成 false（与全局不同）+ 设 maxVisible=20，其余保持默认
        cfg.auto_cascade = Some(false);
        cfg.max_visible = Some(20);
        save_tasks_config(&cfg, &cwd);

        let written: Value =
            serde_json::from_str(&fs::read_to_string(project_config_path(&cwd)).unwrap()).unwrap();
        assert_eq!(written.get("autoCascade"), Some(&json!(false)));
        assert_eq!(written.get("maxVisible"), Some(&json!(20)));
        assert!(
            written.get("taskScope").is_none(),
            "与全局一致/未显式设置的键不写"
        );

        // save 回填了项目层缓存：紧接着读应拿到刚写的值。
        let reloaded = load_tasks_config(&cwd);
        assert!(!reloaded.auto_cascade());
        assert_eq!(reloaded.max_visible(), 20);
    }

    #[test]
    fn load_reflects_external_edits() {
        let _g = AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd = cwd.path().to_string_lossy().to_string();
        let path = project_config_path(&cwd);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        fs::write(&path, r#"{"maxVisible": 20}"#).unwrap();
        assert_eq!(load_tasks_config(&cwd).max_visible(), 20);

        // 外部改写 → (path, mtime, len) 失效，读到新值
        fs::write(&path, r#"{"maxVisible": 50}"#).unwrap();
        assert_eq!(load_tasks_config(&cwd).max_visible(), 50);

        // 删除后回落默认
        fs::remove_file(&path).unwrap();
        assert_eq!(load_tasks_config(&cwd).max_visible(), DEFAULT_MAX_VISIBLE);
    }
}
