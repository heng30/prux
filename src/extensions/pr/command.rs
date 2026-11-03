//! `/pr` 命令族：看配置与最近一次交付、手动跑一次。
//!
//! 命令只读写本扩展自身的配置与运行态，因此声明 `busy_safe`。

use super::{run_now, status_text};
use crate::{
    core::extensions::{CommandHost, SubcommandDef, UiNotifyLevel},
    extensions::{command_arg, util::notify_text},
};

/// `/pr` 子命令（顺序 = 输入框候选顺序，与 [`handle`] 的 match 分支一一对应）。
pub(super) const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "status",
        description: "Show the config, the session branch / PR and the last result",
    },
    SubcommandDef {
        name: "run",
        description: "Commit, push, open the pull request and report the CI result",
    },
];

/// `/pr` 命令入口。返回值恒为 `false`（不退出 TUI）。
pub(super) fn handle(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let sub = match arg.split_once(char::is_whitespace) {
        Some((head, _)) => head,
        None => arg,
    };

    match sub {
        "" | "status" => host.push_message(&status_text(), UiNotifyLevel::Info),
        "run" => run_async(host),
        other => host.push_message(
            &format!("pr: unknown subcommand `{other}` (status|run)"),
            UiNotifyLevel::Warning,
        ),
    }
    false
}

/// `/pr run`：交付一次（后台线程，结果回聊天区）。
fn run_async(host: &mut dyn CommandHost) {
    // 提交信息要在后台线程里生成，模型得先在这里拿（宿主只在同步回边可得）。
    let model = host.current_model();
    host.push_message(
        "pr: delivering now — results land here when done",
        UiNotifyLevel::Info,
    );
    // 等 CI 可能是分钟级，不能卡住事件循环：扔到独立线程。
    std::thread::spawn(move || {
        let text = run_now(model);
        notify_text(&format!("pr: {text}"), UiNotifyLevel::Info);
    });
}
