//! `/vet` 命令族：跑检查、看配置与最近结果、按项目标记探测候选、增删检查项。
//!
//! 命令只读写本扩展自身的配置与运行态，因此声明 `busy_safe`——流式输出期间也能用
//! （典型场景：agent 正被一轮检查打断，用户想先看看配了什么）。

use super::{
    ADD_OPTIONAL, ADD_PROJECT, DETECT_WRITE, EXT, RunGuard, config, current_cwd, last_run_label,
    record_run, reset_turn_state, run, state,
};
use crate::{
    core::extensions::{CommandHost, SubcommandDef, UiNotifyLevel},
    extensions::{command_arg, util::notify_text},
};
use config::{Check, Scope};

/// `/vet` 子命令（顺序 = 输入框候选顺序，与 [`handle`] 的 match 分支一一对应）。
pub(super) const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "settings",
        description: "Open the vet settings panel (same as `/vet` with no arguments)",
    },
    SubcommandDef {
        name: "run",
        description: "Run every configured check now and print the result",
    },
    SubcommandDef {
        name: "status",
        description: "Show the effective configuration, checks and the last result",
    },
    SubcommandDef {
        name: "detect",
        description: "Suggest checks from project markers: /vet detect [--write]",
    },
    SubcommandDef {
        name: "add",
        description: "Add or replace a check: /vet add [--project] [--optional] <name> <command>",
    },
    SubcommandDef {
        name: "remove",
        description: "Delete a check: /vet remove [--project] <name>",
    },
    SubcommandDef {
        name: "reset",
        description: "Clear this turn's check state (attempt counter and failure fingerprint)",
    },
];

/// `/vet` 命令入口。返回值恒为 `false`（不退出 TUI：斜杠命令的 `true` 是结束 TUI 的信号）。
pub(super) fn handle(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((head, tail)) => (head, tail.trim()),
        None => (arg, ""),
    };

    match sub {
        "" | "settings" => toggle_settings(host),
        "run" => run_now(host),
        "status" => status(host),
        "detect" => detect(host, rest),
        "add" => add(host, rest),
        "remove" => remove(host, rest),
        "reset" => {
            reset_turn_state();
            host.push_message(
                "vet: cleared this turn's check state",
                UiNotifyLevel::Success,
            );
        }
        other => host.push_message(
            &format!(
                "vet: unknown subcommand `{other}` (settings|run|status|detect|add|remove|reset)"
            ),
            UiNotifyLevel::Warning,
        ),
    }
    false
}

/// `/vet` / `/vet settings`：打开（或关闭）本扩展的设置面板。
fn toggle_settings(host: &mut dyn CommandHost) {
    host.open_extension_settings(EXT);
}

/// `/vet run`：手动跑一遍全部检查（后台线程，结果回聊天区）。
fn run_now(host: &mut dyn CommandHost) {
    let cwd = current_cwd();
    let cfg = config::load(&cwd);
    if cfg.checks.is_empty() {
        host.push_message(
            &format!(
                "vet: no checks configured. Run /vet detect to see candidates, or /vet add <name> \
                 <command>. Config: {}",
                config::global_config_path().display()
            ),
            UiNotifyLevel::Warning,
        );
        return;
    }

    let Some(guard) = RunGuard::acquire() else {
        host.push_message("vet: checks are already running", UiNotifyLevel::Warning);
        return;
    };

    let checks = cfg.checks.clone();
    let timeout = cfg.timeout;
    host.push_message(
        &format!(
            "vet: running {} check(s) in the background — results land here when done",
            checks.len()
        ),
        UiNotifyLevel::Info,
    );

    // 手动路径不能卡住事件循环：扔到独立线程，结果经 UI 队列回聊天区。
    std::thread::spawn(move || {
        let _guard = guard;
        let results = run::run_all(&cwd, &checks, timeout);
        record_run(&results);
        let level = if run::has_blocking_failure(&results) {
            UiNotifyLevel::Warning
        } else {
            UiNotifyLevel::Success
        };
        notify_text(&run::detailed_report(&results).join("\n"), level);
    });
}

/// `/vet status`：配置来源、开关、检查项、本回合状态与最近结果。
fn status(host: &mut dyn CommandHost) {
    let cwd = current_cwd();
    let cfg = config::load(&cwd);

    let announce: &'static str = cfg.announce.into();
    let iterations = if cfg.max_iterations == 0 {
        "off".to_string()
    } else {
        cfg.max_iterations.to_string()
    };
    let mut lines = vec![
        "vet: configuration".to_string(),
        format!(
            "  autoRun: {} · maxIterations: {iterations} · timeout: {}s · announce: {announce} · \
             requireEdits: {}",
            on_off(cfg.auto_run),
            cfg.timeout.as_secs(),
            on_off(cfg.require_edits),
        ),
    ];

    if cfg.sources.is_empty() {
        lines.push(format!(
            "  source: (none) — run /vet detect --write or /vet add to configure {}",
            config::global_config_path().display()
        ));
    } else {
        for source in &cfg.sources {
            lines.push(format!("  source: {}", source.display()));
        }
    }

    if cfg.checks.is_empty() {
        lines.push("  checks: (none)".to_string());
    } else {
        for check in &cfg.checks {
            lines.push(format!("  check: {}", config::describe_check(check)));
        }
    }
    for warning in &cfg.warnings {
        lines.push(format!("  warning: {warning}"));
    }

    let st_state = state();
    lines.push(format!(
        "  this turn: {}/{} resume(s), edits seen: {}",
        st_state.iteration,
        cfg.max_iterations,
        if st_state.dirty { "yes" } else { "no" }
    ));
    lines.push(format!(
        "  last run: {} — {}",
        last_run_label(st_state.last_run_at),
        st_state.last_verdict.as_deref().unwrap_or("no result yet")
    ));
    if st_state.runs > 0 {
        lines.push(format!("  total runs: {}", st_state.runs));
    }
    drop(st_state);

    host.push_message(&lines.join("\n"), UiNotifyLevel::Info);
}

/// `/vet detect [--write]`：按项目标记给出候选检查，`--write` 直接写进全局配置。
fn detect(host: &mut dyn CommandHost, rest: &str) {
    let write = rest.split_whitespace().any(|t| t == DETECT_WRITE);
    let cwd = current_cwd();
    let candidates = config::detect(&cwd);

    if candidates.is_empty() {
        host.push_message(
            &format!(
                "vet: no project markers recognized in {}. Add checks yourself: /vet add <name> \
                 <command>",
                cwd.display()
            ),
            UiNotifyLevel::Warning,
        );
        return;
    }

    let mut lines = vec![if write {
        "vet: writing detected checks".to_string()
    } else {
        "vet: detected checks (confirm with /vet add <name> <command>)".to_string()
    }];
    for check in &candidates {
        lines.push(format!("  {}", config::describe_check(check)));
    }

    if write {
        let mut added = 0usize;
        let mut failed = 0usize;
        for check in &candidates {
            match config::upsert_check(Scope::Global, &cwd, check) {
                Ok(_) => added += 1,
                Err(_) => failed += 1,
            }
        }
        lines.push(format!(
            "  wrote {added} check(s) to {}",
            config::global_config_path().display()
        ));
        if failed > 0 {
            lines.push(format!("  {failed} check(s) could not be written"));
        }
    }

    host.push_message(
        &lines.join("\n"),
        if write {
            UiNotifyLevel::Success
        } else {
            UiNotifyLevel::Info
        },
    );
}

/// `/vet add [--project] [--optional] <name> <command...>`：新增或覆盖一条检查。
fn add(host: &mut dyn CommandHost, rest: &str) {
    let (flags, positional) = split_flags(rest, &[ADD_PROJECT, ADD_OPTIONAL]);
    let Some(name) = positional.first().cloned() else {
        host.push_message(
            "vet: usage: /vet add [--project] [--optional] <name> <command>",
            UiNotifyLevel::Warning,
        );
        return;
    };
    let command = positional[1..].join(" ");
    if command.trim().is_empty() {
        host.push_message(
            "vet: missing command. Usage: /vet add [--project] [--optional] <name> <command>",
            UiNotifyLevel::Warning,
        );
        return;
    }

    let scope = if flags.contains(&ADD_PROJECT) {
        Scope::Project
    } else {
        Scope::Global
    };
    let check = Check {
        name,
        command,
        optional: flags.contains(&ADD_OPTIONAL),
    };

    let cwd = current_cwd();
    match config::upsert_check(scope, &cwd, &check) {
        Ok(replaced) => host.push_message(
            &format!(
                "vet: {} `{}` in the {} config",
                if replaced { "replaced" } else { "added" },
                check.name,
                scope.as_str()
            ),
            UiNotifyLevel::Success,
        ),
        Err(err) => host.push_message(&format!("vet: {err}"), UiNotifyLevel::Error),
    }
}

/// `/vet remove [--project] <name>`：删除一条检查。
fn remove(host: &mut dyn CommandHost, rest: &str) {
    let (flags, positional) = split_flags(rest, &[ADD_PROJECT]);
    let Some(name) = positional.first().cloned() else {
        host.push_message(
            "vet: usage: /vet remove [--project] <name>",
            UiNotifyLevel::Warning,
        );
        return;
    };

    let scope = if flags.contains(&ADD_PROJECT) {
        Scope::Project
    } else {
        Scope::Global
    };

    let cwd = current_cwd();
    match config::remove_check(scope, &cwd, &name) {
        Ok(true) => host.push_message(
            &format!("vet: removed `{name}` from the {} config", scope.as_str()),
            UiNotifyLevel::Success,
        ),
        Ok(false) => host.push_message(
            &format!(
                "vet: no check named `{name}` in the {} config",
                scope.as_str()
            ),
            UiNotifyLevel::Warning,
        ),
        Err(err) => host.push_message(&format!("vet: {err}"), UiNotifyLevel::Error),
    }
}

/// 拆出已知 flag 与位置参数（`--` 之后的内容一律当位置参数）。
fn split_flags(rest: &str, known: &[&'static str]) -> (Vec<&'static str>, Vec<String>) {
    let mut flags: Vec<&'static str> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut literal = false;
    for token in rest.split_whitespace() {
        if !literal && token == "--" {
            literal = true;
            continue;
        }
        if !literal && let Some(flag) = known.iter().find(|f| **f == token) {
            flags.push(flag);
            continue;
        }
        positional.push(token.to_string());
    }
    (flags, positional)
}

/// 布尔开关的展示文本。
fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_flags_separates_known_flags_from_positionals() {
        let (flags, positional) = split_flags(
            "--project --optional test cargo test --all",
            &[ADD_PROJECT, ADD_OPTIONAL],
        );
        assert_eq!(flags, vec![ADD_PROJECT, ADD_OPTIONAL]);
        assert_eq!(
            positional,
            vec!["test", "cargo", "test", "--all"],
            "未知的 `--all` 属于命令本身，不能被当成 flag 吃掉"
        );
    }

    #[test]
    fn split_flags_treats_everything_after_double_dash_as_positional() {
        let (flags, positional) = split_flags("-- --project", &[ADD_PROJECT]);
        assert!(flags.is_empty());
        assert_eq!(positional, vec!["--project"]);
    }

    #[test]
    fn on_off_is_stable() {
        assert_eq!(on_off(true), "on");
        assert_eq!(on_off(false), "off");
    }

    #[test]
    fn subcommands_cover_every_match_arm() {
        // handle 的 match 分支（含无参）与候选列表必须一致
        let names: Vec<&str> = SUBCOMMANDS.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "settings", "run", "status", "detect", "add", "remove", "reset"
            ]
        );
    }
}
