//! 配置层：全局 + 项目两处 `config.json` 的读取、深合合并、策略解析与落盘。
//!
//! 位置（与其它扩展同构）：
//! - 全局：`agent_dir()/extensions/permissions/config.json`
//! - 项目：`<cwd>/.prux/extensions/permissions/config.json`，**仅在项目受信任时加载**
//!   （未受信的仓库不能通过提交一份配置来放宽本机策略）。
//!
//! 合并是「深合浅覆盖」：项目覆盖全局；两边都是对象时逐键递归合并（`permission` 下的
//! 每个表面各自合并 pattern 表），类型不同时高优先级的那份整个生效。
//!
//! 策略解析失败**不放行**：认不出来的规则值会被跳过并记进 [`Settings::warnings`]，
//! 而该表面剩下的规则照旧参与求值；整个文件读不出来时策略为空（等价于全部走
//! `permission["*"]` 兜底，缺省 `ask`）。

use super::policy::{Action, PatternMap, Policy, RuleValue, Surface};
use crate::core::{
    config_cache::ConfigCache, project_trust::is_project_trusted, settings_manager::agent_dir,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

/// 全局配置文件名（位于 `agent_dir()/extensions/permissions/` 下）。
pub(super) const CONFIG_FILE: &str = "config.json";

/// 配置版本戳；升级后缺键会被回填，版本不符时同样补齐。
const CONFIG_VERSION: i64 = 1;

/// 全局配置读写锁（锁序恒为「RwLock → 缓存 Mutex」）。
static GLOBAL_LOCK: RwLock<()> = RwLock::new(());

/// 项目配置读写锁。
static PROJECT_LOCK: RwLock<()> = RwLock::new(());

/// 全局配置的读缓存（按 `(path, mtime, len)` 自动失效，手改文件即时生效）。
static GLOBAL_CACHE: ConfigCache = ConfigCache::new();

/// 项目配置的读缓存。
static PROJECT_CACHE: ConfigCache = ConfigCache::new();

/// 一份生效配置：策略 + 两个运行开关 + 来源与告警。
#[derive(Debug, Clone)]
pub(super) struct Settings {
    /// 合并后的生效策略。
    pub policy: Policy,
    /// 是否把每次判定写进 review log（`permissionReviewLog`，缺省开）。
    pub review_log: bool,
    /// 是否把 `ask` 当作 `allow`（`yoloMode`，缺省关；打开等于放弃确认语义）。
    pub yolo_mode: bool,
    /// 实际参与合并的配置文件（`/permissions show` 列出）。
    pub sources: Vec<PathBuf>,
    /// 解析过程中的告警（无效规则值、被跳过的项目配置等）。
    pub warnings: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            policy: Policy {
                universal: RuleValue::new(Action::Ask),
                ..Policy::default()
            },
            review_log: true,
            yolo_mode: false,
            sources: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// 全局配置文件路径。
pub(super) fn global_config_path() -> PathBuf {
    agent_dir()
        .join("extensions")
        .join("permissions")
        .join(CONFIG_FILE)
}

/// 项目配置文件路径（`<cwd>/.prux/extensions/permissions/config.json`）。
pub(super) fn project_config_path(cwd: &Path) -> PathBuf {
    cwd.join(crate::PROJECT_SCOPE_NAME)
        .join("extensions")
        .join("permissions")
        .join(CONFIG_FILE)
}

/// review log 文件路径（`agent_dir()/extensions/permissions/logs/permissions-review.jsonl`）。
pub(super) fn review_log_path() -> PathBuf {
    agent_dir()
        .join("extensions")
        .join("permissions")
        .join("logs")
        .join("permissions-review.jsonl")
}

/// 读取一个配置文件（缺失 / 损坏都返回空对象，由调用方决定语义）。
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

/// 深合合并：`over` 覆盖 `base`。
///
/// 两边都是对象时逐键递归；否则 `over` 整个替换 `base`。这样「项目配置里写一个字符串」
/// 能整体改写全局的一张 pattern 表，而写一个对象时只合并它点到的条目。
fn merge_json(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (key, value) in o {
                match b.get_mut(key) {
                    Some(slot) => merge_json(slot, value),
                    None => {
                        b.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (slot, value) => *slot = value.clone(),
    }
}

/// 加载生效配置：全局 + （受信时的）项目，合并后解析成 [`Settings`]。
pub(super) fn load(cwd: &Path) -> Settings {
    let global_path = global_config_path();
    let global = read_json(&GLOBAL_CACHE, &GLOBAL_LOCK, &global_path);

    let mut merged = global.clone();
    let mut sources = Vec::new();
    let mut warnings = Vec::new();

    if !global.as_object().is_some_and(|o| o.is_empty()) {
        sources.push(global_path);
    }

    let project_path = project_config_path(cwd);
    if project_path.is_file() {
        if is_project_trusted(cwd, &agent_dir()) {
            let project = read_json(&PROJECT_CACHE, &PROJECT_LOCK, &project_path);
            merge_json(&mut merged, &project);
            sources.push(project_path);
        } else {
            warnings.push(format!(
                "skipped untrusted project config: {}",
                project_path.display()
            ));
        }
    }

    parse_settings(&merged, sources, warnings)
}

/// 把合并后的 JSON 解析成 [`Settings`]（纯函数，便于测试）。
fn parse_settings(root: &Value, sources: Vec<PathBuf>, mut warnings: Vec<String>) -> Settings {
    let review_log = root
        .get("permissionReviewLog")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let yolo_mode = root
        .get("yoloMode")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let Some(permission) = root.get("permission").and_then(Value::as_object) else {
        return Settings {
            policy: Policy::default(),
            review_log,
            yolo_mode,
            sources,
            warnings,
        };
    };

    let mut policy = Policy::default();
    for (key, value) in permission {
        match key.as_str() {
            "*" => match RuleValue::parse(value) {
                Some(rule) => policy.universal = rule,
                None => warnings.push("permission[\"*\"]: invalid rule value".to_string()),
            },
            "path"
            | "path_read"
            | "path_write"
            | "external_directory"
            | "external_directory_read"
            | "external_directory_write" => {
                let map = parse_pattern_map(value, key, &mut warnings);
                match key.as_str() {
                    "path" => policy.path = Some(map),
                    "path_read" => policy.path_read = Some(map),
                    "path_write" => policy.path_write = Some(map),
                    "external_directory" => policy.external_directory = Some(map),
                    "external_directory_read" => policy.external_directory_read = Some(map),
                    _ => policy.external_directory_write = Some(map),
                }
            }
            tool => {
                let surface = match value {
                    Value::String(_) | Value::Object(_) if !is_pattern_map(value) => {
                        match RuleValue::parse(value) {
                            Some(rule) => Surface::CatchAll(rule),
                            None => {
                                warnings.push(format!("permission[{tool}]: invalid rule value"));
                                continue;
                            }
                        }
                    }
                    _ => Surface::Patterns(parse_pattern_map(value, tool, &mut warnings)),
                };
                policy.surfaces.insert(tool.to_string(), surface);
            }
        }
    }

    Settings {
        policy,
        review_log,
        yolo_mode,
        sources,
        warnings,
    }
}

/// 该值是否是一张 pattern 表（对象里不含 `action` 键）。
///
/// `{"action": "deny", "reason": "..."}` 是规则值，`{"*": "deny", "git *": "ask"}` 是表。
/// 判据就是有没有 `action`——配置里不会有人把 `action` 当成待匹配的模式名。
fn is_pattern_map(value: &Value) -> bool {
    match value {
        Value::Object(map) => !map.contains_key("action"),
        _ => false,
    }
}

/// 解析一张 pattern 表；字符串简写 `"allow"` 等价于 `{"*": "allow"}`。
fn parse_pattern_map(value: &Value, label: &str, warnings: &mut Vec<String>) -> PatternMap {
    if let Some(rule) = RuleValue::parse(value) {
        let mut map = PatternMap::default();
        map.set("*", rule);
        return map;
    }
    let (map, bad) = PatternMap::parse(value);
    for pattern in bad {
        warnings.push(format!(
            "permission[{label}][{pattern}]: invalid rule value"
        ));
    }
    map
}

/// 缺省配置（`/permissions init` 写入的内容）。
///
/// 取向：**开箱不打断正常工作流**，只拦两类真正危险的动作——
/// 1. 敏感文件（`.env`、私钥、`~/.ssh`、`~/.aws`、`~/.gnupg`）在**所有**工具与 bash 里一律 deny；
/// 2. 灾难性 bash（`rm -rf /`、`mkfs`、`dd`、关机、强推、`curl … | sh`）deny 或 ask。
///
/// 不设 `external_directory` 边界（不配置 = 不限制），否则第一次访问 `~/.cargo` 就会被拦。
/// 需要边界时按 [`assets/docs/extensions/permissions.md`] 的配方自行加。
pub(super) fn default_config() -> Value {
    json!({
        "configVersion": CONFIG_VERSION,
        "permissionReviewLog": true,
        "yoloMode": false,
        "permission": {
            "*": "allow",
            "path": {
                "*": "allow",
                "*.env": "deny",
                "*.env.*": "deny",
                "*.env.example": "allow",
                "~/.ssh/*": "deny",
                "~/.aws/*": "deny",
                "~/.gnupg/*": "deny",
                "*.pem": "deny",
                "*.key": "deny",
                "*id_rsa*": "deny"
            },
            "bash": {
                "*": "allow",
                "rm -rf /*": "deny",
                "rm -fr /*": "deny",
                "rm -rf ~*": "deny",
                "mkfs*": "deny",
                "dd if=*": "ask",
                "shutdown*": "deny",
                "reboot*": "deny",
                "git push --force*": "ask",
                "git push -f*": "ask",
                "git reset --hard*": "ask",
                "git clean -f*": "ask",
                "chmod 777 *": "ask",
                "chown *": "ask",
                "sudo *": "ask",
                "eval *": "ask",
                "bash -c *": "ask",
                "sh -c *": "ask",
                "xargs *": "ask"
            }
        }
    })
}

/// 写入一份配置（`/permissions init`）。
///
/// `overwrite = false` 且目标已存在时返回 `Ok(false)`（不覆盖用户已有配置）。
pub(super) fn write_default(overwrite: bool) -> std::io::Result<bool> {
    let path = global_config_path();
    if path.is_file() && !overwrite {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&default_config()).unwrap_or_default();
    fs::write(&path, format!("{text}\n"))?;
    let _guard = GLOBAL_LOCK.write().unwrap_or_else(|e| e.into_inner());
    GLOBAL_CACHE.invalidate();
    Ok(true)
}

/// 整体改写某个标量开关（`permissionReviewLog` / `yoloMode`），保留其它键。
///
/// 文件不存在时以缺省配置为底，因此从设置面板打开开关也会顺手把缺省策略落盘。
pub(super) fn set_switch(key: &str, value: bool) -> std::io::Result<()> {
    let path = global_config_path();
    let mut root = read_json(&GLOBAL_CACHE, &GLOBAL_LOCK, &path);
    if !root.is_object() {
        root = default_config();
    }
    if let Value::Object(map) = &mut root {
        map.insert(key.to_string(), json!(value));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&root).unwrap_or_default();
    fs::write(&path, format!("{text}\n"))?;
    let _guard = GLOBAL_LOCK.write().unwrap_or_else(|e| e.into_inner());
    GLOBAL_CACHE.invalidate();
    Ok(())
}

/// 列出策略里所有「显式写了 `action` 对象」的规则，供 `/permissions show` 展示理由。
pub(super) fn describe_surface(policy: &Policy, surface: &str) -> Option<String> {
    let s = policy.surfaces.get(surface)?;
    let mut parts = Vec::new();
    match s {
        Surface::CatchAll(rule) => parts.push(format!("*={}", rule.action)),
        Surface::Patterns(map) => {
            for pattern in map.patterns() {
                if let Some(hit) = map.evaluate(&[pattern.to_string()]) {
                    parts.push(format!("{pattern}={}", hit.rule.action));
                }
            }
        }
    }
    Some(parts.join("  "))
}

/// 测试用：只解析策略部分（跳过来源与告警）。
#[cfg(test)]
pub(super) fn parse_settings_for_tests(root: &Value) -> Policy {
    parse_settings(root, Vec::new(), Vec::new()).policy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_overrides_scalars_and_merges_maps() {
        let mut base =
            json!({"permission": {"read": "allow", "bash": {"*": "allow", "rm *": "ask"}}});
        let over = json!({"permission": {"bash": {"rm *": "deny"}}});
        merge_json(&mut base, &over);
        assert_eq!(base["permission"]["read"], json!("allow"));
        assert_eq!(base["permission"]["bash"]["*"], json!("allow"));
        assert_eq!(base["permission"]["bash"]["rm *"], json!("deny"));
    }

    #[test]
    fn merge_replaces_when_types_differ() {
        let mut base = json!({"permission": {"bash": {"*": "allow"}}});
        let over = json!({"permission": {"bash": "deny"}});
        merge_json(&mut base, &over);
        assert_eq!(base["permission"]["bash"], json!("deny"));
    }

    #[test]
    fn parses_policy_surfaces() {
        let settings = parse_settings(
            &json!({"permission": {
                "*": "allow",
                "read": "deny",
                "write": {"*": "deny", "src/*": "allow"},
                "path": {"*": "allow", "*.env": "deny"},
                "external_directory": "ask"
            }}),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(settings.policy.universal.action, Action::Allow);
        assert!(settings.policy.surface_all_deny("read"));
        assert!(!settings.policy.surface_all_deny("write"));
        assert_eq!(
            settings
                .policy
                .path_decision(
                    &["src/.env".to_string()],
                    super::super::policy::Direction::Read
                )
                .expect("hit")
                .action,
            Action::Deny
        );
        assert!(settings.policy.has_external());
    }

    #[test]
    fn invalid_rule_values_warn_and_do_not_allow() {
        let settings = parse_settings(
            &json!({"permission": {"bash": {"*": "bogus"}}}),
            Vec::new(),
            Vec::new(),
        );
        assert!(settings.warnings.iter().any(|w| w.contains("bash")));
        // 无效值被跳过 ⇒ 该表面没有规则命中 ⇒ 落到通用兜底（缺省 ask，不放行）。
        assert!(
            settings
                .policy
                .bash_decision(&["rm -rf /".to_string()])
                .is_none()
        );
        assert_eq!(settings.policy.universal.action, Action::Ask);
    }

    #[test]
    fn string_shorthand_becomes_catch_all() {
        let settings = parse_settings(
            &json!({"permission": {"grep": "deny"}}),
            Vec::new(),
            Vec::new(),
        );
        let decision = settings
            .policy
            .tool_decision("grep", &["*".to_string()])
            .expect("catch-all hit");
        assert_eq!(decision.action, Action::Deny);
    }

    #[test]
    fn object_with_action_is_a_rule_value_not_a_map() {
        assert!(!is_pattern_map(&json!({"action": "deny"})));
        assert!(is_pattern_map(&json!({"*": "deny"})));
        let settings = parse_settings(
            &json!({"permission": {"bash": {"action": "deny", "reason": "no shell"}}}),
            Vec::new(),
            Vec::new(),
        );
        let decision = settings
            .policy
            .bash_decision(&["ls".to_string()])
            .expect("hit");
        assert_eq!(decision.action, Action::Deny);
        assert_eq!(decision.reason.as_deref(), Some("no shell"));
    }

    #[test]
    fn write_default_round_trips_and_respects_existing() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let cwd = std::env::temp_dir();
        assert!(write_default(false).expect("write default config"));
        assert!(global_config_path().is_file());

        // 写下去的缺省配置能被 load 读回，且真的拦 `.env`。
        let settings = load(&cwd);
        assert!(!settings.sources.is_empty());
        assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
        assert_eq!(
            settings
                .policy
                .path_decision(&[".env".to_string()], super::super::policy::Direction::Read)
                .expect("hit")
                .action,
            Action::Deny
        );

        // 已存在时不覆盖；`--force` 才覆盖。
        assert!(!write_default(false).expect("keep existing"));
        assert!(write_default(true).expect("force overwrite"));
    }

    #[test]
    fn default_config_protects_sensitive_files() {
        let settings = parse_settings(&default_config(), Vec::new(), Vec::new());
        let dir = super::super::policy::Direction::Read;
        assert_eq!(
            settings
                .policy
                .path_decision(&[".env".to_string()], dir)
                .expect("hit")
                .action,
            Action::Deny
        );
        assert_eq!(
            settings
                .policy
                .path_decision(&[".env.example".to_string()], dir)
                .expect("hit")
                .action,
            Action::Allow
        );
        assert!(settings.warnings.is_empty());
    }
}
