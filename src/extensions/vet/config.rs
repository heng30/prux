//! vet 的配置层：全局 + 项目两处 `vet.json` 的读取与合并、检查项校验、面板档位与候选探测。
//!
//! 位置（与其它扩展同构）：
//! - 全局：`agent_dir()/extensions/vet.json`
//! - 项目：`<cwd>/.prux/extensions/vet.json`，**仅在项目受信任时读取/写入**——
//!   未受信的仓库不能靠提交一份配置就让 prux 执行任意命令。
//!
//! 合并语义是「浅覆盖 + 数组整体替换」：项目里写了的键覆盖全局同名键，`checks` 整个替换
//! （不做逐项合并：一半来自全局、一半来自项目的检查列表谁也说不清跑的是哪几条）。
//! 项目里没写的键继续用全局值，两边都没写的键用缺省值。
//!
//! 检查项是 `{name, command, optional}`：
//! - `name` 展示名，也是 `/vet remove <name>` 的键，重复时**后者覆盖前者**；
//! - `command` 交给 shell 执行（unix `sh -c`、windows `cmd /C`），空串整项丢弃；
//! - `optional: true` 的检查失败不阻止 agent 收工，只在报告里标注。
//!
//! 缺省值是「安全空转」：`autoRun` 开、`checks` 为空 → 什么也不做，直到用户显式加检查。

use crate::{
    PROJECT_SCOPE_NAME,
    core::{
        config_cache::ConfigCache, extensions::ExtensionSetting, project_trust::is_project_trusted,
        settings_manager::agent_dir,
    },
    extensions::util::{choice_num, cycle_setting},
    utils::truncate::truncate_chars_within,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
    time::Duration,
};
use strum_macros::{EnumString, IntoStaticStr};

/// 扩展名（注册名，也是 `/extension` 面板里的名字）。
pub(super) const EXT: &str = "vet";

/// 斜杠命令名（`/vet`）。
pub(super) const CMD: &str = "vet";

/// 扩展配置目录（`agent_dir()` 下）。
const CONFIG_SUBDIR: &str = "extensions";

/// 全局配置文件名（位于 `agent_dir()/extensions/` 下）。
pub(super) const CONFIG_FILE: &str = "vet.json";

/// 配置版本戳；缺键或版本不符时回填缺省键。
const CONFIG_VERSION: i64 = 1;

/// 自动续跑次数缺省值（`0` = 只跑检查、不续跑）。
pub(super) const DEFAULT_MAX_ITERATIONS: usize = 2;

/// 单条检查的超时缺省值（秒）。
pub(super) const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// `/vet` 展示检查项名称时的字符上限（过长的名字撑爆单行）。
pub(super) const MAX_NAME_CHARS: usize = 40;

/// `/vet` 展示检查命令时的字符上限（超出以省略号收尾，保证一行放得下）。
pub(super) const MAX_COMMAND_CHARS: usize = 200;

/// `maxIterations` 面板档位表：`(数值, 标签)`；`0` 表示只检查不续跑。
const ITERATIONS_TABLE: &[(f64, &str)] =
    &[(0.0, "off"), (1.0, "1"), (2.0, "2"), (3.0, "3"), (5.0, "5")];

/// `timeoutSecs` 面板档位表。
const TIMEOUT_TABLE: &[(f64, &str)] =
    &[(60.0, "1m"), (300.0, "5m"), (600.0, "10m"), (1800.0, "30m")];

/// 全局配置读写锁（锁序恒为「RwLock → 缓存 Mutex」）。
static GLOBAL_LOCK: RwLock<()> = RwLock::new(());

/// 项目配置读写锁。
static PROJECT_LOCK: RwLock<()> = RwLock::new(());

/// 全局配置的读缓存（按 `(path, mtime, len)` 失效，手改文件即时生效）。
static GLOBAL_CACHE: ConfigCache = ConfigCache::new();

/// 项目配置的读缓存。
static PROJECT_CACHE: ConfigCache = ConfigCache::new();

/// 检查结果的通知口径（`announce` 键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub(super) enum Announce {
    /// 从不主动通知（只有注入模型的失败报告照旧）。
    Off,
    /// 只在检查失败时通知（缺省）。
    Fail,
    /// 通过与否都通知。
    Always,
}

/// 一条检查项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Check {
    /// 展示名，也是 `/vet remove` 的操作键。
    pub name: String,
    /// 交给 shell 执行的命令原文。
    pub command: String,
    /// 失败是否允许收工（true = 只报告，不阻止结算）。
    pub optional: bool,
}

/// 配置写入的作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    /// 全局配置（`agent_dir()/extensions/vet.json`）。
    Global,
    /// 项目配置（`<cwd>/.prux/extensions/vet.json`，需项目受信）。
    Project,
}

impl Scope {
    /// 稳定的字符串标识（`/vet add --project` 的 flag 与展示共用）。
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project => "project",
        }
    }

    /// 该作用域的配置文件路径。
    pub(super) fn path(self, cwd: &Path) -> PathBuf {
        match self {
            Scope::Global => global_config_path(),
            Scope::Project => project_config_path(cwd),
        }
    }
}

/// 一份生效配置：运行参数 + 检查项 + 来源与告警。
#[derive(Debug, Clone)]
pub(super) struct Config {
    /// 结算时是否自动跑检查（`autoRun`，缺省开）。
    pub auto_run: bool,
    /// 一次回合最多自动续跑几次（`maxIterations`，缺省 2；`0` = 只检查不续跑）。
    pub max_iterations: usize,
    /// 单条检查的超时（`timeoutSecs`，缺省 300s）。
    pub timeout: Duration,
    /// 检查结果的通知口径（`announce`，缺省 `fail`）。
    pub announce: Announce,
    /// 是否只在本次回合改过文件时才跑（`requireEdits`，缺省开）。
    pub require_edits: bool,
    /// 检查项（项目配置整体替换全局配置）。
    pub checks: Vec<Check>,
    /// 实际参与合并的配置文件（`/vet status` 列出）。
    pub sources: Vec<PathBuf>,
    /// 解析过程中的告警（无效检查项、被跳过的项目配置等）。
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            auto_run: true,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            announce: Announce::Fail,
            require_edits: true,
            checks: Vec::new(),
            sources: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

impl Config {
    /// 是否有可执行的检查项（没有时整条自动路径都不启动）。
    pub(super) fn has_checks(&self) -> bool {
        !self.checks.is_empty()
    }
}

/// 全局配置文件路径。
pub(super) fn global_config_path() -> PathBuf {
    agent_dir().join(CONFIG_SUBDIR).join(CONFIG_FILE)
}

/// 项目配置文件路径（`<cwd>/.prux/extensions/vet.json`）。
pub(super) fn project_config_path(cwd: &Path) -> PathBuf {
    cwd.join(PROJECT_SCOPE_NAME)
        .join(CONFIG_SUBDIR)
        .join(CONFIG_FILE)
}

/// 读取一个配置文件（缺失 / 损坏 / 非对象都返回空对象）。
fn read_json(cache: &ConfigCache, lock: &RwLock<()>, path: &Path) -> Value {
    let _guard = lock.read().unwrap_or_else(|e| e.into_inner());
    cache.read(path, || {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    })
}

/// 加载生效配置：全局 + （受信时的）项目。
pub(super) fn load(cwd: &Path) -> Config {
    let global_path = global_config_path();
    let global = read_json(&GLOBAL_CACHE, &GLOBAL_LOCK, &global_path);

    let mut merged = global.clone();
    let mut sources = Vec::new();
    let mut warnings = Vec::new();

    if global.as_object().is_some_and(|o| !o.is_empty()) {
        sources.push(global_path);
    }

    let project_path = project_config_path(cwd);
    if project_path.is_file() {
        if is_project_trusted(cwd, &agent_dir()) {
            let project = read_json(&PROJECT_CACHE, &PROJECT_LOCK, &project_path);
            if project.as_object().is_some_and(|o| !o.is_empty()) {
                overlay(&mut merged, &project);
                sources.push(project_path);
            }
        } else {
            warnings.push(format!(
                "project config ignored (untrusted project): {}",
                project_path.display()
            ));
        }
    }

    parse(&merged, sources, warnings)
}

/// 项目覆盖全局：键级替换，`checks` 数组整体替换。
fn overlay(base: &mut Value, project: &Value) {
    let (Some(base_map), Some(project_map)) = (base.as_object_mut(), project.as_object()) else {
        *base = project.clone();
        return;
    };
    for (key, value) in project_map {
        base_map.insert(key.clone(), value.clone());
    }
}

/// 把合并后的 JSON 解析成 [`Config`]（纯函数，便于测试）。
fn parse(value: &Value, sources: Vec<PathBuf>, mut warnings: Vec<String>) -> Config {
    let mut cfg = Config {
        sources,
        warnings: Vec::new(),
        ..Config::default()
    };

    cfg.auto_run = value
        .get("autoRun")
        .and_then(Value::as_bool)
        .unwrap_or(cfg.auto_run);
    cfg.require_edits = value
        .get("requireEdits")
        .and_then(Value::as_bool)
        .unwrap_or(cfg.require_edits);
    cfg.max_iterations = value
        .get("maxIterations")
        .and_then(Value::as_f64)
        .filter(|v| *v >= 0.0 && v.fract() == 0.0)
        .map(|v| v as usize)
        .unwrap_or(cfg.max_iterations);
    cfg.timeout = Duration::from_secs(
        value
            .get("timeoutSecs")
            .and_then(Value::as_f64)
            .filter(|v| *v > 0.0)
            .map(|v| v as u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS),
    );

    if let Some(raw) = value.get("announce").and_then(Value::as_str) {
        match raw.trim().to_ascii_lowercase().parse::<Announce>() {
            Ok(a) => cfg.announce = a,
            Err(_) => warnings.push(format!("invalid announce value `{raw}` (off|fail|always)")),
        }
    }

    cfg.checks = parse_checks(value.get("checks"), &mut warnings);
    cfg.warnings = warnings;
    cfg
}

/// 解析 `checks` 数组：逐项校验，后者覆盖同名的前者。
fn parse_checks(value: Option<&Value>, warnings: &mut Vec<String>) -> Vec<Check> {
    let Some(items) = value.and_then(Value::as_array) else {
        if value.is_some() {
            warnings.push("checks is not an array (ignored)".to_string());
        }
        return Vec::new();
    };

    let mut out: Vec<Check> = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let command = item
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let (Some(name), Some(command)) = (name, command) else {
            warnings.push(format!(
                "checks[{idx}] skipped (name and command are required)"
            ));
            continue;
        };

        let check = Check {
            name: name.to_string(),
            command: command.to_string(),
            optional: item
                .get("optional")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        match out.iter_mut().find(|c| c.name == check.name) {
            Some(slot) => {
                warnings.push(format!("checks[{idx}] overrides earlier `{}`", check.name));
                *slot = check;
            }
            None => out.push(check),
        }
    }
    out
}

/// 读盘（不合并）后改键写回：避免把缺省值 / 项目值糊进全局文件。
fn update_file<F>(scope: Scope, cwd: &Path, mutate: F) -> std::result::Result<(), String>
where
    F: FnOnce(&mut serde_json::Map<String, Value>),
{
    let path = scope.path(cwd);
    if scope == Scope::Project && !is_project_trusted(cwd, &agent_dir()) {
        return Err(format!(
            "project is not trusted, refusing to write {}",
            path.display()
        ));
    }

    let lock = match scope {
        Scope::Global => &GLOBAL_LOCK,
        Scope::Project => &PROJECT_LOCK,
    };
    let cache = match scope {
        Scope::Global => &GLOBAL_CACHE,
        Scope::Project => &PROJECT_CACHE,
    };

    let mut map = {
        let _guard = lock.read().unwrap_or_else(|e| e.into_inner());
        cache
            .read(&path, || {
                fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}))
            })
            .as_object()
            .cloned()
            .unwrap_or_default()
    };

    mutate(&mut map);
    map.insert("configVersion".to_string(), json!(CONFIG_VERSION));
    let value = Value::Object(map);

    let _guard = lock.write().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create {} failed: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(&value)
        .map_err(|e| format!("serialize config failed: {e}"))?;
    fs::write(&path, format!("{text}\n"))
        .map_err(|e| format!("write {} failed: {e}", path.display()))?;
    cache.store(&path, value);
    Ok(())
}

/// 写入一个标量键（面板与 `/vet` 子命令共用）。
pub(super) fn set_field(
    scope: Scope,
    cwd: &Path,
    key: &str,
    value: Value,
) -> std::result::Result<(), String> {
    update_file(scope, cwd, |map| {
        map.insert(key.to_string(), value);
    })
}

/// 追加一条检查项（同名覆盖）。返回是否覆盖了已有项。
pub(super) fn upsert_check(
    scope: Scope,
    cwd: &Path,
    check: &Check,
) -> std::result::Result<bool, String> {
    let mut replaced = false;
    update_file(scope, cwd, |map| {
        let mut items = map
            .get("checks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let entry = json!({
            "name": check.name,
            "command": check.command,
            "optional": check.optional,
        });
        match items
            .iter_mut()
            .position(|item| item.get("name").and_then(Value::as_str) == Some(check.name.as_str()))
        {
            Some(idx) => {
                items[idx] = entry;
                replaced = true;
            }
            None => items.push(entry),
        }
        map.insert("checks".to_string(), Value::Array(items));
    })?;
    Ok(replaced)
}

/// 删除一条检查项。返回是否真的删掉了。
pub(super) fn remove_check(
    scope: Scope,
    cwd: &Path,
    name: &str,
) -> std::result::Result<bool, String> {
    let mut removed = false;
    update_file(scope, cwd, |map| {
        let mut items = map
            .get("checks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let before = items.len();
        items.retain(|item| item.get("name").and_then(Value::as_str) != Some(name));
        removed = items.len() != before;
        map.insert("checks".to_string(), Value::Array(items));
    })?;
    Ok(removed)
}

/// 面板项（顺序即面板显示顺序）。
pub(super) fn panel_settings(cfg: &Config) -> Vec<ExtensionSetting> {
    vec![
        cycle_setting(
            "autoRun",
            "Auto run",
            "Run the configured checks before the agent finishes and feed failures back",
            on_off_text(cfg.auto_run),
            &["on", "off"],
        ),
        cycle_setting(
            "maxIterations",
            "Max iterations",
            "How many times one turn may be resumed to fix failing checks (off = report only)",
            iterations_label(cfg.max_iterations),
            &["off", "1", "2", "3", "5"],
        ),
        cycle_setting(
            "timeout",
            "Check timeout",
            "Kill a single check after this long and treat it as failed",
            timeout_label(cfg.timeout),
            &["1m", "5m", "10m", "30m"],
        ),
        cycle_setting(
            "announce",
            "Announce",
            "When to post check results in the chat (the failure report always reaches the model)",
            announce_text(cfg.announce),
            &["off", "fail", "always"],
        ),
        cycle_setting(
            "requireEdits",
            "Only after edits",
            "Skip checks for turns that changed no files (pure Q&A turns)",
            on_off_text(cfg.require_edits),
            &["on", "off"],
        ),
    ]
}

/// 应用面板选择（写入全局配置，立即生效）。
pub(super) fn apply_panel_choice(
    cwd: &Path,
    key: &str,
    value: &str,
) -> std::result::Result<(), String> {
    match key {
        "autoRun" => set_field(Scope::Global, cwd, "autoRun", json!(value == "on")),
        "requireEdits" => set_field(Scope::Global, cwd, "requireEdits", json!(value == "on")),
        "maxIterations" => set_field(
            Scope::Global,
            cwd,
            "maxIterations",
            json!(choice_num(ITERATIONS_TABLE, value)? as i64),
        ),
        "timeout" => set_field(
            Scope::Global,
            cwd,
            "timeoutSecs",
            json!(choice_num(TIMEOUT_TABLE, value)? as i64),
        ),
        "announce" => {
            // 面板标签与 `Announce` 的 strum 标识一致，解析一次即校验。
            value
                .parse::<Announce>()
                .map_err(|_| format!("invalid option: {value}"))?;
            set_field(Scope::Global, cwd, "announce", json!(value))
        }
        other => Err(format!("unknown setting: {other}")),
    }
}

/// 布尔开关的面板标签。
fn on_off_text(value: bool) -> String {
    if value {
        "on".to_string()
    } else {
        "off".to_string()
    }
}

/// `announce` 的面板标签（与 strum 的序列化标识一致）。
fn announce_text(announce: Announce) -> String {
    let text: &'static str = announce.into();
    text.to_string()
}

/// `maxIterations` 数值 → 面板标签。
fn iterations_label(n: usize) -> String {
    ITERATIONS_TABLE
        .iter()
        .find(|(v, _)| *v as usize == n)
        .map(|(_, l)| (*l).to_string())
        .unwrap_or_else(|| n.to_string())
}

/// `timeout` 时长 → 面板标签（不在档位表里时回退成秒数）。
fn timeout_label(timeout: Duration) -> String {
    let secs = timeout.as_secs();
    TIMEOUT_TABLE
        .iter()
        .find(|(v, _)| *v as u64 == secs)
        .map(|(_, l)| (*l).to_string())
        .unwrap_or_else(|| format!("{secs}s"))
}

/// 按项目里的标记文件推测可用的检查命令（`/vet detect`）。
///
/// 只看文件名与极少量内容标记，不解析构建系统；探测结果只是候选，写入需用户确认。
pub(super) fn detect(cwd: &Path) -> Vec<Check> {
    let mut out: Vec<Check> = Vec::new();
    let mut push = |name: &str, command: &str| {
        if !out.iter().any(|c| c.command == command) {
            out.push(Check {
                name: name.to_string(),
                command: command.to_string(),
                optional: false,
            });
        }
    };

    if cwd.join("Cargo.toml").is_file() {
        push("test", "cargo test");
        push("clippy", "cargo clippy --all-targets");
    }

    if cwd.join("package.json").is_file() {
        let runner = if cwd.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if cwd.join("yarn.lock").is_file() {
            "yarn"
        } else if cwd.join("bun.lockb").is_file() || cwd.join("bun.lock").is_file() {
            "bun"
        } else {
            "npm"
        };
        if has_package_script(cwd, "test") {
            push("test", &format!("{runner} test"));
        }
        if has_package_script(cwd, "lint") {
            push("lint", &format!("{runner} run lint"));
        }
        if has_package_script(cwd, "typecheck") {
            push("typecheck", &format!("{runner} run typecheck"));
        }
    }

    if has_just_target(cwd, "check") {
        push("check", "just check");
    } else if has_make_target(cwd, "check") {
        push("check", "make check");
    }

    if cwd.join("pyproject.toml").is_file() || cwd.join("pytest.ini").is_file() {
        if cwd.join("tests").is_dir() {
            push("test", "pytest");
        }
        if mentions(cwd, "pyproject.toml", "ruff") {
            push("lint", "ruff check .");
        }
    }

    if cwd.join("go.mod").is_file() {
        push("test", "go test ./...");
    }

    if cwd.join("deno.json").is_file() || cwd.join("deno.jsonc").is_file() {
        push("test", "deno test");
    }

    out
}

/// `package.json` 的 `scripts` 里是否有该键（只做字符串匹配，不解析 JSON 语义之外的东西）。
fn has_package_script(cwd: &Path, name: &str) -> bool {
    let Ok(text) = fs::read_to_string(cwd.join("package.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    value
        .get("scripts")
        .and_then(|s| s.get(name))
        .is_some_and(|v| v.is_string())
}

/// `justfile` 里是否声明了该配方（按行首匹配 `name:` / `name args:`）。
fn has_just_target(cwd: &Path, name: &str) -> bool {
    ["justfile", "Justfile"]
        .iter()
        .filter_map(|f| fs::read_to_string(cwd.join(f)).ok())
        .any(|text| {
            text.lines().any(|line| {
                let line = line.trim_start();
                line.strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with(':') || rest.starts_with(' '))
                    && !line.starts_with('#')
            })
        })
}

/// `Makefile` 里是否声明了该目标（按行首匹配 `name:`）。
fn has_make_target(cwd: &Path, name: &str) -> bool {
    ["Makefile", "makefile", "GNUmakefile"]
        .iter()
        .filter_map(|f| fs::read_to_string(cwd.join(f)).ok())
        .any(|text| {
            text.lines().any(|line| {
                let line = line.trim_start_matches('\t');
                line.strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with(':'))
                    && !line.starts_with('#')
            })
        })
}

/// 文件里是否出现某个字样（用于 ruff 这类「有配置才建议」的候选）。
fn mentions(cwd: &Path, file: &str, needle: &str) -> bool {
    fs::read_to_string(cwd.join(file))
        .map(|text| text.contains(needle))
        .unwrap_or(false)
}

/// 检查项命令的展示串：折叠成单行并限长。
pub(super) fn describe_check(check: &Check) -> String {
    let command = truncate_chars_within(
        &check
            .command
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        MAX_COMMAND_CHARS,
        "…",
    );
    if check.optional {
        format!("{} (optional) · `{command}`", truncate_name(&check.name))
    } else {
        format!("{} · `{command}`", truncate_name(&check.name))
    }
}

/// 检查项名称的展示串：超长截断（名称来自用户配置，可能很长）。
pub(super) fn truncate_name(name: &str) -> String {
    truncate_chars_within(name, MAX_NAME_CHARS, "…")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一份带 checks 的配置 JSON。
    fn sample() -> Value {
        json!({
            "autoRun": false,
            "maxIterations": 3,
            "timeoutSecs": 60,
            "announce": "always",
            "requireEdits": false,
            "checks": [
                {"name": "test", "command": "cargo test"},
                {"name": "lint", "command": "cargo clippy", "optional": true},
            ]
        })
    }

    #[test]
    fn parse_reads_every_field() {
        let cfg = parse(&sample(), Vec::new(), Vec::new());
        assert!(!cfg.auto_run);
        assert_eq!(cfg.max_iterations, 2 + 1);
        assert_eq!(cfg.timeout, Duration::from_secs(60));
        assert_eq!(cfg.announce, Announce::Always);
        assert!(!cfg.require_edits);
        assert_eq!(cfg.checks.len(), 2);
        assert_eq!(cfg.checks[0].name, "test");
        assert!(cfg.checks[1].optional);
        assert!(cfg.warnings.is_empty());
    }

    #[test]
    fn parse_falls_back_to_defaults_on_garbage() {
        let cfg = parse(
            &json!({
                "autoRun": "yes",
                "maxIterations": -1,
                "timeoutSecs": 0,
                "announce": "loud",
                "checks": "nope"
            }),
            Vec::new(),
            Vec::new(),
        );
        assert!(cfg.auto_run);
        assert_eq!(cfg.max_iterations, DEFAULT_MAX_ITERATIONS);
        assert_eq!(cfg.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(cfg.announce, Announce::Fail);
        assert!(cfg.checks.is_empty());
        // announce 与 checks 各一条告警
        assert_eq!(cfg.warnings.len(), 2);
    }

    #[test]
    fn parse_drops_incomplete_checks_and_lets_later_duplicates_win() {
        let cfg = parse(
            &json!({
                "checks": [
                    {"name": "test"},
                    {"command": "cargo test"},
                    {"name": " ", "command": "x"},
                    {"name": "test", "command": "cargo test --all"},
                ]
            }),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(cfg.checks.len(), 1);
        assert_eq!(cfg.checks[0].command, "cargo test --all");
        // 三条被跳过/覆盖的告警
        assert_eq!(cfg.warnings.len(), 3);
    }

    #[test]
    fn overlay_replaces_arrays_and_keeps_untouched_keys() {
        let mut base = json!({"autoRun": false, "checks": [{"name": "a", "command": "a"}]});
        overlay(
            &mut base,
            &json!({"checks": [{"name": "b", "command": "b"}]}),
        );
        assert_eq!(base["autoRun"], json!(false));
        assert_eq!(base["checks"][0]["name"], json!("b"));
        assert_eq!(base["checks"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn panel_labels_round_trip_through_the_tables() {
        let cfg = Config::default();
        let settings = panel_settings(&cfg);
        assert_eq!(settings.len(), 5);
        // 缺省值必须能在档位表里找到（否则面板会把数字当标签显示）
        assert_eq!(settings[1].value, "2");
        assert_eq!(settings[2].value, "5m");
        assert_eq!(settings[3].value, "fail");
        assert_eq!(choice_num(ITERATIONS_TABLE, "off").unwrap(), 0.0);
        assert_eq!(choice_num(TIMEOUT_TABLE, "30m").unwrap(), 1800.0);
        assert!(choice_num(TIMEOUT_TABLE, "2m").is_err());
    }

    #[test]
    fn detect_reports_candidates_from_project_markers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(
            root.join("Makefile"),
            "build:\n\ttrue\n\ncheck:\n\tcargo test\n",
        )
        .unwrap();

        let found = detect(root);
        let commands: Vec<&str> = found.iter().map(|c| c.command.as_str()).collect();
        assert!(commands.contains(&"cargo test"));
        assert!(commands.contains(&"cargo clippy --all-targets"));
        assert!(commands.contains(&"make check"));
    }

    #[test]
    fn detect_reads_package_scripts_and_lockfile_runner() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("package.json"),
            r#"{"scripts": {"test": "vitest run", "lint": "eslint ."}}"#,
        )
        .unwrap();
        fs::write(root.join("pnpm-lock.yaml"), "").unwrap();

        let commands: Vec<String> = detect(root).into_iter().map(|c| c.command).collect();
        assert!(commands.contains(&"pnpm test".to_string()));
        assert!(commands.contains(&"pnpm run lint".to_string()));
        // 没有 typecheck 脚本就不给候选
        assert!(!commands.iter().any(|c| c.contains("typecheck")));
    }

    #[test]
    fn detect_is_empty_for_an_unknown_project() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect(dir.path()).is_empty());
    }

    #[test]
    fn describe_check_marks_optional_and_truncates_name() {
        let long = "n".repeat(MAX_NAME_CHARS + 10);
        let check = Check {
            name: long.clone(),
            command: "cargo   test\n --all".to_string(),
            optional: true,
        };
        let text = describe_check(&check);
        assert!(text.contains("(optional)"));
        assert!(text.contains('…'));
        // 命令折叠成单行
        assert!(!text.contains('\n'));
        assert!(text.contains("cargo test --all"));
    }

    #[test]
    fn scope_paths_point_at_the_two_locations() {
        let cwd = Path::new("/tmp/project");
        assert!(Scope::Global.path(cwd).ends_with("extensions/vet.json"));
        assert_eq!(
            Scope::Project.path(cwd),
            cwd.join(crate::PROJECT_SCOPE_NAME)
                .join("extensions")
                .join("vet.json")
        );
    }
}
