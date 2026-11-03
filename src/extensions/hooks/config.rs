//! hooks 的配置层：把 `agent_dir()/extensions/hooks.json` 解析成可执行的钩子表。
//!
//! 事件名对齐 Claude Code 的同名格式（`PreToolUse` / `PostToolUse` /
//! `UserPromptSubmit` / `Stop`），因此已有的 `hooks` 配置去掉外层包装即可搬进来。
//!
//! ```json
//! {
//!   "PreToolUse": [
//!     { "matcher": "bash|write", "hooks": [{ "type": "command", "command": "./check.sh", "timeout": 10 }] }
//!   ]
//! }
//! ```
//!
//! 校验不通过的条目跳过并把原因记进 [`HooksConfig::errors`]（`/hooks` 打印），
//! 一条坏钩子不该让整份配置失效。

use crate::core::settings_manager::agent_dir;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::Value;
use std::{fs, path::PathBuf, time::Duration};
use strum_macros::{EnumString, IntoStaticStr};

/// 配置文件名（位于 `agent_dir()/extensions/` 下）。
pub(super) const CONFIG_FILE: &str = "hooks.json";

/// 单条钩子的缺省超时（秒）：钩子在工具调用路径上同步等待，不能无限期占着。
pub(super) const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// 钩子的生命周期事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, IntoStaticStr)]
pub(super) enum HookEvent {
    /// 工具调用之前：可阻止、可改写入参。
    #[strum(serialize = "PreToolUse")]
    PreToolUse,
    /// 工具调用之后：可改写工具结果文本。
    #[strum(serialize = "PostToolUse")]
    PostToolUse,
    /// 用户提交输入时：可拒绝这次提交、可追加一段上下文。
    #[strum(serialize = "UserPromptSubmit")]
    UserPromptSubmit,
    /// 一回合即将结算时：可要求继续跑一轮（把 reason 作为新一轮的用户消息）。
    #[strum(serialize = "Stop")]
    Stop,
}

impl HookEvent {
    /// 全部支持的事件（用于报错文案与 `/hooks` 概览）。
    pub(super) const ALL: [HookEvent; 4] = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::UserPromptSubmit,
        HookEvent::Stop,
    ];

    /// 事件名（`Str` 侧唯一来源，用于信封的 `hook_event_name` 与展示）。
    pub(super) fn as_str(self) -> &'static str {
        self.into()
    }
}

/// 一条可执行的钩子命令。
#[derive(Debug, Clone)]
pub(super) struct HookCommand {
    /// 交给 shell 执行的命令串（Unix `sh -c`，Windows `cmd /C`）。
    pub command: String,
    /// 等待上限；超时即连同子进程一起杀掉并按「没有表态」处理。
    pub timeout: Duration,
}

/// 一个 matcher 分组：`matcher` 命中后依次跑组内全部命令。
#[derive(Debug, Clone)]
pub(super) struct HookGroup {
    /// 事件类型（决定信封字段与结果解释方式）。
    pub event: HookEvent,
    /// 工具名正则（大小写不敏感）；`None` = 匹配所有工具，非工具事件恒为 `None`。
    pub matcher: Option<Regex>,
    /// 配置里写的原始 matcher 文本，仅供 `/hooks` 展示。
    pub matcher_src: String,
    /// 组内命令，按配置顺序执行。
    pub commands: Vec<HookCommand>,
}

impl HookGroup {
    /// 该分组是否参与本事件下这个工具的调用；非工具事件只比事件类型。
    pub(super) fn matches(&self, event: HookEvent, tool_name: &str) -> bool {
        if self.event != event {
            return false;
        }
        match &self.matcher {
            Some(re) => re.is_match(tool_name),
            None => true,
        }
    }
}

/// 解析后的整份配置。
#[derive(Debug, Clone, Default)]
pub(super) struct HooksConfig {
    /// 全部有效分组（配置书写顺序决定执行顺序）。
    pub groups: Vec<HookGroup>,
    /// 被跳过的条目及原因（`/hooks` 打印，空表示没有配置问题）。
    pub errors: Vec<String>,
}

impl HooksConfig {
    /// 本事件下匹配 `tool_name` 的分组，按配置顺序。
    pub(super) fn matching(&self, event: HookEvent, tool_name: &str) -> Vec<&HookGroup> {
        self.groups
            .iter()
            .filter(|g| g.matches(event, tool_name))
            .collect()
    }

    /// 整份配置里一条命令都没有（此时各钩子回调直接返回，不做任何 IO）。
    pub(super) fn is_empty(&self) -> bool {
        self.groups.iter().all(|g| g.commands.is_empty())
    }
}

/// 配置文件路径：`agent_dir()/extensions/hooks.json`。
pub(super) fn config_path() -> PathBuf {
    agent_dir().join("extensions").join(CONFIG_FILE)
}

/// 读配置文件并解析。
///
/// 每次分发都重新读一次（钩子之后马上就要起子进程，一次几十字节的读盘不算开销，
/// 手改配置文件也能立刻生效）。文件缺失 = 没有钩子；读不出来或不是合法 JSON 时
/// 记一条 [`HooksConfig::errors`]，不当作配置为空静默吞掉。
pub(super) fn load() -> HooksConfig {
    let path = config_path();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HooksConfig::default(),
        Err(err) => return load_error(format!("{}: {err}", path.display())),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => parse(&value),
        Err(err) => load_error(format!("{}: {err}", path.display())),
    }
}

/// 构造一份「只有一条错误」的配置。
fn load_error(message: String) -> HooksConfig {
    HooksConfig {
        errors: vec![message],
        ..HooksConfig::default()
    }
}

/// 把配置 JSON 解析成钩子表；无法解释的条目记进 `errors` 后跳过。
pub(super) fn parse(value: &Value) -> HooksConfig {
    let mut config = HooksConfig::default();

    let Some(events) = value.as_object() else {
        if !value.is_null() {
            config.errors.push("config must be an object".to_string());
        }
        return config;
    };

    for (key, raw_groups) in events {
        let event = match key.parse::<HookEvent>() {
            Ok(event) => event,
            Err(_) => {
                let supported: Vec<&str> = HookEvent::ALL.iter().map(|e| e.as_str()).collect();
                config.errors.push(format!(
                    "unknown hook event `{key}` (supported: {})",
                    supported.join(", ")
                ));
                continue;
            }
        };

        let Some(groups) = raw_groups.as_array() else {
            config
                .errors
                .push(format!("`{key}` must be an array of hook groups"));
            continue;
        };

        for (idx, raw) in groups.iter().enumerate() {
            match parse_group(event, raw, idx) {
                Ok(group) => config.groups.push(group),
                Err(err) => config.errors.push(format!("{key}[{idx}]: {err}")),
            }
        }
    }

    config
}

/// 一个分组的原始形状（serde 只负责形状，语义校验在 [`parse_group`] 里）。
#[derive(Deserialize)]
struct RawGroup {
    /// 工具名正则；非工具事件也允许写，但从不参与匹配。
    matcher: Option<String>,
    /// 组内命令。
    hooks: Vec<RawHook>,
}

/// 一条命令的原始形状。
#[derive(Deserialize)]
struct RawHook {
    /// 钩子类型，当前只支持 `command`。
    #[serde(rename = "type")]
    kind: Option<String>,
    /// 要执行的命令串。
    command: Option<String>,
    /// 超时秒数；缺省 [`DEFAULT_TIMEOUT_SECS`]。
    timeout: Option<u64>,
}

/// 解析一个分组，失败返回给人看的原因。
fn parse_group(event: HookEvent, raw: &Value, _idx: usize) -> Result<HookGroup, String> {
    let raw: RawGroup =
        serde_json::from_value(raw.clone()).map_err(|e| format!("invalid hook group: {e}"))?;

    let (matcher, matcher_src) = match raw.matcher.as_deref().map(str::trim) {
        None | Some("") | Some("*") => (None, String::new()),
        Some(pattern) => {
            let re = RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("invalid matcher `{pattern}`: {e}"))?;
            (Some(re), pattern.to_string())
        }
    };

    let mut commands = Vec::with_capacity(raw.hooks.len());
    for hook in raw.hooks {
        let kind = hook.kind.unwrap_or_else(|| "command".to_string());
        if kind != "command" {
            return Err(format!("unsupported hook type `{kind}` (only `command`)"));
        }
        let command = hook.command.unwrap_or_default().trim().to_string();
        if command.is_empty() {
            return Err("hook `command` is empty".to_string());
        }
        let timeout = Duration::from_secs(hook.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1));
        commands.push(HookCommand { command, timeout });
    }

    if commands.is_empty() {
        return Err("hook group has no hooks".to_string());
    }

    Ok(HookGroup {
        event,
        matcher,
        matcher_src,
        commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 一条合法配置解析出分组、matcher 与超时。
    #[test]
    fn parses_matcher_and_timeout() {
        let cfg = parse(&json!({
            "PreToolUse": [
                { "matcher": "ba.*", "hooks": [
                    { "type": "command", "command": " echo ok ", "timeout": 3 }
                ] }
            ]
        }));
        assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
        assert_eq!(cfg.groups.len(), 1);
        let group = &cfg.groups[0];
        assert!(group.matches(HookEvent::PreToolUse, "bash"));
        assert!(!group.matches(HookEvent::PreToolUse, "read"));
        assert!(!group.matches(HookEvent::Stop, "bash"));
        assert_eq!(group.commands[0].command, "echo ok");
        assert_eq!(group.commands[0].timeout, Duration::from_secs(3));
    }

    /// 缺省 matcher 匹配所有工具；未知事件 / 非 command 类型 / 空命令都被记成错误而不是整份失效。
    #[test]
    fn reports_bad_entries_and_keeps_the_good_ones() {
        let cfg = parse(&json!({
            "PreToolUse": [
                { "hooks": [{ "command": "a" }] },
                { "hooks": [{ "type": "http", "command": "b" }] },
                { "hooks": [{ "command": "  " }] }
            ],
            "SessionEnd": [],
            "Stop": "nope"
        }));
        assert_eq!(cfg.groups.len(), 1);
        assert!(cfg.groups[0].matcher.is_none());
        assert_eq!(cfg.errors.len(), 4, "{:?}", cfg.errors);
        assert!(cfg.errors[0].contains("PreToolUse[1]"));
        assert!(cfg.errors.iter().any(|e| e.contains("SessionEnd")));
        assert!(cfg.errors.iter().any(|e| e.contains("Stop")));
    }

    /// 缺省（Null）不产生分组也不报错；非对象的段值报一条错。
    #[test]
    fn tolerates_missing_section() {
        assert!(parse(&Value::Null).errors.is_empty());
        assert_eq!(parse(&json!(1)).errors.len(), 1);
    }
}
