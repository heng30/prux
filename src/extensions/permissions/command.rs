//! `/permissions` 命令族：查看生效策略、管理会话放行、初始化配置、查看 review log。
//!
//! 命令都是**只读或只改本扩展自身状态**的，因此声明 `busy_safe`——流式输出期间也能用
//! （典型场景：agent 正被一条 `ask` 拦住，用户当场放行后让它重试）。

use super::{EXT, clear_grants, config, list_grants, review, session_cwd, set_grant};
use crate::{
    extensions::command_arg,
    modes::interactive::app::{App, MsgLevel},
};

/// `/permissions` 的默认日志条数。
const DEFAULT_LOG_LINES: usize = 10;

/// `/permissions` 命令入口。
///
/// 返回值恒为 `false`（不退出 TUI）：斜杠命令的 `true` 是「立刻结束 TUI」的信号，
/// 本命令族只是打印/改自身状态，绝不能返回它。
pub(super) fn handle(st: &mut App, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((head, tail)) => (head, tail.trim()),
        None => (arg, ""),
    };

    match sub {
        "" | "show" => show(st),
        "settings" => settings(st),
        "allow" => grant(st, rest, true),
        "drop" => grant(st, rest, false),
        "allowlist" => list(st),
        "init" => init(st, rest),
        "log" => log(st, rest),
        "reset" => {
            let n = clear_grants();
            st.push_msg(
                format!("permissions: cleared {n} session grant(s)"),
                MsgLevel::Success,
            );
        }
        other => st.push_msg(
            format!("permissions: unknown subcommand `{other}` (show|settings|allow|drop|allowlist|init|log|reset)"),
            MsgLevel::Warning,
        ),
    }
    false
}

/// `/permissions settings`：打开本扩展的设置面板（再执行一次关闭）。
///
/// 面板内容来自 [`Extension::settings`]，只有 `reviewLog` / `yoloMode` 两个开关；
/// 策略表是任意 pattern 字符串，面板的离散选项表达不了，仍走配置文件或 `allow` / `drop`。
fn settings(st: &mut App) {
    if st.ext_settings.is_open_for(EXT) {
        st.ext_settings.close();
    } else {
        st.ext_settings.open(EXT);
    }
}

/// 打印生效策略：来源文件、通用兜底、各表面、会话放行与告警。
fn show(st: &mut App) {
    let cwd = session_cwd();
    let settings = config::load(&cwd);
    let policy = &settings.policy;
    let mut lines: Vec<String> = Vec::new();

    lines.push("permissions: effective policy".to_string());
    if settings.sources.is_empty() {
        lines.push(format!(
            "  sources: (none) — run /permissions init to write {}",
            config::global_config_path().display()
        ));
    } else {
        for source in &settings.sources {
            lines.push(format!("  source: {}", source.display()));
        }
    }
    lines.push(format!(
        "  fallback permission[\"*\"]: {}",
        policy.universal.action
    ));

    if policy.has_path() {
        for (label, map) in [
            ("path", policy.path.as_ref()),
            ("path_read", policy.path_read.as_ref()),
            ("path_write", policy.path_write.as_ref()),
        ] {
            if let Some(map) = map {
                lines.push(format!("  {label}: {}", describe(map)));
            }
        }
    } else {
        lines.push("  path: (not configured)".to_string());
    }

    if policy.has_external() {
        for (label, map) in [
            ("external_directory", policy.external_directory.as_ref()),
            (
                "external_directory_read",
                policy.external_directory_read.as_ref(),
            ),
            (
                "external_directory_write",
                policy.external_directory_write.as_ref(),
            ),
        ] {
            if let Some(map) = map {
                lines.push(format!("  {label}: {}", describe(map)));
            }
        }
    } else {
        lines.push("  external_directory: (not configured — no CWD boundary)".to_string());
    }

    for tool in ["bash", "read", "write", "edit", "grep", "find", "ls", "mcp"] {
        if policy.has_surface(tool)
            && let Some(text) = config::describe_surface(policy, tool)
        {
            lines.push(format!("  {tool}: {text}"));
        }
    }
    for tool in policy.surface_names() {
        if ["bash", "read", "write", "edit", "grep", "find", "ls", "mcp"].contains(&tool.as_str())
            || tool == "path"
            || tool == "external_directory"
        {
            continue;
        }
        let Some(surface) = policy.surfaces.get(&tool) else {
            continue;
        };
        lines.push(format!(
            "  {tool}: {}",
            surface.patterns().into_iter().collect::<Vec<_>>().join(" ")
        ));
    }

    lines.push(format!(
        "  yoloMode: {}   review log: {} ({})",
        if settings.yolo_mode { "on" } else { "off" },
        if settings.review_log { "on" } else { "off" },
        config::review_log_path().display()
    ));

    let grants = list_grants();
    if grants.is_empty() {
        lines.push("  session grants: (none)".to_string());
    } else {
        lines.push(format!("  session grants: {}", grants.len()));
        for (surface, pattern) in grants {
            lines.push(format!("    {surface}  `{pattern}`"));
        }
    }

    for warning in &settings.warnings {
        lines.push(format!("  warning: {warning}"));
    }

    st.push_msg(lines.join("\n"), MsgLevel::Info);
}

/// 把一张 pattern 表压成一行：`* =allow  *.env =deny`。
fn describe(map: &super::policy::PatternMap) -> String {
    let mut parts = Vec::new();
    for pattern in map.patterns() {
        if let Some(hit) = map.evaluate(&[pattern.to_string()]) {
            parts.push(format!("{pattern} ={}", hit.rule.action));
        }
    }
    if parts.is_empty() {
        "(empty)".to_string()
    } else {
        parts.join("  ")
    }
}

/// `/permissions allow|drop <surface> <pattern>`。
fn grant(st: &mut App, rest: &str, allow: bool) {
    let Some((surface, pattern)) = rest.split_once(char::is_whitespace) else {
        st.push_msg(
            format!(
                "Usage: /permissions {} <surface> <pattern>   (surface: path | external_directory | bash | <tool>, pattern: e.g. \"sudo *\")",
                if allow { "allow" } else { "drop" }
            ),
            MsgLevel::Warning,
        );
        return;
    };
    let surface = surface.trim();
    let pattern = pattern.trim().trim_matches('"').trim_matches('\'');
    if surface.is_empty() || pattern.is_empty() {
        st.push_msg(
            "permissions: surface and pattern are both required".to_string(),
            MsgLevel::Warning,
        );
        return;
    }

    if set_grant(surface, pattern, allow) {
        if allow {
            st.push_msg(
                format!(
                    "permissions: granted `{pattern}` on {surface} for this session — ask the agent to retry"
                ),
                MsgLevel::Success,
            );
        } else {
            st.push_msg(
                format!("permissions: revoked `{pattern}` on {surface}"),
                MsgLevel::Success,
            );
        }
    } else if allow {
        st.push_msg(
            format!("permissions: `{pattern}` on {surface} was already granted"),
            MsgLevel::Info,
        );
    } else {
        st.push_msg(
            format!("permissions: no such grant: `{pattern}` on {surface}"),
            MsgLevel::Warning,
        );
    }
}

/// `/permissions allowlist`。
fn list(st: &mut App) {
    let grants = list_grants();
    if grants.is_empty() {
        st.push_msg("permissions: no session grants".to_string(), MsgLevel::Info);
        return;
    }
    let mut lines = vec![format!("permissions: {} session grant(s)", grants.len())];
    for (surface, pattern) in grants {
        lines.push(format!("  {surface}  `{pattern}`"));
    }
    st.push_msg(lines.join("\n"), MsgLevel::Info);
}

/// `/permissions init [--force]`：写一份保守缺省配置。
fn init(st: &mut App, rest: &str) {
    let force = rest.split_whitespace().any(|a| a == "--force");
    match config::write_default(force) {
        Ok(true) => st.push_msg(
            format!(
                "permissions: wrote default config to {}",
                config::global_config_path().display()
            ),
            MsgLevel::Success,
        ),
        Ok(false) => st.push_msg(
            format!(
                "permissions: {} already exists (use /permissions init --force to overwrite)",
                config::global_config_path().display()
            ),
            MsgLevel::Info,
        ),
        Err(e) => st.push_msg(
            format!("permissions: cannot write config: {e}"),
            MsgLevel::Error,
        ),
    }
}

/// `/permissions log [n]`：打印最近 n 条 review log（缺省 [`DEFAULT_LOG_LINES`]）。
fn log(st: &mut App, rest: &str) {
    let limit = rest
        .split_whitespace()
        .next()
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LOG_LINES);
    let lines = review::tail(limit);
    let path = config::review_log_path();
    if lines.is_empty() {
        st.push_msg(
            format!("permissions: no review log entries at {}", path.display()),
            MsgLevel::Info,
        );
        return;
    }
    let mut out = vec![format!(
        "permissions: last {} entries ({})",
        lines.len(),
        path.display()
    )];
    out.extend(lines);
    st.push_msg(out.join("\n"), MsgLevel::Info);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_flattens_pattern_map() {
        let (map, bad) = super::super::policy::PatternMap::parse(&serde_json::json!({
            "*": "allow", "*.env": "deny"
        }));
        assert!(bad.is_empty());
        assert_eq!(describe(&map), "* =allow  *.env =deny");
    }
}
