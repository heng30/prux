//! `/delivery` 命令族：看配置与最近一次交付、跑一次交付、切 stage。
//!
//! 命令只读写本扩展自身的配置与运行态，因此声明 `busy_safe`。

use super::{config, run_now, status_text};
use crate::{
    core::extensions::{CommandHost, SubcommandDef, UiNotifyLevel},
    extensions::{command_arg, util::notify_text},
};

/// `/delivery` 子命令（顺序 = 输入框候选顺序，与 [`handle`] 的 match 分支一一对应）。
pub(super) const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "status",
        description: "Show the delivery config, the session branch / PR and the last result",
    },
    SubcommandDef {
        name: "run",
        description: "Commit, push and open / update the PR now, then report the CI result",
    },
    SubcommandDef {
        name: "stage",
        description: "Where to stop: /delivery stage branch|push|pr",
    },
];

/// `/delivery` 命令入口。返回值恒为 `false`（不退出 TUI）。
pub(super) fn handle(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((head, tail)) => (head, tail.trim()),
        None => (arg, ""),
    };

    match sub {
        "" | "status" => host.push_message(&status_text(), UiNotifyLevel::Info),
        "run" => run_async(host),
        "stage" => set_stage(host, rest),
        other => host.push_message(
            &format!("delivery: unknown subcommand `{other}` (status|run|stage)"),
            UiNotifyLevel::Warning,
        ),
    }
    false
}

/// `/delivery run`：手动交付一次（后台线程，结果回聊天区）。
fn run_async(host: &mut dyn CommandHost) {
    host.push_message(
        "delivery: delivering now — results land here when done",
        UiNotifyLevel::Info,
    );
    // 手动路径不能卡住事件循环（等 CI 可能是分钟级）：扔到独立线程。
    std::thread::spawn(|| {
        let text = run_now();
        notify_text(&format!("delivery: {text}"), UiNotifyLevel::Info);
    });
}

/// `/delivery stage <branch|push|pr>`：写配置。
fn set_stage(host: &mut dyn CommandHost, rest: &str) {
    let Some(stage) = config::Stage::parse(rest) else {
        host.push_message(
            "delivery: usage /delivery stage branch|push|pr",
            UiNotifyLevel::Warning,
        );
        return;
    };
    let mut cfg = config::load();
    cfg.stage = stage;
    match config::write(&cfg) {
        Ok(()) => host.push_message(
            &format!("delivery: stage set to {}", stage.as_label()),
            UiNotifyLevel::Success,
        ),
        Err(err) => host.push_message(
            &format!("delivery: write config failed: {err}"),
            UiNotifyLevel::Warning,
        ),
    }
}
